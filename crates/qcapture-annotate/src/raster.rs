//! CPU rasterizer: timed vector strokes -> straight-alpha RGBA overlay.
//!
//! Design notes:
//! - Overlay is straight (non-premultiplied) RGBA at feed size; blending onto
//!   the top-down BGRA video frame is one src-over pass over the coverage
//!   region (union of applied stroke bboxes). The overlay persists while video
//!   changes, so every frame re-blends coverage — the skip is only "no strokes
//!   applied yet" (zero blend cost when annotating nothing).
//! - Hand-rolled primitives (no tiny-skia): pen stamps, shapes, ab_glyph text,
//!   nearest-neighbor image blits. Keeps the binary lean and dependency-light.

use super::{AnnotateDoc, DrawEvent, Rgba, Stroke, Tool, Watermark};
use std::collections::HashMap;

/// Integer pixel bbox, half-open [x0,x1) x [y0,y1).
#[derive(Debug, Clone, Copy, Default)]
struct BBox {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl BBox {
    fn empty() -> Self {
        Self {
            x0: i32::MAX,
            y0: i32::MAX,
            x1: i32::MIN,
            y1: i32::MIN,
        }
    }

    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    fn union(&mut self, o: BBox) {
        if o.is_empty() {
            return;
        }
        if self.is_empty() {
            *self = o;
            return;
        }
        self.x0 = self.x0.min(o.x0);
        self.y0 = self.y0.min(o.y0);
        self.x1 = self.x1.max(o.x1);
        self.y1 = self.y1.max(o.y1);
    }

