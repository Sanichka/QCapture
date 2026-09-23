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

/// Color button without egui's Normal/Additive toggle. That toggle encodes
/// "additive" as negative alpha, which collapses to alpha 0 in u8 storage —
/// every click looks broken (nothing changes, swatch flashes). Additive glow
/// is an explicit flag wherever it is supported instead.
pub(crate) fn pick_color_no_additive(
    ui: &mut eframe::egui::Ui,
    c: &mut eframe::egui::Color32,
) -> bool {
    eframe::egui::widgets::color_picker::color_edit_button_srgba(
        ui,
        c,
        eframe::egui::widgets::color_picker::Alpha::OnlyBlend,
    )
    .changed()
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
