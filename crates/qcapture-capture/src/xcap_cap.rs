//! Portable capture via xcap (Linux/macOS): monitor video recording where
//! the platform has it (AVFoundation on macOS), screenshot polling
//! elsewhere (Linux X11/Wayland, window targets everywhere), bridged into
//! the shared pump so annotations, live drawing, previews, ffmpeg encoding
//! and audio all work unchanged.
//!
//! Differences from the Windows WGC path worth knowing:
//! - Frames arrive as RGBA and are swizzled to BGRA in place (zero alloc).
//! - Cursor sampling is Windows-only: cursor fx flags are rejected before
//!   we get here, and frames carry `cursor: None`.
//! - The OS cursor is typically baked into frames already, so `--no-cursor`
//!   has no effect on this backend (warned once per run).
//! - Linux screenshot polling is CPU-heavy by nature; fps pacing + latest-
//!   only draining keep memory bounded and the encoder fed.

use super::pump::{crop_rgba, rgba_to_bgra_in_place, spawn_pump, FfmpegJob, FfmpegStats, RawFrame};
use super::CaptureError;
use qcapture_annotate::DrawEvent;
use qcapture_core::{CanvasConfig, DisplayInfo};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// How many consecutive source errors before giving up (window closed,
/// Wayland denying screenshots, display unplugged). ~10 s at 30 fps.
const MAX_CONSEC_ERRORS: u32 = 300;

/// Even-align feed dims (ffmpeg yuv420p needs even) with a 64px floor.
fn feed_size(w: u32, h: u32) -> Result<(u32, u32), CaptureError> {
    let (w, h) = (w & !1, h & !1);
    if w < 64 || h < 64 {
        return Err(CaptureError::Backend(
            "capture feed too small after alignment (min 64x64 even)".into(),
        ));
    }
    Ok((w, h))
}

struct Pacer {
    interval: Duration,
    next_tick: Instant,
}

impl Pacer {
    fn new(fps: u32) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / fps.max(1) as f64),
            next_tick: Instant::now(),
        }
    }

    /// False when this tick should be skipped (source faster than target).
    fn poll(&mut self) -> bool {
        let now = Instant::now();
        if now < self.next_tick {
            return false;
        }
        self.next_tick = now + self.interval;
        if self.next_tick + Duration::from_millis(500) < Instant::now() {
            self.next_tick = Instant::now() + self.interval;
        }
        true
    }
}

/// Shared tail: spawn the pump, run `bridge` on the calling thread, then
/// shut audio down BEFORE joining the pump (ffmpeg only exits once all
/// inputs hit EOF — reversed order deadlocks; same contract as WGC).
fn run_with_bridge(
    feed_w: u32,
    feed_h: u32,
    canvas: CanvasConfig,
    fps: u32,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
    source_note: &str,
    bridge: impl FnOnce(flume::Sender<RawFrame>) -> Result<(), CaptureError>,
) -> Result<FfmpegStats, CaptureError> {
    if fps == 0 || fps > 240 {
        return Err(CaptureError::Backend("--fps must be 1..240".into()));
    }
    if !job.show_cursor {
        eprintln!("note: xcap frames bake in the OS cursor; --no-cursor has no effect here");
    }
    eprintln!(
        "xcap {source_note} {feed_w}x{feed_h}@{fps} -> {}",
        job.output
    );
    let (tx, pump_handle) =
        spawn_pump(feed_w, feed_h, fps, canvas, &job, live_rx).map_err(CaptureError::Backend)?;
    let bridge_res = bridge(tx);
    on_capture_end();
    let pump_res = pump_handle
        .join()
        .map_err(|_| CaptureError::Backend("ffmpeg pump thread panicked".into()))?
        .map_err(CaptureError::Backend);
    bridge_res?;
    pump_res
}

fn deadline_of(job: &FfmpegJob) -> Option<Instant> {
    job.duration.map(|d| Instant::now() + d)
}

