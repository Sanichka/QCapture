//! Phase 4c-take-2: drawing panel on eframe with a live video texture.
//!
//! The raw-winit layered overlay proved unhittable on this Win10 build
//! (visible/topmost/HTCLIENT yet skipped by hit-testing and starved of
//! pointer input — root cause never isolated). eframe windows ARE clickable
//! here (pick-region drag-select proves it), so drawing lives in eframe and
//! the "live desktop underneath" becomes a live VIDEO texture: the pump
//! forwards decimated post-blend frames, and the panel shows exactly what
//! the encoder sees. No transparency needed anywhere.
//!
//! Used two ways: standalone eframe app on the CLI main thread (capture runs
//! on a background thread), and an embedded deferred viewport in the widget
//! (same UI thread, same event loop). Strokes leave as [`DrawEvent`]s over
//! flume — the pump burns them; undo/clear mirror locally for counts.

use eframe::egui;
use qcapture_annotate::{AnnotateDoc, Rgba, Stroke, Tool};

pub struct DrawPanel {
    tool: DrawTool,
    color: egui::Color32,
    width: f32,
    font_size: f32,
    strokes: Vec<Stroke>,
    active_pen: Vec<egui::Pos2>,
    shape_anchor: Option<egui::Pos2>,
    shape_current: egui::Pos2,
    text_at: Option<egui::Pos2>,
    text_buf: String,
    video_tex: Option<egui::TextureHandle>,
    scratch: Vec<u8>,
    feed_w: u32,
    feed_h: u32,
    events: flume::Sender<DrawEvent>,
    chrome_rect: egui::Rect,
    popup_rect: egui::Rect,
    frames_shown: u64,
}

use qcapture_annotate::DrawEvent;

#[derive(PartialEq, Eq, Clone, Copy)]
enum DrawTool {
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Text,
}

impl DrawTool {
    fn label(self) -> &'static str {
        match self {
            Self::Pen => "Pen",
            Self::Line => "Line",
            Self::Arrow => "Arrow",
            Self::Rect => "Rect",
            Self::Ellipse => "Ell",
            Self::Text => "Text",
        }
    }

    fn as_tool(self) -> Tool {
        match self {
            Self::Pen => Tool::Pen,
            Self::Line => Tool::Line,
            Self::Arrow => Tool::Arrow,
            Self::Rect => Tool::Rect,
            Self::Ellipse => Tool::Ellipse,
            Self::Text => Tool::Text,
        }
    }
}

const PRESET_COLORS: [egui::Color32; 6] = [
    egui::Color32::RED,
    egui::Color32::GREEN,
    egui::Color32::from_rgb(0, 150, 255),
    egui::Color32::YELLOW,
    egui::Color32::WHITE,
    egui::Color32::BLACK,
];

impl DrawPanel {
    pub fn new(feed_w: u32, feed_h: u32, events: flume::Sender<DrawEvent>) -> Self {
        Self {
            tool: DrawTool::Pen,
            color: egui::Color32::RED,
            width: 4.0,
            font_size: 40.0,
            strokes: Vec::new(),
            active_pen: Vec::new(),
            shape_anchor: None,
            shape_current: egui::Pos2::ZERO,
            text_at: None,
            text_buf: String::new(),
            video_tex: None,
            scratch: Vec::new(),
            feed_w: feed_w.max(64),
            feed_h: feed_h.max(64),
            events,
            chrome_rect: egui::Rect::NOTHING,
            popup_rect: egui::Rect::NOTHING,
            frames_shown: 0,
        }
    }

    pub fn stroke_count(&self) -> usize {
        self.strokes.len()
    }

    pub fn frames_shown(&self) -> u64 {
        self.frames_shown
    }

    /// Feed-size doc of everything committed (sidecar/save parity).
    pub fn doc(&self) -> AnnotateDoc {
        AnnotateDoc {
            version: 1,
            canvas_w: self.feed_w,
            canvas_h: self.feed_h,
            strokes: self.strokes.clone(),
            watermarks: Vec::new(),
            burn_in: true,
        }
    }

    /// Push a fresh top-down tight BGRA frame into the video texture.
    /// BGRA->RGBA swizzle into a reused scratch buffer. Self-corrects the
    /// feed size when the initial guess (region align/clamp) was off — norm
    /// coords survive a resize, so committed strokes stay valid.
    pub fn push_video(&mut self, ctx: &egui::Context, bgra: &[u8]) {
        let expect = (self.feed_w as usize) * (self.feed_h as usize) * 4;
        if bgra.len() != expect {
            return; // legacy unsized path; sized push_preview resizes instead
        }
        self.push_video_sized(ctx, self.feed_w, self.feed_h, bgra);
    }

