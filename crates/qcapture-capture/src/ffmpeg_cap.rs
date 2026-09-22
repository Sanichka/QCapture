//! Phase 5a: byte-frame WGC capture feeding an ffmpeg rawvideo stdin pipe.
//!
//! Unlike the MediaFoundation path (DirectX surfaces stay on GPU), this path
//! pulls tight top-down BGRA bytes per frame so later stages (annotation
//! burn-in, scaler choice) can operate on pixels. Orchestration
//! (`run_ffmpeg_*`) lives here; the pipe itself is `qcapture-encode`.
//!
//! Layout discipline: ffmpeg rawvideo `bgra` input is TOP-DOWN — no vertical
//! flip here (contrast the MF `send_frame_buffer` path, which wants
//! bottom-to-top). Column order is untouched, so no mirror risk.

// Frame/job/stats/pump live in the shared pump module (also used by the
// portable xcap bridge); re-exported here so existing paths keep working.
use super::pump;
pub use super::pump::{FfmpegJob, FfmpegStats, PreviewFrame, RawFrame};
use super::CaptureError;
use qcapture_annotate::DrawEvent;
use qcapture_core::CanvasConfig;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

#[derive(Clone)]
struct PusherFlags {
    fps: u32,
    crop: Option<(u32, u32, u32, u32)>,
    tx: flume::Sender<RawFrame>,
    stop_at: Option<Instant>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    /// Virtual-screen origin of the feed (monitor top-left; region adds the
    /// crop offset). Maps `GetCursorPos` into frame pixels for screen/region.
    origin: (i32, i32),
    /// Window-target HWND for per-frame `GetWindowRect` mapping (survives
    /// window moves). None for screen/region (fixed geometry above).
    hwnd: Option<isize>,
    /// False when fx is off: skips all Win32 sampling (zero overhead).
    sample_cursor: bool,
}

/// Map a virtual-screen cursor point into feed-frame pixels.
/// Window targets use the live window rect; screen/region use the fixed
/// origin minus the crop offset. Returns None when off feed.
fn map_cursor_to_frame(
    pt: (i32, i32),
    win_rect: Option<(i32, i32)>,
    origin: (i32, i32),
    crop: Option<(u32, u32, u32, u32)>,
    feed_w: u32,
    feed_h: u32,
) -> Option<(i32, i32)> {
    let (fx, fy) = match win_rect {
        Some((left, top)) => (pt.0 - left, pt.1 - top),
        None => {
            let (cx, cy, _, _) = crop.unwrap_or((0, 0, 0, 0));
            (pt.0 - origin.0 - cx as i32, pt.1 - origin.1 - cy as i32)
        }
    };
    (fx >= 0 && fy >= 0 && (fx as u32) < feed_w && (fy as u32) < feed_h).then_some((fx, fy))
}

/// Sample cursor position + left/right button down-edges on the capture
/// thread (one Win32 call pair per kept frame, ~30Hz). Edge polling instead
/// of a global hook: no message-loop thread, and sub-frame clicks just
/// merge into the next ripple — fine for a visual effect.
fn sample_cursor(
    flags: &PusherFlags,
    feed_w: u32,
    feed_h: u32,
    prev: &mut (bool, bool),
) -> (Option<(i32, i32)>, bool) {
    use windows::Win32::Foundation::{HWND, POINT, RECT};
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return (None, false);
        }
        let win_rect = match flags.hwnd {
            Some(hwnd) => {
                let mut rc = RECT::default();
                if GetWindowRect(HWND(hwnd as *mut core::ffi::c_void), &mut rc).is_err() {
                    return (None, false);
                }
                Some((rc.left, rc.top))
            }
            None => None,
        };
        let cursor = map_cursor_to_frame(
            (pt.x, pt.y),
            win_rect,
            flags.origin,
            flags.crop,
            feed_w,
            feed_h,
        );
        let left = GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0;
        let right = GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0;
        let edge = (left && !prev.0) || (right && !prev.1);
        *prev = (left, right);
        (cursor, edge && cursor.is_some())
    }
}

