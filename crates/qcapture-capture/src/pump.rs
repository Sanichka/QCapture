//! Shared encode pump: compositor + ffmpeg feed, used by every capture backend.
//!
//! The Windows WGC pusher (`ffmpeg_cap`) and the portable xcap bridge
//! (`xcap_cap`) both deliver tight top-down BGRA [`RawFrame`]s here. The pump
//! owns everything downstream: annotation burn-in, live-draw events, cursor
//! fx, fixed-canvas size discipline, ffmpeg pipe writes, and decimated live
//! previews for drawing UIs. Backends differ only in how frames arrive.

use qcapture_annotate::raster::Annotator;
use qcapture_annotate::{AnnotateDoc, DrawEvent};
use qcapture_core::{CanvasConfig, CursorFx, EncoderKind, RateControl};
use qcapture_encode::{ffmpeg_encoder_name, RawvideoEncoder};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Tight top-down BGRA frame for the ffmpeg pipe.
pub struct RawFrame {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
    /// Cursor position in frame pixels sampled with this frame (None = off
    /// feed or fx disabled). `clicked` is a left/right button down-edge on
    /// this frame — the pump turns it into a ripple.
    pub cursor: Option<(i32, i32)>,
    pub clicked: bool,
}

/// Decimated post-blend preview frame for drawing UIs (top-down tight BGRA).
/// Carries its own size so the panel self-corrects if the initial feed guess
/// (region align/clamp) was off by a pixel pair.
#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// Job description shared by monitor/window/region runs on every backend.
#[derive(Clone)]
pub struct FfmpegJob {
    pub fps: u32,
    pub encoder: EncoderKind,
    pub rate: RateControl,
    /// Fixed output canvas (ffmpeg `-vf scale`); None = capture size.
    pub canvas: Option<(u32, u32)>,
    pub show_cursor: bool,
    pub output: String,
    pub duration: Option<Duration>,
    pub stop_flag: Arc<AtomicBool>,
    /// Pause flag (widget button / CLI timer). While set, backends stop
    /// forwarding frames and mixers stop emitting quanta — both clocks
    /// freeze together, so A/V stay in sync across the gap.
    pub pause_flag: Arc<AtomicBool>,
    /// Timed annotations to burn in (norm coords resolved against feed size).
    /// None = clean feed. MF path does not support this in 4a.
    pub annotate: Option<AnnotateDoc>,
    /// Cursor highlight ring + click ripple (None = off, zero sampling cost).
    /// Needs CPU pixels: forces the ffmpeg path (auto-switches MF).
    /// Windows-only for now (no portable cursor position APIs in the stack).
    pub cursor_fx: Option<CursorFx>,
    /// HWND for window-target cursor mapping (per-frame `GetWindowRect`
    /// survives window moves). None for screen/region (fixed geometry);
    /// always None outside Windows.
    pub cursor_window: Option<isize>,
    /// Named-pipe path with mixed i16 stereo 48 kHz PCM, or None for `-an`.
    pub audio_pipe: Option<String>,
    /// Live video preview for drawing UIs: every Nth post-blend frame is
    /// cloned here (bounded channel, drops on backpressure). The draw panel
    /// shows what the encoder sees, so no transparent overlay is ever needed.
    pub preview_tx: Option<flume::Sender<PreviewFrame>>,
}

#[derive(Debug, Default)]
pub struct FfmpegStats {
    /// Frames ffmpeg acknowledged (output frame count).
    pub written: u64,
    /// Frames that arrived at a different size than the encoder canvas
    /// (window borders, resize mid-record) and were center-cropped/padded
    /// to fit instead of dropped.
    pub skipped: u64,
    /// Frames that carried blended annotation pixels.
    pub annotated_frames: u64,
}

/// Fit a top-down tight BGRA frame onto a differently-sized canvas.
/// Center-crops when the frame is larger, black-pads when smaller.
/// Window capture is the trigger: `Window::width/height` (outer rect with
/// borders) disagrees with actual WGC frame size (client area) by ~16px,
/// and user resizes drift further mid-record. Dropping those frames
/// freezes the video; adapting keeps it continuous. Norm-coord strokes
/// stay valid because they resolve against the canvas.
pub fn adapt_frame(src_w: u32, src_h: u32, src: &[u8], dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    let (copy_w, copy_h) = (src_w.min(dst_w), src_h.min(dst_h));
    let (src_x, src_y) = ((src_w - copy_w) / 2, (src_h - copy_h) / 2);
    let (dst_x, dst_y) = ((dst_w - copy_w) / 2, (dst_h - copy_h) / 2);
    for row in 0..copy_h {
        let s0 = (((src_y + row) * src_w + src_x) * 4) as usize;
        let d0 = (((dst_y + row) * dst_w + dst_x) * 4) as usize;
        let n = (copy_w * 4) as usize;
        dst[d0..d0 + n].copy_from_slice(&src[s0..s0 + n]);
    }
    dst
}

