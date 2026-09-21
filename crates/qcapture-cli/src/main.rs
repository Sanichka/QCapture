//! qcapture CLI: enumeration + recording + region picker.
//!   qcapture --list-screens | --list-windows | --list-audio | --probe-ffmpeg | --canvas-presets
//!   qcapture record --screen 0 --fps 30 [--canvas 1920x1080] [--encoder auto] [--bitrate 8000]
//!                   [--output out.mp4] [--duration 10] [--region x,y,w,h] [--window-title sub]
//!                   [--no-cursor]
//!   qcapture pick-region [--screen 0] [--json]   (fullscreen drag-to-select overlay)

use clap::{ArgGroup, Parser, Subcommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "qcapture",
    version,
    about = "QCapture — lightweight cross-platform screen recorder (Rust)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// List detected screens (id, geometry, scale, primary)
    #[arg(long, global = true)]
    list_screens: bool,

    /// List recordable windows (title, app, geometry)
    #[arg(long, global = true)]
    list_windows: bool,

    /// List mic + output audio devices
    #[arg(long, global = true)]
    list_audio: bool,

    /// Probe system ffmpeg for HW encoders (nvenc/amf/qsv/videotoolbox/x264)
    #[arg(long, global = true)]
    probe_ffmpeg: bool,

    /// Show fixed-canvas presets (output stays fixed during recording)
    #[arg(long, global = true)]
    canvas_presets: bool,

    /// Emit query output as JSON (for UI overlay + scripts)
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Record screen/region/window to MP4 (system audio on by default; see --no-audio)
    Record(Box<RecordArgs>),
    /// Fullscreen drag-to-select region overlay; prints "x,y,w,h" (monitor-relative)
    PickRegion(PickRegionArgs),
    /// Visual annotation editor: select a region, draw on it, save .qcap.json.
    /// Prints "x,y,w,h" for `record --region ... --annotate <doc>`.
    Annotate(AnnotateArgs),
    /// Floating control widget: target picker, Start/Stop, live audio, Advanced window
    Widget,
    /// Write a demo annotation doc (timed rect + line + text) for --annotate
    AnnotateDemo(AnnotateDemoArgs),
}

#[derive(Debug, Parser)]
struct AnnotateDemoArgs {
    /// Output .qcap.json path.
    #[arg(long, default_value = "demo.qcap.json")]
    output: String,

    /// Canvas the norm coords are authored against (should match feed size).
    #[arg(long, default_value = "1920x1080")]
    canvas: String,
}

#[derive(Debug, Parser)]
struct PickRegionArgs {
    /// Screen index (0 = primary, N = Nth display in --list-screens order).
    #[arg(long, default_value = "0")]
    screen: usize,

    /// Emit JSON {screen, x, y, w, h} instead of "x,y,w,h".
    #[arg(long, default_value = "false")]
    json: bool,
}

#[derive(Debug, Parser)]
struct AnnotateArgs {
    /// Screen index (0 = primary, N = Nth display in --list-screens order).
    #[arg(long, default_value = "0")]
    screen: usize,

    /// Where to save the .qcap.json doc (also printed on success).
    #[arg(long, default_value = "annotated.qcap.json")]
    save: String,
}

#[derive(Debug, Parser)]
#[command(group(ArgGroup::new("target").multiple(false)))]
struct RecordArgs {
    /// Screen index into --list-screens order (0 = primary).
    #[arg(long, default_value = "0", group = "target")]
    screen: u32,

    /// Crop region "x,y,w,h" in physical pixels, monitor-relative
    /// (origin at the target monitor's top-left; use `pick-region` to select).
    #[arg(long, group = "target")]
    region: Option<String>,

    /// Record window whose title contains this substring (case-insensitive).
    #[arg(long, group = "target")]
    window_title: Option<String>,

    /// Screen index used together with --region (which display to crop from).
    #[arg(long, default_value = "0")]
    region_screen: u32,

    /// Capture frame rate.
    #[arg(long, default_value = "30")]
    fps: u32,

    /// Fixed output canvas "WxH" (encoder inits once; scaled by MediaFoundation).
    /// Omit to keep native capture size.
    #[arg(long)]
    canvas: Option<String>,

    /// Encoder: auto|h264|hevc stay native (MediaFoundation);
    /// nvenc|amf|qsv|x264 go through ffmpeg (HW encode + AAC via named pipe).
    /// Annotate/draw auto-switch native encoders to ffmpeg.
    #[arg(long, default_value = "auto")]
    encoder: String,

    /// Video bitrate in kbps (CBR target / VBR target).
    #[arg(long, default_value = "8000")]
    bitrate: u32,

    /// Rate control: cbr (default) | vbr | cqp | crf.
    /// cqp needs --qp (NVENC/AMF/QSV); crf needs --crf (x264 only).
    /// vbr caps at --maxrate (default 1.5x bitrate). MF path is CBR-only.
    #[arg(long, default_value = "cbr")]
    rc: String,

    /// Quantizer 0..51 for --rc cqp (lower = better).
    #[arg(long)]
    qp: Option<u8>,

    /// CRF 0..51 for --rc crf with x264 (default 23).
    #[arg(long)]
    crf: Option<u8>,

    /// VBR ceiling in kbps (default: 1.5x --bitrate).
    #[arg(long)]
    maxrate: Option<u32>,

    /// Output path (.mp4). Default: qcapture_<timestamp>.mp4 in CWD.
    #[arg(long)]
    output: Option<String>,

    /// Stop after N seconds (for tests). Omit = record until Ctrl-C.
    #[arg(long)]
    duration: Option<u64>,

    /// Hide cursor in recording.
    #[arg(long, default_value = "false")]
    no_cursor: bool,

    /// Disable all audio (video-only, pre-Phase-3 behavior).
    #[arg(long, default_value = "false")]
    no_audio: bool,

    /// Also capture this mic (substring of `qcapture --list-audio` name,
    /// or "default"). Mixed with system audio into one AAC track.
    #[arg(long)]
    mic: Option<String>,

    /// System-audio gain in dB (-60..+12).
    #[arg(long, default_value = "0")]
    system_gain: f32,

    /// Mic gain in dB (-60..+12).
    #[arg(long, default_value = "0")]
    mic_gain: f32,

    /// Mute system audio (still records a silent-mix track).
    #[arg(long, default_value = "false")]
    system_mute: bool,

    /// Mute the mic.
    #[arg(long, default_value = "false")]
    mic_mute: bool,

    /// Burn timed annotations from a .qcap.json doc (see `annotate-demo`).
    /// Native encoders auto-switch to ffmpeg for burn-in.
    #[arg(long)]
    annotate: Option<String>,