    /// Sized preview push (preferred): resizes the feed when `w/h` differ.
    pub fn push_preview(
        &mut self,
        ctx: &egui::Context,
        frame: &qcapture_capture::pump::PreviewFrame,
    ) {
        if frame.w < 16 || frame.h < 16 {
            return;
        }
        if frame.w != self.feed_w || frame.h != self.feed_h {
            self.feed_w = frame.w;
            self.feed_h = frame.h;
            self.video_tex = None;
            self.scratch.clear();
        }
        self.push_video_sized(ctx, frame.w, frame.h, &frame.bgra);
    }

    fn push_video_sized(&mut self, ctx: &egui::Context, w: u32, h: u32, bgra: &[u8]) {
        let expect = (w as usize) * (h as usize) * 4;
        if bgra.len() != expect {
            return;
        }
        if self.scratch.len() != expect {
            self.scratch.resize(expect, 0);
        }
        let (dst_chunks, _) = self.scratch.as_chunks_mut::<4>();
        let (src_chunks, _) = bgra.as_chunks::<4>();
        for (d, s) in dst_chunks.iter_mut().zip(src_chunks.iter()) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &self.scratch);
        self.video_tex = Some(ctx.load_texture("draw-video", img, egui::TextureOptions::LINEAR));
        self.frames_shown += 1;
    }

    /// Drain all pending previews, keeping the latest. Returns frames consumed.
    /// Call once per UI frame before [`Self::show`].
    pub fn drain_previews(
        &mut self,
        ctx: &egui::Context,
        rx: &flume::Receiver<qcapture_capture::pump::PreviewFrame>,
    ) -> usize {
        let mut n = 0;
        // Bounded channel, UI thread only: try_recv loop never blocks.
        // Keep only the latest to avoid texture upload backlog.
        let mut latest = None;
        while let Ok(f) = rx.try_recv() {
            latest = Some(f);
            n += 1;
        }
        if let Some(f) = latest {
            self.push_preview(ctx, &f);
        }
        n
    }

    fn to_norm(&self, p: egui::Pos2, draw: egui::Rect) -> Option<(f32, f32)> {
        if !draw.contains(p) {
            return None;
        }
        Some((
            ((p.x - draw.min.x) / draw.width()).clamp(0.0, 1.0),
            ((p.y - draw.min.y) / draw.height()).clamp(0.0, 1.0),
        ))
    }

    fn push_stroke(
        &mut self,
        points: Vec<(f32, f32)>,
        tool: Tool,
        text: Option<String>,
        width_px: f32,
    ) {
        if points.len() < 2 && tool != Tool::Text {
            return;
        }
        // Feed-pixel sizes: points live in feed space (draw rect maps 1:1
        // modulo letterbox — to_norm already accounts for it). For Text,
        // `width_px` carries the scaled font size (see push_stroke_text).
        let c = self.color;
        let font_px = if tool == Tool::Text {
            Some(width_px.max(6.0))
        } else {
            // Ignored by the rasterizer for non-text tools.
            Some((self.font_size).max(6.0))
        };
        let stroke = Stroke {
            points,
            color: Rgba(c.r(), c.g(), c.b(), c.a()),
            width_px: if tool == Tool::Text {
                1.0
            } else {
                width_px.max(1.0)
            },
            tool,
            text,
            appear_ms: 0, // pump stamps receipt time
            font_px,
            filled: false,
            font_path: None,
        };
        self.strokes.push(stroke.clone());
        let _ = self.events.try_send(DrawEvent::AddStroke(stroke));
    }

    /// Aspect-fit draw rect for the feed inside `avail`.
    fn draw_rect(&self, avail: egui::Rect) -> egui::Rect {
        let (fw, fh) = (self.feed_w as f32, self.feed_h as f32);
        let s = (avail.width() / fw).min(avail.height() / fh).max(0.01);
        let (w, h) = (fw * s, fh * s);
        egui::Rect::from_min_size(
            egui::Pos2::new(
                avail.min.x + (avail.width() - w) / 2.0,
                avail.min.y + (avail.height() - h) / 2.0,
            ),
            egui::Vec2::new(w, h),
        )
    }

    /// Show video + canvas + floating toolbar. Call every frame.
    pub fn show(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let draw = self.draw_rect(avail);
        // Scale factor draw-px -> feed-px for width authoring.
        let k = self.feed_w as f32 / draw.width().max(1.0);

        // ---- floating toolbar ----
        let mut chrome = egui::Rect::NOTHING;
        egui::Window::new("Draw")
            .anchor(egui::Align2::LEFT_TOP, [10.0, 10.0])
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for t in [
                        DrawTool::Pen,
                        DrawTool::Line,
                        DrawTool::Arrow,
                        DrawTool::Rect,
                        DrawTool::Ellipse,
                        DrawTool::Text,
                    ] {
                        ui.selectable_value(&mut self.tool, t, t.label());
                    }
                });
                ui.horizontal(|ui| {
                    for c in PRESET_COLORS {
                        let mark = if self.color == c { "◉" } else { "●" };
                        if ui.button(egui::RichText::new(mark).color(c)).clicked() {
                            self.color = c;
                        }
                    }
                    ui.color_edit_button_srgba(&mut self.color);
                });
                ui.horizontal(|ui| {
                    ui.label("Width");
                    ui.add(egui::Slider::new(&mut self.width, 1.0..=32.0).show_value(true));
                    ui.label("Text");
                    ui.add(egui::Slider::new(&mut self.font_size, 8.0..=120.0).show_value(true));
                });
                ui.horizontal(|ui| {
                    if ui.button("Undo").clicked() {
                        self.strokes.pop();
                        let _ = self.events.try_send(DrawEvent::Undo);
                    }
                    if ui.button("Clear").clicked() {
                        self.strokes.clear();
                        let _ = self.events.try_send(DrawEvent::Clear);
                    }
                    ui.label(format!(
                        "{} strokes · {} video frames",
                        self.strokes.len(),
                        self.frames_shown
                    ));
                });
                chrome = ui.min_rect();
            });
        self.chrome_rect = chrome;

        // ---- text popup ----
        let mut popup = egui::Rect::NOTHING;
        let mut commit_text = false;
        let mut cancel_text = false;
        if self.text_at.is_some() {
            egui::Window::new("Text…")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    let r = ui.text_edit_singleline(&mut self.text_buf);
                    r.request_focus();
                    ui.horizontal(|ui| {
                        if ui.button("Add").clicked()
                            || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            commit_text = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_text = true;
                        }
                    });
                    popup = ui.min_rect();
                });
        }
        self.popup_rect = popup;
        if commit_text {
            if let Some(at) = self.text_at.take() {
                if !self.text_buf.trim().is_empty() {
                    if let Some(pt) = self.to_norm(at, draw) {
                        let buf = std::mem::take(&mut self.text_buf);
                        let fp = self.font_size;
                        self.push_stroke_text(pt, buf, fp, k);
                    }
                }
                self.text_buf.clear();
            }
        }
        if cancel_text {
            self.text_at = None;
            self.text_buf.clear();
        }

        // ---- canvas gestures (ignore presses starting on chrome) ----
        let pointer = ctx.input(|i| i.pointer.clone());
        let over_chrome =
            |p: egui::Pos2| self.chrome_rect.contains(p) || self.popup_rect.contains(p);
        if let Some(p) = pointer.interact_pos() {
            if self.tool == DrawTool::Text {
                if pointer.primary_pressed() && !over_chrome(p) && draw.contains(p) {
                    self.text_at = Some(p);
                    self.text_buf.clear();
                }
            } else if self.tool == DrawTool::Pen {
                if pointer.primary_pressed() && !over_chrome(p) && draw.contains(p) {
                    self.active_pen = vec![p];
                } else if pointer.primary_down()
                    && !self.active_pen.is_empty()
                    && (*self.active_pen.last().unwrap() - p).length() >= 2.0
                {
                    self.active_pen.push(p);
                }
                if pointer.primary_released() && !self.active_pen.is_empty() {
                    let pts: Vec<(f32, f32)> = std::mem::take(&mut self.active_pen)
                        .iter()
                        .filter_map(|q| self.to_norm(*q, draw))
                        .collect();
                    let w = self.width * k;
                    self.push_stroke(pts, Tool::Pen, None, w);
                }
                if !pointer.primary_down() {
                    self.active_pen.clear();
                }
            } else {
                if pointer.primary_pressed() && !over_chrome(p) && draw.contains(p) {
                    self.shape_anchor = Some(p);
                    self.shape_current = p;
                } else if pointer.primary_down() && self.shape_anchor.is_some() {
                    self.shape_current = p;
                }
                if pointer.primary_released() {
                    if let Some(a) = self.shape_anchor.take() {
                        let b = self.shape_current;
                        let r = egui::Rect::from_two_pos(a, b);
                        if r.width() >= 4.0 && r.height() >= 4.0 {
                            let tool = self.tool.as_tool();
                            if let (Some(pa), Some(pb)) =
                                (self.to_norm(a, draw), self.to_norm(b, draw))
                            {
                                let w = self.width * k;
                                self.push_stroke(vec![pa, pb], tool, None, w);
                            }
                        }
                    }
                }
                if !pointer.primary_down() {
                    self.shape_anchor = None;
                }
            }
        }

        // ---- paint: video, strokes preview is IN the video (burned) ----
        let painter = ui.painter_at(avail);
        if let Some(tex) = &self.video_tex {
            painter.image(
                tex.id(),
                draw,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            painter.rect_filled(draw, 0.0, egui::Color32::from_gray(24));
            painter.text(
                draw.center(),
                egui::Align2::CENTER_CENTER,
                "waiting for video…",
                egui::FontId::proportional(16.0),
                egui::Color32::GRAY,
            );
        }
        painter.rect_stroke(
            draw,
            0.0,
            egui::Stroke::new(1.0_f32, egui::Color32::WHITE),
            egui::StrokeKind::Outside,
        );
        // In-progress rubber band.
        let band = egui::Stroke::new(1.0_f32, egui::Color32::YELLOW);
        if self.active_pen.len() >= 2 {
            painter.add(egui::Shape::line(self.active_pen.clone(), band));
        }
        if let Some(a) = self.shape_anchor {
            let b = self.shape_current;
            match self.tool {
                DrawTool::Line | DrawTool::Arrow => {
                    painter.line_segment([a, b], band);
                }
                _ => {
                    painter.rect_stroke(
                        egui::Rect::from_two_pos(a, b),
                        0.0,
                        band,
                        egui::StrokeKind::Outside,
                    );
                }
            }
        }
        // Size label.
        painter.text(
            egui::Pos2::new(draw.min.x, draw.max.y + 4.0),
            egui::Align2::LEFT_TOP,
            format!(
                "{}×{} feed · {} drawn",
                self.feed_w,
                self.feed_h,
                self.strokes.len()
            ),
            egui::FontId::monospace(12.0),
            egui::Color32::GRAY,
        );
    }

    fn push_stroke_text(&mut self, pt: (f32, f32), buf: String, font_px_logical: f32, k: f32) {
        // Scale logical font size to feed pixels so text matches the video 1:1.
        self.push_stroke(vec![pt], Tool::Text, Some(buf), font_px_logical * k);
    }

    /// Feed size accessors (preview resize updates these).
    pub fn feed_size(&self) -> (u32, u32) {
        (self.feed_w, self.feed_h)
    }
}

