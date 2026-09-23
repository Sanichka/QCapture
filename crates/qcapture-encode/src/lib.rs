//! qcapture-encode: encoder config + ffmpeg CLI orchestration.
//! Video enters via rawvideo stdin; audio via an OS pipe (`audio_pipe`:
//! Windows named pipe, Unix socket), since a process has only one stdin.

pub mod audio_pipe;

use qcapture_core::{CanvasConfig, Container, EncodeConfig, EncoderKind, RateControl};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error(
        "ffmpeg not found in PATH — install ffmpeg 7.x or place ffmpeg.dll beside qcapture.exe"
    )]
    FfmpegMissing,
    #[error("ffmpeg probe failed: {0}")]
    Probe(String),
    #[error("unsupported encoder {0:?} for current OS")]
    UnsupportedEncoder(EncoderKind),
}

#[derive(Debug, Clone)]
pub struct FfmpegInfo {
    pub version_line: String,
    pub has_nvenc: bool,
    pub has_amf: bool,
    pub has_qsv: bool,
    pub has_videotoolbox: bool,
    pub has_libx264: bool,
}

/// Probe system ffmpeg CLI. Parses `-encoders` output for HW flags.
/// Returns Err(FfmpegMissing) with a helpful message instead of panicking.
pub fn probe_ffmpeg() -> Result<FfmpegInfo, EncodeError> {
    let mut ver_cmd = Command::new("ffmpeg");
    qcapture_core::hide_child_console(&mut ver_cmd);
    let ver = ver_cmd
        .arg("-version")
        .output()
        .map_err(|_| EncodeError::FfmpegMissing)?;
    if !ver.status.success() {
        return Err(EncodeError::Probe("ffmpeg -version failed".into()));
    }
    let version_line = String::from_utf8_lossy(&ver.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();

    let mut enc_cmd = Command::new("ffmpeg");
    qcapture_core::hide_child_console(&mut enc_cmd);
    let enc = enc_cmd
        .args(["-hide_banner", "-encoders"])
        .output()
        .map_err(|e| EncodeError::Probe(e.to_string()))?;
    let text = String::from_utf8_lossy(&enc.stdout).to_string();
    Ok(FfmpegInfo {
        version_line,
        has_nvenc: text.contains("h264_nvenc"),
        has_amf: text.contains("h264_amf"),
        has_qsv: text.contains("h264_qsv"),
        has_videotoolbox: text.contains("h264_videotoolbox"),
        has_libx264: text.contains("libx264"),
    })
}

/// Pick effective encoder for Auto: HW first, software fallback last.
/// Order: NVENC -> AMF -> QSV -> VideoToolbox -> MediaFoundation -> libx264.
pub fn resolve_auto_encoder(info: &FfmpegInfo) -> EncoderKind {
    if info.has_nvenc {
        EncoderKind::H264Nvenc
    } else if info.has_amf {
        EncoderKind::H264Amf
    } else if info.has_qsv {
        EncoderKind::H264Qsv
    } else if info.has_videotoolbox {
        EncoderKind::H264VideoToolbox
    } else {
        EncoderKind::LibX264
    }
}

/// Best available H.264 kind from a probe (probe order matches
/// [`resolve_auto_encoder` minus the non-H.264 entries).
pub fn best_h264(info: &FfmpegInfo) -> Option<EncoderKind> {
    if info.has_nvenc {
        Some(EncoderKind::H264Nvenc)
    } else if info.has_amf {
        Some(EncoderKind::H264Amf)
    } else if info.has_qsv {
        Some(EncoderKind::H264Qsv)
    } else if info.has_videotoolbox {
        Some(EncoderKind::H264VideoToolbox)
    } else if info.has_libx264 {
        Some(EncoderKind::LibX264)
    } else {
        None
    }
}

/// Best available HEVC kind from a probe (NVENC, then QSV).
pub fn best_hevc(info: &FfmpegInfo) -> Option<EncoderKind> {
    if info.has_nvenc {
        Some(EncoderKind::HevcNvenc)
    } else if info.has_qsv {
        Some(EncoderKind::HevcQsv)
    } else {
        None
    }
}

/// Does this ffmpeg build provide `kind`? Used to fail fast with a clear
/// message instead of a cryptic ffmpeg stderr dump.
pub fn supports(kind: EncoderKind, info: &FfmpegInfo) -> bool {
    match kind {
        EncoderKind::Auto => true,
        EncoderKind::H264Nvenc | EncoderKind::HevcNvenc => info.has_nvenc,
        EncoderKind::H264Amf => info.has_amf,
        EncoderKind::H264Qsv | EncoderKind::HevcQsv | EncoderKind::Av1Qsv => info.has_qsv,
        EncoderKind::H264VideoToolbox => info.has_videotoolbox,
        EncoderKind::H264MediaFoundation => true, // OS-provided, not ffmpeg's
        EncoderKind::LibX264 => info.has_libx264,
    }
}

/// Rate-control + codec-tuning args for `-c:v <name>` (pure, unit-tested).
///
/// Behavior contract per mode:
///
/// - CBR keeps the exact Phase-5a arg sets (bitrate honored, e.g. NVENC CBR).
/// - VBR caps at maxrate with a 2x bufsize; x264 keeps `veryfast`.
/// - CQP is constant-QP (NVENC `constqp`, AMF `cqp`, QSV global `-q`).
///   x264 has no CQP — use CRF instead (loud error, no silent remap).
/// - CRF exists only on x264; NVENC/QSV/AMF point at their CQP equivalents.
///
/// AMF/QSV non-CBR mappings come from ffmpeg docs (no AMD/Intel HW here to
/// verify); unknown ffmpeg flags fail loudly at spawn, never silently wrong.
pub fn rate_control_args(
    encoder_name: &str,
    rate: &RateControl,
    fps: u32,
) -> Result<Vec<String>, EncodeError> {
    let gop = (fps.max(1) * 4).to_string();
    let kb = |k: u32| format!("{k}k");
    let mut out: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, s: &str| a.push(s.to_string());
    let qp_in_range = |qp: u8| {
        if qp > 51 {
            Err(EncodeError::Probe(format!(
                "qp {qp} out of range 0..51 (lower = better quality)"
            )))
        } else {
            Ok(())
        }
    };

    match (encoder_name, rate) {
        // ------------------------------- x264 -------------------------------
        ("libx264", RateControl::Cbr { bitrate_kbps }) => {
            if *bitrate_kbps == 0 {
                return Err(EncodeError::Probe("--bitrate must be > 0".into()));
            }
            push(&mut out, "-b:v");
            push(&mut out, &kb(*bitrate_kbps));
            push(&mut out, "-preset");
            push(&mut out, "veryfast");
        }
        (
            "libx264",
            RateControl::Vbr {
                target_kbps,
                max_kbps,
            },
        ) => {
            check_vbr(*target_kbps, *max_kbps)?;
            push(&mut out, "-b:v");
            push(&mut out, &kb(*target_kbps));
            push(&mut out, "-maxrate");
            push(&mut out, &kb(*max_kbps));
            push(&mut out, "-bufsize");
            push(&mut out, &kb(max_kbps.saturating_mul(2)));
            push(&mut out, "-preset");
            push(&mut out, "veryfast");
        }
        ("libx264", RateControl::Crf { crf }) => {
            qp_in_range(*crf)?;
            push(&mut out, "-crf");
            push(&mut out, &crf.to_string());
            push(&mut out, "-preset");
            push(&mut out, "veryfast");
        }
        ("libx264", RateControl::Cqp { .. }) => {
            return Err(EncodeError::Probe(
                "x264 has no CQP mode — use --rc crf --crf N instead".into(),
            ));
        }
        // ------------------------------- NVENC -------------------------------
        // Note: no `-preset` — p-series presets need ffmpeg 6+, and 5.x rejects
        // them; NVENC defaults are sane.
        ("h264_nvenc" | "hevc_nvenc", RateControl::Cbr { bitrate_kbps }) => {
            if *bitrate_kbps == 0 {
                return Err(EncodeError::Probe("--bitrate must be > 0".into()));
            }
            push(&mut out, "-b:v");
            push(&mut out, &kb(*bitrate_kbps));
            push(&mut out, "-rc");
            push(&mut out, "cbr");
        }
        (
            "h264_nvenc" | "hevc_nvenc",
            RateControl::Vbr {
                target_kbps,
                max_kbps,
            },
        ) => {
            check_vbr(*target_kbps, *max_kbps)?;
            push(&mut out, "-b:v");
            push(&mut out, &kb(*target_kbps));
            push(&mut out, "-maxrate");
            push(&mut out, &kb(*max_kbps));
            push(&mut out, "-bufsize");
            push(&mut out, &kb(max_kbps.saturating_mul(2)));
            push(&mut out, "-rc");
            push(&mut out, "vbr");
        }
        ("h264_nvenc" | "hevc_nvenc", RateControl::Cqp { qp }) => {
            qp_in_range(*qp)?;
            push(&mut out, "-rc");
            push(&mut out, "constqp");
            push(&mut out, "-qp");
            push(&mut out, &qp.to_string());
        }
        ("h264_nvenc" | "hevc_nvenc", RateControl::Crf { .. }) => {
            return Err(EncodeError::Probe(
                "NVENC has no CRF mode — use --rc cqp --qp N instead".into(),
            ));
        }
        // ------------------------------- AMF -------------------------------
        ("h264_amf", RateControl::Cbr { bitrate_kbps }) => {
            if *bitrate_kbps == 0 {
                return Err(EncodeError::Probe("--bitrate must be > 0".into()));
            }
            push(&mut out, "-b:v");
            push(&mut out, &kb(*bitrate_kbps));
            push(&mut out, "-quality");
            push(&mut out, "balanced");
        }
        (
            "h264_amf",
            RateControl::Vbr {
                target_kbps,
                max_kbps,
            },
        ) => {
            check_vbr(*target_kbps, *max_kbps)?;
            push(&mut out, "-b:v");
            push(&mut out, &kb(*target_kbps));
            push(&mut out, "-maxrate");
            push(&mut out, &kb(*max_kbps));
            push(&mut out, "-bufsize");
            push(&mut out, &kb(max_kbps.saturating_mul(2)));
            push(&mut out, "-rc");
            push(&mut out, "vbr");
            push(&mut out, "-quality");
            push(&mut out, "balanced");
        }
        ("h264_amf", RateControl::Cqp { qp }) => {
            qp_in_range(*qp)?;
            push(&mut out, "-rc");
            push(&mut out, "cqp");
            push(&mut out, "-qp");
            push(&mut out, &qp.to_string());
            push(&mut out, "-quality");
            push(&mut out, "balanced");
        }
        ("h264_amf", RateControl::Crf { .. }) => {
            return Err(EncodeError::Probe(
                "AMF has no CRF mode — use --rc cqp --qp N instead".into(),
            ));
        }
        // ------------------------------- QSV -------------------------------
        ("h264_qsv" | "hevc_qsv" | "av1_qsv", RateControl::Cbr { bitrate_kbps }) => {
            if *bitrate_kbps == 0 {
                return Err(EncodeError::Probe("--bitrate must be > 0".into()));
            }
            push(&mut out, "-b:v");
            push(&mut out, &kb(*bitrate_kbps));
            push(&mut out, "-preset");
            push(&mut out, "veryfast");
        }
        (
            "h264_qsv" | "hevc_qsv" | "av1_qsv",
            RateControl::Vbr {
                target_kbps,
                max_kbps,
            },
        ) => {
            check_vbr(*target_kbps, *max_kbps)?;
            push(&mut out, "-b:v");
            push(&mut out, &kb(*target_kbps));
            push(&mut out, "-maxrate");
            push(&mut out, &kb(*max_kbps));
            push(&mut out, "-bufsize");
            push(&mut out, &kb(max_kbps.saturating_mul(2)));
            push(&mut out, "-preset");
            push(&mut out, "veryfast");
        }
        ("h264_qsv" | "hevc_qsv" | "av1_qsv", RateControl::Cqp { qp }) => {
            qp_in_range(*qp)?;
            push(&mut out, "-q");
            push(&mut out, &qp.to_string());
            push(&mut out, "-preset");
            push(&mut out, "veryfast");
        }
        (_, RateControl::Crf { .. }) => {
            return Err(EncodeError::Probe(
                "CRF is only supported with --encoder x264".into(),
            ));
        }
        (other, _) => {
            return Err(EncodeError::Probe(format!(
                "no rate mapping for encoder '{other}' on this ffmpeg"
            )));
        }
    }
    push(&mut out, "-g");
    push(&mut out, &gop);
    Ok(out)
}

fn check_vbr(target_kbps: u32, max_kbps: u32) -> Result<(), EncodeError> {
    if target_kbps == 0 {
        return Err(EncodeError::Probe("--bitrate must be > 0".into()));
    }
    if max_kbps < target_kbps {
        return Err(EncodeError::Probe(format!(
            "--maxrate {max_kbps}k must be >= --bitrate {target_kbps}k"
        )));
    }
    Ok(())
}

/// Map our EncoderKind to ffmpeg CLI encoder name.
pub fn ffmpeg_encoder_name(kind: EncoderKind, auto_resolved: EncoderKind) -> &'static str {
    let k = if kind == EncoderKind::Auto {
        auto_resolved
    } else {
        kind
    };
    match k {
        EncoderKind::H264Nvenc => "h264_nvenc",
        EncoderKind::H264Amf => "h264_amf",
        EncoderKind::H264Qsv => "h264_qsv",
        EncoderKind::H264VideoToolbox => "h264_videotoolbox",
        EncoderKind::H264MediaFoundation => "h264_mf",
        EncoderKind::LibX264 => "libx264",
        EncoderKind::HevcNvenc => "hevc_nvenc",
        EncoderKind::HevcQsv => "hevc_qsv",
        EncoderKind::Av1Qsv => "av1_qsv",
        EncoderKind::Auto => "libx264",
    }
}