    /// Open the live draw window during recording (screen/region/window).
    /// Live video + pen/shapes/text; closing the window stops recording.
    /// Native encoders auto-switch to ffmpeg for drawing.
    #[arg(long, default_value = "false")]
    draw: bool,

    /// Draw self-test: inject scripted strokes, then exit. Implies --draw.
    /// Used by CI/smoke tests in place of a mouse.
    #[arg(long)]
    draw_test: Option<u64>,

    /// Burn a highlight ring around the cursor (ffmpeg path; native
    /// encoders auto-switch just like --annotate/--draw).
    #[arg(long, default_value = "false")]
    cursor_highlight: bool,

    /// Burn an expanding ripple on mouse clicks (same path rules as above).
    #[arg(long, default_value = "false")]
    cursor_ripple: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if let Some(Commands::Record(args)) = cli.command {
        return run_record(*args);
    }
    if let Some(Commands::PickRegion(args)) = cli.command {
        return run_pick_region(args, cli.json);
    }
    if let Some(Commands::Annotate(args)) = cli.command {
        return run_annotate(args);
    }
    if matches!(cli.command, Some(Commands::Widget)) {
        return qcapture_ui::widget::run().map_err(|e| anyhow::anyhow!("{e}"));
    }
    if let Some(Commands::AnnotateDemo(args)) = cli.command {
        return run_annotate_demo(args);
    }
    // Back-compat query flags (no subcommand).
    if cli.list_screens {
        return query_screens(cli.json);
    }
    if cli.list_windows {
        return query_windows(cli.json);
    }
    if cli.list_audio {
        return query_audio(cli.json);
    }
    if cli.probe_ffmpeg {
        return query_ffmpeg(cli.json);
    }
    if cli.canvas_presets {
        return query_presets(cli.json);
    }

    println!("QCapture 0.1.0 — Phase 1 (video record).");
    println!("Try: qcapture --list-screens | qcapture record --help");
    Ok(())
}

// ---------------------------------------------------------------- queries ---

fn query_screens(json: bool) -> anyhow::Result<()> {
    let displays = qcapture_capture::list_displays().unwrap_or_default();
    if json {
        println!("{}", serde_json::to_string_pretty(&displays)?);
    } else if displays.is_empty() {
        println!("No displays detected.");
    } else {
        for d in &displays {
            println!(
                "#{} {} {}x{}+{}+{} scale={:.2} primary={} refresh={:?}",
                d.id,
                d.name,
                d.width,
                d.height,
                d.x,
                d.y,
                d.scale_factor,
                d.is_primary,
                d.refresh_hz
            );
        }
        println!("\nNote: `record --screen N` follows --list-screens order (0=primary).");
    }
    Ok(())
}

fn query_windows(json: bool) -> anyhow::Result<()> {
    match qcapture_capture::list_windows() {
        Ok(wins) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&wins)?);
            } else if wins.is_empty() {
                println!("No windows with titles found.");
            } else {
                for w in &wins {
                    println!(
                        "[{}] {} — {} ({}x{}+{}+{}{})",
                        w.id,
                        w.app_name,
                        w.title,
                        w.width,
                        w.height,
                        w.x,
                        w.y,
                        if w.minimized { " minimized" } else { "" }
                    );
                }
                println!("\nTip: `record --window-title <substring>` matches the title column.");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("window enumeration failed (Wayland may need Portal): {e}");
            std::process::exit(2);
        }
    }
}

fn query_audio(json: bool) -> anyhow::Result<()> {
    let inputs = qcapture_audio::list_input_devices().unwrap_or_default();
    let outputs = qcapture_audio::list_output_devices().unwrap_or_default();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"inputs": inputs, "outputs": outputs})
            )?
        );
    } else {
        println!("Inputs (mic):");
        for d in &inputs {
            println!("  - {}", d.name);
        }
        if inputs.is_empty() {
            println!("  (none)");
        }
        println!("Outputs (for loopback reference):");
        for d in &outputs {
            println!("  - {}", d.name);
        }
        if outputs.is_empty() {
            println!("  (none)");
        }
        println!("\nNote: system loopback capture uses WASAPI/PipeWire/SCKit (Phase 3), not cpal.");
    }
    Ok(())
}

fn query_ffmpeg(json: bool) -> anyhow::Result<()> {
    match qcapture_encode::probe_ffmpeg() {
        Ok(info) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "version": info.version_line,
                        "h264_nvenc": info.has_nvenc,
                        "h264_amf": info.has_amf,
                        "h264_qsv": info.has_qsv,
                        "h264_videotoolbox": info.has_videotoolbox,
                        "libx264": info.has_libx264,
                        "auto": format!("{:?}", qcapture_encode::resolve_auto_encoder(&info)),
                    }))?
                );
            } else {
                println!("{}", info.version_line);
                println!("h264_nvenc:        {}", info.has_nvenc);
                println!("h264_amf:          {}", info.has_amf);
                println!("h264_qsv:          {}", info.has_qsv);
                println!("h264_videotoolbox: {}", info.has_videotoolbox);
                println!("libx264:           {}", info.has_libx264);
                println!(
                    "auto choice:       {:?}",
                    qcapture_encode::resolve_auto_encoder(&info)
                );
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(3);
        }
    }
}

fn query_presets(json: bool) -> anyhow::Result<()> {
    let presets = qcapture_core::CanvasConfig::presets();
    if json {
        println!("{}", serde_json::to_string_pretty(&presets)?);
    } else {
        println!("Fixed canvas presets (encoder inits once; region resize only crops/scales):");
        for c in &presets {
            println!("  {}x{} @ {}fps", c.width, c.height, c.fps);
        }
    }
    Ok(())
}

// ------------------------------------------------------------ pick-region ---

fn run_pick_region(args: PickRegionArgs, global_json: bool) -> anyhow::Result<()> {
    let json = args.json || global_json;
    let (d, backdrop) = resolve_screen_with_backdrop(args.screen)?;
    eprintln!(
        "Select a region on '{}' ({}x{}, scale {:.2}) — Enter: confirm, Esc: cancel",
        d.name, d.width, d.height, d.scale_factor
    );
    let geom = qcapture_ui::pick_region::MonitorGeom {
        x: d.x,
        y: d.y,
        w: d.width,
        h: d.height,
        scale: d.scale_factor,
    };
    match qcapture_ui::pick_region::run(geom, backdrop).map_err(|e| anyhow::anyhow!("{e}"))? {
        Some(r) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "screen": args.screen,
                        "x": r.x, "y": r.y, "w": r.w, "h": r.h,
                        "region": format!("{},{},{},{}", r.x, r.y, r.w, r.h),
                    }))?
                );
            } else {
                println!("{},{},{},{}", r.x, r.y, r.w, r.h);
            }
            Ok(())
        }
        None => {
            eprintln!("cancelled.");
            std::process::exit(1);
        }
    }
}

