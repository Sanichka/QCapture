//! Phase 2a: fullscreen region picker overlay.
//!
//! Opens an always-on-top borderless window over one monitor showing a frozen
//! screenshot of that monitor as its backdrop (VokoScreenNG-style). The user
//! drags to select a monitor-relative region; on Enter the app closes and the
//! physical-pixel rect is returned for `record --region`.
//!
//! Why a screenshot backdrop instead of a transparent window: DWM per-pixel
//! alpha silently falls back to an opaque black surface on some Win10 drivers
//! for both the glow and wgpu renderers. Painting the desktop ourselves makes
//! the picker independent of compositor alpha. Bonus: the image is frozen, so
//! nothing shifts under the cursor while selecting.
//!
//! Coordinate discipline: egui works in logical points; the recorder needs
//! physical pixels. `physical = logical * pixels_per_point`, rounded. The
//! window is sized/positioned in points (`physical / monitor_scale`) so the
//! overlay exactly covers the target monitor on HiDPI setups.

use eframe::egui;
use qcapture_core::Rect;
use std::sync::{Arc, Mutex};

/// Monitor geometry in physical pixels (virtual-screen space) + scale factor.
#[derive(Debug, Clone, Copy)]
pub struct MonitorGeom {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub scale: f32,
}

/// Frozen monitor screenshot (tight RGBA, physical pixels) painted as backdrop.
/// `None` falls back to the old transparent-window behavior.
pub type Backdrop = Option<qcapture_capture::RgbaShot>;

/// Minimum selectable edge in physical pixels (matches encoder floor).
pub const MIN_EDGE: i32 = 64;

/// Run the picker over `monitor`. Returns `Ok(None)` on Esc/cancel/close.
pub fn run(monitor: MonitorGeom, backdrop: Backdrop) -> Result<Option<Rect>, String> {
    let out: Arc<Mutex<Option<Option<Rect>>>> = Arc::new(Mutex::new(None));
    let backdrop_image = backdrop.as_ref().and_then(|shot| {
        // Guard against a stale/wrong-size screenshot (mode flip between capture
        // and overlay open): fall back to transparency rather than stretch.
        (shot.width == monitor.w && shot.height == monitor.h).then(|| {
            egui::ColorImage::from_rgba_unmultiplied(
                [shot.width as usize, shot.height as usize],
                &shot.data,
            )
        })
    });
    let state = PickerApp {
        anchor: None,
        current: egui::Pos2::ZERO,
        selection: None,
        moving: None,
        backdrop_image,
        backdrop_tex: None,
        done: out.clone(),
    };

    let scale = monitor.scale.max(1.0);
    let size_pts = [monitor.w as f32 / scale, monitor.h as f32 / scale];
    let pos_pts = [monitor.x as f32 / scale, monitor.y as f32 / scale];

    let viewport = egui::ViewportBuilder::default()
        .with_title("QCapture — drag to select region (Enter: confirm, Esc: cancel)")
        .with_transparent(true)
        .with_decorations(false)
        .with_always_on_top()
        .with_resizable(false)
        .with_position(pos_pts)
        .with_inner_size(size_pts)
        .with_active(true);

    let options = eframe::NativeOptions {
        viewport,
        // Explicit: eframe defaults to glow when both renderers are compiled in,
        // and glow falls back to opaque on drivers without transparent WGL configs.
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "qcapture-pick-region",
        options,
        Box::new(|_cc| Ok(Box::new(state))),
    )
    .map_err(|e| format!("overlay failed: {e}"))?;

    let guard = out.lock().map_err(|e| format!("state lock: {e}"))?;
    Ok(guard.flatten())
}

struct PickerApp {
    anchor: Option<egui::Pos2>,
    current: egui::Pos2,
    /// Logical-point selection rect (normalized, window-relative).
    selection: Option<egui::Rect>,
    /// Active move-drag: grab offset from selection min.
    moving: Option<egui::Vec2>,
    backdrop_image: Option<egui::ColorImage>,
    backdrop_tex: Option<egui::TextureHandle>,
    done: Arc<Mutex<Option<Option<Rect>>>>,
}