    fn clamp(&self, w: u32, h: u32) -> BBox {
        BBox {
            x0: self.x0.clamp(0, w as i32),
            y0: self.y0.clamp(0, h as i32),
            x1: self.x1.clamp(0, w as i32),
            y1: self.y1.clamp(0, h as i32),
        }
    }
}

/// A live preview older than this with no resend is dropped (lost commit).
const LIVE_TIMEOUT_MS: u64 = 2000;

/// Timed compositor: owns the overlay, applies strokes/watermarks up to a
/// clock, and blends the dirty region onto video frames.
pub struct Annotator {
    doc: AnnotateDoc,
    stroke_order: Vec<usize>,
    wm_order: Vec<usize>,
    applied: usize,
    wm_applied: usize,
    w: u32,
    h: u32,
    layer: Vec<u8>,
    /// Union of all applied stroke bboxes. The overlay persists (whiteboard
    /// semantics) while video frames change underneath, so EVERY frame blends
    /// this region — the skip is only "overlay still empty".
    coverage: BBox,
    /// In-progress pen stroke (`PenLive`): rasterized into a separate layer
    /// so each resend replaces (never double-darkens), then committed into
    /// the main layer by the matching `AddStroke`. Cleared by Undo/Clear
    /// and by a staleness timeout (lost commit edge).
    live_layer: Vec<u8>,
    live_bb: BBox,
    live_since_ms: Option<u64>,
    fonts: HashMap<String, Option<ab_glyph::FontArc>>,
    images: HashMap<String, Option<DecodedImage>>,
    warned_font: bool,
    last_now_ms: u64,
}

struct DecodedImage {
    data: Vec<u8>, // tight RGBA
    w: u32,
    h: u32,
}

impl Annotator {
    pub fn new(doc: AnnotateDoc, w: u32, h: u32) -> Self {
        let mut stroke_order: Vec<usize> = (0..doc.strokes.len()).collect();
        stroke_order.sort_by_key(|&i| doc.strokes[i].appear_ms);
        let mut wm_order: Vec<usize> = (0..doc.watermarks.len()).collect();
        wm_order.sort_by_key(|&i| doc.watermarks[i].appear_ms);
        Self {
            doc,
            stroke_order,
            wm_order,
            applied: 0,
            wm_applied: 0,
            w,
            h,
            layer: vec![0u8; (w as usize) * (h as usize) * 4],
            coverage: BBox::empty(),
            live_layer: vec![0u8; (w as usize) * (h as usize) * 4],
            live_bb: BBox::empty(),
            live_since_ms: None,
            fonts: HashMap::new(),
            images: HashMap::new(),
            warned_font: false,
            last_now_ms: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.doc.strokes.is_empty() && self.doc.watermarks.is_empty()
    }

    /// Rasterize everything with `appear_ms <= now_ms` not yet applied.
    pub fn apply_until(&mut self, now_ms: u64) {
        self.last_now_ms = now_ms.max(self.last_now_ms);
        while self.applied < self.stroke_order.len() {
            let i = self.stroke_order[self.applied];
            if self.doc.strokes[i].appear_ms > now_ms {
                break;
            }
            let dirty = self.raster_stroke(&self.doc.strokes[i].clone());
            self.coverage.union(dirty);
            self.applied += 1;
        }
        while self.wm_applied < self.wm_order.len() {
            let i = self.wm_order[self.wm_applied];
            if self.doc.watermarks[i].appear_ms > now_ms {
                break;
            }
            let dirty = self.raster_watermark(&self.doc.watermarks[i].clone());
            self.coverage.union(dirty);
            self.wm_applied += 1;
        }
        // Stale live preview (commit lost between resends): expire it so a
        // ghost stroke can't linger. Normal streams refresh well inside this.
        if let Some(since) = self.live_since_ms {
            if now_ms.saturating_sub(since) > LIVE_TIMEOUT_MS {
                self.clear_live();
            }
        }
    }

    /// Blend one straight-RGBA layer region onto a top-down tight BGRA
    /// frame in place. Returns blended pixel count.
    fn blend_region(frame: &mut [u8], layer: &[u8], w: u32, h: u32, bb: BBox) -> usize {
        if bb.is_empty() {
            return 0;
        }
        let d = bb.clamp(w, h);
        if d.is_empty() {
            return 0;
        }
        debug_assert_eq!(frame.len(), (w as usize) * (h as usize) * 4);
        let w = w as usize;
        let mut count = 0;
        for y in d.y0 as usize..d.y1 as usize {
            for x in d.x0 as usize..d.x1 as usize {
                let li = (y * w + x) * 4;
                let a = layer[li + 3] as f32 / 255.0;
                if a <= 0.0 {
                    continue;
                }
                let fi = li;
                let inv = 1.0 - a;
                // Layer RGBA -> frame BGRA, src-over.
                frame[fi] = (layer[li + 2] as f32 * a + frame[fi] as f32 * inv) as u8;
                frame[fi + 1] = (layer[li + 1] as f32 * a + frame[fi + 1] as f32 * inv) as u8;
                frame[fi + 2] = (layer[li] as f32 * a + frame[fi + 2] as f32 * inv) as u8;
                count += 1;
            }
        }
        count
    }

    /// Blend the coverage region plus any open live stroke onto a top-down
    /// tight BGRA frame in place. Returns blended pixel count (0 = overlay
    /// still empty, skipped).
    pub fn blend_bgra(&mut self, frame: &mut [u8]) -> usize {
        let mut count = Self::blend_region(frame, &self.layer, self.w, self.h, self.coverage);
        count += Self::blend_region(frame, &self.live_layer, self.w, self.h, self.live_bb);
        count
    }

    /// Current overlay (straight RGBA at feed size) for previews/thumbnails.
    pub fn overlay_rgba(&self) -> &[u8] {
        &self.layer
    }

    /// Drop any open live stroke (commit lost, undo/clear, timeout).
    fn clear_live(&mut self) {
        self.live_layer.fill(0);
        self.live_bb = BBox::empty();
        self.live_since_ms = None;
    }

    /// Rasterize a pen stroke into the live layer, replacing the previous
    /// live preview (resends carry full smoothed-so-far points, so replace
    /// semantics keep coverage exact — never double-darkened).
    fn raster_live(&mut self, color: &Rgba, width_px: f32, points: &[(f32, f32)], now_ms: u64) {
        if points.is_empty() {
            self.clear_live();
            return;
        }
        let stroke = Stroke {
            points: points.to_vec(),
            color: color.clone(),
            width_px: width_px.max(1.0),
            tool: Tool::Pen,
            text: None,
            appear_ms: now_ms,
            font_px: None,
            filled: false,
            font_path: None,
        };
        // raster_stroke writes into self.layer: swap the buffers, rasterize,
        // swap back. Main layer untouched, no full rebuild per resend.
        self.live_layer.fill(0);
        std::mem::swap(&mut self.layer, &mut self.live_layer);
        let bb = self.raster_stroke(&stroke);
        std::mem::swap(&mut self.layer, &mut self.live_layer);
        self.live_bb = bb;
        self.live_since_ms = Some(now_ms);
    }

    /// Apply one live-drawing event at clock `now_ms`.
    pub fn apply_event(&mut self, ev: &DrawEvent, now_ms: u64) {
        match ev {
            DrawEvent::AddStroke(s) => {
                // A pen commit always follows its live stream: drop the
                // preview first so the committed raster doesn't double up.
                self.clear_live();
                let mut s = s.clone();
                s.appear_ms = now_ms;
                self.doc.strokes.push(s);
                let idx = self.doc.strokes.len() - 1;
                // Splice the time-ordered index: entries are consumed in
                // appear order, so keep the vec sorted and mark the newcomer
                // consumed (it is rasterized immediately below).
                let pos = self
                    .stroke_order
                    .partition_point(|&i| self.doc.strokes[i].appear_ms <= now_ms);
                self.stroke_order.insert(pos, idx);
                self.applied += 1;
                let bb = self.raster_stroke(&self.doc.strokes[idx].clone());
                self.coverage.union(bb);
            }
            DrawEvent::PenLive {
                color,
                width_px,
                points,
            } => {
                self.raster_live(color, *width_px, points, now_ms);
            }
            DrawEvent::Undo => {
                self.clear_live();
                self.doc.strokes.pop();
                self.rebuild();
            }
            DrawEvent::Clear => {
                self.clear_live();
                self.doc.strokes.clear();
                self.doc.watermarks.clear();
                self.rebuild();
            }
        }
    }

    /// Re-rasterize everything after a destructive edit (undo/clear).
    /// `last_now_ms` replays timed strokes so scripted + live content agree.
    pub fn rebuild(&mut self) {
        self.layer.fill(0);
        self.coverage = BBox::empty();
        self.clear_live();
        self.stroke_order = (0..self.doc.strokes.len()).collect();
        self.stroke_order
            .sort_by_key(|&i| self.doc.strokes[i].appear_ms);
        self.wm_order = (0..self.doc.watermarks.len()).collect();
        self.wm_order
            .sort_by_key(|&i| self.doc.watermarks[i].appear_ms);
        self.applied = 0;
        self.wm_applied = 0;
        self.apply_until(self.last_now_ms);
    }

    fn nx(&self, v: f32) -> i32 {
        (v * self.w as f32).round() as i32
    }

    fn ny(&self, v: f32) -> i32 {
        (v * self.h as f32).round() as i32
    }

    /// Src-over blend one layer pixel with extra coverage multiplier.
    fn blend_px(&mut self, x: i32, y: i32, c: &Rgba, coverage: f32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 || coverage <= 0.0 {
            return;
        }
        let i = ((y as usize) * (self.w as usize) + (x as usize)) * 4;
        let sa = (c.3 as f32 / 255.0) * coverage.clamp(0.0, 1.0);
        if sa <= 0.0 {
            return;
        }
        let inv = 1.0 - sa;
        let l = &mut self.layer;
        l[i] = (c.0 as f32 * sa + l[i] as f32 * inv) as u8;
        l[i + 1] = (c.1 as f32 * sa + l[i + 1] as f32 * inv) as u8;
        l[i + 2] = (c.2 as f32 * sa + l[i + 2] as f32 * inv) as u8;
        l[i + 3] = ((sa + l[i + 3] as f32 / 255.0 * inv) * 255.0) as u8;
    }

    fn stamp(&mut self, cx: i32, cy: i32, radius: f32, c: &Rgba) -> BBox {
        let r = (radius.ceil() as i32).max(1);
        let mut bb = BBox::empty();
        for y in (cy - r)..=(cy + r) {
            for x in (cx - r)..=(cx + r) {
                let d = (((x - cx) as f32).powi(2) + ((y - cy) as f32).powi(2)).sqrt();
                if d <= radius + 0.5 {
                    // 1px antialiased rim.
                    let cov = ((radius + 0.5 - d).clamp(0.0, 1.0)).min(1.0);
                    self.blend_px(x, y, c, cov);
                    bb.union(BBox {
                        x0: x,
                        y0: y,
                        x1: x + 1,
                        y1: y + 1,
                    });
                }
            }
        }
        bb
    }

    fn segment(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, width: f32, c: &Rgba) -> BBox {
        let len = (((x1 - x0) as f32).hypot((y1 - y0) as f32)).max(1.0);
        let steps = len.ceil() as i32;
        let mut bb = BBox::empty();
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = (x0 as f32 + (x1 - x0) as f32 * t).round() as i32;
            let y = (y0 as f32 + (y1 - y0) as f32 * t).round() as i32;
            bb.union(self.stamp(x, y, width / 2.0, c));
        }
        bb
    }