/// Shared monitor resolution (0 = primary) + frozen screenshot backdrop.
/// Screenshot failure is warn-and-continue: overlays fall back to dim/dark.
fn resolve_screen_with_backdrop(
    screen: usize,
) -> anyhow::Result<(
    qcapture_core::DisplayInfo,
    Option<qcapture_capture::RgbaShot>,
)> {
    let displays = qcapture_capture::list_displays()
        .map_err(|e| anyhow::anyhow!("display enumeration failed: {e}"))?;
    if displays.is_empty() {
        anyhow::bail!("no displays detected");
    }
    // 0 = primary; N = Nth in enumeration order (matches `record --screen` note).
    let d = if screen == 0 {
        displays
            .iter()
            .find(|d| d.is_primary)
            .or(displays.first())
            .unwrap()
            .clone()
    } else {
        displays
            .get(screen)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "--screen {} out of range ({} displays; 0=primary)",
                    screen,
                    displays.len()
                )
            })?
            .clone()
    };
    let backdrop = match qcapture_capture::screenshot_monitor_at(d.x, d.y) {
        Ok(shot) => Some(shot),
        Err(e) => {
            eprintln!("warning: backdrop screenshot failed ({e}); falling back");
            None
        }
    };
    Ok((d, backdrop))
}

// -------------------------------------------------------------- annotate ---

/// Visual editor: fullscreen select -> draw on the frozen screenshot ->
/// save doc. Prints the selected region for `record --region ... --annotate`.
fn run_annotate(args: AnnotateArgs) -> anyhow::Result<()> {
    let (d, backdrop) = resolve_screen_with_backdrop(args.screen)?;
    eprintln!(
        "Annotate on '{}' ({}x{}, scale {:.2}): drag to select, Enter to draw, Record saves {}",
        d.name, d.width, d.height, d.scale_factor, args.save
    );
    let geom = qcapture_ui::annotate_editor::MonitorGeom {
        x: d.x,
        y: d.y,
        w: d.width,
        h: d.height,
        scale: d.scale_factor,
    };
    match qcapture_ui::annotate_editor::run(geom, backdrop, args.save.clone())
        .map_err(|e| anyhow::anyhow!("{e}"))?
    {
        Some(r) => {
            println!("{},{},{},{}", r.x, r.y, r.w, r.h);
            eprintln!(
                "doc: {} — burn with: record --encoder nvenc --region {},{},{},{} --annotate {}",
                args.save, r.x, r.y, r.w, r.h, args.save
            );
            Ok(())
        }
        None => {
            eprintln!("cancelled.");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------- annotate-demo ---

/// Build a small demo doc: red border rect at 0s, green diagonal at 1s,
/// white caption at 2s. Handy for smoke-testing `--annotate` burn-in.
fn run_annotate_demo(args: AnnotateDemoArgs) -> anyhow::Result<()> {
    use qcapture_annotate::{AnnotateDoc, Rgba, Stroke, Tool};
    let (cw, ch, _) = qcapture_encode::parse_canvas(&args.canvas)?;
    let mut doc = AnnotateDoc {
        canvas_w: cw,
        canvas_h: ch,
        ..Default::default()
    };
    let mut stroke = |tool, points, color: Rgba, width, text, appear_ms| {
        doc.add_stroke(Stroke {
            points,
            color,
            width_px: width,
            tool,
            text,
            appear_ms,
            font_px: Some(48.0),
            filled: false,
            font_path: None,
        })
    };
    stroke(
        Tool::Rect,
        vec![(0.05, 0.05), (0.95, 0.95)],
        Rgba(255, 0, 0, 255),
        6.0,
        None,
        0,
    )?;
    stroke(
        Tool::Line,
        vec![(0.05, 0.95), (0.95, 0.05)],
        Rgba(0, 255, 0, 255),
        6.0,
        None,
        1000,
    )?;
    stroke(
        Tool::Text,
        vec![(0.08, 0.78)],
        Rgba(255, 255, 255, 255),
        1.0,
        Some("QCapture demo".to_string()),
        2000,
    )?;
    doc.save(&args.output).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "wrote {} ({} strokes, canvas {cw}x{ch})",
        args.output,
        doc.strokes.len()
    );
    println!(
        "try: qcapture record --encoder nvenc --annotate {} --duration 6 out.mp4",
        args.output
    );
    Ok(())
}

// ---------------------------------------------------------------- record ----

#[allow(dead_code)]
fn parse_region(s: &str) -> anyhow::Result<qcapture_core::Rect> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        anyhow::bail!("bad --region '{s}' (want x,y,w,h in physical pixels)");
    }
    let x: i32 = parts[0].trim().parse()?;
    let y: i32 = parts[1].trim().parse()?;
    let w: u32 = parts[2].trim().parse()?;
    let h: u32 = parts[3].trim().parse()?;
    let r = qcapture_core::Rect::new(x, y, w, h);
    qcapture_core::validate_rect(r)?;
    Ok(r)
}

fn default_output() -> String {
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    format!("qcapture_{ts}.mp4")
}

/// Either OS audio pipeline behind one type so both record paths stay
/// single-path across platforms (construction still differs per OS).
enum CliAudio {
    #[cfg(windows)]
    Win(qcapture_audio::win_audio::WinAudioPipeline),
    #[cfg(not(windows))]
    Port(qcapture_audio::portable::PortAudioPipeline),
}

impl CliAudio {
    fn mixed_rx(&self) -> flume::Receiver<Vec<u8>> {
        match self {
            #[cfg(windows)]
            Self::Win(p) => p.mixed_rx(),
            #[cfg(not(windows))]
            Self::Port(p) => p.mixed_rx(),
        }
    }

    fn shutdown(self) -> qcapture_audio::AudioStats {
        match self {
            #[cfg(windows)]
            Self::Win(p) => p.shutdown(),
            #[cfg(not(windows))]
            Self::Port(p) => p.shutdown(),
        }
    }
}