/// Build output path with correct container extension.
pub fn output_with_extension(base: &str, container: Container) -> String {
    if base.ends_with(&format!(".{}", container.extension())) {
        base.to_string()
    } else {
        format!("{}.{}", base.trim_end_matches('.'), container.extension())
    }
}

/// Validate encode config early (before spawning encoder thread).
pub fn validate_config(cfg: &EncodeConfig) -> Result<(), EncodeError> {
    if cfg.canvas.width < 64 || cfg.canvas.height < 64 {
        return Err(EncodeError::Probe("canvas too small (min 64x64)".into()));
    }
    if cfg.canvas.fps == 0 || cfg.canvas.fps > 240 {
        return Err(EncodeError::Probe("fps must be 1..240".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1: rawvideo BGRA stdin -> ffmpeg HW encode -> mp4.
// Fixed-canvas invariant: if requested canvas != native capture size, ffmpeg
// `-vf scale=W:H` handles it GPU/CPU-side so the encoder inits once.
// Audio is video-only in Phase 1 (`-an`); mic/loopback mixing lands Phase 3.
// ---------------------------------------------------------------------------

/// Parse "1920x1080" or "1920x1080@30" into (w, h, fps_opt).
pub fn parse_canvas(s: &str) -> Result<(u32, u32, Option<u32>), EncodeError> {
    let (geom, fps_part) = match s.split_once('@') {
        Some((g, f)) => (g, Some(f)),
        None => (s, None),
    };
    let (w, h) = geom
        .split_once('x')
        .ok_or_else(|| EncodeError::Probe(format!("bad canvas '{s}' (want WxH[@fps])")))?;
    let width: u32 = w
        .trim()
        .parse()
        .map_err(|_| EncodeError::Probe(format!("bad canvas width in '{s}'")))?;
    let height: u32 = h
        .trim()
        .parse()
        .map_err(|_| EncodeError::Probe(format!("bad canvas height in '{s}'")))?;
    let fps = match fps_part {
        Some(f) => Some(
            f.trim()
                .parse()
                .map_err(|_| EncodeError::Probe(format!("bad canvas fps in '{s}'")))?,
        ),
        None => None,
    };
    if width < 64 || height < 64 {
        return Err(EncodeError::Probe("canvas too small (min 64x64)".into()));
    }
    Ok((width, height, fps))
}

/// Map CLI `--encoder auto|nvenc|amf|qsv|x264` to EncoderKind.
pub fn parse_encoder_kind(s: &str) -> Result<EncoderKind, EncodeError> {
    match s.to_lowercase().as_str() {
        "auto" => Ok(EncoderKind::Auto),
        "nvenc" | "h264_nvenc" => Ok(EncoderKind::H264Nvenc),
        "amf" | "h264_amf" => Ok(EncoderKind::H264Amf),
        "qsv" | "h264_qsv" => Ok(EncoderKind::H264Qsv),
        "videotoolbox" | "vt" => Ok(EncoderKind::H264VideoToolbox),
        "x264" | "libx264" => Ok(EncoderKind::LibX264),
        other => Err(EncodeError::Probe(format!(
            "unknown encoder '{other}' (want auto|nvenc|amf|qsv|x264)"
        ))),
    }
}

/// Spawned ffmpeg child accepting BGRA frames on stdin.
pub struct RawvideoEncoder {
    child: Child,
    stdin: ChildStdin,
    pub encoder_name: String,
    pub native_w: u32,
    pub native_h: u32,
    pub canvas: CanvasConfig,
    frames_written: u64,
    bytes_written: u64,
}

impl RawvideoEncoder {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        native_w: u32,
        native_h: u32,
        fps: u32,
        canvas: CanvasConfig,
        encoder: EncoderKind,
        auto_resolved: EncoderKind,
        rate: RateControl,
        output: &str,
        audio_pipe: Option<&str>,
    ) -> Result<Self, EncodeError> {
        let encoder_name = ffmpeg_encoder_name(encoder, auto_resolved).to_string();
        let size = format!("{native_w}x{native_h}");
        let with_audio = audio_pipe.is_some();

        let mut args: Vec<String> = vec![
            "-y".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "bgra".into(),
            "-s".into(),
            size,
            "-framerate".into(),
            fps.to_string(),
            "-i".into(),
            "-".into(),
        ];
        if let Some(pipe) = audio_pipe {
            // Second input: mixed i16 stereo 48 kHz PCM through a named pipe
            // (a process has a single stdin, already video). `-shortest` trims
            // the audio lead-in (pipeline starts before capture warmup).
            args.extend(
                [
                    "-f", "s16le", "-ar", "48000", "-ac", "2", "-i", pipe, "-map", "0:v", "-map",
                    "1:a",
                ]
                .into_iter()
                .map(str::to_string),
            );
        }
        args.push("-c:v".into());
        args.push(encoder_name.clone());
        args.extend(rate_control_args(&encoder_name, &rate, fps)?);
        if with_audio {
            args.push("-c:a".into());
            args.push("aac".into());
            args.push("-b:a".into());
            args.push("192k".into());
            args.push("-shortest".into());
        } else {
            args.push("-an".into());
        }
        args.push("-pix_fmt".into());
        args.push("yuv420p".into());

        // Fixed canvas: scale in ffmpeg so encoder sees constant size.
        if canvas.width != native_w || canvas.height != native_h {
            args.push("-vf".into());
            args.push(format!("scale={}:{}", canvas.width, canvas.height));
        }

        args.push("-movflags".into());
        args.push("+faststart".into());
        args.push(output.into());

        tracing::info!(?args, "spawning ffmpeg");
        let mut ffmpeg_cmd = Command::new("ffmpeg");
        qcapture_core::hide_child_console(&mut ffmpeg_cmd);
        let mut child = ffmpeg_cmd
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| EncodeError::FfmpegMissing)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| EncodeError::Probe("failed to open ffmpeg stdin".into()))?;
        Ok(Self {
            child,
            stdin,
            encoder_name,
            native_w,
            native_h,
            canvas,
            frames_written: 0,
            bytes_written: 0,
        })
    }

    /// Write one BGRA frame (must be native_w*native_h*4 bytes). Blocks on pipe.
    pub fn write_frame(&mut self, data: &[u8]) -> Result<(), EncodeError> {
        let expect = (self.native_w as usize) * (self.native_h as usize) * 4;
        if data.len() != expect {
            return Err(EncodeError::Probe(format!(
                "frame size {} != {expect} ({ }x{ }) — skipping",
                data.len(),
                self.native_w,
                self.native_h
            )));
        }
        self.stdin
            .write_all(data)
            .map_err(|e| EncodeError::Probe(format!("ffmpeg stdin write failed: {e}")))?;
        self.frames_written += 1;
        self.bytes_written += data.len() as u64;
        Ok(())
    }

    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }

    /// Close stdin, wait for ffmpeg trailer write. Returns ffmpeg stderr tail on failure.
    pub fn finish(self) -> Result<(), EncodeError> {
        drop(self.stdin);
        let out = self
            .child
            .wait_with_output()
            .map_err(|e| EncodeError::Probe(format!("ffmpeg wait failed: {e}")))?;
        if out.status.success() {
            tracing::info!(frames = self.frames_written, "ffmpeg finished ok");
            Ok(())
        } else {
            let tail = String::from_utf8_lossy(&out.stderr);
            let tail: String = tail
                .lines()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            Err(EncodeError::Probe(format!(
                "ffmpeg exited {}:\n{tail}",
                out.status
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_name_mapping() {
        assert_eq!(
            ffmpeg_encoder_name(EncoderKind::LibX264, EncoderKind::LibX264),
            "libx264"
        );
        assert_eq!(
            ffmpeg_encoder_name(EncoderKind::Auto, EncoderKind::H264Nvenc),
            "h264_nvenc"
        );
    }

    #[test]
    fn output_ext() {
        assert_eq!(output_with_extension("rec", Container::Mp4), "rec.mp4");
        assert_eq!(output_with_extension("rec.mp4", Container::Mp4), "rec.mp4");
        assert_eq!(output_with_extension("rec", Container::Mkv), "rec.mkv");
    }

    #[test]
    fn canvas_parse() {
        assert_eq!(parse_canvas("1920x1080").unwrap(), (1920, 1080, None));
        assert_eq!(parse_canvas("1280x720@60").unwrap(), (1280, 720, Some(60)));
        assert!(parse_canvas("bad").is_err());
        assert!(parse_canvas("10x10").is_err());
    }

    #[test]
    fn encoder_parse() {
        assert_eq!(parse_encoder_kind("auto").unwrap(), EncoderKind::Auto);
        assert_eq!(parse_encoder_kind("nvenc").unwrap(), EncoderKind::H264Nvenc);
        assert_eq!(parse_encoder_kind("x264").unwrap(), EncoderKind::LibX264);
        assert!(parse_encoder_kind("nope").is_err());
    }

    fn has(s: &str, args: &[String]) -> bool {
        args.iter().any(|a| a == s)
    }

    #[test]
    fn rc_cbr_matches_legacy_argsets() {
        // NVENC CBR keeps the proven 5a set: -b:v + -rc cbr + -g, no preset.
        let a =
            rate_control_args("h264_nvenc", &RateControl::Cbr { bitrate_kbps: 8000 }, 30).unwrap();
        assert!(has("-b:v", &a) && has("8000k", &a) && has("cbr", &a) && has("-g", &a));
        assert!(!has("-preset", &a));
        let a = rate_control_args("libx264", &RateControl::Cbr { bitrate_kbps: 8000 }, 30).unwrap();
        assert!(has("veryfast", &a) && has("120", &a)); // gop = 30*4
    }

    #[test]
    fn rc_cqp_and_crf_routing() {
        let a = rate_control_args("h264_nvenc", &RateControl::Cqp { qp: 23 }, 30).unwrap();
        assert!(has("constqp", &a) && has("-qp", &a) && has("23", &a));
        assert!(!has("-b:v", &a));
        let a = rate_control_args("libx264", &RateControl::Crf { crf: 23 }, 30).unwrap();
        assert!(has("-crf", &a) && has("23", &a));
        // Cross-wired modes fail loudly, never silently remapped.
        assert!(rate_control_args("libx264", &RateControl::Cqp { qp: 23 }, 30).is_err());
        assert!(rate_control_args("h264_nvenc", &RateControl::Crf { crf: 23 }, 30).is_err());
        assert!(rate_control_args("h264_nvenc", &RateControl::Cqp { qp: 99 }, 30).is_err());
    }

    #[test]
    fn rc_vbr_caps_and_validates() {
        let a = rate_control_args(
            "h264_nvenc",
            &RateControl::Vbr {
                target_kbps: 6000,
                max_kbps: 9000,
            },
            30,
        )
        .unwrap();
        assert!(has("6000k", &a) && has("9000k", &a) && has("18000k", &a) && has("vbr", &a));
        assert!(rate_control_args(
            "h264_nvenc",
            &RateControl::Vbr {
                target_kbps: 9000,
                max_kbps: 6000
            },
            30
        )
        .is_err());
        assert!(
            rate_control_args("nope_enc", &RateControl::Cbr { bitrate_kbps: 1000 }, 30).is_err()
        );
    }
}
