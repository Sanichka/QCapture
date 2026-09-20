//! qcapture-capture: display/window enumeration + Capturer trait.
//! Phase 0/1: cross-platform enumeration via display-info + xcap.
//! Phase 2+: native WGC (windows-capture), SCKit (macOS), Portal+PipeWire (Linux).
//! Overlay self-exclusion is handled by native impls; fallback logs a warning.

use qcapture_core::{CaptureMethod, CaptureTarget, DisplayInfo, Rect, WindowInfo};
use tracing::warn;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no displays found")]
    NoDisplays,
    #[error("display {0} not found")]
    DisplayNotFound(u32),
    #[error("capture backend failed: {0}")]
    Backend(String),
    #[error(transparent)]
    DisplayInfo(#[from] display_info::error::DIError),
}

/// Hot-path frame handle placeholder. Phase 1 will carry BGRA bytes or GPU handles.
/// Kept as metadata-only here so Phase 0 builds without GPU deps.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Monotonic timestamp in nanoseconds since recording start (base_pts epoch).
    pub pts_ns: u64,
    pub screen_id: u32,
}

/// Minimal capturer trait. Implementations must be Send; frames flow via rtrb channel
/// (see architecture doc) to avoid allocs in capture callback.
pub trait Capturer: Send {
    fn method(&self) -> CaptureMethod;
    fn start(&mut self, target: CaptureTarget) -> Result<(), CaptureError>;
    fn stop(&mut self);
    fn is_running(&self) -> bool;
}

/// Map a 0-based screen position (0 = primary, as in `--list-screens` order)
/// to the 1-based WGC monitor index (`Monitor::from_index`).
///
/// Both enumerations are primary-first on Windows, so this is `+ 1`. The old
/// code special-cased 0 -> 1 and passed N through unchanged, which silently
/// recorded the PRIMARY monitor whenever a secondary was selected.
#[cfg(windows)]
pub fn wgc_monitor_index(screen_0based: u32) -> usize {
    screen_0based as usize + 1
}

/// Cross-platform display enumeration (X11, Wayland via compositor, Win, macOS).
pub fn list_displays() -> Result<Vec<DisplayInfo>, CaptureError> {
    let infos = display_info::DisplayInfo::all().map_err(CaptureError::DisplayInfo)?;
    if infos.is_empty() {
        return Err(CaptureError::NoDisplays);
    }
    Ok(infos
        .into_iter()
        .map(|d| DisplayInfo {
            // display-info 0.5: id, name/friendly_name, x/y/w/h, scale_factor, frequency, is_primary
            id: d.id,
            name: if d.friendly_name.is_empty() {
                d.name.clone()
            } else {
                d.friendly_name.clone()
            },
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
            scale_factor: d.scale_factor,
            is_primary: d.is_primary,
            refresh_hz: if d.frequency > 0.0 {
                Some(d.frequency.round() as u32)
            } else {
                None
            },
        })
        .collect())
}

/// Cross-platform window enumeration via xcap. Filters empty titles.
/// Per-OS note: WGC needs cloaked-window filtering on Windows — Phase 2 native impl.
pub fn list_windows() -> Result<Vec<WindowInfo>, CaptureError> {
    let windows = xcap::Window::all().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let mut out = Vec::new();
    for w in windows {
        // xcap 0.4 getters return XCapResult; skip windows that fail to query.
        let title = w.title().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let app_name = w.app_name().unwrap_or_default();
        let x = w.x().unwrap_or(0);
        let y = w.y().unwrap_or(0);
        let width = w.width().unwrap_or(0);
        let height = w.height().unwrap_or(0);
        let minimized = w.is_minimized().unwrap_or(false);
        let id = w
            .id()
            .map(|id| id as i64)
            .unwrap_or_else(|_| title_hash(&title, x, y));
        out.push(WindowInfo {
            id,
            title,
            app_name,
            x,
            y,
            width,
            height,
            minimized,
        });
    }
    Ok(out)
}

fn title_hash(title: &str, x: i32, y: i32) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    title.hash(&mut h);
    x.hash(&mut h);
    y.hash(&mut h);
    h.finish() as i64
}