/// Shared audio startup for both record paths: system loopback on unless
/// `--no-audio`, mic opt-in. Fail-soft on system-only trouble; loud on
/// explicit `--mic` typos. Backend per OS: WASAPI loopback on Windows,
/// cpal monitor source on Linux, mic-only on macOS.
fn start_cli_audio(args: &RecordArgs) -> anyhow::Result<Option<CliAudio>> {
    if args.no_audio {
        return Ok(None);
    }
    let levels = qcapture_audio::SharedLevels::new(qcapture_audio::MixerLevels {
        system_gain: qcapture_audio::MixerLevels::db_to_linear(args.system_gain),
        mic_gain: qcapture_audio::MixerLevels::db_to_linear(args.mic_gain),
        system_muted: args.system_mute,
        mic_muted: args.mic_mute,
    });
    #[cfg(windows)]
    let started = {
        let cfg = qcapture_audio::win_audio::WinAudioConfig {
            capture_system: true,
            mic_query: args.mic.clone(),
            levels,
        };
        qcapture_audio::win_audio::start_pipeline(cfg).map(CliAudio::Win)
    };
    #[cfg(not(windows))]
    let started = {
        let cfg = qcapture_audio::portable::PortAudioConfig {
            capture_system: true,
            mic_query: args.mic.clone(),
            levels,
        };
        qcapture_audio::portable::start_pipeline(cfg).map(CliAudio::Port)
    };
    match started {
        Ok(p) => {
            eprintln!(
                "audio: system loopback on{}",
                args.mic
                    .as_ref()
                    .map(|m| format!(" + mic '{m}'"))
                    .unwrap_or_default()
            );
            Ok(Some(p))
        }
        Err(e) => {
            if args.mic.is_some() {
                anyhow::bail!("audio pipeline failed: {e}");
            }
            eprintln!("warning: audio unavailable ({e}) — recording video-only");
            Ok(None)
        }
    }
}

/// Self-test driver: two scripted strokes through the same channel a mouse
/// would use. Pixels are asserted by the operator (see Phase 4c notes).
fn spawn_draw_test_driver(tx: flume::Sender<qcapture_annotate::DrawEvent>, w: u32, h: u32) {
    use qcapture_annotate::{DrawEvent, Rgba, Stroke, Tool};
    let _ = (w, h);
    std::thread::Builder::new()
        .name("qcapture-draw-test".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            let diag: Vec<(f32, f32)> = (0..20)
                .map(|i| {
                    let t = 0.1 + 0.8 * i as f32 / 19.0;
                    (t, t)
                })
                .collect();
            let _ = tx.send(DrawEvent::AddStroke(Stroke {
                points: diag,
                color: Rgba(255, 0, 0, 255),
                width_px: 6.0,
                tool: Tool::Pen,
                text: None,
                appear_ms: 0,
                font_px: None,
                filled: false,
                font_path: None,
            }));
            std::thread::sleep(Duration::from_secs(1));
            let _ = tx.send(DrawEvent::AddStroke(Stroke {
                points: vec![(0.6, 0.1), (0.9, 0.4)],
                color: Rgba(0, 255, 0, 255),
                width_px: 5.0,
                tool: Tool::Rect,
                text: None,
                appear_ms: 0,
                font_px: None,
                filled: false,
                font_path: None,
            }));
        })
        .ok();
}

/// Look up a target display by 0-based screen position (0 = primary).
/// Shared by all backends so `--screen` means the same thing everywhere.
fn resolve_display(screen: u32) -> anyhow::Result<qcapture_core::DisplayInfo> {
    let idx = screen as usize;
    let displays = qcapture_capture::list_displays()
        .map_err(|e| anyhow::anyhow!("display enumeration failed: {e}"))?;
    if idx == 0 {
        displays
            .iter()
            .find(|d| d.is_primary)
            .or(displays.first())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no displays detected"))
    } else {
        displays.get(idx).cloned().ok_or_else(|| {
            anyhow::anyhow!("--screen {idx} out of range ({} displays)", displays.len())
        })
    }
}

/// Resolve a `--encoder` name without MediaFoundation (non-Windows):
/// explicit vendor kinds pass through, `auto` probes, bare `h264`/`hevc`
/// pick the best available family member or bail loudly.
#[cfg(not(windows))]
fn resolve_unix_encoder(name: &str) -> anyhow::Result<qcapture_core::EncoderKind> {
    let probe = || qcapture_encode::probe_ffmpeg().map_err(|e| anyhow::anyhow!("{e}"));
    match name {
        "auto" => Ok(qcapture_encode::resolve_auto_encoder(&probe()?)),
        "h264" => qcapture_encode::best_h264(&probe()?).ok_or_else(|| {
            anyhow::anyhow!("this ffmpeg has no H.264 encoder — see `qcapture --probe-ffmpeg`")
        }),
        "hevc" | "h265" => qcapture_encode::best_hevc(&probe()?)
            .ok_or_else(|| anyhow::anyhow!("this ffmpeg has no HEVC encoder — try --encoder x264")),
        other => {
            Ok(qcapture_encode::parse_encoder_kind(other).map_err(|e| anyhow::anyhow!("{e}"))?)
        }
    }
}