pub fn run_xcap_monitor(
    display: &DisplayInfo,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError> {
    // Canvas scaling works here too (ffmpeg -vf scale in the pump spawn).
    let monitor = xcap::Monitor::from_point(display.x, display.y).map_err(|e| {
        CaptureError::Backend(format!("xcap monitor at {},{}: {e}", display.x, display.y))
    })?;
    let mw = monitor
        .width()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let mh = monitor
        .height()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let (feed_w, feed_h) = feed_size(mw, mh)?;
    let canvas = CanvasConfig {
        width: job.canvas.map(|(w, _)| w).unwrap_or(feed_w),
        height: job.canvas.map(|(_, h)| h).unwrap_or(feed_h),
        fps: job.fps,
    };
    let fps = job.fps;
    let stop_at = deadline_of(&job);
    let stop_flag = job.stop_flag.clone();
    run_with_bridge(
        feed_w,
        feed_h,
        canvas,
        fps,
        job,
        on_capture_end,
        live_rx,
        "monitor",
        move |tx| {
            let (rec, rx) = monitor
                .video_recorder()
                .map_err(|e| CaptureError::Backend(format!("xcap recorder: {e}")))?;
            rec.start()
                .map_err(|e| CaptureError::Backend(format!("xcap recorder start: {e}")))?;
            let mut pacer = Pacer::new(fps);
            let mut sent = 0u64;
            let mut dropped = 0u64;
            let mut last_report = Instant::now();
            loop {
                if stop_flag.load(Ordering::SeqCst) || stop_at.is_some_and(|t| Instant::now() >= t)
                {
                    break;
                }
                // Blocking wait with a timeout so stop/duration stays
                // responsive; then drain to latest (Linux polls faster
                // than target fps on an unbounded channel).
                let first = match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        eprintln!("xcap source closed — saving partial recording.");
                        break;
                    }
                };
                let mut latest = first;
                while let Ok(f) = rx.try_recv() {
                    latest = f;
                }
                if !pacer.poll() {
                    continue;
                }
                // Mode flips mid-record forward as-is; the pump adapts
                // (center crop/pad) under the fixed-canvas rule.
                let mut bgra = latest.raw;
                rgba_to_bgra_in_place(&mut bgra);
                let frame = RawFrame {
                    w: latest.width,
                    h: latest.height,
                    bgra,
                    cursor: None,
                    clicked: false,
                };
                match tx.try_send(frame) {
                    Ok(()) => sent += 1,
                    Err(flume::TrySendError::Full(_)) => dropped += 1,
                    Err(flume::TrySendError::Disconnected(_)) => break,
                }
                if last_report.elapsed() >= Duration::from_secs(2) {
                    eprintln!("  {sent} frames @{fps}fps ({dropped} dropped)");
                    last_report = Instant::now();
                }
            }
            let _ = rec.stop();
            Ok(())
        },
    )
}

pub fn run_xcap_region(
    display: &DisplayInfo,
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
    let monitor = xcap::Monitor::from_point(display.x, display.y).map_err(|e| {
        CaptureError::Backend(format!("xcap monitor at {},{}: {e}", display.x, display.y))
    })?;
    let mw = monitor
        .width()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let mh = monitor
        .height()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let x = x.min(mw.saturating_sub(64));
    let y = y.min(mh.saturating_sub(64));
    let (mut w, mut h) = (w.min(mw - x).max(64), h.min(mh - y).max(64));
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
    let fps = job.fps;
    let stop_at = deadline_of(&job);
    let stop_flag = job.stop_flag.clone();
    run_with_bridge(
        w,
        h,
        canvas,
        fps,
        job,
        on_capture_end,
        live_rx,
        &format!("region {w}x{h}+{x}+{y} (of {mw}x{mh})"),
        move |tx| {
            let (rec, rx) = monitor
                .video_recorder()
                .map_err(|e| CaptureError::Backend(format!("xcap recorder: {e}")))?;
            rec.start()
                .map_err(|e| CaptureError::Backend(format!("xcap recorder start: {e}")))?;
            let mut pacer = Pacer::new(fps);
            let mut sent = 0u64;
            let mut last_report = Instant::now();
            loop {
                if stop_flag.load(Ordering::SeqCst) || stop_at.is_some_and(|t| Instant::now() >= t)
                {
                    break;
                }
                let first = match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(f) => f,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        eprintln!("xcap source closed — saving partial recording.");
                        break;
                    }
                };
                let mut latest = first;
                while let Ok(f) = rx.try_recv() {
                    latest = f;
                }
                if !pacer.poll() {
                    continue;
                }
                if x + w > latest.width || y + h > latest.height {
                    continue; // mode flip mid-record; pump would adapt anyway
                }
                let mut cropped = crop_rgba(&latest.raw, latest.width, x, y, w, h);
                rgba_to_bgra_in_place(&mut cropped);
                let frame = RawFrame {
                    w,
                    h,
                    bgra: cropped,
                    cursor: None,
                    clicked: false,
                };
                if tx.try_send(frame).is_err() {
                    break; // Full (backpressure, counted as drop) or pump gone.
                }
                sent += 1;
                if last_report.elapsed() >= Duration::from_secs(2) {
                    eprintln!("  {sent} frames @{fps}fps");
                    last_report = Instant::now();
                }
            }
            let _ = rec.stop();
            Ok(())
        },
    )
}