/// Swap RGBA to BGRA in place (xcap frames are uniformly RGBA: macOS swaps
/// explicitly, `RgbaImage::into_raw` is RGBA everywhere else).
pub fn rgba_to_bgra_in_place(buf: &mut [u8]) {
    let (chunks, _) = buf.as_chunks_mut::<4>();
    for px in chunks {
        px.swap(0, 2);
    }
}

/// CPU-crop a tight RGBA frame (region targets on backends without GPU crop).
pub fn crop_rgba(src: &[u8], sw: u32, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    for row in 0..h {
        let s0 = (((y + row) * sw + x) * 4) as usize;
        let d0 = ((row * w) * 4) as usize;
        let n = (w * 4) as usize;
        out[d0..d0 + n].copy_from_slice(&src[s0..s0 + n]);
    }
    out
}

/// Ring stroke widths stay fixed (radii/colors/duration are styled).
const HL_THICK: f32 = 3.0;
const RIPPLE_R0: f32 = 8.0;
const RIPPLE_THICK: f32 = 3.0;

/// Ripple geometry at a given age: expanding radius, fading alpha.
/// None once the age passes the duration.
pub fn ripple_frame(ripple_radius: f32, duration_ms: u32, age_ms: u64) -> Option<(f32, u8)> {
    if duration_ms == 0 || age_ms >= duration_ms as u64 {
        return None;
    }
    let k = age_ms as f32 / duration_ms as f32;
    let r = (RIPPLE_R0 + (ripple_radius - RIPPLE_R0) * k).max(RIPPLE_R0);
    Some((r, (255.0 * (1.0 - k)) as u8))
}

/// Blend one antialiased ring outline onto a top-down tight BGRA frame.
/// Out-of-frame centers are culled; partial rings clip at the edges.
/// `additive` adds light instead of blending over (glow).
#[allow(clippy::too_many_arguments)]
pub fn blend_ring(
    frame: &mut [u8],
    w: u32,
    h: u32,
    cx: i32,
    cy: i32,
    radius: f32,
    thick: f32,
    color: (u8, u8, u8),
    alpha: u8,
    additive: bool,
) {
    if alpha == 0 || radius <= 0.0 {
        return;
    }
    let r_out = radius + thick / 2.0;
    let r_in = (radius - thick / 2.0).max(0.0);
    let x0 = (cx - r_out.ceil() as i32).max(0);
    let y0 = (cy - r_out.ceil() as i32).max(0);
    let x1 = (cx + r_out.ceil() as i32 + 1).min(w as i32);
    let y1 = (cy + r_out.ceil() as i32 + 1).min(h as i32);
    for y in y0..y1 {
        for x in x0..x1 {
            let d = ((x - cx) as f32).hypot((y - cy) as f32);
            if d < r_in || d > r_out {
                continue;
            }
            // 1px antialiased rim on both edges.
            let edge = ((d - r_in).min(r_out - d)).clamp(0.0, 1.0);
            let a = alpha as f32 / 255.0 * edge;
            if a <= 0.0 {
                continue;
            }
            let i = ((y as usize) * (w as usize) + (x as usize)) * 4;
            if additive {
                frame[i] = (frame[i] as f32 + color.2 as f32 * a).min(255.0) as u8;
                frame[i + 1] = (frame[i + 1] as f32 + color.1 as f32 * a).min(255.0) as u8;
                frame[i + 2] = (frame[i + 2] as f32 + color.0 as f32 * a).min(255.0) as u8;
            } else {
                let inv = 1.0 - a;
                frame[i] = (color.2 as f32 * a + frame[i] as f32 * inv) as u8;
                frame[i + 1] = (color.1 as f32 * a + frame[i + 1] as f32 * inv) as u8;
                frame[i + 2] = (color.0 as f32 * a + frame[i + 2] as f32 * inv) as u8;
            }
        }
    }
}