/// FFmpeg + eframe draw path: capture runs on a background thread, the draw
/// window (live video texture + tools) runs on the main thread. Closing the
/// draw window stops the recording. Backend per OS: WGC on Windows, xcap
/// bridge elsewhere.
#[allow(clippy::too_many_arguments)]
fn run_draw_record(
    args: &RecordArgs,
    kind: qcapture_core::EncoderKind,
    rate: qcapture_core::RateControl,
    canvas: Option<(u32, u32)>,
    output: &str,
    show_cursor: bool,
    duration: Option<Duration>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    use qcapture_capture::ffmpeg_cap as fc;
    use qcapture_capture::pump as pc;
    #[cfg(not(windows))]
    use qcapture_capture::xcap_cap as xc;
    let info = qcapture_encode::probe_ffmpeg().map_err(|e| anyhow::anyhow!("{e}"))?;
    if !qcapture_encode::supports(kind, &info) {
        anyhow::bail!(
            "this ffmpeg has no {} ({}) — see `qcapture --probe-ffmpeg`",
            pc::encoder_name(kind),
            info.version_line
        );
    }
    eprintln!(
        "ffmpeg: {} -> {}",
        info.version_line,
        pc::encoder_name(kind)
    );
    qcapture_encode::rate_control_args(pc::encoder_name(kind), &rate, args.fps)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    #[cfg(not(windows))]
    if args.cursor_highlight || args.cursor_ripple {
        anyhow::bail!(
            "cursor fx is Windows-only in this release (no portable cursor position API yet)"
        );
    }
    let annotate = match &args.annotate {
        Some(path) => {
            let doc =
                qcapture_annotate::AnnotateDoc::load(path).map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "annotations: {} strokes + {} watermarks from {path}",
                doc.strokes.len(),
                doc.watermarks.len()
            );
            Some(doc)
        }
        None => None,
    };
    // Resolve the window once: feed-size guess for the panel plus (Windows)
    // the HWND for cursor mapping (preview resize self-corrects drift;
    // strokes are norm coords so they survive it).
    #[cfg(windows)]
    let resolved_window = args
        .window_title
        .as_ref()
        .map(|t| qcapture_capture::resolve_window(t))
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    #[cfg(not(windows))]
    let resolved_window: Option<(u32, u32)> = match &args.window_title {
        Some(title) => {
            let needle = title.to_lowercase();
            let found = qcapture_capture::list_windows()
                .map_err(|e| anyhow::anyhow!("window enumeration failed: {e}"))?
                .into_iter()
                .find(|w| w.title.to_lowercase().contains(&needle));
            match found {
                Some(w) => Some(((w.width.max(64)) & !1, (w.height.max(64)) & !1)),
                None => anyhow::bail!("no window matching '{title}'"),
            }
        }
        None => None,
    };
    // Feed-size guess for the panel (preview resize self-corrects align/clamp
    // and window-size drift; strokes are norm coords so they survive it).
    #[cfg(windows)]
    let (feed_w, feed_h) = if let Some(res) = &resolved_window {
        (res.w, res.h)
    } else if let Some(r) = &args.region {
        let rect = parse_region(r)?;
        if rect.x < 0 || rect.y < 0 {
            anyhow::bail!("--region x,y must be >= 0 (monitor-relative)");
        }
        ((rect.w & !1).max(64), (rect.h & !1).max(64))
    } else {
        let d = resolve_display(args.screen)?;
        (d.width.max(64) & !1, d.height.max(64) & !1)
    };
    #[cfg(not(windows))]
    let (feed_w, feed_h) = if let Some((w, h)) = &resolved_window {
        (*w, *h)
    } else if let Some(r) = &args.region {
        let rect = parse_region(r)?;
        if rect.x < 0 || rect.y < 0 {
            anyhow::bail!("--region x,y must be >= 0 (monitor-relative)");
        }
        ((rect.w & !1).max(64), (rect.h & !1).max(64))
    } else {
        let d = resolve_display(args.screen)?;
        (d.width.max(64) & !1, d.height.max(64) & !1)
    };
    let t0 = Instant::now();
    let audio = start_cli_audio(args)?;
    let pipe = if audio.is_some() {
        Some(
            qcapture_encode::audio_pipe::FfmpegAudioPipe::create()
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )
    } else {
        None
    };
    if let (Some(a), Some(p)) = (audio.as_ref(), pipe.as_ref()) {
        qcapture_encode::audio_pipe::forward_to_pipe(a.mixed_rx(), p.sender());
    }
    let audio_pipe_name = pipe.as_ref().map(|p| p.name.clone());
    let (stat_tx, stat_rx) = std::sync::mpsc::channel();
    let on_end: Box<dyn FnOnce() + Send> = match audio {
        Some(p) => Box::new(move || {
            let _ = stat_tx.send(p.shutdown());
        }),
        None => Box::new(move || {
            let _ = stat_tx.send(Default::default());
        }),
    };
    // Draw + preview channels (bounded; preview drops on backpressure).
    let (draw_tx, draw_rx) = flume::bounded::<qcapture_annotate::DrawEvent>(256);
    let (preview_tx, preview_rx) = flume::bounded::<pc::PreviewFrame>(4);
    if args.draw_test.is_some() {
        spawn_draw_test_driver(draw_tx.clone(), feed_w, feed_h);
    }
    eprintln!("draw window: {feed_w}x{feed_h} live preview (close window to stop)…");
    // Capture thread owns the blocking WGC pump + ffmpeg child.
    let done_flag = Arc::new(AtomicBool::new(false));
    let done_flag_t = done_flag.clone();
    let (cap_tx, cap_rx) = std::sync::mpsc::channel();
    let region_s = args.region.clone();
    let region_screen = args.region_screen;
    let window_s = args.window_title.clone();
    #[cfg(windows)]
    let window_hwnd = resolved_window.map(|r| r.hwnd);
    #[cfg(not(windows))]
    let window_hwnd = None;
    let screen = args.screen;
    let fps = args.fps;
    let stop_t = stop.clone();
    let output_s = output.to_string();
    let annotate_t = annotate.clone();
    let canvas_t = canvas;
    let cursor_fx = qcapture_core::CursorFx::opt(args.cursor_highlight, args.cursor_ripple);
    std::thread::Builder::new()
        .name("qcapture-draw-capture".into())
        .spawn(move || {
            let job = |c: Option<(u32, u32)>| pc::FfmpegJob {
                fps,
                encoder: kind,
                rate,
                canvas: c,
                show_cursor,
                output: output_s.clone(),
                duration,
                stop_flag: stop_t.clone(),
                annotate: annotate_t.clone(),
                cursor_fx,
                cursor_window: window_hwnd,
                audio_pipe: audio_pipe_name.clone(),
                preview_tx: Some(preview_tx.clone()),
            };
            let res = if let Some(title) = &window_s {
                // Window resize mid-record hits the fixed-canvas rule (frames
                // adapted center crop/pad, encoder never re-inits); the draw
                // panel keeps working on the initial feed geometry.
                #[cfg(windows)]
                let r = fc::run_ffmpeg_window(title, job(canvas_t), on_end, Some(draw_rx));
                #[cfg(not(windows))]
                let r = xc::run_xcap_window(title, job(canvas_t), on_end, Some(draw_rx));
                r
            } else if let Some(r) = &region_s {
                match parse_region(r) {
                    Ok(rect) => {
                        #[cfg(windows)]
                        {
                            let m = qcapture_capture::wgc_monitor_index(region_screen);
                            fc::run_ffmpeg_region(
                                m,
                                rect.x.max(0) as u32,
                                rect.y.max(0) as u32,
                                rect.w,
                                rect.h,
                                job(None),
                                on_end,
                                Some(draw_rx),
                            )
                        }
                        #[cfg(not(windows))]
                        {
                            let d = resolve_display(region_screen);
                            match d {
                                Ok(d) => xc::run_xcap_region(
                                    &d,
                                    rect.x.max(0) as u32,
                                    rect.y.max(0) as u32,
                                    rect.w,
                                    rect.h,
                                    job(None),
                                    on_end,
                                    Some(draw_rx),
                                ),
                                Err(e) => {
                                    Err(qcapture_capture::CaptureError::Backend(e.to_string()))
                                }
                            }
                        }
                    }
                    Err(e) => Err(qcapture_capture::CaptureError::Backend(e.to_string())),
                }
            } else {
                #[cfg(windows)]
                {
                    let m = qcapture_capture::wgc_monitor_index(screen);
                    fc::run_ffmpeg_monitor(m, job(canvas_t), on_end, Some(draw_rx))
                }
                #[cfg(not(windows))]
                match resolve_display(screen) {
                    Ok(d) => xc::run_xcap_monitor(&d, job(canvas_t), on_end, Some(draw_rx)),
                    Err(e) => Err(qcapture_capture::CaptureError::Backend(e.to_string())),
                }
            };
            done_flag_t.store(true, Ordering::SeqCst);
            let _ = cap_tx.send(res);
        })
        .map_err(|e| anyhow::anyhow!("capture thread: {e}"))?;
    // Main thread: draw window (eframe). Auto-closes when capture ends.
    let win_res = qcapture_ui::draw_panel::run_draw_window(
        feed_w,
        feed_h,
        draw_tx,
        preview_rx,
        done_flag.clone(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Window closed first (user X): stop the capture, then join + report.
    stop.store(true, Ordering::SeqCst);
    let stats = cap_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("capture thread hung up"))??;
    if let Some(p) = pipe {
        match p.finish() {
            Ok(n) => eprintln!("audio pipe: {:.2} MB -> ffmpeg", n as f64 / 1_000_000.0),
            Err(e) => eprintln!("warning: audio pipe: {e}"),
        }
    }
    let el = t0.elapsed();
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    let audio_note = match stat_rx.try_recv().ok() {
        Some(s) if s.quanta_emitted > 0 => format!(
            " + AAC {} quanta ({} sys/{} mic underruns)",
            s.quanta_emitted, s.sys_underruns, s.mic_underruns
        ),
        _ => String::new(),
    };
    let fx_note = cursor_fx
        .map(|c| match (c.highlight, c.ripple) {
            (true, true) => " +cursor highlight+ripple",
            (true, false) => " +cursor highlight",
            (false, true) => " +cursor ripple",
            (false, false) => "",
        })
        .unwrap_or("");
    println!(
        "Saved {output} — {} frames ({} annotated, {} preview) in {:.1}s, {:.2} MB via {}{}{}.",
        stats.written,
        stats.annotated_frames,
        win_res.frames_shown,
        el.as_secs_f64(),
        size as f64 / 1_000_000.0,
        pc::encoder_name(kind),
        fx_note,
        audio_note
    );
    // Hybrid sidecar: scripted doc + live strokes merged for re-editing.
    let mut sidecar_doc = annotate.clone().unwrap_or(qcapture_annotate::AnnotateDoc {
        version: 1,
        canvas_w: feed_w,
        canvas_h: feed_h,
        strokes: Vec::new(),
        watermarks: Vec::new(),
        burn_in: true,
    });
    let live_n = win_res.doc.strokes.len();
    if live_n > 0 {
        // Feed sizes agree up to align/clamp; norm coords survive.
        for s in win_res.doc.strokes {
            let _ = sidecar_doc.add_stroke(s);
        }
        let sidecar = qcapture_annotate::AnnotateDoc::sidecar_path_for(output);
        match sidecar_doc.save(&sidecar) {
            Ok(()) => eprintln!("sidecar: {sidecar} ({} live strokes)", live_n),
            Err(e) => eprintln!("warning: sidecar save failed: {e}"),
        }
    } else if annotate.is_some() {
        let sidecar = qcapture_annotate::AnnotateDoc::sidecar_path_for(output);
        match annotate.unwrap().save(&sidecar) {
            Ok(()) => eprintln!("sidecar: {sidecar}"),
            Err(e) => eprintln!("warning: sidecar save failed: {e}"),
        }
    }
    Ok(())
}

