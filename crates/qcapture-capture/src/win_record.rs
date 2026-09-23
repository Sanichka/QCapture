//! Windows native record path: WGC monitor capture -> MediaFoundation HW encode.
//! `Capture::start(settings)` takes over the calling thread; stop via duration,
//! Ctrl-C flag, or window-close. Encoder defaults to H264 (max compatibility);
//! HEVC is opt-in via `codec` param in Phase 5 advanced settings.

use super::CaptureError;
use qcapture_annotate::raster::Annotator;
use qcapture_annotate::AnnotateDoc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::{
    AudioSettingsBuilder, ContainerSettingsBuilder, VideoEncoder, VideoSettingsBuilder,
    VideoSettingsSubType,
};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// Flags threaded through Settings -> handler `new()`.
/// Clone (not Debug): carries the audio chunk receiver when Phase 3 audio is on.
/// `flume::Receiver` is Clone but not Debug, and `Settings` only needs Clone.
#[derive(Clone)]
pub struct RecordFlags {
    pub output: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub use_hevc: bool,
    /// Monitor-relative crop rect (x, y, w, h) in frame pixels. When set, each
    /// frame is GPU-cropped via `buffer_crop` and pushed with `send_frame_buffer`
    /// at crop size — the encoder still inits once (fixed-canvas rule).
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Mixed i16-stereo-48k chunks from `qcapture_audio::win_audio`. Drained on
    /// every video frame; the encoder keeps its own monotonic audio clock.
    pub audio_rx: Option<flume::Receiver<Vec<u8>>>,
    pub stop_at: Option<Instant>,
    pub stop_flag: Arc<AtomicBool>,
    /// Pause flag: while set, video frames are skipped AND audio chunks are
    /// drained-and-dropped, freezing both clocks together (A/V stay in sync).
    pub pause_flag: Arc<AtomicBool>,
    /// Timed annotations to burn in (None = zero-copy surface path).
    /// Present = CPU path: surface -> tight BGRA -> blend -> flip -> push.
    pub annotate: Option<AnnotateDoc>,
}