/// Pump thread body: drain live overlay events, blend annotations, burn
/// cursor fx, write frames, adapt size flips (fixed-canvas rule), finish.
/// Clock is pump-start; annotation `appear_ms` is relative to recording
/// start (WGC warmup ≈ 200 ms early — noted).
pub fn pump(
    mut enc: RawvideoEncoder,
    rx: flume::Receiver<RawFrame>,
    doc: Option<AnnotateDoc>,
    live_rx: Option<flume::Receiver<DrawEvent>>,
    preview_tx: Option<flume::Sender<PreviewFrame>>,
    preview_every: u64,
    cursor_fx: Option<CursorFx>,
) -> Result<FfmpegStats, String> {
    let t0 = Instant::now();
    let mut annotator = doc.map(|d| {
        if d.canvas_w != enc.native_w || d.canvas_h != enc.native_h {
            tracing::warn!(
                "annotate doc authored for {}x{}, recording feed is {}x{} — norm coords stretch to fit",
                d.canvas_w,
                d.canvas_h,
                enc.native_w,
                enc.native_h
            );
        }
        Annotator::new(d, enc.native_w, enc.native_h)
    });
    // Live overlay implies an annotator even with an empty scripted doc.
    if live_rx.is_some() && annotator.is_none() {
        annotator = Some(Annotator::new(
            AnnotateDoc::default(),
            enc.native_w,
            enc.native_h,
        ));
    }
    let (mut written, mut skipped, mut annotated) = (0u64, 0u64, 0u64);
    let mut ripples: Vec<(i32, i32, u64)> = Vec::new();
    for mut f in rx {
        if f.w != enc.native_w || f.h != enc.native_h {
            // Never re-init the encoder mid-record (fixed-canvas rule) and
            // never drop: adapt keeps video + preview + draw alive across
            // window borders and resizes.
            skipped += 1;
            if skipped <= 3 {
                eprintln!(
                    "  frame {}x{} != canvas {}x{} — adapting (center crop/pad)",
                    f.w, f.h, enc.native_w, enc.native_h
                );
            }
            // Rebase the sampled cursor into canvas coords: undo the source
            // centering offset, apply the canvas one (same math as adapt).
            let cursor = f.cursor.map(|(x, y)| {
                let (sw, sh) = (f.w, f.h);
                let (dw, dh) = (enc.native_w, enc.native_h);
                let (cw, ch) = (sw.min(dw), sh.min(dh));
                (
                    x - ((sw - cw) / 2) as i32 + ((dw - cw) / 2) as i32,
                    y - ((sh - ch) / 2) as i32 + ((dh - ch) / 2) as i32,
                )
            });
            f = RawFrame {
                w: enc.native_w,
                h: enc.native_h,
                bgra: adapt_frame(f.w, f.h, &f.bgra, enc.native_w, enc.native_h),
                cursor,
                clicked: f.clicked,
            };
        }
        if let Some(a) = annotator.as_mut() {
            let now = t0.elapsed().as_millis() as u64;
            if let Some(lrx) = live_rx.as_ref() {
                while let Ok(ev) = lrx.try_recv() {
                    a.apply_event(&ev, now);
                }
            }
            a.apply_until(now);
            if a.blend_bgra(&mut f.bgra) > 0 {
                annotated += 1;
            }
        }
        // Cursor fx last: ring + ripples sit on top of video + annotations,
        // and flow into the preview too (draw panel stays WYSIWYG).
        if let Some(cfg) = cursor_fx.as_ref() {
            let now = t0.elapsed().as_millis() as u64;
            if f.clicked {
                if let Some((x, y)) = f.cursor {
                    ripples.push((x, y, now));
                    if ripples.len() > 8 {
                        ripples.remove(0);
                    }
                }
            }
            let dur = cfg.style.ripple_ms;
            ripples.retain(|&(_, _, t)| now.saturating_sub(t) < dur as u64);
            let additive = cfg.style.additive;
            if cfg.highlight {
                if let Some((x, y)) = f.cursor {
                    let (r, g, b, a) = (
                        cfg.style.hl_rgba[0],
                        cfg.style.hl_rgba[1],
                        cfg.style.hl_rgba[2],
                        cfg.style.hl_rgba[3],
                    );
                    blend_ring(
                        &mut f.bgra,
                        enc.native_w,
                        enc.native_h,
                        x,
                        y,
                        cfg.style.hl_radius,
                        HL_THICK,
                        (r, g, b),
                        a,
                        additive,
                    );
                }
            }
            if cfg.ripple {
                for &(x, y, t) in &ripples {
                    if let Some((r, fade)) =
                        ripple_frame(cfg.style.ripple_radius, dur, now.saturating_sub(t))
                    {
                        let (rr, gg, bb, base) = (
                            cfg.style.ripple_rgba[0],
                            cfg.style.ripple_rgba[1],
                            cfg.style.ripple_rgba[2],
                            cfg.style.ripple_rgba[3],
                        );
                        let a = (fade as u16 * base as u16 / 255) as u8;
                        blend_ring(
                            &mut f.bgra,
                            enc.native_w,
                            enc.native_h,
                            x,
                            y,
                            r,
                            RIPPLE_THICK,
                            (rr, gg, bb),
                            a,
                            additive,
                        );
                    }
                }
            }
        }
        enc.write_frame(&f.bgra)
            .map_err(|e| format!("ffmpeg pipe: {e}"))?;
        written += 1;
        // Preview for drawing UIs: decimated post-blend clone, dropped (never
        // blocking) when the UI thread is busy.
        if preview_every > 0 && written % preview_every == 0 {
            if let Some(ptx) = preview_tx.as_ref() {
                let _ = ptx.try_send(PreviewFrame {
                    w: enc.native_w,
                    h: enc.native_h,
                    bgra: f.bgra.clone(),
                });
            }
        }
    }
    enc.finish().map_err(|e| format!("ffmpeg: {e}"))?;
    Ok(FfmpegStats {
        written,
        skipped,
        annotated_frames: annotated,
    })
}

