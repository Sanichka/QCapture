//! Phase 4b: visual annotation editor.
//!
//! Fullscreen flow over one monitor: drag-select a region (same interaction as
//! `pick_region`), then draw on the frozen screenshot — pen, line, arrow,
//! rect, ellipse, text — with a live preview of exactly what burns into the
//! video. "Record" saves the `.qcap.json` doc and returns the selected region
//! so the caller can record it with `--annotate` burn-in.
//!
//! Strokes are authored with `appear_ms = 0` (visible from the first frame)
//! in coordinates normalized to the SELECTED REGION, which is the burn-in
//! feed size. Like the picker, this needs no compositor alpha: the window is
//! opaque, the desktop is a frozen screenshot.

use eframe::egui;
use qcapture_annotate::{AnnotateDoc, Rgba, Stroke, Tool};
use qcapture_core::Rect;
use std::sync::{Arc, Mutex};

pub use super::pick_region::{MonitorGeom, MIN_EDGE};

/// Run the editor. Returns the selected region on Record, `None` on cancel.
/// The doc is saved to `save_path` on Record (and on demand via Save).
/// `backdrop` is the frozen screenshot; None falls back to a dark canvas.
pub fn run(
    monitor: MonitorGeom,
    backdrop: Option<qcapture_capture::RgbaShot>,
    save_path: String,
) -> Result<Option<Rect>, String> {
    let out: Arc<Mutex<Option<Option<Rect>>>> = Arc::new(Mutex::new(None));
    let backdrop_image = backdrop.as_ref().and_then(|shot| {
        (shot.width == monitor.w && shot.height == monitor.h).then(|| {
            egui::ColorImage::from_rgba_unmultiplied(
                [shot.width as usize, shot.height as usize],
                &shot.data,
            )
        })
    });
    let app = editor_app(monitor, save_path, backdrop_image, out.clone());

    let scale = monitor.scale.max(1.0);
    let viewport = egui::ViewportBuilder::default()
        .with_title("QCapture — select region, then annotate (Enter: next, Esc: back/cancel)")
        .with_icon(super::app_icon())
        .with_transparent(false)
        .with_decorations(false)
        .with_always_on_top()
        .with_resizable(false)
        .with_position([monitor.x as f32 / scale, monitor.y as f32 / scale])
        .with_inner_size([monitor.w as f32 / scale, monitor.h as f32 / scale])
        .with_active(true);

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "qcapture-annotate",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| format!("editor failed: {e}"))?;

    let guard = out.lock().map_err(|e| format!("state lock: {e}"))?;
    Ok(guard.flatten())
}

/// Build the editor app (split from [`run`] so headless UI tests drive the
/// exact same controls the production overlay shows).
fn editor_app(
    monitor: MonitorGeom,
    save_path: String,
    backdrop_image: Option<egui::ColorImage>,
    done: Arc<Mutex<Option<Option<Rect>>>>,
) -> EditorApp {
    EditorApp {
        monitor,
        save_path,
        backdrop_image,
        backdrop_tex: None,
        phase: Phase::Select,
        sel_anchor: None,
        sel_current: egui::Pos2::ZERO,
        selection: None,
        tool: DrawTool::Pen,
        color: egui::Color32::RED,
        width: 4.0,
        font_size: 40.0,
        filled: false,
        image_path: None,
        pick_rx: None,
        strokes: Vec::new(),
        active_pen: Vec::new(),
        shape_anchor: None,
        shape_current: egui::Pos2::ZERO,
        text_at: None,
        text_buf: String::new(),
        preview_tex: None,
        preview_rev: 0,
        built_rev: u64::MAX,
        save_msg: String::new(),
        done,
    }
}