    fn raster_stroke(&mut self, s: &Stroke) -> BBox {
        if s.points.is_empty() {
            return BBox::empty();
        }
        let pts: Vec<(i32, i32)> = s
            .points
            .iter()
            .map(|&(x, y)| (self.nx(x), self.ny(y)))
            .collect();
        match s.tool {
            Tool::Pen => {
                let mut bb = BBox::empty();
                if pts.len() == 1 {
                    return self.stamp(pts[0].0, pts[0].1, s.width_px.max(1.0) / 2.0, &s.color);
                }
                for w in pts.windows(2) {
                    bb.union(self.segment(
                        w[0].0,
                        w[0].1,
                        w[1].0,
                        w[1].1,
                        s.width_px.max(1.0),
                        &s.color,
                    ));
                }
                bb
            }
            Tool::Line => {
                let (a, b) = (pts[0], pts[pts.len() - 1]);
                self.segment(a.0, a.1, b.0, b.1, s.width_px.max(1.0), &s.color)
            }
            Tool::Arrow => {
                let (a, b) = (pts[0], pts[pts.len() - 1]);
                let mut bb = self.segment(a.0, a.1, b.0, b.1, s.width_px.max(1.0), &s.color);
                let ang = ((b.1 - a.1) as f32).atan2((b.0 - a.0) as f32);
                let head = (s.width_px.max(6.0) * 2.5).max(12.0);
                for da in [25.0f32.to_radians(), -25.0f32.to_radians()] {
                    let ha = ang + std::f32::consts::PI + da;
                    let hx = (b.0 as f32 + head * ha.cos()).round() as i32;
                    let hy = (b.1 as f32 + head * ha.sin()).round() as i32;
                    bb.union(self.segment(b.0, b.1, hx, hy, s.width_px.max(1.0), &s.color));
                }
                bb
            }
            Tool::Rect => {
                let (a, b) = (pts[0], pts[pts.len() - 1]);
                let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
                let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
                if s.filled {
                    self.fill_rect(x0, y0, x1, y1, &s.color)
                } else {
                    let w = s.width_px.max(1.0);
                    let mut bb = BBox::empty();
                    bb.union(self.segment(x0, y0, x1, y0, w, &s.color));
                    bb.union(self.segment(x1, y0, x1, y1, w, &s.color));
                    bb.union(self.segment(x1, y1, x0, y1, w, &s.color));
                    bb.union(self.segment(x0, y1, x0, y0, w, &s.color));
                    bb
                }
            }
            Tool::Ellipse => {
                let (a, b) = (pts[0], pts[pts.len() - 1]);
                let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
                let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
                if s.filled {
                    self.fill_ellipse(x0, y0, x1, y1, &s.color)
                } else {
                    self.stroke_ellipse(x0, y0, x1, y1, s.width_px.max(1.0), &s.color)
                }
            }
            Tool::Text => {
                let text = s.text.as_deref().unwrap_or("");
                if text.is_empty() {
                    return BBox::empty();
                }
                let size = s.font_px.unwrap_or(32.0).max(6.0);
                self.raster_text(
                    pts[0].0,
                    pts[0].1,
                    text,
                    size,
                    &s.color,
                    s.font_path.as_deref(),
                )
            }
            Tool::Image => {
                // points[0..1] = corners like Rect; text = image path.
                let path = match s.text.as_deref() {
                    Some(p) if !p.is_empty() => p,
                    _ => return BBox::empty(),
                };
                let (a, b) = (pts[0], pts[pts.len() - 1]);
                self.blit_image(
                    path,
                    a.0.min(b.0),
                    a.1.min(b.1),
                    (a.0 - b.0).abs().max(1),
                    (a.1 - b.1).abs().max(1),
                    1.0,
                )
            }
        }
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: &Rgba) -> BBox {
        for y in y0..y1 {
            for x in x0..x1 {
                self.blend_px(x, y, c, 1.0);
            }
        }
        BBox { x0, y0, x1, y1 }
    }