pub fn run_xcap_window(
    title: &str,
    job: FfmpegJob,
    on_capture_end: Box<dyn FnOnce() + Send>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<FfmpegStats, CaptureError> {
    let needle = title.to_lowercase();
    let window = xcap::Window::all()
        .map_err(|e| CaptureError::Backend(e.to_string()))?
        .into_iter()
        .find(|w| {
            w.title()
                .map(|t| t.to_lowercase().contains(&needle))
                .unwrap_or(false)
        })
        .ok_or_else(|| CaptureError::Backend(format!("no window matching '{title}'")))?;
    let found_title = window.title().unwrap_or_default();
    let (ww, wh) = (
        window.width().unwrap_or(1280).max(64),
        window.height().unwrap_or(720).max(64),
    );
    let (feed_w, feed_h) = feed_size(ww, wh)?;
    let canvas = CanvasConfig {
        width: job.canvas.map(|(w, _)| w).unwrap_or(feed_w),
        height: job.canvas.map(|(_, h)| h).unwrap_or(feed_h),
        fps: job.fps,
    };
    eprintln!("xcap window '{found_title}' -> ffmpeg");
    let fps = job.fps;
    let stop_at = deadline_of(&job);
    let stop_flag = job.stop_flag.clone();
    run_with_bridge(
        feed_w,
        feed_h,
        canvas,
        fps,
        job,
        on_capture_end,
        live_rx,
        "window",
        move |tx| {
            // No streaming window API in xcap: poll screenshots at target
            // fps. On Wayland this typically errors (security model) — the
            // error cap below turns that into a loud, fast failure.
            let mut pacer = Pacer::new(fps);
            let mut sent = 0u64;
            let mut errors = 0u32;
            let mut last_report = Instant::now();
            loop {
                if stop_flag.load(Ordering::SeqCst) || stop_at.is_some_and(|t| Instant::now() >= t)
                {
                    break;
                }
                if !pacer.poll() {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                let img = match window.capture_image() {
                    Ok(img) => {
                        errors = 0;
                        img
                    }
                    Err(e) => {
                        errors += 1;
                        if errors == 1 {
                            eprintln!("window capture failing ({e}) — retrying…");
                        }
                        if errors >= MAX_CONSEC_ERRORS {
                            return Err(CaptureError::Backend(format!(
                                "window capture failed {errors}x in a row \
                                 (closed? Wayland blocks window screenshots): {e}"
                            )));
                        }
                        continue;
                    }
                };
                let (iw, ih) = (img.width(), img.height());
                if iw == 0 || ih == 0 {
                    continue;
                }
                let mut bgra = img.into_raw();
                rgba_to_bgra_in_place(&mut bgra);
                let frame = RawFrame {
                    w: iw,
                    h: ih,
                    bgra,
                    cursor: None,
                    clicked: false,
                };
                match tx.try_send(frame) {
                    Ok(()) => sent += 1,
                    Err(flume::TrySendError::Full(_)) => {}
                    Err(flume::TrySendError::Disconnected(_)) => break,
                }
                if last_report.elapsed() >= Duration::from_secs(2) {
                    eprintln!("  {sent} frames @{fps}fps");
                    last_report = Instant::now();
                }
            }
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_size_aligns_and_floors() {
        assert_eq!(feed_size(1921, 1081).unwrap(), (1920, 1080));
        assert_eq!(feed_size(1920, 1080).unwrap(), (1920, 1080));
        assert!(feed_size(10, 10).is_err());
    }
}
