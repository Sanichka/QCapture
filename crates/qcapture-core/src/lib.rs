//! qcapture-core: shared types for capture targets, fixed canvas, encode config.
//! Zero frame data lives here — only descriptors, configs and control messages.
//! Frames move by OS handle (DXGI/DMABUF/IOSurface) in qcapture-capture, never cloned here.

use serde::{Deserialize, Serialize};

/// Physical-pixel rectangle. Origin is top-left in virtual-screen space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// Normalized rect 0.0..1.0 relative to fixed canvas.
/// Used by annotations so strokes survive crop/resize.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Fixed output canvas. Encoder inits ONCE; region changes only alter crop/scale.
/// This is the key invariant that lets us resize during recording without re-init.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
        }
    }
}

impl CanvasConfig {
    pub fn presets() -> Vec<Self> {
        vec![
            Self {
                width: 1280,
                height: 720,
                fps: 30,
            },
            Self {
                width: 1280,
                height: 720,
                fps: 60,
            },
            Self {
                width: 1920,
                height: 1080,
                fps: 30,
            },
            Self {
                width: 1920,
                height: 1080,
                fps: 60,
            },
            Self {
                width: 2560,
                height: 1440,
                fps: 60,
            },
            Self {
                width: 3840,
                height: 2160,
                fps: 30,
            },
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
    /// Refresh rate in Hz if known.
    pub refresh_hz: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Platform handle: HWND (Win), CGWindowID (mac), X11 Window.
    /// Stored as i64 for cross-platform serialization.
    pub id: i64,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub minimized: bool,
}

/// What the user wants to record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTarget {
    Screen(u32),
    Window(i64),
    /// Region on a given screen. Coords in physical pixels.
    Region {
        screen: u32,
        rect: Rect,
    },
}

/// Capture backend selected in Advanced Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CaptureMethod {
    #[default]
    Auto,
    WindowsWgc,
    WindowsDxgiDuplication,
    MacScreenCaptureKit,
    LinuxPortalPipeWire,
    LinuxX11Xshm,
    FallbackXcap,
}

impl CaptureMethod {
    /// Recommended default per OS. Called when user leaves "Auto".
    pub fn recommended_for_current_os() -> Self {
        #[cfg(target_os = "windows")]
        return Self::WindowsWgc;
        #[cfg(target_os = "macos")]
        return Self::MacScreenCaptureKit;
        #[cfg(target_os = "linux")]
        return Self::LinuxPortalPipeWire;
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        return Self::FallbackXcap;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EncoderKind {
    #[default]
    Auto,
    H264Nvenc,
    H264Amf,
    H264Qsv,
    H264VideoToolbox,
    H264MediaFoundation,
    LibX264,
    HevcNvenc,
    HevcQsv,
    Av1Qsv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RateControl {
    Cbr { bitrate_kbps: u32 },
    Vbr { target_kbps: u32, max_kbps: u32 },
    Cqp { qp: u8 },
    Crf { crf: u8 },
}

impl Default for RateControl {
    fn default() -> Self {
        Self::Cbr { bitrate_kbps: 8000 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeConfig {
    pub canvas: CanvasConfig,
    pub encoder: EncoderKind,
    pub rate_control: RateControl,
    /// GOP size / keyframe interval in frames.
    pub keyint: u32,
    pub preset: String,
    pub container: Container,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            canvas: CanvasConfig::default(),
            encoder: EncoderKind::Auto,
            rate_control: RateControl::default(),
            keyint: 120,
            preset: "veryfast".into(),
            container: Container::Mp4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    Mp4,
    Mkv,
    Mov,
}

impl Container {
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Mov => "mov",
        }
    }
}

/// Cursor fx visual style: ring/ripple colors (sRGB + alpha), radii in
/// feed pixels, ripple lifetime in milliseconds, and an additive glow mode
/// (adds light instead of blending over — egui's picker "Additive" radio
/// can't survive u8 storage, so glow is an explicit flag instead).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorStyle {
    pub hl_rgba: [u8; 4],
    pub hl_radius: f32,
    pub ripple_rgba: [u8; 4],
    pub ripple_radius: f32,
    pub ripple_ms: u32,
    pub additive: bool,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self {
            hl_rgba: [255, 210, 0, 255],
            hl_radius: 14.0,
            ripple_rgba: [255, 255, 255, 255],
            ripple_radius: 42.0,
            ripple_ms: 600,
            additive: false,
        }
    }
}

/// Cursor highlight ring + click ripple burned into the ffmpeg feed.
/// CPU-pixel effect: needs the ffmpeg byte path (auto-switches MF like
/// annotations). Defaults off to preserve the current recording look;
/// opt in per recording.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct CursorFx {
    pub highlight: bool,
    pub ripple: bool,
    pub style: CursorStyle,
}

impl CursorFx {
    pub fn opt(highlight: bool, ripple: bool) -> Option<Self> {
        Self::opt_with(highlight, ripple, CursorStyle::default())
    }

    pub fn opt_with(highlight: bool, ripple: bool, style: CursorStyle) -> Option<Self> {
        (highlight || ripple).then_some(Self {
            highlight,
            ripple,
            style,
        })
    }

    pub fn is_off(&self) -> bool {
        !self.highlight && !self.ripple
    }
}

/// Control-plane messages. Sent over flume channels, never in hot frame path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ControlMsg {
    StartRecording {
        target: CaptureTarget,
        output: String,
    },
    StopRecording,
    SetRegion(Rect),
    SetMicGain(f32),
    SetSystemGain(f32),
    SetMicMuted(bool),
    SetSystemMuted(bool),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub is_input: bool,
    pub is_loopback: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid rect {0:?}: width/height must be > 0")]
    InvalidRect(Rect),
    #[error("unsupported canvas {w}x{h}@ {fps}fps")]
    UnsupportedCanvas { w: u32, h: u32, fps: u32 },
}

pub fn validate_rect(r: Rect) -> Result<(), CoreError> {
    if r.is_empty() {
        return Err(CoreError::InvalidRect(r));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_validation() {
        assert!(validate_rect(Rect::new(0, 0, 100, 100)).is_ok());
        assert!(validate_rect(Rect::new(0, 0, 0, 100)).is_err());
    }

    #[test]
    fn cursor_style_defaults_match_legacy() {
        let style = CursorStyle::default();
        assert_eq!(style.hl_rgba, [255, 210, 0, 255]);
        assert_eq!(style.ripple_rgba, [255, 255, 255, 255]);
        assert_eq!(
            (style.hl_radius, style.ripple_radius, style.ripple_ms),
            (14.0, 42.0, 600)
        );
        assert!(!style.additive);
        let fx = CursorFx::opt(true, true).unwrap();
        assert_eq!(fx.style, style);
        assert!(CursorFx::opt(false, false).is_none());
        assert!(!fx.is_off());
    }

    #[test]
    fn canvas_presets_sane() {
        for c in CanvasConfig::presets() {
            assert!(c.width >= 640 && c.fps >= 15);
        }
    }
}
