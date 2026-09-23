//! qcapture-ui: overlay + settings windows (Phase 2+).
//!
//! - [`pick_region`]: Phase 2a fullscreen drag-to-select overlay (egui).
//! - Floating widget, annotations toolbar and advanced settings land in 2b/4/5.

pub mod annotate_editor;
pub mod draw_panel;
pub mod pick_region;
pub mod settings;
pub mod widget;

/// Window/taskbar icon shared by every eframe viewport (widget, picker,
/// editor, draw): white Q ring + red record dot on a dark tile. Baked in
/// as raw 128x128 RGBA so no image decoder is needed at runtime.
pub fn app_icon() -> eframe::egui::IconData {
    const W: usize = 128;
    const H: usize = 128;
    let bytes = include_bytes!("../assets/icon-128.rgba");
    if bytes.len() == W * H * 4 {
        eframe::egui::IconData {
            rgba: bytes.to_vec(),
            width: W as u32,
            height: H as u32,
        }
    } else {
        // Asset missing/corrupt: 1x1 transparent rather than no icon.
        eframe::egui::IconData {
            rgba: vec![0, 0, 0, 0],
            width: 1,
            height: 1,
        }
    }
}

use qcapture_core::{CanvasConfig, CaptureTarget};

/// Color button with an explicit-× picker window. The stock
/// `color_edit_button` popup has no close affordance (only click-away),
/// so this owns a small window with the same inline picker plus the
/// native window ×. `id_salt` distinguishes the ring/ripple/custom sites.
pub(crate) fn pick_color_closable(
    ui: &mut eframe::egui::Ui,
    id_salt: &str,
    c: &mut eframe::egui::Color32,
) -> bool {
    use eframe::egui::widgets::color_picker::{color_picker_color32, Alpha};
    let popup_id = ui.make_persistent_id(id_salt);
    let mut open = ui.data_mut(|d| d.get_temp::<bool>(popup_id).unwrap_or(false));
    let mut changed = false;
    let swatch = eframe::egui::RichText::new("■").color(*c).size(18.0);
    if ui.button(swatch).on_hover_text("Pick color…").clicked() {
        open = !open;
    }
    if open {
        let mut close = false;
        eframe::egui::Window::new(format!("Color — {id_salt}"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                // Roomier selection canvas than the cramped default: the 2D
                // spectrum + sliders scale with slider_width.
                ui.spacing_mut().slider_width = 230.0;
                if color_picker_color32(ui, c, Alpha::OnlyBlend) {
                    changed = true;
                }
                ui.horizontal(|ui| {
                    if ui.small_button("Close").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            open = false;
        }
    }
    ui.data_mut(|d| d.insert_temp(popup_id, open));
    changed
}

/// What the overlay picker returns. Norm coords keep annotations stable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerSelection {
    Screen(u32),
    Window(i64),
    Region {
        screen: u32,
        rect: qcapture_core::Rect,
    },
}

impl From<PickerSelection> for CaptureTarget {
    fn from(s: PickerSelection) -> Self {
        match s {
            PickerSelection::Screen(id) => Self::Screen(id),
            PickerSelection::Window(id) => Self::Window(id),
            PickerSelection::Region { screen, rect } => Self::Region { screen, rect },
        }
    }
}

/// Stub entry point. Real overlay replaces body; CLI never calls this in headless CI.
pub fn run_overlay_stub(_canvas: CanvasConfig) -> Result<(), UiError> {
    // Intentionally unimplemented: prevents accidentally shipping a no-op GUI.
    Err(UiError::NotImplemented(
        "overlay arrives in Phase 2 (winit+egui)".into(),
    ))
}

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("not implemented: {0}")]
    NotImplemented(String),
    #[error("windowing backend failed: {0}")]
    Backend(String),
}
