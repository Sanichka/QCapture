//! qcapture-ui: overlay + settings windows (Phase 2+).
//!
//! - [`pick_region`]: Phase 2a fullscreen drag-to-select overlay (egui).
//! - Floating widget, annotations toolbar and advanced settings land in 2b/4/5.

pub mod annotate_editor;
pub mod draw_panel;
pub mod pick_region;
pub mod settings;
pub mod widget;

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