/// Pump feed + join handle from [`spawn_pump`].
pub type PumpEnds = (
    flume::Sender<RawFrame>,
    std::thread::JoinHandle<Result<FfmpegStats, String>>,
);

/// Spawn the pump thread with the standard bound (8 ≈ 2 MB at 1080p absorbs
/// the capture startup burst without unbounded growth).
/// Returns the receiver the backend feeds plus the join handle.
pub fn spawn_pump(
    feed_w: u32,
    feed_h: u32,
    fps: u32,
    canvas: CanvasConfig,
    job: &FfmpegJob,
    live_rx: Option<flume::Receiver<DrawEvent>>,
) -> Result<PumpEnds, String> {
    let enc = RawvideoEncoder::spawn(
        feed_w,
        feed_h,
        fps,
        canvas,
        job.encoder,
        job.encoder,
        job.rate,
        &job.output,
        job.audio_pipe.as_deref(),
    )
    .map_err(|e| format!("ffmpeg spawn: {e}"))?;
    eprintln!(
        "ffmpeg {}: {feed_w}x{feed_h}@{} -> {}x{} -> {}",
        enc.encoder_name, fps, canvas.width, canvas.height, job.output
    );
    let (tx, rx) = flume::bounded::<RawFrame>(8);
    let doc = job.annotate.clone();
    let preview_tx = job.preview_tx.clone();
    let preview_every = (fps.max(1) / 5).max(1) as u64;
    let cursor_fx = job.cursor_fx;
    let handle = std::thread::Builder::new()
        .name("qcapture-ffmpeg-pump".into())
        .spawn(move || pump(enc, rx, doc, live_rx, preview_tx, preview_every, cursor_fx))
        .map_err(|e| format!("pump spawn: {e}"))?;
    Ok((tx, handle))
}

