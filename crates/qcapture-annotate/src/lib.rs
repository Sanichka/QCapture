//! qcapture-annotate: hybrid annotation doc (burn-in + sidecar JSON).
//! Coords are normalized 0..1 relative to the feed size so strokes survive
//! crop/scale changes during recording. `raster` turns timed strokes into a
//! straight-alpha RGBA overlay with dirty-rect tracking for cheap blending.

pub mod raster;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tool {
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stroke {
    /// Normalized points 0..1 in canvas space.
    pub points: Vec<(f32, f32)>,
    pub color: Rgba,
    pub width_px: f32,
    pub tool: Tool,
    /// For text tool: UTF-8 payload. For image: asset id in `images`.
    pub text: Option<String>,
    /// ms since recording start when the stroke appears; persists afterwards
    /// (whiteboard semantics, matches live drawing).
    #[serde(default)]
    pub appear_ms: u64,
    /// Text height in canvas pixels (Text tool). Default 32.
    #[serde(default)]
    pub font_px: Option<f32>,
    /// Fill shape interior (Rect/Ellipse tools). Default: outline only.
    #[serde(default)]
    pub filled: bool,
    /// Font file path (Text tool). None = OS default (Arial on Windows).
    #[serde(default)]
    pub font_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watermark {
    /// Image file path (png/jpg), resolved relative to CWD at record time.
    pub asset_id: String,
    /// Normalized corner rect.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
    /// ms since recording start when the watermark appears.
    #[serde(default)]
    pub appear_ms: u64,
}

/// Versioned doc saved as `<recording>.qcap.json` alongside video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotateDoc {
    pub version: u32,
    /// Canvas size the norm coords were authored against (for sanity check).
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub strokes: Vec<Stroke>,
    pub watermarks: Vec<Watermark>,
    pub burn_in: bool,
}

impl Default for AnnotateDoc {
    fn default() -> Self {
        Self {
            version: 1,
            canvas_w: 1920,
            canvas_h: 1080,
            strokes: vec![],
            watermarks: vec![],
            burn_in: true,
        }
    }
}

/// Live drawing events (Phase 4c draw panel -> pump thread). The pump stamps
/// `appear_ms` at receipt; the panel keeps a local stroke list for counts.
/// Serialize derives stay for `.qcap.json` sidecar compat.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum DrawEvent {
    AddStroke(Stroke),
    Undo,
    Clear,
}

#[derive(Debug, thiserror::Error)]
pub enum AnnotateError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("stroke has no points")]
    EmptyStroke,
}

impl AnnotateDoc {
    pub fn add_stroke(&mut self, s: Stroke) -> Result<(), AnnotateError> {
        if s.points.is_empty() {
            return Err(AnnotateError::EmptyStroke);
        }
        // Clamp to 0..1 so out-of-canvas drags don't poison compositor.
        let mut clamped = s;
        for p in &mut clamped.points {
            p.0 = p.0.clamp(0.0, 1.0);
            p.1 = p.1.clamp(0.0, 1.0);
        }
        self.strokes.push(clamped);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.strokes.clear();
        self.watermarks.clear();
    }

    pub fn to_json(&self) -> Result<String, AnnotateError> {
        serde_json::to_string_pretty(self).map_err(|e| AnnotateError::Json(e.to_string()))
    }

    pub fn from_json(s: &str) -> Result<Self, AnnotateError> {
        serde_json::from_str(s).map_err(|e| AnnotateError::Json(e.to_string()))
    }

    pub fn load(path: &str) -> Result<Self, AnnotateError> {
        let s =
            std::fs::read_to_string(path).map_err(|e| AnnotateError::Io(format!("{path}: {e}")))?;
        Self::from_json(&s)
    }

    pub fn save(&self, path: &str) -> Result<(), AnnotateError> {
        let s = self.to_json()?;
        std::fs::write(path, s).map_err(|e| AnnotateError::Io(format!("{path}: {e}")))
    }

    pub fn sidecar_path_for(video_path: &str) -> String {
        format!("{video_path}.qcap.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_and_roundtrip() {
        let mut doc = AnnotateDoc::default();
        doc.add_stroke(Stroke {
            points: vec![(-0.5, 2.0), (0.5, 0.5)],
            color: Rgba(255, 0, 0, 255),
            width_px: 3.0,
            tool: Tool::Pen,
            text: None,
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        })
        .unwrap();
        assert_eq!(doc.strokes[0].points[0], (0.0, 1.0));
        let json = doc.to_json().unwrap();
        let back = AnnotateDoc::from_json(&json).unwrap();
        assert_eq!(doc, back);
    }

    #[test]
    fn rejects_empty() {
        let mut doc = AnnotateDoc::default();
        let r = doc.add_stroke(Stroke {
            points: vec![],
            color: Rgba(0, 0, 0, 255),
            width_px: 2.0,
            tool: Tool::Line,
            text: None,
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        });
        assert!(r.is_err());
    }
}