/// FFmpeg HW path: probe -> availability check -> byte-frame capture
/// -> rawvideo pipe (+ AAC from the OS pipe when audio is on).
/// Backend per OS: WGC on Windows, xcap bridge elsewhere.
#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_record(
    args: &RecordArgs,
    kind: qcapture_core::EncoderKind,
    rate: qcapture_core::RateControl,
    canvas: Option<(u32, u32)>,
    output: &str,
    show_cursor: bool,
    duration: Option<Duration>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    #[cfg(windows)]
    use qcapture_capture::ffmpeg_cap as fc;
    use qcapture_capture::pump as pc;
    #[cfg(not(windows))]
    use qcapture_capture::xcap_cap as xc;
    let info = qcapture_encode::probe_ffmpeg().map_err(|e| anyhow::anyhow!("{e}"))?;
    if !qcapture_encode::supports(kind, &info) {
        anyhow::bail!(
            "this ffmpeg has no {} ({}) — see `qcapture --probe-ffmpeg`",
            pc::encoder_name(kind),
            info.version_line
        );
    }
    eprintln!(
        "ffmpeg: {} -> {}",
        info.version_line,
        pc::encoder_name(kind)
    );
    // Dry-run the rate mapping now: bad combos (x264+cqp, qp>51,
    // maxrate<bitrate) must fail before any thread or capture starts.
    qcapture_encode::rate_control_args(pc::encoder_name(kind), &rate, args.fps)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    #[cfg(not(windows))]
    if args.cursor_highlight || args.cursor_ripple {
        anyhow::bail!(
            "cursor fx is Windows-only in this release (no portable cursor position API yet)"
        );
    }
    // Load annotations early: bad JSON must fail before a minute of capture.
    let annotate = match &args.annotate {
        Some(path) => {
            let doc =
                qcapture_annotate::AnnotateDoc::load(path).map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "annotations: {} strokes + {} watermarks from {path}",
                doc.strokes.len(),
                doc.watermarks.len()
            );
            Some(doc)
        }
        None => None,
    };
    let t0 = Instant::now();
    // Audio: same pipeline as MF; chunks forward into the ffmpeg named pipe.
    // The on_capture_end callback shuts the pipeline down BEFORE the pump join
    // (pipe EOF lets ffmpeg exit — reversed order deadlocks).
    let audio = start_cli_audio(args)?;
    let pipe = if audio.is_some() {
        Some(
            qcapture_encode::audio_pipe::FfmpegAudioPipe::create()
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )
    } else {
        None
    };
    if let (Some(a), Some(p)) = (audio.as_ref(), pipe.as_ref()) {
        qcapture_encode::audio_pipe::forward_to_pipe(a.mixed_rx(), p.sender());
    }
    let audio_pipe_name = pipe.as_ref().map(|p| p.name.clone());
    let (stat_tx, stat_rx) = std::sync::mpsc::channel();
    let on_end: Box<dyn FnOnce() + Send> = match audio {
        Some(p) => Box::new(move || {
            let _ = stat_tx.send(p.shutdown());
        }),
        None => Box::new(move || {
            let _ = stat_tx.send(Default::default());
        }),
    };
    let cursor_fx = qcapture_core::CursorFx::opt(args.cursor_highlight, args.cursor_ripple);
    // Window HWND for cursor mapping (Windows; fail fast: bad title must
    // not record). Other OSes map nothing (cursor fx rejected above).
    #[cfg(windows)]
    let cursor_hwnd = args
        .window_title
        .as_ref()
        .map(|t| qcapture_capture::resolve_window(t))
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .map(|r| r.hwnd);
    #[cfg(not(windows))]
    let cursor_hwnd = None;
    let job = |c: Option<(u32, u32)>| pc::FfmpegJob {
        fps: args.fps,
        encoder: kind,
        rate,
        canvas: c,
        show_cursor,
        output: output.to_string(),
        duration,
        stop_flag: stop.clone(),
        annotate: annotate.clone(),
        cursor_fx,
        cursor_window: cursor_hwnd,
        audio_pipe: audio_pipe_name.clone(),
        preview_tx: None,
    };
    // Draw mode is handled by run_draw_record (eframe panel on main thread);
    // this headless path never sees those flags (routed in run_record).
    if args.draw || args.draw_test.is_some() {
        anyhow::bail!("internal: draw flags reached headless record — use run_draw_record");
    }
    let stats = if let Some(title) = &args.window_title {
        #[cfg(windows)]
        let s = fc::run_ffmpeg_window(title, job(canvas), on_end, None)?;
        #[cfg(not(windows))]
        let s = xc::run_xcap_window(title, job(canvas), on_end, None)?;
        s
    } else if let Some(r) = &args.region {
        let rect = parse_region(r)?;
        if rect.x < 0 || rect.y < 0 {
            anyhow::bail!("--region x,y must be >= 0 (monitor-relative)");
        }
        #[cfg(windows)]
        let s = {
            let m = qcapture_capture::wgc_monitor_index(args.region_screen);
            fc::run_ffmpeg_region(
                m,
                rect.x as u32,
                rect.y as u32,
                rect.w,
                rect.h,
                job(None),
                on_end,
                None,
            )?
        };
        #[cfg(not(windows))]
        let s = {
            let d = resolve_display(args.region_screen)?;
            xc::run_xcap_region(
                &d,
                rect.x.max(0) as u32,
                rect.y.max(0) as u32,
                rect.w,
                rect.h,
                job(None),
                on_end,
                None,
            )?
        };
        s
    } else {
        #[cfg(windows)]
        let s = {
            let m = qcapture_capture::wgc_monitor_index(args.screen);
            fc::run_ffmpeg_monitor(m, job(canvas), on_end, None)?
        };
        #[cfg(not(windows))]
        let s = {
            let d = resolve_display(args.screen)?;
            xc::run_xcap_monitor(&d, job(canvas), on_end, None)?
        };
        s
    };
    // Forwarder + writer drained by now; join the pipe thread, then report.
    if let Some(p) = pipe {
        match p.finish() {
            Ok(n) => eprintln!("audio pipe: {:.2} MB -> ffmpeg", n as f64 / 1_000_000.0),
            Err(e) => eprintln!("warning: audio pipe: {e}"),
        }
    }
    let el = t0.elapsed();
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    let audio_note = match stat_rx.try_recv().ok() {
        Some(s) if s.quanta_emitted > 0 => format!(
            " + AAC {} quanta ({} sys/{} mic underruns)",
            s.quanta_emitted, s.sys_underruns, s.mic_underruns
        ),
        _ => String::new(),
    };
    let fx_note = cursor_fx
        .map(|c| match (c.highlight, c.ripple) {
            (true, true) => " +cursor highlight+ripple",
            (true, false) => " +cursor highlight",
            (false, true) => " +cursor ripple",
            (false, false) => "",
        })
        .unwrap_or("");
    println!(
        "Saved {output} — {} frames ({} annotated) in {:.1}s, {:.2} MB via {}{}{}.",
        stats.written,
        stats.annotated_frames,
        el.as_secs_f64(),
        size as f64 / 1_000_000.0,
        pc::encoder_name(kind),
        fx_note,
        audio_note
    );
    // Hybrid model: the same doc is saved next to the video for re-editing.
    if let Some(doc) = annotate {
        let sidecar = qcapture_annotate::AnnotateDoc::sidecar_path_for(output);
        match doc.save(&sidecar) {
            Ok(()) => eprintln!("sidecar: {sidecar}"),
            Err(e) => eprintln!("warning: sidecar save failed: {e}"),
        }
    }
    Ok(())
}