struct Recorder {
    encoder: Option<VideoEncoder>,
    start: Instant,
    flags: RecordFlags,
    frames: u64,
    skipped: u64,
    audio_chunks: u64,
    annotated_frames: u64,
    last_report: Instant,
    crop_scratch: Vec<u8>,
    crop_flipped: Vec<u8>,
    /// Burn-in compositor (Some iff annotations requested), sized at the
    /// encoder input size.
    annotator: Option<Annotator>,
    /// Blend working buffer. `as_nopadding_buffer` returns a direct frame
    /// view when there is no padding (leaving scratch empty), so the tight
    /// bytes are staged here first — then blended, flipped and pushed.
    burn_buf: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Recorder {
    type Flags = RecordFlags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let f = ctx.flags.clone();
        let sub = if f.use_hevc {
            VideoSettingsSubType::HEVC
        } else {
            VideoSettingsSubType::H264
        };
        let video = VideoSettingsBuilder::new(f.width, f.height)
            .sub_type(sub)
            .bitrate(f.bitrate_bps)
            .frame_rate(f.fps);
        // Audio defaults (AAC, 48 kHz, stereo, 16-bit) match the mixer output
        // exactly: consecutive i16-LE stereo chunks at 48 kHz.
        let audio = if f.audio_rx.is_some() {
            AudioSettingsBuilder::new()
        } else {
            AudioSettingsBuilder::default().disabled(true)
        };
        let encoder =
            VideoEncoder::new(video, audio, ContainerSettingsBuilder::default(), &f.output)?;
        // Pre-size the crop scratch buffers so the hot loop never reallocs.
        let scratch_cap = f
            .crop
            .map(|(_, _, w, h)| (w as usize) * (h as usize) * 4)
            .unwrap_or(0);
        // Burn-in works at encoder input size (== feed size: monitor-native
        // when canvas matches, crop size for regions, window size).
        let annotator = f
            .annotate
            .clone()
            .map(|doc| Annotator::new(doc, f.width, f.height));
        Ok(Self {
            encoder: Some(encoder),
            start: Instant::now(),
            flags: f,
            frames: 0,
            skipped: 0,
            audio_chunks: 0,
            annotated_frames: 0,
            last_report: Instant::now(),
            crop_scratch: Vec::with_capacity(scratch_cap),
            crop_flipped: Vec::with_capacity(scratch_cap),
            burn_buf: Vec::new(),
            annotator,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.stop_flag.load(Ordering::SeqCst) {
            self.encoder.take().unwrap().finish()?;
            control.stop();
            eprintln!("\nstop requested — finalized.");
            return Ok(());
        }
        if let Some(deadline) = self.flags.stop_at {
            if Instant::now() >= deadline {
                self.encoder.take().unwrap().finish()?;
                control.stop();
                return Ok(());
            }
        }
        if self.flags.pause_flag.load(Ordering::Relaxed) {
            // Freeze both clocks: drop the video frame and drain-drop audio
            // so no backlog plays as a burst on resume (mixer idles too).
            if let Some(rx) = &self.flags.audio_rx {
                while rx.try_recv().is_ok() {}
            }
            return Ok(());
        }
        // Audio first: drain all pending mixed chunks (timestamps ignored by
        // the encoder — monotonic sample clock). WGC delivers 100+ frames/s so
        // audio latency stays under one video frame interval.
        if let Some(rx) = &self.flags.audio_rx {
            while let Ok(chunk) = rx.try_recv() {
                self.encoder
                    .as_mut()
                    .unwrap()
                    .send_audio_buffer(&chunk, 0)
                    .map_err(|e| -> Self::Error { Box::new(e) })?;
                self.audio_chunks += 1;
            }
        }
        if let Some((cx, cy, cw, ch)) = self.flags.crop {
            // Region path: GPU crop -> tight BGRA -> raw buffer push at crop size.
            // Out-of-range crops (display mode flip mid-record) are skipped,
            // never re-initing the encoder (fixed-canvas rule).
            let fw = frame.width();
            let fh = frame.height();
            if cx + cw > fw || cy + ch > fh {
                self.skipped += 1;
                return Ok(());
            }
            let ts = frame
                .timestamp()
                .map(|t| t.Duration)
                .unwrap_or_else(|_| self.start.elapsed().as_nanos() as i64 / 100);
            let cropped = frame
                .buffer_crop(cx, cy, cx + cw, cy + ch)
                .map_err(|e| -> Self::Error { Box::new(e) })?;
            // `send_frame_buffer` expects bottom-to-top BGRA (MediaFoundation
            // DIB convention) but the crop buffer is top-down — flip rows or
            // the recording comes out upside-down (fullscreen `send_frame`
            // takes the DirectX surface and is unaffected).
            if self.annotator.is_some() {
                // Blend first (top-down space), then flip: blending after
                // the flip would mirror the annotations vertically.
                let tight = cropped.as_nopadding_buffer(&mut self.crop_scratch);
                let n = stage_burn(tight, &mut self.burn_buf);
                self.push_burned_staged(n, cw, ch, ts)?;
            } else {
                let tight = cropped.as_nopadding_buffer(&mut self.crop_scratch);
                let flipped = flip_rows_vertical(tight, cw, ch, &mut self.crop_flipped);
                self.encoder
                    .as_mut()
                    .unwrap()
                    .send_frame_buffer(flipped, ts)
                    .map_err(|e| -> Self::Error { Box::new(e) })?;
            }
        } else if self.annotator.is_some() {
            // Burn-in path: read the surface to tight top-down BGRA, blend,
            // flip, push. Slower than zero-copy `send_frame` below, so it
            // only runs when a doc was staged.
            let (fw, fh) = (frame.width(), frame.height());
            if fw != self.flags.width || fh != self.flags.height {
                self.skipped += 1;
                return Ok(());
            }
            let ts = frame
                .timestamp()
                .map(|t| t.Duration)
                .unwrap_or_else(|_| self.start.elapsed().as_nanos() as i64 / 100);
            let fb = frame.buffer().map_err(|e| -> Self::Error { Box::new(e) })?;
            let tight = fb.as_nopadding_buffer(&mut self.crop_scratch);
            let n = stage_burn(tight, &mut self.burn_buf);
            self.push_burned_staged(n, fw, fh, ts)?;
        } else {
            self.encoder.as_mut().unwrap().send_frame(frame)?;
        }
        self.frames += 1;
        if self.last_report.elapsed() >= Duration::from_secs(2) {
            let el = self.start.elapsed().as_secs_f64();
            eprintln!(
                "  {:.1}s — {} frames ({:.1} fps, {} skipped, {} audio chunks, {} annotated)",
                el,
                self.frames,
                self.frames as f64 / el.max(0.1),
                self.skipped,
                self.audio_chunks,
                self.annotated_frames
            );
            self.last_report = Instant::now();
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // Item closed (monitor unplugged / window closed): finalize partial file.
        if let Some(enc) = self.encoder.take() {
            let _ = enc.finish();
        }
        eprintln!("capture item closed — saved partial recording.");
        Ok(())
    }
}

/// Handler error shared by the trait impl and the burn-in helper below.
type HandlerError = Box<dyn std::error::Error + Send + Sync>;

/// Stage tight top-down bytes into the blend working buffer. Free function
/// (not a method) so callers can hold the source borrow — e.g. a slice into
/// `crop_scratch` — while staging into the disjoint `burn_buf`.
fn stage_burn(tight: &[u8], burn_buf: &mut Vec<u8>) -> usize {
    let n = tight.len();
    if burn_buf.len() < n {
        burn_buf.resize(n, 0);
    }
    burn_buf[..n].copy_from_slice(tight);
    n
}

impl Recorder {
    /// Blend annotations into `burn_buf[..len]`, flip to the bottom-to-top
    /// DIB layout `send_frame_buffer` wants, and push.
    fn push_burned_staged(
        &mut self,
        len: usize,
        w: u32,
        h: u32,
        ts: i64,
    ) -> Result<(), HandlerError> {
        let now = self.start.elapsed().as_millis() as u64;
        if let Some(a) = self.annotator.as_mut() {
            a.apply_until(now);
            if a.blend_bgra(&mut self.burn_buf[..len]) > 0 {
                self.annotated_frames += 1;
            }
        }
        let flipped = flip_rows_vertical(&self.burn_buf[..len], w, h, &mut self.crop_flipped);
        self.encoder
            .as_mut()
            .unwrap()
            .send_frame_buffer(flipped, ts)
            .map_err(|e| -> HandlerError { Box::new(e) })?;
        Ok(())
    }
}

/// Record `monitor_1based` (1 = primary, per windows-capture API) to `output`.
/// Blocks until duration elapses, Ctrl-C flag trips, or item closes.
/// `canvas_w/h` implements the fixed-canvas rule: encoder inits at canvas size.
/// When canvas == native, frames pass through untouched; when smaller, the
/// MediaFoundation encoder scales on GPU.
/// With `annotate`, frames burn in on CPU — which needs the encoder input
/// to equal the feed, so a mismatched canvas bails (use an ffmpeg encoder
/// for scaled burn-in instead).
#[allow(clippy::too_many_arguments)]
pub fn record_monitor(
    monitor_1based: usize,
    output: String,
    fps: u32,
    bitrate_kbps: u32,
    canvas: Option<(u32, u32)>,
    show_cursor: bool,
    use_hevc: bool,
    audio_rx: Option<flume::Receiver<Vec<u8>>>,
    duration: Option<Duration>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    annotate: Option<AnnotateDoc>,
) -> Result<u64, CaptureError> {
    let monitor =
        Monitor::from_index(monitor_1based).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let native_w = monitor
        .width()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let native_h = monitor
        .height()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let (out_w, out_h) = canvas.unwrap_or((native_w, native_h));
    if out_w < 64 || out_h < 64 {
        return Err(CaptureError::Backend("canvas too small (min 64x64)".into()));
    }
    if annotate.is_some() && (out_w, out_h) != (native_w, native_h) {
        return Err(CaptureError::Backend(
            "--annotate with --canvas needs an ffmpeg encoder (--encoder nvenc|amf|qsv|x264); MF burn-in records at native size".into(),
        ));
    }

    let cursor = if show_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };

    let settings = Settings::new(
        monitor,
        cursor,
        // NOTE: WithoutBorder errors on Win10 22H2 ("toggling border not supported").
        // Default leaves the OS yellow-border policy untouched (required for WGC consent UI).
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        // Bgra8, not Rgba8: the region crop path pushes raw bytes through
        // `send_frame_buffer`, which expects BGRA — Rgba8 swaps red/blue.
        // Also WGC's native surface format, so the surface path is unaffected.
        ColorFormat::Bgra8,
        RecordFlags {
            output: output.clone(),
            width: out_w,
            height: out_h,
            fps,
            bitrate_bps: bitrate_kbps * 1000,
            use_hevc,
            crop: None,
            audio_rx,
            stop_at: duration.map(|d| Instant::now() + d),
            stop_flag,
            pause_flag,
            annotate,
        },
    );

    eprintln!(
        "WGC monitor#{monitor_1based} {native_w}x{native_h} -> canvas {out_w}x{out_h}@{fps} -> {output}"
    );
    Recorder::start(settings).map_err(|e| CaptureError::Backend(e.to_string()))?;

    // Recorder::start returns after control.stop(); count frames via file probe.
    // Return 0 as "unknown" — CLI reports file size + duration instead.
    Ok(0)
}

/// Record a monitor-relative region (x, y, w, h in frame pixels, origin at the
/// monitor's top-left) to `output`. The encoder inits at region size; each frame
/// is GPU-cropped before push. Dims are clamped to the monitor and rounded down
/// to even (H264 requirement).
#[allow(clippy::too_many_arguments)]
pub fn record_region(
    monitor_1based: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    output: String,
    fps: u32,
    bitrate_kbps: u32,
    show_cursor: bool,
    audio_rx: Option<flume::Receiver<Vec<u8>>>,
    duration: Option<Duration>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    annotate: Option<AnnotateDoc>,
) -> Result<u64, CaptureError> {
    let monitor =
        Monitor::from_index(monitor_1based).map_err(|e| CaptureError::Backend(e.to_string()))?;
    let native_w = monitor
        .width()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;
    let native_h = monitor
        .height()
        .map_err(|e| CaptureError::Backend(e.to_string()))?;

    // Clamp to monitor, then even-align down for H264.
    let x = x.min(native_w.saturating_sub(64));
    let y = y.min(native_h.saturating_sub(64));
    let mut w = w.min(native_w - x).max(64);
    let mut h = h.min(native_h - y).max(64);
    w &= !1;
    h &= !1;
    if w < 64 || h < 64 {
        return Err(CaptureError::Backend(
            "region too small after alignment (min 64x64 even)".into(),
        ));
    }

    let cursor = if show_cursor {
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
        ColorFormat::Bgra8, // see monitor path: raw buffer path requires BGRA
        RecordFlags {
            output: output.clone(),
            width: w,
            height: h,
            fps,
            bitrate_bps: bitrate_kbps * 1000,
            use_hevc: false,
            crop: Some((x, y, w, h)),
            audio_rx,
            stop_at: duration.map(|d| Instant::now() + d),
            stop_flag,
            pause_flag,
            annotate,
        },
    );
    eprintln!("WGC monitor#{monitor_1based} region {w}x{h}+{x}+{y} (of {native_w}x{native_h}) -> {output}");
    Recorder::start(settings).map_err(|e| CaptureError::Backend(e.to_string()))?;
    Ok(0)
}

/// Record the top-level window whose title contains `needle` (case-insensitive).
#[allow(clippy::too_many_arguments)]
pub fn record_window_title(
    needle: &str,
    output: String,
    fps: u32,
    bitrate_kbps: u32,
    show_cursor: bool,
    audio_rx: Option<flume::Receiver<Vec<u8>>>,
    duration: Option<Duration>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    annotate: Option<AnnotateDoc>,
) -> Result<u64, CaptureError> {
    let window = Window::from_contains_name(needle)
        .map_err(|e| CaptureError::Backend(format!("no window matching '{needle}': {e}")))?;
    let title = window.title().unwrap_or_default();
    let w = window.width().unwrap_or(1280);
    let h = window.height().unwrap_or(720);
    eprintln!("WGC window '{title}' {w}x{h} -> {output}");

    let cursor = if show_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let settings = Settings::new(
        window,
        cursor,
        // See monitor path: WithoutBorder is Win11-only; Default for Win10 compat.
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8, // consistent BGRA everywhere (see monitor path)
        RecordFlags {
            output: output.clone(),
            width: w.max(64) as u32,
            height: h.max(64) as u32,
            fps,
            bitrate_bps: bitrate_kbps * 1000,
            use_hevc: false,
            crop: None,
            audio_rx,
            stop_at: duration.map(|d| Instant::now() + d),
            stop_flag,
            pause_flag,
            annotate,
        },
    );
    Recorder::start(settings).map_err(|e| CaptureError::Backend(e.to_string()))?;
    Ok(0)
}

/// Reverse row order of a tight BGRA frame in place of allocation: copies
/// `src` (top-down, `w*h` pixels) bottom-up into `dst` (reused across frames).
/// Returns the flipped slice for `send_frame_buffer`, which documents a
/// bottom-to-top layout (MediaFoundation DIB convention).
fn flip_rows_vertical<'b>(src: &[u8], w: u32, h: u32, dst: &'b mut Vec<u8>) -> &'b [u8] {
    let row_bytes = (w as usize) * 4;
    let total = row_bytes * (h as usize);
    debug_assert_eq!(src.len(), total);
    if dst.len() < total {
        dst.resize(total, 0);
    }
    for (i, row) in src.chunks_exact(row_bytes).enumerate() {
        let dst_off = total - (i + 1) * row_bytes;
        dst[dst_off..dst_off + row_bytes].copy_from_slice(row);
    }
    &dst[..total]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: u8, g: u8, b: u8) -> [u8; 4] {
        [b, g, r, 255] // BGRA memory order
    }

    #[test]
    fn flip_swaps_rows_2x2() {
        // Row 0 (top): red, green. Row 1 (bottom): blue, white.
        let mut src = Vec::new();
        for p in [
            px(255, 0, 0),
            px(0, 255, 0),
            px(0, 0, 255),
            px(255, 255, 255),
        ] {
            src.extend_from_slice(&p);
        }
        let mut dst = Vec::new();
        let out = flip_rows_vertical(&src, 2, 2, &mut dst);
        // Bottom-to-top: blue, white first, then red, green.
        assert_eq!(&out[0..8], &[px(0, 0, 255), px(255, 255, 255)].concat());
        assert_eq!(&out[8..16], &[px(255, 0, 0), px(0, 255, 0)].concat());
    }

    #[test]
    fn flip_keeps_middle_row_odd_height() {
        let mut src = Vec::new();
        for v in [10u8, 20, 30] {
            src.extend_from_slice(&[v, v, v, 255]);
        }
        let mut dst = Vec::new();
        let out = flip_rows_vertical(&src, 1, 3, &mut dst);
        assert_eq!(out[0], 30); // was bottom
        assert_eq!(out[4], 20); // middle stays
        assert_eq!(out[8], 10); // was top
    }

    #[test]
    fn flip_reuses_destination_buffer() {
        let src = vec![1u8; 64 * 64 * 4];
        let mut dst = Vec::new();
        flip_rows_vertical(&src, 64, 64, &mut dst);
        let cap = dst.capacity();
        flip_rows_vertical(&src, 64, 64, &mut dst);
        assert_eq!(dst.capacity(), cap, "hot loop must not realloc");
        assert_eq!(dst.len(), 64 * 64 * 4);
    }

    #[test]
    fn burn_before_flip_keeps_orientation() {
        use qcapture_annotate::{Rgba, Stroke, Tool};
        // 8x8 top-down frame; red line near the TOP (norm y 0.1).
        let mut doc = AnnotateDoc::default();
        doc.add_stroke(Stroke {
            points: vec![(0.1, 0.1), (0.9, 0.1)],
            color: Rgba(255, 0, 0, 255),
            width_px: 2.0,
            tool: Tool::Line,
            text: None,
            appear_ms: 0,
            font_px: None,
            filled: false,
            font_path: None,
        })
        .unwrap();
        let mut ann = Annotator::new(doc, 8, 8);
        ann.apply_until(0);
        let mut tight = vec![0u8; 8 * 8 * 4];
        assert!(ann.blend_bgra(&mut tight) > 0);
        // Norm y=0.1 on 8px lands on top-down row 1; x=4 sits on the line.
        // Byte index: row 1, col 4 of 8-wide frame, B channel → 50.
        assert_eq!(tight[50], 255, "red line on top-down row 1");
        assert_eq!(tight[(7 * 8 + 4) * 4 + 2], 0, "bottom row clean");
        // Flip for the DIB push: row 1 must move to bottom-up row 6.
        let mut flipped = Vec::new();
        let out = flip_rows_vertical(&tight, 8, 8, &mut flipped);
        assert_eq!(out[(6 * 8 + 4) * 4 + 2], 255, "red line on bottom-up row 6");
        // Byte index: row 0, col 4 of 8-wide frame, B channel → 18.
        assert_eq!(out[18], 0, "first row clean after flip");
    }
}