struct Pusher {
    flags: PusherFlags,
    interval: Duration,
    next_tick: Instant,
    bufs: [Vec<u8>; 2],
    slot: usize,
    scratch: Vec<u8>,
    frames: u64,
    dropped: u64,
    start: Instant,
    last_report: Instant,
    prev_buttons: (bool, bool),
}

impl GraphicsCaptureApiHandler for Pusher {
    type Flags = PusherFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags.clone();
        let interval = Duration::from_secs_f64(1.0 / f.fps.max(1) as f64);
        Ok(Self {
            flags: f,
            interval,
            next_tick: Instant::now(),
            bufs: [Vec::new(), Vec::new()],
            slot: 0,
            scratch: Vec::new(),
            frames: 0,
            dropped: 0,
            start: Instant::now(),
            last_report: Instant::now(),
            prev_buttons: (false, false),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.stop_flag.load(Ordering::SeqCst) {
            control.stop();
            return Ok(());
        }
        if let Some(deadline) = self.flags.stop_at {
            if Instant::now() >= deadline {
                control.stop();
                return Ok(());
            }
        }
        // Paused: drop the frame entirely (pump idles, mixer idles — both
        // clocks freeze, so A/V stay in sync). next_tick goes stale, which
        // is exactly right: the first frame after resume sends immediately.
        if self.flags.pause_flag.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Decimate WGC's bursty delivery (up to display refresh) to target fps.
        let now = Instant::now();
        if now < self.next_tick {
            return Ok(());
        }
        self.next_tick = now + self.interval;
        if self.next_tick + Duration::from_millis(500) < Instant::now() {
            self.next_tick = Instant::now() + self.interval;
        }

        // Crop (GPU) or full frame -> tight top-down BGRA, zero-alloc steady state.
        let (w, h, bytes) = if let Some((cx, cy, cw, ch)) = self.flags.crop {
            let fw = frame.width();
            let fh = frame.height();
            if cx + cw > fw || cy + ch > fh {
                self.dropped += 1;
                return Ok(());
            }
            let cropped = frame
                .buffer_crop(cx, cy, cx + cw, cy + ch)
                .map_err(|e| -> Self::Error { Box::new(e) })?;
            let slot = self.slot;
            self.slot ^= 1;
            let mut buf = std::mem::take(&mut self.bufs[slot]);
            let n = {
                let t = cropped.as_nopadding_buffer(&mut self.scratch);
                if buf.len() < t.len() {
                    buf.resize(t.len(), 0);
                }
                buf.copy_from_slice(t);
                t.len()
            };
            buf.truncate(n);
            (cw, ch, buf)
        } else {
            let (w, h) = (frame.width(), frame.height());
            let slot = self.slot;
            self.slot ^= 1;
            let mut buf = std::mem::take(&mut self.bufs[slot]);
            let n = {
                let fb = frame.buffer().map_err(|e| -> Self::Error { Box::new(e) })?;
                let t = fb.as_nopadding_buffer(&mut self.scratch);
                if buf.len() < t.len() {
                    buf.resize(t.len(), 0);
                }
                buf.copy_from_slice(t);
                t.len()
            };
            buf.truncate(n);
            (w, h, buf)
        };

        // Cursor fx sampling rides the kept-frame rate (~30Hz): position in
        // frame pixels plus any button down-edge for the pump's ripple.
        let (cursor, clicked) = if self.flags.sample_cursor {
            sample_cursor(&self.flags, w, h, &mut self.prev_buttons)
        } else {
            (None, false)
        };

        match self.flags.tx.try_send(RawFrame {
            w,
            h,
            bgra: bytes,
            cursor,
            clicked,
        }) {
            Ok(()) => self.frames += 1,
            Err(flume::TrySendError::Full(f)) => {
                // Recycle the allocation; pipe backpressure means the encoder
                // (not capture) is the bottleneck — count and move on.
                self.bufs[self.slot ^ 1] = f.bgra;
                self.dropped += 1;
            }
            Err(flume::TrySendError::Disconnected(_)) => {
                control.stop(); // pump/encoder died — nothing to feed
                return Ok(());
            }
        }
        if self.last_report.elapsed() >= Duration::from_secs(2) {
            let el = self.start.elapsed().as_secs_f64();
            eprintln!(
                "  {:.1}s — {} frames @{}fps ({} dropped)",
                el, self.frames, self.flags.fps, self.dropped
            );
            self.last_report = Instant::now();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        eprintln!("capture item closed.");
        Ok(())
    }
}

// Compositor lives in the shared pump module (also used by the portable
// xcap bridge); this file only pushes WGC frames into it.

/// `origin` is the virtual-screen origin of the feed for cursor mapping
/// (unused for window targets, which map via `job.cursor_window` instead).
fn run_with_item<T>(
    item: T,
    feed_w: u32,
    feed_h: u32,
    origin: (i32, i32),
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError>
where
    T: TryInto<windows_capture::settings::GraphicsCaptureItemType>,
{
    if job.fps == 0 || job.fps > 240 {
        return Err(CaptureError::Backend("--fps must be 1..240".into()));
    }
    let canvas = CanvasConfig {
        width: job.canvas.map(|(w, _)| w).unwrap_or(feed_w),
        height: job.canvas.map(|(_, h)| h).unwrap_or(feed_h),
        fps: job.fps,
    };
    let sample_cursor = job.cursor_fx.is_some();
    let hwnd = job.cursor_window;
    let (tx, pump_handle) = pump::spawn_pump(feed_w, feed_h, job.fps, canvas, &job, live_rx)
        .map_err(CaptureError::Backend)?;

    let cursor = if job.show_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let settings = Settings::new(
        item,
        cursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        PusherFlags {
            fps: job.fps,
            crop: None,
            tx,
            stop_at: job.duration.map(|d| Instant::now() + d),
            stop_flag: job.stop_flag,
            pause_flag: job.pause_flag.clone(),
            origin,
            hwnd,
            sample_cursor,
        },
    );
    // NOTE on ordering (deadlock safety): the audio shutdown MUST run before
    // the pump join in every exit path. ffmpeg only exits once ALL inputs hit
    // EOF; the pump join waits for ffmpeg. Reversed = hang. `on_capture_end`
    // also runs when capture itself fails, so no thread ever leaks.
    let cap_res = Pusher::start(settings).map_err(|e| CaptureError::Backend(e.to_string()));
    on_capture_end();
    // ...then the pump drains the video pipe and finalizes the trailer.
    let pump_res = pump_handle
        .join()
        .map_err(|_| CaptureError::Backend("ffmpeg pump thread panicked".into()))?
        .map_err(CaptureError::Backend);
    cap_res?;
    pump_res
}

/// Like [`run_with_item`], but every frame is GPU-cropped to `rect` first.
/// `feed_w/h` (the encoder input size) is the crop size — top-down, no flip.
#[allow(clippy::too_many_arguments)]
fn run_cropped(
    monitor_1based: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError> {
    let monitor =
        Monitor::from_index(monitor_1based).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let native_w = monitor
        .width()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let native_h = monitor
        .height()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let x = x.min(native_w.saturating_sub(64));
    let y = y.min(native_h.saturating_sub(64));
    let (mut w, mut h) = (w.min(native_w - x).max(64), h.min(native_h - y).max(64));
    w &= !1;
    h &= !1;
    if w < 64 || h < 64 {
        return Err(CaptureError::Backend(
            "region too small after alignment (min 64x64 even)".into(),
        ));
    }

    let canvas = CanvasConfig {
        width: w,
        height: h,
        fps: job.fps,
    };
    eprintln!(
        "ffmpeg region {w}x{h}+{x}+{y} (of {native_w}x{native_h}) -> {}",
        job.output
    );
    let sample_cursor = job.cursor_fx.is_some();
    // Region frames are already cropped: cursor maps via monitor origin
    // minus the crop offset (see map_cursor_to_frame).
    let origin = super::monitor_origin(monitor_1based);
    let (tx, pump_handle) =
        pump::spawn_pump(w, h, job.fps, canvas, &job, live_rx).map_err(CaptureError::Backend)?;

    let cursor = if job.show_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let settings = Settings::new(
        monitor,
        cursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        PusherFlags {
            fps: job.fps,
            crop: Some((x, y, w, h)),
            tx,
            stop_at: job.duration.map(|d| Instant::now() + d),
            stop_flag: job.stop_flag,
            pause_flag: job.pause_flag.clone(),
            origin,
            hwnd: None,
            sample_cursor,
        },
    );
    // Same ordering contract as run_with_item (see above): audio EOF first,
    // pump join second, in every exit path.
    let cap_res = Pusher::start(settings).map_err(|e| CaptureError::Backend(e.to_string()));
    on_capture_end();
    let pump_res = pump_handle
        .join()
        .map_err(|_| CaptureError::Backend("ffmpeg pump thread panicked".into()))?
        .map_err(CaptureError::Backend);
    cap_res?;
    pump_res
}

pub fn run_ffmpeg_monitor(
    monitor_1based: usize,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError> {
    let monitor =
        Monitor::from_index(monitor_1based).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let (w, h) = (
        monitor
            .width()
            .map_err(|e| CaptureError::Backend(e.to_string()))?,
        monitor
            .height()
            .map_err(|e| CaptureError::Backend(e.to_string()))?,
    );
    eprintln!("WGC monitor#{monitor_1based} {w}x{h} -> ffmpeg");
    let origin = super::monitor_origin(monitor_1based);
    run_with_item(monitor, w, h, origin, job, on_capture_end, live_rx)
}

pub fn run_ffmpeg_window(
    title: &str,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError> {
    let window = Window::from_contains_name(title)
        .map_err(|e| CaptureError::Backend(format!("no window matching '{title}': {e}")))?;
    let (w, h) = (
        window.width().unwrap_or(1280).max(64) as u32,
        window.height().unwrap_or(720).max(64) as u32,
    );
    eprintln!("WGC window '{title}' -> ffmpeg");
    // Origin unused for windows (mapping goes through job.cursor_window);
    // without cursor fx the HWND is never touched.
    run_with_item(window, w, h, (0, 0), job, on_capture_end, live_rx)
}

#[allow(clippy::too_many_arguments)]
pub fn run_ffmpeg_region(
    monitor_1based: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError> {
    if job.canvas.is_some() {
        return Err(CaptureError::Backend(
            "--region and --canvas are mutually exclusive (region already fixes output size)"
                .into(),
        ));
    }
    run_cropped(monitor_1based, x, y, w, h, job, on_capture_end, live_rx)
}

/// ffmpeg CLI encoder name for logs (mirrors `RawvideoEncoder` resolution).
pub use super::pump::encoder_name;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_map_screen_region_and_window() {
        // Screen: origin subtracted.
        assert_eq!(
            map_cursor_to_frame((100, 200), None, (0, 0), None, 1920, 1080),
            Some((100, 200))
        );
        // Secondary monitor at -2560: origin shifted.
        assert_eq!(
            map_cursor_to_frame((-2500, 300), None, (-2560, 0), None, 2560, 1440),
            Some((60, 300))
        );
        // Region: origin + crop offset.
        assert_eq!(
            map_cursor_to_frame(
                (500, 400),
                None,
                (0, 0),
                Some((100, 100, 800, 600)),
                800,
                600
            ),
            Some((400, 300))
        );
        // Outside the feed hides the ring.
        assert_eq!(
            map_cursor_to_frame((50, 50), None, (0, 0), Some((100, 100, 800, 600)), 800, 600),
            None
        );
        // Window: live rect wins over origin/crop.
        assert_eq!(
            map_cursor_to_frame((1100, 500), Some((1000, 400)), (0, 0), None, 800, 600),
            Some((100, 100))
        );
    }
}