fn run_record(args: RecordArgs) -> anyhow::Result<()> {
    if args.fps == 0 || args.fps > 240 {
        anyhow::bail!("--fps must be 1..240");
    }
    if args.bitrate < 500 || args.bitrate > 100_000 {
        anyhow::bail!("--bitrate must be 500..100000 kbps");
    }
    if args.region.is_some() && args.canvas.is_some() {
        anyhow::bail!(
            "--region and --canvas are mutually exclusive (region already fixes output size)"
        );
    }
    for (name, db) in [
        ("system-gain", args.system_gain),
        ("mic-gain", args.mic_gain),
    ] {
        if !(-60.0..=12.0).contains(&db) {
            anyhow::bail!("--{name} must be -60..+12 dB");
        }
    }

    // Encoder routing: auto/h264/hevc stay on the native MediaFoundation path
    // (Windows); vendor selectors go through the ffmpeg rawvideo pipe.
    // Off Windows there is no MF — everything resolves to ffmpeg.
    let enc_lower = args.encoder.to_lowercase();
    #[cfg(windows)]
    enum PathSel {
        Mf { hevc: bool },
        Ffmpeg(qcapture_core::EncoderKind),
    }
    #[cfg(windows)]
    let mut path = match enc_lower.as_str() {
        "auto" | "h264" => PathSel::Mf { hevc: false },
        "hevc" | "h265" => PathSel::Mf { hevc: true },
        other => PathSel::Ffmpeg(qcapture_encode::parse_encoder_kind(other)?),
    };

    // Rate control (ffmpeg path; MF is CBR-only and keeps using --bitrate).
    let rate = {
        use qcapture_core::RateControl as RC;
        match args.rc.to_lowercase().as_str() {
            "cbr" => RC::Cbr {
                bitrate_kbps: args.bitrate,
            },
            "vbr" => RC::Vbr {
                target_kbps: args.bitrate,
                max_kbps: args.maxrate.unwrap_or(args.bitrate * 3 / 2),
            },
            "cqp" => RC::Cqp {
                qp: args
                    .qp
                    .ok_or_else(|| anyhow::anyhow!("--rc cqp needs --qp N (0..51)"))?,
            },
            "crf" => RC::Crf {
                crf: args.crf.unwrap_or(23),
            },
            other => anyhow::bail!("unknown --rc '{other}' (want cbr|vbr|cqp|crf)"),
        }
    };
    // Widget parity: staged annotations AND live drawing need the ffmpeg byte
    // path. Auto-switch a native encoder rather than failing (NVENC
    // availability is probed inside the ffmpeg branch).
    #[cfg(windows)]
    {
        if matches!(path, PathSel::Mf { .. })
            && (args.annotate.is_some()
                || args.draw
                || args.draw_test.is_some()
                || args.cursor_highlight
                || args.cursor_ripple)
        {
            let info = qcapture_encode::probe_ffmpeg().map_err(|e| anyhow::anyhow!("{e}"))?;
            let kind = qcapture_encode::resolve_auto_encoder(&info);
            eprintln!(
                "auto-switched to {kind:?} for annotations/drawing/cursor-fx (MF is video-only)"
            );
            path = PathSel::Ffmpeg(kind);
        }
    }
    #[cfg(windows)]
    if matches!(path, PathSel::Mf { .. }) && !matches!(rate, qcapture_core::RateControl::Cbr { .. })
    {
        anyhow::bail!(
            "--rc modes need an ffmpeg encoder (--encoder nvenc|amf|qsv|x264); MF is CBR-only"
        );
    }

    let canvas = match &args.canvas {
        Some(c) => {
            let (w, h, fps_opt) = qcapture_encode::parse_canvas(c)?;
            if let Some(f) = fps_opt {
                if f != args.fps {
                    eprintln!(
                        "warning: --canvas fps {f} ignored, using --fps {} (single clock)",
                        args.fps
                    );
                }
            }
            Some((w, h))
        }
        None => None,
    };

    let output = match &args.output {
        Some(o) => qcapture_encode::output_with_extension(o, qcapture_core::Container::Mp4),
        None => default_output(),
    };
    let show_cursor = !args.no_cursor;
    let duration = args.duration.map(Duration::from_secs);

    // Ctrl-C flag shared with the capture thread (the capture backend owns
    // its thread; the handler polls this flag per frame).
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst));
    }

    #[cfg(not(windows))]
    {
        // No MediaFoundation off Windows: resolve everything to ffmpeg
        // (probe-backed, so missing HW fails loudly before threads start).
        let kind = resolve_unix_encoder(&enc_lower)?;
        if args.draw || args.draw_test.is_some() {
            return run_draw_record(
                &args,
                kind,
                rate,
                canvas,
                &output,
                show_cursor,
                duration,
                stop,
            );
        }
        return run_ffmpeg_record(
            &args,
            kind,
            rate,
            canvas,
            &output,
            show_cursor,
            duration,
            stop,
        );
    }

    // Phase 3 audio: system loopback on by default, mic opt-in. Fail-soft —
    // a broken audio device must never lose the video recording.
    // (MF path only here — the ffmpeg branch starts its own via start_cli_audio.)
    #[cfg(windows)]
    let audio = if !args.no_audio && !matches!(path, PathSel::Ffmpeg(_)) {
        start_cli_audio(&args)?
    } else {
        None
    };
    #[cfg(windows)]
    let audio_rx = audio.as_ref().map(|p| p.mixed_rx());

    #[cfg(windows)]
    {
        // FFmpeg HW path: byte frames -> rawvideo stdin -> NVENC / AMF / QSV /
        // x264, audio via named pipe (Phase 5b). Draw mode runs the eframe
        // panel on the main thread with capture on a worker (take-2).
        if let PathSel::Ffmpeg(kind) = path {
            if args.draw || args.draw_test.is_some() {
                return run_draw_record(
                    &args,
                    kind,
                    rate,
                    canvas,
                    &output,
                    show_cursor,
                    duration,
                    stop,
                );
            }
            return run_ffmpeg_record(
                &args,
                kind,
                rate,
                canvas,
                &output,
                show_cursor,
                duration,
                stop,
            );
        }
        let use_hevc = matches!(path, PathSel::Mf { hevc: true });
        let t0 = Instant::now();
        if let Some(title) = &args.window_title {
            eprintln!(
                "capturing window matching '{title}' @ {}fps → {output}…",
                args.fps
            );
            qcapture_capture::win_record::record_window_title(
                title,
                output.clone(),
                args.fps,
                args.bitrate,
                show_cursor,
                audio_rx,
                duration,
                stop,
            )?;
        } else if let Some(r) = &args.region {
            // Region is monitor-relative (origin at the target monitor's top-left).
            // `pick-region` prints monitor-relative geometry; virtual-screen to
            // monitor-relative mapping for multi-monitor lands with overlay 2b.
            let rect = parse_region(r)?;
            if rect.x < 0 || rect.y < 0 {
                anyhow::bail!("--region x,y must be >= 0 (monitor-relative in Phase 2a)");
            }
            let monitor_1based = qcapture_capture::wgc_monitor_index(args.region_screen);
            qcapture_capture::win_record::record_region(
                monitor_1based,
                rect.x as u32,
                rect.y as u32,
                rect.w,
                rect.h,
                output.clone(),
                args.fps,
                args.bitrate,
                show_cursor,
                audio_rx.clone(),
                duration,
                stop,
            )?;
        } else {
            // 0-based list position -> 1-based WGC index (both primary-first).
            let monitor_1based = qcapture_capture::wgc_monitor_index(args.screen);
            eprintln!(
                "capturing screen #{monitor_1based} @ {}fps → {output} (Ctrl-C to stop)…",
                args.fps
            );
            qcapture_capture::win_record::record_monitor(
                monitor_1based,
                output.clone(),
                args.fps,
                args.bitrate,
                canvas,
                show_cursor,
                use_hevc,
                audio_rx,
                duration,
                stop,
            )?;
        }
        let el = t0.elapsed();
        let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
        // Join audio threads first so the stats below are final.
        let audio_note = match audio {
            Some(p) => {
                let s = p.shutdown();
                format!(
                    "audio: AAC {} quanta ({} sys / {} mic underruns)",
                    s.quanta_emitted, s.sys_underruns, s.mic_underruns
                )
            }
            None => "video-only (--no-audio or unavailable)".into(),
        };
        println!(
            "Saved {output} — {:.1}s, {:.2} MB. {audio_note}.",
            el.as_secs_f64(),
            size as f64 / 1_000_000.0
        );
        Ok(())
    }
}