    fn stroke_ellipse(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, width: f32, c: &Rgba) -> BBox {
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        let (rx, ry) = ((x1 - x0).abs() as f32 / 2.0, (y1 - y0).abs() as f32 / 2.0);
        if rx < 1.0 || ry < 1.0 {
            return BBox::empty();
        }
        let steps = ((rx + ry) * 3.0).ceil() as i32;
        let mut bb = BBox::empty();
        for i in 0..=steps {
            let t = i as f32 / steps as f32 * std::f32::consts::TAU;
            let x = (cx + rx * t.cos()).round() as i32;
            let y = (cy + ry * t.sin()).round() as i32;
            bb.union(self.stamp(x, y, width / 2.0, c));
        }
        bb
    }

    fn fill_ellipse(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: &Rgba) -> BBox {
        let (cx, cy) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
        let (rx, ry) = ((x1 - x0).abs() as f32 / 2.0, (y1 - y0).abs() as f32 / 2.0);
        if rx < 1.0 || ry < 1.0 {
            return BBox::empty();
        }
        for y in y0..y1 {
            let ny = (y as f32 + 0.5 - cy) / ry;
            let half = rx * (1.0 - ny * ny).max(0.0).sqrt();
            let (xa, xb) = ((cx - half).floor() as i32, (cx + half).ceil() as i32);
            for x in xa..xb {
                self.blend_px(x, y, c, 1.0);
            }
        }
        BBox { x0, y0, x1, y1 }
    }