/// Frozen screenshot of one monitor (tight RGBA, physical pixels).
///
/// Used as the region-picker backdrop (VokoScreenNG-style): painting a static
/// image makes the overlay independent of DWM per-pixel-alpha compositing,
/// which silently falls back to opaque black on some Win10 drivers for both
/// the glow and wgpu renderers.
#[derive(Debug, Clone)]
pub struct RgbaShot {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Capture the monitor containing virtual-screen point (x, y).
pub fn screenshot_monitor_at(x: i32, y: i32) -> Result<RgbaShot, CaptureError> {
    let monitor =
        xcap::Monitor::from_point(x, y).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let img = monitor
        .capture_image()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let (width, height) = (img.width(), img.height());
    if width == 0 || height == 0 {
        return Err(CaptureError::Backend("screenshot came back empty".into()));
    }
    Ok(RgbaShot {
        data: img.into_raw(),
        width,
        height,
    })
}

/// A resolved window target: feed-size guess plus the HWND for per-frame
/// cursor mapping (`GetWindowRect` survives window moves; the stored rect
/// would go stale).
#[cfg(windows)]
pub struct ResolvedWindow {
    pub w: u32,
    pub h: u32,
    pub hwnd: isize,
}

/// Resolve a window target by title substring (first case-insensitive
/// contains match wins — same match the capture item itself uses).
#[cfg(windows)]
pub fn resolve_window(needle: &str) -> Result<ResolvedWindow, CaptureError> {
    let window = windows_capture::window::Window::from_contains_name(needle)
        .map_err(|e| CaptureError::Backend(format!("no window matching '{needle}': {e}")))?;
    let w = window.width().unwrap_or(1280).max(64) as u32;
    let h = window.height().unwrap_or(720).max(64) as u32;
    Ok(ResolvedWindow {
        w: w & !1,
        h: h & !1,
        hwnd: window.as_raw_hwnd() as isize,
    })
}

/// Initial feed size guess for a window target (live preview self-corrects).
/// Used by draw UIs before capture starts; the pump's fixed-canvas rule
/// (adapt size flips, never re-init) covers resizes mid-record.
#[cfg(windows)]
pub fn window_feed_size(needle: &str) -> Result<(u32, u32), CaptureError> {
    resolve_window(needle).map(|r| (r.w, r.h))
}

/// Virtual-screen origin of a 1-based WGC monitor (inverse of
/// [`wgc_monitor_index`], same primary-first lookup the CLI uses).
/// Falls back to (0, 0) — cursor fx just offsets slightly on exotic layouts.
#[cfg(windows)]
pub fn monitor_origin(monitor_1based: usize) -> (i32, i32) {
    let screen = monitor_1based.saturating_sub(1) as u32;
    list_displays()
        .ok()
        .and_then(|ds| {
            if screen == 0 {
                ds.iter()
                    .find(|d| d.is_primary)
                    .or(ds.first())
                    .map(|d| (d.x, d.y))
            } else {
                ds.get(screen as usize).map(|d| (d.x, d.y))
            }
        })
        .unwrap_or((0, 0))
}

/// Validate a region against known displays. Used by CLI + overlay before Start.
pub fn validate_region(screen: u32, rect: Rect) -> Result<(), CaptureError> {
    qcapture_core::validate_rect(rect).map_err(|_| CaptureError::Backend("empty region".into()))?;
    let displays = list_displays()?;
    let d = displays
        .iter()
        .find(|d| d.id == screen)
        .ok_or(CaptureError::DisplayNotFound(screen))?;
    // Allow regions that extend slightly beyond (compositor clamps), but reject fully outside.
    if rect.x >= d.x + d.width as i32 || rect.y >= d.y + d.height as i32 {
        warn!(
            ?rect,
            ?d,
            "region fully outside display, compositor will clamp"
        );
    }
    Ok(())
}

/// Phase-0 dummy capturer so pipeline threads can be tested without OS capture.
pub struct DummyCapturer {
    running: bool,
    target: Option<CaptureTarget>,
}

impl DummyCapturer {
    pub fn new() -> Self {
        Self {
            running: false,
            target: None,
        }
    }
}

impl Default for DummyCapturer {
    fn default() -> Self {
        Self::new()
    }
}

impl Capturer for DummyCapturer {
    fn method(&self) -> CaptureMethod {
        CaptureMethod::FallbackXcap
    }
    fn start(&mut self, target: CaptureTarget) -> Result<(), CaptureError> {
        self.target = Some(target);
        self.running = true;
        Ok(())
    }
    fn stop(&mut self) {
        self.running = false;
    }
    fn is_running(&self) -> bool {
        self.running
    }
}

// ---------------------------------------------------------------------------
// Phase 1: native Windows record path (WGC + MediaFoundation HW encoder via
// windows-capture 2.x). Zero ffmpeg dependency for basic `record` — frames go
// Direct3D -> encoder without leaving the GPU. The ffmpeg rawvideo pipe in
// qcapture-encode stays for Phase 5 advanced settings (NVENC/AMF/QSV choice,
// fixed-canvas scale, multi-track).
// Linux (PipeWire) + macOS (SCKit) record paths land Phase 6.
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub mod win_record;

#[cfg(windows)]
pub use win_record::record_monitor as record_monitor_win;

#[cfg(windows)]
pub mod ffmpeg_cap;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_lifecycle() {
        let mut c = DummyCapturer::new();
        assert!(!c.is_running());
        c.start(CaptureTarget::Screen(0)).unwrap();
        assert!(c.is_running());
        c.stop();
        assert!(!c.is_running());
    }

    #[test]
    fn region_rejects_empty() {
        let r = Rect::new(10, 10, 0, 50);
        // No displays needed: empty fails before display lookup.
        assert!(validate_region(9999, r).is_err());
    }
}