struct EditorApp {
    monitor: MonitorGeom,
    save_path: String,
    backdrop_image: Option<egui::ColorImage>,
    backdrop_tex: Option<egui::TextureHandle>,
    phase: Phase,
    sel_anchor: Option<egui::Pos2>,
    sel_current: egui::Pos2,
    selection: Option<egui::Rect>,
    tool: DrawTool,
    color: egui::Color32,
    width: f32,
    font_size: f32,
    filled: bool,
    image_path: Option<String>,
    /// Native file picker in flight (overlay minimized meanwhile so the
    /// dialog is on top and interactive; minimized/restored like the widget
    /// overlays do — hide/show does not reliably restore a borderless
    /// always-on-top window). Result polled per frame below.
    /// NOTE for tests: never click "Pick image…" headlessly — the worker
    /// opens a real native dialog and hangs CI.
    pick_rx: Option<std::sync::mpsc::Receiver<Option<String>>>,
    strokes: Vec<Stroke>,
    active_pen: Vec<egui::Pos2>,
    shape_anchor: Option<egui::Pos2>,
    shape_current: egui::Pos2,
    text_at: Option<egui::Pos2>,
    text_buf: String,
    preview_tex: Option<egui::TextureHandle>,
    preview_rev: u64,
    built_rev: u64,
    save_msg: String,
    done: Arc<Mutex<Option<Option<Rect>>>>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Phase {
    Select,
    Draw,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum DrawTool {
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Text,
    Image,
}

impl DrawTool {
    fn label(self) -> &'static str {
        match self {
            Self::Pen => "Pen",
            Self::Line => "Line",
            Self::Arrow => "Arrow",
            Self::Rect => "Rect",
            Self::Ellipse => "Ellipse",
            Self::Text => "Text",
            Self::Image => "Img",
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
            Self::Image => Tool::Image,
        }
    }
}

impl EditorApp {
    fn close_with(&self, ctx: &egui::Context, rect: Option<Rect>) {
        if let Ok(mut guard) = self.done.lock() {
            *guard = Some(rect);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn save_doc(&mut self, ppp: f32) -> Result<(), String> {
        let doc = self.build_doc(ppp);
        doc.save(&self.save_path)
            .map_err(|e| format!("save {}: {e}", self.save_path))?;
        self.save_msg = format!("saved {} ({} strokes)", self.save_path, doc.strokes.len());
        Ok(())
    }

    fn region_phys(&self, ppp: f32) -> Option<(u32, u32)> {
        let sel = self.selection?;
        let w = (sel.width() * ppp).round() as i32;
        let h = (sel.height() * ppp).round() as i32;
        (w >= MIN_EDGE && h >= MIN_EDGE).then_some((w as u32, h as u32))
    }

    fn build_doc(&self, ppp: f32) -> AnnotateDoc {
        let (rw, rh) = self
            .region_phys(ppp)
            .unwrap_or((self.monitor.w, self.monitor.h));
        AnnotateDoc {
            version: 1,
            canvas_w: rw,
            canvas_h: rh,
            strokes: self.strokes.clone(),
            watermarks: Vec::new(),
            burn_in: true,
        }
    }

    /// Logical window-relative point -> region-normalized (0..1) coords.
    fn to_norm(&self, p: egui::Pos2, sel: egui::Rect, ppp: f32) -> Option<(f32, f32)> {
        let (rw, rh) = self.region_phys(ppp)?;
        let px = (p.x - sel.min.x) * ppp;
        let py = (p.y - sel.min.y) * ppp;
        Some((
            (px / rw as f32).clamp(0.0, 1.0),
            (py / rh as f32).clamp(0.0, 1.0),
        ))
    }

    fn push_stroke(&mut self, points: Vec<(f32, f32)>, tool: Tool, text: Option<String>, ppp: f32) {
        if points.is_empty() {
            return;
        }
        // Single-point pen (click without drag) is a dot; shapes need a span.
        if points.len() < 2 && !matches!(tool, Tool::Text | Tool::Pen) {
            return;
        }
        // Image without a picked file is a no-op (raster would skip it anyway).
        if tool == Tool::Image && text.as_deref().map(str::is_empty).unwrap_or(true) {
            return;
        }
        // Endpoint-preserving Chaikin, like the live draw panel.
        let points = if tool == Tool::Pen {
            qcapture_annotate::smooth_polyline(&points, 2)
        } else {
            points
        };
        let c = self.color;
        self.strokes.push(Stroke {
            points,
            color: Rgba(c.r(), c.g(), c.b(), c.a()),
            // Feed pixels (not logical points) — the burn-in rasterizer works
            // in feed space, so scale once here at commit time.
            width_px: (self.width * ppp).max(1.0),
            tool,
            text,
            appear_ms: 0,
            font_px: Some((self.font_size * ppp).max(6.0)),
            // Only Rect/Ellipse read this; the raster ignores it elsewhere.
            filled: self.filled,
            font_path: None,
        });
        self.preview_rev += 1;
    }

    /// Window-relative logical selection -> monitor-relative physical rect.
    fn selection_physical(&self, sel: egui::Rect, ppp: f32, win: egui::Rect) -> Option<Rect> {
        let x = ((sel.min.x - win.min.x) * ppp).round() as i32;
        let y = ((sel.min.y - win.min.y) * ppp).round() as i32;
        let w = (sel.width() * ppp).round() as i32;
        let h = (sel.height() * ppp).round() as i32;
        (w >= MIN_EDGE && h >= MIN_EDGE).then_some(Rect::new(x, y, w as u32, h as u32))
    }

    /// Rebuild the preview texture when strokes changed.
    fn rebuild_preview(&mut self, ctx: &egui::Context, ppp: f32) {
        if self.built_rev == self.preview_rev {
            return;
        }
        self.built_rev = self.preview_rev;
        let (rw, rh) = match self.region_phys(ppp) {
            Some(v) => v,
            None => return,
        };
        let doc = self.build_doc(ppp);
        let mut ann = qcapture_annotate::raster::Annotator::new(doc, rw, rh);
        ann.apply_until(u64::MAX);
        let img = egui::ColorImage::from_rgba_unmultiplied(
            [rw as usize, rh as usize],
            ann.overlay_rgba(),
        );
        self.preview_tex =
            Some(ctx.load_texture("annotate-preview", img, egui::TextureOptions::NEAREST));
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

impl EditorApp {
    fn live_rect(&self) -> Option<egui::Rect> {
        match (self.sel_anchor, self.selection) {
            (Some(a), _) => {
                let r = egui::Rect::from_two_pos(a, self.sel_current);
                (r.width() >= 2.0 && r.height() >= 2.0).then_some(r)
            }
            (None, Some(s)) => Some(s),
            (None, None) => None,
        }
    }

    fn dim_around(
        &self,
        painter: &egui::Painter,
        win: egui::Rect,
        sel: Option<egui::Rect>,
        alpha: u8,
    ) {
        let dim = egui::Color32::from_black_alpha(alpha);
        match sel {
            Some(s) => {
                painter.rect_filled(
                    egui::Rect::from_min_max(win.min, egui::Pos2::new(win.max.x, s.min.y)),
                    0.0,
                    dim,
                );
                painter.rect_filled(
                    egui::Rect::from_min_max(egui::Pos2::new(win.min.x, s.max.y), win.max),
                    0.0,
                    dim,
                );
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::Pos2::new(win.min.x, s.min.y),
                        egui::Pos2::new(s.min.x, s.max.y),
                    ),
                    0.0,
                    dim,
                );
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::Pos2::new(s.max.x, s.min.y),
                        egui::Pos2::new(win.max.x, s.max.y),
                    ),
                    0.0,
                    dim,
                );
            }
            None => {
                painter.rect_filled(win, 0.0, egui::Color32::from_black_alpha(alpha / 2));
            }
        }
    }