impl PickerApp {
    fn close_with(&self, ctx: &egui::Context, rect: Option<Rect>) {
        if let Ok(mut guard) = self.done.lock() {
            *guard = Some(rect);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn live_rect(&self) -> Option<egui::Rect> {
        match (self.anchor, self.selection) {
            (Some(a), _) => {
                let r = egui::Rect::from_two_pos(a, self.current);
                (r.width() >= 2.0 && r.height() >= 2.0).then_some(r)
            }
            (None, Some(s)) => Some(s),
            (None, None) => None,
        }
    }

    fn to_physical(&self, r: egui::Rect, ppp: f32, win: egui::Rect) -> Option<Rect> {
        // Window-relative logical -> monitor-relative physical.
        let x = ((r.min.x - win.min.x) * ppp).round() as i32;
        let y = ((r.min.y - win.min.y) * ppp).round() as i32;
        let w = (r.width() * ppp).round() as i32;
        let h = (r.height() * ppp).round() as i32;
        if w < MIN_EDGE || h < MIN_EDGE {
            return None;
        }
        Some(Rect::new(x, y, w as u32, h as u32))
    }
}

impl eframe::App for PickerApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Opaque black: the screenshot backdrop covers everything from the
        // first frame, so there is no translucency flash while loading.
        [0.0, 0.0, 0.0, 1.0]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let ppp = ctx.pixels_per_point();
        ctx.set_cursor_icon(egui::CursorIcon::Crosshair);
        // Esc cancels from anywhere.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_with(ctx, None);
            return;
        }
        // Upload the frozen screenshot once, then paint it every frame.
        if self.backdrop_tex.is_none() {
            if let Some(img) = self.backdrop_image.take() {
                self.backdrop_tex =
                    Some(ctx.load_texture("monitor-backdrop", img, egui::TextureOptions::LINEAR));
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
            .show(ctx, |ui| {
                let win = ui.max_rect();
                let pointer = ctx.input(|i| i.pointer.clone());
                let pos = pointer.interact_pos();

                // Enter confirms the current selection.
                if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Some(r) = self.live_rect() {
                        match self.to_physical(r, ppp, win) {
                            Some(phys) => {
                                self.close_with(ctx, Some(phys));
                                return;
                            }
                            None => {
                                // Too small — keep picking.
                            }
                        }
                    }
                }

                // Mouse state machine.
                if let Some(p) = pos {
                    self.current = p;
                    if pointer.primary_pressed() {
                        if let Some(sel) = self.selection {
                            if self.anchor.is_none() && sel.contains(p) {
                                // Start moving the existing selection.
                                self.moving = Some(p - sel.min);
                            } else if self.moving.is_none() {
                                self.anchor = Some(p);
                            }
                        } else {
                            self.anchor = Some(p);
                        }
                    }
                    if pointer.primary_down() {
                        if let Some(off) = self.moving {
                            if let Some(sel) = self.selection {
                                let size = sel.size();
                                let mut min = p - off;
                                // Clamp move inside window.
                                min.x = min.x.clamp(win.min.x, win.max.x - size.x);
                                min.y = min.y.clamp(win.min.y, win.max.y - size.y);
                                self.selection = Some(egui::Rect::from_min_size(min, size));
                            }
                        }
                    }
                    if pointer.primary_released() && self.moving.take().is_none() {
                        if let Some(a) = self.anchor.take() {
                            let r = egui::Rect::from_two_pos(a, p);
                            if r.width() >= 4.0 && r.height() >= 4.0 {
                                // Shift locks to square.
                                let square = ctx.input(|i| i.modifiers.shift);
                                self.selection = Some(if square {
                                    let side = r.width().max(r.height());
                                    egui::Rect::from_min_size(r.min, egui::Vec2::new(side, side))
                                } else {
                                    r
                                });
                            }
                        }
                    }
                    // Right-click cancels the in-progress selection (second click exits).
                    if pointer.secondary_pressed() {
                        if self.selection.take().is_none() {
                            self.close_with(ctx, None);
                            return;
                        }
                        self.anchor = None;
                    }
                }

                let painter = ui.painter_at(win);
                // Frozen desktop screenshot first — this is what makes the
                // picker usable when DWM alpha compositing is unavailable.
                if let Some(tex) = &self.backdrop_tex {
                    painter.image(
                        tex.id(),
                        win,
                        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                match self.live_rect() {
                    Some(sel) => {
                        // Dim everything outside the selection (4 rects).
                        let dim = egui::Color32::from_black_alpha(140);
                        painter.rect_filled(
                            egui::Rect::from_min_max(win.min, egui::Pos2::new(win.max.x, sel.min.y)),
                            0.0,
                            dim,
                        );
                        painter.rect_filled(
                            egui::Rect::from_min_max(egui::Pos2::new(win.min.x, sel.max.y), win.max),
                            0.0,
                            dim,
                        );
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::Pos2::new(win.min.x, sel.min.y),
                                egui::Pos2::new(sel.min.x, sel.max.y),
                            ),
                            0.0,
                            dim,
                        );
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::Pos2::new(sel.max.x, sel.min.y),
                                egui::Pos2::new(win.max.x, sel.max.y),
                            ),
                            0.0,
                            dim,
                        );
                        // Border + handles.
                        painter.rect_stroke(
                            sel,
                            0.0,
                            egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                            egui::StrokeKind::Outside,
                        );
                        let hs = 5.0;
                        for hx in [sel.min.x, sel.center().x, sel.max.x] {
                            for hy in [sel.min.y, sel.center().y, sel.max.y] {
                                painter.rect_filled(
                                    egui::Rect::from_center_size(
                                        egui::Pos2::new(hx, hy),
                                        egui::Vec2::splat(hs),
                                    ),
                                    0.0,
                                    egui::Color32::WHITE,
                                );
                            }
                        }
                        // Size label (physical pixels).
                        if let Some(phys) =
                            self.to_physical(sel, ppp, win).or_else(|| {
                                // Show live size even below MIN_EDGE.
                                let w = (sel.width() * ppp).round() as i32;
                                let h = (sel.height() * ppp).round() as i32;
                                (w > 0 && h > 0).then(|| {
                                    let x =
                                        ((sel.min.x - win.min.x) * ppp).round() as i32;
                                    let y =
                                        ((sel.min.y - win.min.y) * ppp).round() as i32;
                                    Rect::new(x, y, w as u32, h as u32)
                                })
                            })
                        {
                            let too_small =
                                phys.w < MIN_EDGE as u32 || phys.h < MIN_EDGE as u32;
                            let text = format!(
                                "{} × {} @ {},{} {}",
                                phys.w,
                                phys.h,
                                phys.x,
                                phys.y,
                                if too_small { "(min 64×64)" } else { "" }
                            );
                            painter.text(
                                egui::Pos2::new(sel.min.x, (sel.min.y - 22.0).max(win.min.y + 4.0)),
                                egui::Align2::LEFT_TOP,
                                text,
                                egui::FontId::monospace(14.0),
                                if too_small {
                                    egui::Color32::LIGHT_RED
                                } else {
                                    egui::Color32::WHITE
                                },
                            );
                        }
                    }
                    None => {
                        painter.rect_filled(
                            win,
                            0.0,
                            egui::Color32::from_black_alpha(70),
                        );
                        if let Some(p) = pos {
                            // Crosshair when idle.
                            let c = egui::Color32::from_white_alpha(90);
                            painter.line_segment(
                                [egui::Pos2::new(win.min.x, p.y), egui::Pos2::new(win.max.x, p.y)],
                                egui::Stroke::new(1.0_f32, c),
                            );
                            painter.line_segment(
                                [egui::Pos2::new(p.x, win.min.y), egui::Pos2::new(p.x, win.max.y)],
                                egui::Stroke::new(1.0_f32, c),
                            );
                        }
                    }
                }

                // Hint bar.
                painter.text(
                    egui::Pos2::new(win.center().x, win.max.y - 34.0),
                    egui::Align2::CENTER_BOTTOM,
                    "drag: select · drag inside: move · Shift: square · Enter: confirm · Esc: cancel",
                    egui::FontId::proportional(14.0),
                    egui::Color32::from_white_alpha(220),
                );
            });

        // Keep repainting for smooth drag feedback.
        ctx.request_repaint();
    }
}