/// Result extracted from the draw window after it closes.
#[derive(Debug, Clone)]
pub struct DrawWindowResult {
    pub doc: AnnotateDoc,
    pub frames_shown: u64,
}

struct DrawWindowApp {
    panel: DrawPanel,
    preview_rx: flume::Receiver<qcapture_capture::pump::PreviewFrame>,
    /// Set by the capture thread when encoding ends (duration/Ctrl-C/error).
    /// The window auto-closes so `run_draw_window` returns to join/report.
    done_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    out: std::sync::Arc<std::sync::Mutex<Option<DrawWindowResult>>>,
}

impl eframe::App for DrawWindowApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Auto-close when capture ends; the caller joins + reports afterwards.
        if self.done_flag.load(std::sync::atomic::Ordering::SeqCst) {
            if let Ok(mut g) = self.out.lock() {
                *g = Some(DrawWindowResult {
                    doc: self.panel.doc(),
                    frames_shown: self.panel.frames_shown(),
                });
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // Keep the video alive: egui only repaints on input otherwise.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        self.panel.drain_previews(ctx, &self.preview_rx);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                self.panel.show(ctx, ui);
            });
    }
}

/// Run the standalone draw window on the calling (main) thread.
///
/// Capture runs on a background thread and streams previews in; strokes flow
/// out through `events_tx` to the pump. Returns the committed doc + preview
/// count for sidecar saving. Closing the window is the Stop button for CLI.
pub fn run_draw_window(
    feed_w: u32,
    feed_h: u32,
    events_tx: flume::Sender<DrawEvent>,
    preview_rx: flume::Receiver<qcapture_capture::pump::PreviewFrame>,
    done_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<DrawWindowResult, String> {
    let out: std::sync::Arc<std::sync::Mutex<Option<DrawWindowResult>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let out2 = out.clone();
    // Initial window fits on screen; the panel aspect-fits the feed inside.
    let (fw, fh) = (feed_w.max(64) as f32, feed_h.max(64) as f32);
    let s = (1280.0 / fw).min(720.0 / fh).clamp(0.15, 1.0);
    let viewport = egui::ViewportBuilder::default()
        .with_title("QCapture — draw live (close window to stop recording)")
        .with_inner_size([fw * s, fh * s])
        .with_active(true);
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "qcapture-draw",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(DrawWindowApp {
                panel: DrawPanel::new(feed_w, feed_h, events_tx.clone()),
                preview_rx: preview_rx.clone(),
                done_flag: done_flag.clone(),
                out: out2.clone(),
            }))
        }),
    )
    .map_err(|e| format!("draw window failed: {e}"))?;
    let guard = out.lock().map_err(|e| format!("state lock: {e}"))?;
    Ok(guard.clone().unwrap_or(DrawWindowResult {
        doc: AnnotateDoc {
            version: 1,
            canvas_w: feed_w,
            canvas_h: feed_h,
            strokes: Vec::new(),
            watermarks: Vec::new(),
            burn_in: true,
        },
        frames_shown: 0,
    }))
}