    fn resolve_font(&mut self, path: Option<&str>) -> Option<ab_glyph::FontArc> {
        let key = path.unwrap_or("").to_string();
        if let Some(cached) = self.fonts.get(&key) {
            return cached.clone();
        }
        let bytes = if let Some(p) = path {
            std::fs::read(p).ok()
        } else {
            default_font_bytes()
        };
        let font = bytes.and_then(|b| ab_glyph::FontArc::try_from_vec(b).ok());
        if font.is_none() && !self.warned_font {
            self.warned_font = true;
            tracing::warn!(
                "no usable font for text annotations (tried {:?}); skipping text",
                path
            );
        }
        self.fonts.insert(key, font.clone());
        font
    }

    fn raster_text(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        size: f32,
        c: &Rgba,
        font_path: Option<&str>,
    ) -> BBox {
        use ab_glyph::{Font, Glyph, PxScale, ScaleFont};
        let font = match self.resolve_font(font_path) {
            Some(f) => f,
            None => return BBox::empty(),
        };
        let scale = PxScale::from(size);
        let scaled = font.as_scaled(scale);
        let mut bb = BBox::empty();
        let mut caret_y = y as f32 + scaled.ascent();
        for line in text.lines() {
            let mut caret_x = x as f32;
            for ch in line.chars() {
                let id = scaled.glyph_id(ch);
                let glyph = Glyph {
                    id,
                    scale,
                    position: ab_glyph::point(caret_x, caret_y),
                };
                if let Some(outlined) = font.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    outlined.draw(|gx, gy, cov| {
                        let px = bounds.min.x as i32 + gx as i32;
                        let py = bounds.min.y as i32 + gy as i32;
                        self.blend_px(px, py, c, cov);
                        bb.union(BBox {
                            x0: px,
                            y0: py,
                            x1: px + 1,
                            y1: py + 1,
                        });
                    });
                }
                caret_x += scaled.h_advance(id);
            }
            caret_y += scaled.ascent() - scaled.descent() + size * 0.2;
        }
        bb
    }

    fn raster_watermark(&mut self, w: &Watermark) -> BBox {
        let dx = (w.x * self.w as f32).round() as i32;
        let dy = (w.y * self.h as f32).round() as i32;
        let dw = (w.w * self.w as f32).round().max(1.0) as i32;
        let dh = (w.h * self.h as f32).round().max(1.0) as i32;
        self.blit_image(&w.asset_id, dx, dy, dw, dh, w.opacity.clamp(0.0, 1.0))
    }

    /// Nearest-neighbor image blit with opacity. Shared by watermarks and the
    /// Image stroke tool.
    fn blit_image(&mut self, path: &str, dx: i32, dy: i32, dw: i32, dh: i32, opacity: f32) -> BBox {
        let img = self.decoded_image(path);
        let (data, iw, ih) = match img {
            Some(d) => (d.data.clone(), d.w, d.h),
            None => return BBox::empty(),
        };
        let mut bb = BBox::empty();
        for oy in 0..dh {
            for ox in 0..dw {
                // Nearest-neighbor sample.
                let sx = ((ox as f32 / dw as f32) * iw as f32) as u32;
                let sy = ((oy as f32 / dh as f32) * ih as f32) as u32;
                let si = ((sy.min(ih - 1) * iw + sx.min(iw - 1)) * 4) as usize;
                let a = (data[si + 3] as f32 / 255.0) * opacity;
                if a > 0.0 {
                    let c = Rgba(data[si], data[si + 1], data[si + 2], 255);
                    self.blend_px(dx + ox, dy + oy, &c, a);
                    bb.union(BBox {
                        x0: dx + ox,
                        y0: dy + oy,
                        x1: dx + ox + 1,
                        y1: dy + oy + 1,
                    });
                }
            }
        }
        bb
    }

    fn decoded_image(&mut self, path: &str) -> Option<DecodedImage> {
        if let Some(cached) = self.images.get(path) {
            return cached.as_ref().map(|d| DecodedImage {
                data: d.data.clone(),
                w: d.w,
                h: d.h,
            });
        }
        let decoded = image::open(path)
            .ok()
            .map(|img| img.to_rgba8())
            .map(|rgba| DecodedImage {
                w: rgba.width(),
                h: rgba.height(),
                data: rgba.into_raw(),
            });
        if decoded.is_none() {
            tracing::warn!("watermark image unreadable: {path}");
        }
        self.images.insert(
            path.to_string(),
            decoded.as_ref().map(|d| DecodedImage {
                data: d.data.clone(),
                w: d.w,
                h: d.h,
            }),
        );
        decoded
    }
}