    fn update_select(
        &mut self,
        ctx: &egui::Context,
        _ui: &mut egui::Ui,
        win: egui::Rect,
        ppp: f32,
        painter: &egui::Painter,
    ) {
        let pointer = ctx.input(|i| i.pointer.clone());
        if let Some(p) = pointer.interact_pos() {
            self.sel_current = p;
            if pointer.primary_pressed() {
                self.sel_anchor = Some(p);
            }
            if pointer.primary_released() {
                if let Some(a) = self.sel_anchor.take() {
                    let r = egui::Rect::from_two_pos(a, p);
                    if r.width() >= 4.0 && r.height() >= 4.0 {
                        self.selection = Some(r);
                    }
                }
            }
            if pointer.secondary_pressed() {
                if self.selection.take().is_none() {
                    self.close_with(ctx, None);
                    return;
                }
                self.sel_anchor = None;
            }
        }
        // Enter adopts the selection and moves to drawing.
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Some(r) = self.live_rect() {
                let norm = egui::Rect::from_min_max(
                    egui::Pos2::new(
                        (r.min.x - win.min.x) / win.width(),
                        (r.min.y - win.min.y) / win.height(),
                    ),
                    egui::Pos2::new(
                        (r.max.x - win.min.x) / win.width(),
                        (r.max.y - win.min.y) / win.height(),
                    ),
                );
                let _ = norm;
                self.selection = Some(r);
                // Validate physical size before switching.
                if self.selection_physical(r, ppp, win).is_some() {
                    self.phase = Phase::Draw;
                    self.preview_rev += 1; // build empty preview (cheap, keeps code simple)
                } else {
                    self.selection = None;
                }
            }
        }