/// ffmpeg CLI encoder name for logs (mirrors `RawvideoEncoder` resolution).
pub fn encoder_name(kind: EncoderKind) -> &'static str {
    ffmpeg_encoder_name(kind, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&px);
        }
        v
    }

    #[test]
    fn adapt_same_size_is_identity() {
        let src = solid(4, 4, [10, 20, 30, 255]);
        assert_eq!(adapt_frame(4, 4, &src, 4, 4), src);
    }

    #[test]
    fn adapt_pads_smaller_with_black_centered() {
        // 2x2 red onto 4x4: centered, 1px black border all around.
        let src = solid(2, 2, [0, 0, 255, 255]);
        let out = adapt_frame(2, 2, &src, 4, 4);
        assert_eq!(out.len(), 4 * 4 * 4);
        // Center pixel is red.
        assert_eq!(
            &out[(1 * 4 + 1) * 4..(1 * 4 + 1) * 4 + 4],
            &[0, 0, 255, 255]
        );
        // Corner is black pad.
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn adapt_crops_larger_centered() {
        // 4x4 with a red center 2x2, cropped back to 2x2 keeps the red.
        let mut src = solid(4, 4, [0, 0, 0, 255]);
        for y in 1..3 {
            for x in 1..3 {
                let i = (y * 4 + x) * 4;
                src[i..i + 4].copy_from_slice(&[0, 0, 255, 255]);
            }
        }
        let out = adapt_frame(4, 4, &src, 2, 2);
        assert_eq!(out, solid(2, 2, [0, 0, 255, 255]));
    }

    #[test]
    fn ring_lands_on_circumference_not_center() {
        let mut frame = solid(40, 40, [0, 0, 0, 255]);
        blend_ring(
            &mut frame,
            40,
            40,
            20,
            20,
            8.0,
            3.0,
            (255, 210, 0),
            255,
            false,
        );
        // Center untouched.
        assert_eq!(
            &frame[(20 * 40 + 20) * 4..(20 * 40 + 20) * 4 + 4],
            &[0, 0, 0, 255]
        );
        // East point on the ring is yellow (BGRA memory order).
        let i = (20 * 40 + 28) * 4;
        assert_eq!(frame[i + 2], 255);
        assert!(frame[i + 1] > 100);
        // Far corner untouched.
        assert_eq!(&frame[0..4], &[0, 0, 0, 255]);
    }

    #[test]
    fn rgba_swaps_red_and_blue_in_place() {
        let mut buf = vec![10, 20, 30, 255, 1, 2, 3, 4];
        rgba_to_bgra_in_place(&mut buf);
        assert_eq!(buf, vec![30, 20, 10, 255, 3, 2, 1, 4]);
    }

    #[test]
    fn crop_takes_subrect() {
        // 4x2 frame, pixel value = x + 10*y (in R channel).
        let mut src = Vec::new();
        for y in 0..2u32 {
            for x in 0..4u32 {
                src.extend_from_slice(&[(x + 10 * y) as u8, 0, 0, 255]);
            }
        }
        let out = crop_rgba(&src, 4, 1, 0, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!(out[0], 1);
        assert_eq!(out[4], 2);
        assert_eq!(out[8], 11);
    }

    #[test]
    fn ripple_frame_expands_and_fades() {
        // Birth: small radius, full alpha.
        let (r0, a0) = ripple_frame(42.0, 600, 0).unwrap();
        assert!((r0 - 8.0).abs() < 1e-4);
        assert_eq!(a0, 255);
        // Mid-life: grown, faded.
        let (r1, a1) = ripple_frame(42.0, 600, 300).unwrap();
        assert!(r1 > r0 && r1 < 42.0);
        assert!(a1 < a0 && a1 > 0);
        // At/past duration: gone (also guards duration 0).
        assert_eq!(ripple_frame(42.0, 600, 600), None);
        assert_eq!(ripple_frame(42.0, 600, 9999), None);
        assert_eq!(ripple_frame(42.0, 0, 0), None);
        // Custom radius respected, floor at the inner radius.
        let (r2, _) = ripple_frame(100.0, 600, 300).unwrap();
        assert!((r2 - 54.0).abs() < 1.0);
        let (r3, _) = ripple_frame(4.0, 600, 300).unwrap();
        assert!((r3 - 8.0).abs() < 1e-4);
    }

    #[test]
    fn ring_clips_at_edges_without_panic() {
        let mut frame = solid(10, 10, [5, 5, 5, 255]);
        blend_ring(
            &mut frame,
            10,
            10,
            0,
            0,
            8.0,
            3.0,
            (255, 210, 0),
            255,
            false,
        );
        assert_eq!(frame.len(), 10 * 10 * 4);
        // Zero alpha is a no-op.
        let before = frame.clone();
        blend_ring(&mut frame, 10, 10, 5, 5, 4.0, 2.0, (255, 210, 0), 0, false);
        assert_eq!(frame, before);
    }

    #[test]
    fn ring_additive_brightens_instead_of_replacing() {
        // Mid-gray frame: normal blend pulls toward red, additive adds red.
        let mut normal = solid(40, 40, [128, 128, 128, 255]);
        blend_ring(
            &mut normal,
            40,
            40,
            20,
            20,
            8.0,
            3.0,
            (255, 0, 0),
            255,
            false,
        );
        let mut add = solid(40, 40, [128, 128, 128, 255]);
        blend_ring(&mut add, 40, 40, 20, 20, 8.0, 3.0, (255, 0, 0), 255, true);
        let i = (20 * 40 + 28) * 4;
        // Normal src-over with opaque red: green channel collapses.
        assert!(
            normal[i + 1] < 30,
            "normal blend replaces, got {}",
            normal[i + 1]
        );
        // Additive: red channel saturates, green only gains edge AA (< full).
        assert_eq!(add[i + 2], 255);
        assert!(
            add[i + 1] >= 128,
            "additive never darkens, got {}",
            add[i + 1]
        );
        // Translucent additive still adds (proportionally).
        let mut half = solid(40, 40, [100, 100, 100, 255]);
        blend_ring(&mut half, 40, 40, 20, 20, 8.0, 3.0, (200, 0, 0), 128, true);
        assert!(half[i + 2] > 100 && half[i + 2] < 255);
    }
}