/// Best-effort system font bytes for UI text (toolbar labels, etc.).
/// Windows always ships Arial; Unix/macOS fallbacks for Phase 6 ports.
pub fn system_font_bytes() -> Option<Vec<u8>> {
    default_font_bytes()
}

fn default_font_bytes() -> Option<Vec<u8>> {
    // Windows always ships Arial; Unix/macOS fallbacks for Phase 6 ports.
    let candidates = [
        std::env::var("WINDIR")
            .ok()
            .map(|w| format!("{w}\\Fonts\\arial.ttf")),
        Some("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".to_string()),
        Some("/System/Library/Fonts/Helvetica.ttc".to_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .find_map(|p| std::fs::read(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn red_line_doc() -> AnnotateDoc {
        let mut doc = AnnotateDoc {
            canvas_w: 100,
            canvas_h: 100,
            ..Default::default()
        };
        doc.add_stroke(Stroke {
            points: vec![(0.1, 0.5), (0.9, 0.5)],
            color: Rgba(255, 0, 0, 255),
            width_px: 3.0,
            tool: Tool::Line,
            text: None,
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        })
        .unwrap();
        doc
    }

    #[test]
    fn line_lands_on_expected_pixels() {
        let mut ann = Annotator::new(red_line_doc(), 100, 100);
        ann.apply_until(0);
        let layer = ann.overlay_rgba();
        // y=50 row, middle of the line must be opaque red.
        let i = (50 * 100 + 50) * 4;
        assert_eq!(&layer[i..i + 4], &[255, 0, 0, 255]);
        // Far corner untouched.
        let j = (5 * 100 + 5) * 4;
        assert_eq!(&layer[j..j + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn blend_is_src_over_and_coverage_persists() {
        let mut ann = Annotator::new(red_line_doc(), 100, 100);
        ann.apply_until(0);
        // Black BGRA frame.
        let mut frame = vec![0u8; 100 * 100 * 4];
        let n = ann.blend_bgra(&mut frame);
        assert!(n > 200, "line should touch hundreds of pixels, got {n}");
        let i = (50 * 100 + 50) * 4;
        // BGRA memory order: B=0, G=0, R=255.
        assert_eq!(frame[i], 0);
        assert_eq!(frame[i + 1], 0);
        assert_eq!(frame[i + 2], 255);
        // Overlay persists: a fresh video frame blends the same pixels again.
        let mut frame2 = vec![10u8; 100 * 100 * 4];
        assert_eq!(ann.blend_bgra(&mut frame2), n);
        assert_eq!(frame2[i + 2], 255);
    }

    #[test]
    fn strokes_appear_by_timestamp() {
        let mut doc = red_line_doc();
        doc.add_stroke(Stroke {
            points: vec![(0.1, 0.1), (0.9, 0.1)],
            color: Rgba(0, 255, 0, 255),
            width_px: 3.0,
            tool: Tool::Line,
            text: None,
            appear_ms: 2000,
            font_px: None,
            filled: false,
            font_path: None,
        })
        .unwrap();
        let mut ann = Annotator::new(doc, 100, 100);
        ann.apply_until(0);
        // Green line (y=10) not yet rasterized.
        let i = (10 * 100 + 50) * 4;
        assert_eq!(&ann.overlay_rgba()[i..i + 4], &[0, 0, 0, 0]);
        ann.apply_until(2500);
        assert_eq!(&ann.overlay_rgba()[i..i + 4], &[0, 255, 0, 255]);
    }

    #[test]
    fn image_stamp_blits_pixels() {
        // Solid green 4x4 PNG in temp; stamp it over a 20x20 norm region.
        let path = std::env::temp_dir().join("qcapture-stamp-test.png");
        let img = image::RgbImage::from_fn(4, 4, |_, _| image::Rgb([0, 255, 0]));
        img.save(&path).unwrap();
        let mut doc = AnnotateDoc {
            canvas_w: 100,
            canvas_h: 100,
            ..Default::default()
        };
        doc.add_stroke(Stroke {
            points: vec![(0.2, 0.2), (0.4, 0.4)],
            color: Rgba(255, 0, 0, 255),
            width_px: 1.0,
            tool: Tool::Image,
            text: Some(path.to_string_lossy().into_owned()),
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        })
        .unwrap();
        let mut ann = Annotator::new(doc, 100, 100);
        ann.apply_until(0);
        // Center of the 20,20+20x20 blit is green, not red.
        let i = (30 * 100 + 30) * 4;
        assert_eq!(&ann.overlay_rgba()[i..i + 4], &[0, 255, 0, 255]);
        // Missing file degrades to nothing instead of panicking.
        let missing = std::env::temp_dir().join("qcapture-no-such-stamp.png");
        let _ = std::fs::remove_file(&missing);
        let mut doc2 = AnnotateDoc {
            canvas_w: 100,
            canvas_h: 100,
            ..Default::default()
        };
        doc2.add_stroke(Stroke {
            points: vec![(0.2, 0.2), (0.4, 0.4)],
            color: Rgba(255, 0, 0, 255),
            width_px: 1.0,
            tool: Tool::Image,
            text: Some(missing.to_string_lossy().into_owned()),
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        })
        .unwrap();
        let mut ann2 = Annotator::new(doc2, 100, 100);
        ann2.apply_until(0);
        assert_eq!(&ann2.overlay_rgba()[i..i + 4], &[0, 0, 0, 0]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn filled_rect_covers_interior() {
        let mut doc = AnnotateDoc {
            canvas_w: 100,
            canvas_h: 100,
            ..Default::default()
        };
        doc.add_stroke(Stroke {
            points: vec![(0.2, 0.2), (0.4, 0.4)],
            color: Rgba(0, 0, 255, 255),
            width_px: 1.0,
            tool: Tool::Rect,
            text: None,
            appear_ms: 0,
            font_px: None,
            filled: true,
            font_path: None,
        })
        .unwrap();
        let mut ann = Annotator::new(doc, 100, 100);
        ann.apply_until(0);
        let i = (30 * 100 + 30) * 4;
        assert_eq!(&ann.overlay_rgba()[i..i + 4], &[0, 0, 255, 255]);
    }

    #[test]
    fn text_produces_pixels_when_font_available() {
        let mut doc = AnnotateDoc {
            canvas_w: 200,
            canvas_h: 100,
            ..Default::default()
        };
        doc.add_stroke(Stroke {
            points: vec![(0.05, 0.1)],
            color: Rgba(255, 255, 255, 255),
            width_px: 1.0,
            tool: Tool::Text,
            text: Some("Hi".to_string()),
            appear_ms: 0,
            font_px: Some(40.0),
            filled: false,
            font_path: None,
        })
        .unwrap();
        let mut ann = Annotator::new(doc, 200, 100);
        ann.apply_until(0);
        let lit = ann
            .overlay_rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 20)
            .count();
        if ann.fonts.values().any(|f| f.is_some()) {
            assert!(lit > 50, "expected rasterized glyph pixels, got {lit}");
        } else {
            eprintln!("SKIP: no system font available for text test");
        }
    }

    fn live_red_dot() -> Stroke {
        Stroke {
            points: vec![(0.5, 0.5)],
            color: Rgba(255, 0, 0, 255),
            width_px: 6.0,
            tool: Tool::Pen,
            text: None,
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        }
    }

    #[test]
    fn live_event_rasterizes_and_undo_rebuilds() {
        use super::DrawEvent;
        let mut ann = Annotator::new(AnnotateDoc::default(), 100, 100);
        // Empty overlay blends nothing.
        let mut frame = vec![0u8; 100 * 100 * 4];
        assert_eq!(ann.blend_bgra(&mut frame), 0);
        // Live stroke appears immediately at pump clock.
        ann.apply_event(&DrawEvent::AddStroke(live_red_dot()), 5000);
        let n = ann.blend_bgra(&mut frame);
        assert!(n > 10, "live dot should blend pixels, got {n}");
        let i = (50 * 100 + 50) * 4;
        assert_eq!(frame[i + 2], 255);
        // Undo removes it: rebuild leaves a clean layer.
        ann.apply_event(&DrawEvent::Undo, 5000);
        let mut frame2 = vec![0u8; 100 * 100 * 4];
        assert_eq!(ann.blend_bgra(&mut frame2), 0);
    }

    #[test]
    fn live_and_timed_strokes_coexist() {
        use super::DrawEvent;
        let mut ann = Annotator::new(red_line_doc(), 100, 100);
        ann.apply_until(0);
        ann.apply_event(&DrawEvent::AddStroke(live_red_dot()), 100);
        let mut frame = vec![0u8; 100 * 100 * 4];
        let n = ann.blend_bgra(&mut frame);
        assert!(n > 200, "both strokes should blend, got {n}");
        // Scripted line pixel (y=50, x=20) still red.
        let i = (50 * 100 + 20) * 4;
        assert_eq!(frame[i + 2], 255);
    }

    fn live_pen(color: (u8, u8, u8), points: Vec<(f32, f32)>) -> super::DrawEvent {
        super::DrawEvent::PenLive {
            color: Rgba(color.0, color.1, color.2, 255),
            width_px: 6.0,
            points,
        }
    }

    #[test]
    fn pen_live_shows_before_commit() {
        let mut ann = Annotator::new(AnnotateDoc::default(), 100, 100);
        // Partial stream previews immediately, without any commit.
        ann.apply_event(&live_pen((255, 0, 0), vec![(0.2, 0.5), (0.4, 0.5)]), 1000);
        let mut frame = vec![0u8; 100 * 100 * 4];
        let n = ann.blend_bgra(&mut frame);
        assert!(n > 10, "live preview should blend pixels, got {n}");
        assert_eq!(ann.doc.strokes.len(), 0, "nothing committed yet");
    }

    #[test]
    fn pen_live_resend_replaces_without_doubling() {
        let mut ann = Annotator::new(AnnotateDoc::default(), 100, 100);
        let pts: Vec<(f32, f32)> = vec![(0.2, 0.5), (0.4, 0.5)];
        ann.apply_event(&live_pen((255, 0, 0), pts.clone()), 1000);
        let mut f1 = vec![0u8; 100 * 100 * 4];
        ann.blend_bgra(&mut f1);
        // Same points resent (throttle overlap): identical pixels, no buildup.
        ann.apply_event(&live_pen((255, 0, 0), pts), 1100);
        let mut f2 = vec![0u8; 100 * 100 * 4];
        ann.blend_bgra(&mut f2);
        assert_eq!(f1, f2, "resend must replace, not accumulate");
    }

    #[test]
    fn pen_commit_clears_live_without_doubling() {
        use super::DrawEvent;
        let mut ann = Annotator::new(AnnotateDoc::default(), 100, 100);
        let pts = vec![(0.2, 0.5), (0.4, 0.5), (0.6, 0.5)];
        ann.apply_event(&live_pen((255, 0, 0), pts.clone()), 1000);
        // Commit the same stroke: live preview drops, committed raster lands.
        ann.apply_event(
            &DrawEvent::AddStroke(Stroke {
                points: pts,
                color: Rgba(255, 0, 0, 255),
                width_px: 6.0,
                tool: Tool::Pen,
                text: None,
                appear_ms: 0,
                font_px: None,
                filled: false,
                font_path: None,
            }),
            1200,
        );
        assert_eq!(ann.doc.strokes.len(), 1);
        let mut frame = vec![0u8; 100 * 100 * 4];
        let n = ann.blend_bgra(&mut frame);
        assert!(n > 10);
        // Reference: same stroke committed with no live phase at all.
        let mut ann2 = Annotator::new(AnnotateDoc::default(), 100, 100);
        ann2.apply_event(
            &DrawEvent::AddStroke(Stroke {
                points: vec![(0.2, 0.5), (0.4, 0.5), (0.6, 0.5)],
                color: Rgba(255, 0, 0, 255),
                width_px: 6.0,
                tool: Tool::Pen,
                text: None,
                appear_ms: 0,
                font_px: None,
                filled: false,
                font_path: None,
            }),
            1200,
        );
        let mut frame2 = vec![0u8; 100 * 100 * 4];
        ann2.blend_bgra(&mut frame2);
        assert_eq!(frame, frame2, "commit after live == clean commit");
    }

    #[test]
    fn pen_live_cancelled_by_undo_and_timeout() {
        use super::DrawEvent;
        let mut ann = Annotator::new(AnnotateDoc::default(), 100, 100);
        ann.apply_event(&live_pen((255, 0, 0), vec![(0.2, 0.5), (0.4, 0.5)]), 1000);
        // Undo cancels the open stroke.
        ann.apply_event(&DrawEvent::Undo, 1100);
        let mut frame = vec![0u8; 100 * 100 * 4];
        assert_eq!(ann.blend_bgra(&mut frame), 0);

        // Stale preview (lost commit) expires via the pump clock.
        ann.apply_event(&live_pen((255, 0, 0), vec![(0.2, 0.5), (0.4, 0.5)]), 2000);
        ann.apply_until(2000 + 2001);
        let mut frame2 = vec![0u8; 100 * 100 * 4];
        assert_eq!(ann.blend_bgra(&mut frame2), 0, "stale live must expire");
    }
}