        let sel = self.live_rect();
        self.dim_around(painter, win, sel, 140);
        if let Some(s) = sel {
            painter.rect_stroke(
                s,
                0.0,
                egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                egui::StrokeKind::Outside,
            );
            if let Some(phys) = self.selection_physical(s, ppp, win) {
                painter.text(
                    egui::Pos2::new(s.min.x, (s.min.y - 22.0).max(win.min.y + 4.0)),
                    egui::Align2::LEFT_TOP,
                    format!("{} × {} @ {},{}", phys.w, phys.h, phys.x, phys.y),
                    egui::FontId::monospace(14.0),
                    egui::Color32::WHITE,
                );
            }
        }
    }

    fn update_draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        win: egui::Rect,
        ppp: f32,
        painter: &egui::Painter,
    ) {
        let sel = match self.selection {
            Some(s) => s,
            None => {
                self.phase = Phase::Select;
                return;
            }
        };

        // ---- native picker result (overlay was hidden meanwhile) ----
        match self.pick_rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(picked)) => {
                if picked.is_some() {
                    self.image_path = picked;
                }
                self.pick_rx = None;
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                // Picker thread died before restoring; unminimize defensively.
                self.pick_rx = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }
            _ => {}
        }

        // ---- toolbar (floating window; canvas gestures ignore its rect) ----
        let mut chrome_rect = egui::Rect::NOTHING;
        egui::Window::new("Annotate")
            .anchor(egui::Align2::LEFT_TOP, [12.0, 12.0])
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
                        DrawTool::Image,
                    ] {
                        ui.selectable_value(&mut self.tool, t, t.label());
                    }
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.filled, "Filled")
                        .on_hover_text("Fill rectangles and ellipses (outline otherwise)");
                    if self.tool == DrawTool::Image {
                        // The editor is fullscreen always-on-top: it would bury
                        // the native dialog and eat its input. Minimize first,
                        // pick on a thread (blocking is fine while minimized),
                        // then restore — same pattern as the widget overlays.
                        if ui.button("Pick image…").clicked() && self.pick_rx.is_none() {
                            let ctx2 = ctx.clone();
                            let (tx, rx) = std::sync::mpsc::channel();
                            self.pick_rx = Some(rx);
                            eprintln!("hiding editor for native image picker…");
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                            std::thread::spawn(move || {
                                // Let the minimize land before the dialog opens.
                                std::thread::sleep(std::time::Duration::from_millis(300));
                                let picked = rfd::FileDialog::new()
                                    .set_title("QCapture stamp image")
                                    .add_filter("Images", &["png", "jpg", "jpeg", "bmp"])
                                    .pick_file()
                                    .map(|p| p.to_string_lossy().into_owned());
                                eprintln!(
                                    "image picker done (picked: {}) — restoring editor…",
                                    picked.as_deref().unwrap_or("<cancelled>")
                                );
                                ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                                ctx2.send_viewport_cmd(egui::ViewportCommand::Focus);
                                // Wake the loop: a minimized window may not be
                                // pumping frames, leaving the restore queued.
                                ctx2.request_repaint();
                                let _ = tx.send(picked);
                            });
                        }
                        let picking = self.pick_rx.is_some();
                        ui.label(if picking {
                            "picking…".to_string()
                        } else {
                            match &self.image_path {
                                Some(p) => {
                                    // Show just the file name; the full path burns in.
                                    std::path::Path::new(p)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(p)
                                        .to_string()
                                }
                                None => "pick an image first".to_string(),
                            }
                        });
                    }
                });
                ui.horizontal(|ui| {
                    for c in PRESET_COLORS {
                        let mark = if self.color == c { "◉" } else { "●" };
                        if ui
                            .button(egui::RichText::new(mark).color(c))
                            .on_hover_text(format!("rgb({},{},{})", c.r(), c.g(), c.b()))
                            .clicked()
                        {
                            self.color = c;
                        }
                    }
                    super::pick_color_no_additive(ui, &mut self.color);
                });
                ui.horizontal(|ui| {
                    ui.label("Width");
                    ui.add(egui::Slider::new(&mut self.width, 1.0..=32.0).show_value(true));
                    ui.label("Text");
                    ui.add(egui::Slider::new(&mut self.font_size, 8.0..=120.0).show_value(true));
                });
                ui.horizontal(|ui| {
                    if ui
                        .button("Undo")
                        .on_hover_text("Remove last stroke (Ctrl+Z)")
                        .clicked()
                        || (ui.input(|i| i.modifiers.ctrl)
                            && ui.input(|i| i.key_pressed(egui::Key::Z)))
                    {
                        self.strokes.pop();
                        self.active_pen.clear();
                        self.shape_anchor = None;
                        self.preview_rev += 1;
                    }
                    if ui.button("Clear").clicked() {
                        self.strokes.clear();
                        self.active_pen.clear();
                        self.shape_anchor = None;
                        self.preview_rev += 1;
                    }
                    ui.label(format!("{} strokes", self.strokes.len()));
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("Record").clicked() {
                        self.record_now(ctx, ppp, win);
                    }
                    if ui.button("Save").clicked() {
                        if let Err(e) = self.save_doc(ppp) {
                            self.save_msg = e;
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.close_with(ctx, None);
                    }
                });
                if !self.save_msg.is_empty() {
                    ui.label(&self.save_msg);
                }
                chrome_rect = ui.min_rect();
            });

        // ---- text popup ----
        let mut popup_rect = egui::Rect::NOTHING;
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
                    popup_rect = ui.min_rect();
                });
        }
        if commit_text {
            if let Some(at) = self.text_at.take() {
                if !self.text_buf.trim().is_empty() {
                    if let Some(pt) = self.to_norm(at, sel, ppp) {
                        let buf = std::mem::take(&mut self.text_buf);
                        self.push_stroke(vec![pt], Tool::Text, Some(buf), ppp);
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
        let chrome = |p: egui::Pos2| chrome_rect.contains(p) || popup_rect.contains(p);
        if let Some(p) = pointer.interact_pos() {
            if self.tool == DrawTool::Text {
                if pointer.primary_pressed() && !chrome(p) {
                    self.text_at = Some(p);
                    self.text_buf.clear();
                }
            } else if self.tool == DrawTool::Pen {
                if pointer.primary_pressed() && !chrome(p) {
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
                        .filter_map(|q| self.to_norm(*q, sel, ppp))
                        .collect();
                    self.push_stroke(pts, Tool::Pen, None, ppp);
                }
                if !pointer.primary_down() {
                    self.active_pen.clear();
                }
            } else {
                if pointer.primary_pressed() && !chrome(p) {
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
                            // Image stamps carry the picked file path;
                            // push_stroke drops pathless images.
                            let text = (tool == Tool::Image)
                                .then(|| self.image_path.clone())
                                .flatten();
                            if let (Some(pa), Some(pb)) =
                                (self.to_norm(a, sel, ppp), self.to_norm(b, sel, ppp))
                            {
                                self.push_stroke(vec![pa, pb], tool, text, ppp);
                            }
                        }
                    }
                }
                if !pointer.primary_down() {
                    self.shape_anchor = None;
                }
            }
        }

        // ---- paint ----
        self.rebuild_preview(ctx, ppp);
        self.dim_around(painter, win, Some(sel), 60);
        if let Some(tex) = &self.preview_tex {
            painter.image(
                tex.id(),
                sel,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        painter.rect_stroke(
            sel,
            0.0,
            egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
            egui::StrokeKind::Outside,
        );
        // In-progress pen preview in true style (smoothed like the commit).
        if self.active_pen.len() >= 2 {
            let smooth: Vec<egui::Pos2> = qcapture_annotate::smooth_polyline(
                &self
                    .active_pen
                    .iter()
                    .map(|p| (p.x, p.y))
                    .collect::<Vec<_>>(),
                2,
            )
            .into_iter()
            .map(|(x, y)| egui::Pos2::new(x, y))
            .collect();
            painter.add(egui::Shape::line(
                smooth,
                egui::Stroke::new(self.width, self.color),
            ));
        } else if self.active_pen.len() == 1 {
            painter.circle_filled(self.active_pen[0], self.width / 2.0, self.color);
        }
        // Transient shape guides stay yellow (commit replaces them).
        let band = egui::Stroke::new(1.0_f32, egui::Color32::YELLOW);
        if let Some(a) = self.shape_anchor {
            let b = self.shape_current;
            match self.tool {
                DrawTool::Line | DrawTool::Arrow => {
                    painter.line_segment([a, b], band);
                }
                DrawTool::Rect | DrawTool::Ellipse | DrawTool::Text | DrawTool::Image => {
                    painter.rect_stroke(
                        egui::Rect::from_two_pos(a, b),
                        0.0,
                        band,
                        egui::StrokeKind::Outside,
                    );
                }
                DrawTool::Pen => {}
            }
        }
        let _ = ui;
    }

    fn record_now(&mut self, ctx: &egui::Context, ppp: f32, win: egui::Rect) {
        let sel = match self.selection {
            Some(s) => s,
            None => {
                self.save_msg = "select a region first (Esc)".to_string();
                return;
            }
        };
        let phys = match self.selection_physical(sel, ppp, win) {
            Some(r) => r,
            None => {
                self.save_msg = "region too small (min 64×64)".to_string();
                return;
            }
        };
        if let Err(e) = self.save_doc(ppp) {
            self.save_msg = e;
            return;
        }
        self.close_with(ctx, Some(phys));
    }
}

impl eframe::App for EditorApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let ppp = ctx.pixels_per_point();
        ctx.set_cursor_icon(egui::CursorIcon::Crosshair);

        if self.backdrop_tex.is_none() {
            if let Some(img) = self.backdrop_image.take() {
                self.backdrop_tex =
                    Some(ctx.load_texture("editor-backdrop", img, egui::TextureOptions::LINEAR));
            }
        }

        // Esc ladder: text popup -> Draw-to-Select (clears) -> cancel.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.text_at.take().is_some() {
                self.text_buf.clear();
            } else if self.phase == Phase::Draw {
                self.phase = Phase::Select;
                self.strokes.clear();
                self.preview_tex = None;
                self.preview_rev += 1;
                self.built_rev = u64::MAX - 1;
            } else if self.selection.take().is_none() {
                self.close_with(ctx, None);
                return;
            } else {
                self.sel_anchor = None;
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let win = ui.max_rect();
                let painter = ui.painter_at(win);
                if let Some(tex) = &self.backdrop_tex {
                    painter.image(
                        tex.id(),
                        win,
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }

                match self.phase {
                    Phase::Select => self.update_select(ctx, ui, win, ppp, &painter),
                    Phase::Draw => self.update_draw(ctx, ui, win, ppp, &painter),
                }

                let hint = match self.phase {
                    Phase::Select => {
                        "drag: select region · Enter: annotate · right-click: clear · Esc: cancel"
                    }
                    Phase::Draw => {
                        "draw inside the region · Ctrl+Z: undo · Record burns it in · Esc: back"
                    }
                };
                painter.text(
                    egui::Pos2::new(win.center().x, win.max.y - 34.0),
                    egui::Align2::CENTER_BOTTOM,
                    hint,
                    egui::FontId::proportional(14.0),
                    egui::Color32::from_white_alpha(220),
                );
            });

        ctx.request_repaint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, Modifiers, PointerButton, Pos2, Vec2};
    use egui_kittest::{kittest::Queryable, Harness};

    fn test_editor() -> EditorApp {
        editor_app(
            MonitorGeom {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
                scale: 1.0,
            },
            std::env::temp_dir()
                .join("qcapture-editor-test.qcap.json")
                .to_string_lossy()
                .into_owned(),
            None,
            Arc::new(Mutex::new(None)),
        )
    }

    #[test]
    fn fill_flag_rides_rect_but_not_line() {
        let mut app = test_editor();
        app.filled = true;
        app.push_stroke(vec![(0.1, 0.1), (0.4, 0.4)], Tool::Rect, None, 1.0);
        app.push_stroke(vec![(0.1, 0.1), (0.4, 0.4)], Tool::Line, None, 1.0);
        assert_eq!(app.strokes.len(), 2);
        assert!(app.strokes[0].filled);
        // Stored verbatim even where the raster ignores it.
        assert!(app.strokes[1].filled);

        app.filled = false;
        app.push_stroke(vec![(0.1, 0.1), (0.4, 0.4)], Tool::Ellipse, None, 1.0);
        assert!(!app.strokes[2].filled);
    }

    #[test]
    fn image_commit_needs_picked_path() {
        let mut app = test_editor();
        // No path picked: drag commits nothing.
        app.push_stroke(vec![(0.1, 0.1), (0.4, 0.4)], Tool::Image, None, 1.0);
        app.push_stroke(
            vec![(0.1, 0.1), (0.4, 0.4)],
            Tool::Image,
            Some(String::new()),
            1.0,
        );
        assert!(app.strokes.is_empty());
        // Picked path: commits with the path as payload.
        app.image_path = Some("D:\\stamps\\arrow.png".into());
        app.push_stroke(
            vec![(0.1, 0.1), (0.4, 0.4)],
            Tool::Image,
            app.image_path.clone(),
            1.0,
        );
        assert_eq!(app.strokes.len(), 1);
        assert_eq!(app.strokes[0].tool, Tool::Image);
        assert_eq!(
            app.strokes[0].text.as_deref(),
            Some("D:\\stamps\\arrow.png")
        );
    }

    /// Drive Select -> Draw, then flip the Filled checkbox and Img tool.
    /// Never touches "Pick image…" (native dialog would hang headless CI).
    #[test]
    fn toolbar_fill_and_image_select() {
        let mut h = Harness::builder()
            .with_size(Vec2::new(800.0, 600.0))
            .build_eframe(|_cc| test_editor());
        // Drag-select a region, Enter adopts it and switches to Draw.
        h.input_mut()
            .events
            .push(Event::PointerMoved(Pos2::new(100.0, 100.0)));
        h.input_mut().events.push(Event::PointerButton {
            pos: Pos2::new(100.0, 100.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::default(),
        });
        h.step();
        h.input_mut()
            .events
            .push(Event::PointerMoved(Pos2::new(300.0, 300.0)));
        h.step();
        h.input_mut().events.push(Event::PointerButton {
            pos: Pos2::new(300.0, 300.0),
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        });
        h.step();
        h.key_combination(&[eframe::egui::Key::Enter]);
        // The editor repaints every frame, so run() never settles —
        // fixed steps process the queued input just as well.
        h.run_steps(4);
        assert!(h.state().phase == Phase::Draw);

        h.get_by_label("Filled").click();
        h.run_steps(4);
        assert!(h.state().filled);
        h.get_by_label("Img").click();
        h.run_steps(4);
        assert!(h.state().tool == DrawTool::Image);
    }
}
