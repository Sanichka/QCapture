//! Phase 2b: floating control widget.
//!
//! Compact always-on-top window: target picker (screen / window / region via
//! the `pick-region` overlay subprocess), Start/Stop, live audio controls
//! (mute + gain drive the mixer mid-recording through [`SharedLevels`]),
//! VU meter, and a separate Advanced window (fps, bitrate, canvas, encoder).
//!
//! Recording runs on a background thread (`windows-capture` owns that thread);
//! the UI only flips the stop flag and polls the done slot. Closing the widget
//! mid-recording stops the capture gracefully via [`Drop`].

use eframe::egui;
use qcapture_audio::win_audio::SharedLevels;
use qcapture_core::{DisplayInfo, Rect, WindowInfo};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

/// Advanced capture settings, shared with the detached Advanced window.
#[derive(Debug, Clone)]
pub struct AdvCfg {
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub canvas: Option<(u32, u32)>,
    pub encoder: EncoderSel,
    pub show_cursor: bool,
    pub rc: RateSel,
    pub qp: u8,
    pub crf: u8,
    pub maxrate_kbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateSel {
    Cbr,
    Vbr,
    Cqp,
    Crf,
}

impl RateSel {
    fn label(self) -> &'static str {
        match self {
            Self::Cbr => "CBR",
            Self::Vbr => "VBR",
            Self::Cqp => "CQP",
            Self::Crf => "CRF",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderSel {
    Auto,
    H264,
    Hevc,
    Nvenc,
    Amf,
    Qsv,
    X264,
}

impl EncoderSel {
    /// MediaFoundation path (with audio) vs ffmpeg pipe (video-only in 5a).
    fn is_ffmpeg(self) -> bool {
        matches!(self, Self::Nvenc | Self::Amf | Self::Qsv | Self::X264)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (H264)",
            Self::H264 => "H264",
            Self::Hevc => "HEVC",
            Self::Nvenc => "NVENC (ffmpeg)",
            Self::Amf => "AMF (ffmpeg)",
            Self::Qsv => "QSV (ffmpeg)",
            Self::X264 => "x264 (ffmpeg)",
        }
    }
}

impl Default for AdvCfg {
    fn default() -> Self {
        Self {
            fps: 30,
            bitrate_kbps: 8000,
            canvas: None,
            encoder: EncoderSel::Auto,
            show_cursor: true,
            rc: RateSel::Cbr,
            qp: 23,
            crf: 23,
            maxrate_kbps: 12000,
        }
    }
}

/// Build the ffmpeg [`RateControl`] from widget settings. MF ignores this
/// (CBR-only); non-CBR with an MF encoder is rejected at record time.
fn rate_from_adv(adv: &AdvCfg) -> Result<qcapture_core::RateControl, String> {
    use qcapture_core::RateControl as RC;
    Ok(match adv.rc {
        RateSel::Cbr => RC::Cbr {
            bitrate_kbps: adv.bitrate_kbps,
        },
        RateSel::Vbr => RC::Vbr {
            target_kbps: adv.bitrate_kbps,
            max_kbps: adv.maxrate_kbps,
        },
        RateSel::Cqp => RC::Cqp { qp: adv.qp },
        RateSel::Crf => RC::Crf { crf: adv.crf },
    })
}

#[derive(Debug, Clone)]
enum TargetSel {
    Screen(u32),
    Window(String),
    Region { screen: u32, rect: Rect },
}

struct ActiveRec {
    started: Instant,
    output: String,
    stop: Arc<AtomicBool>,
    done: Arc<Mutex<Option<RecDone>>>,
}

struct RecDone {
    message: String,
}

pub fn run() -> Result<(), String> {
    let screens = qcapture_capture::list_displays().unwrap_or_default();
    let windows = qcapture_capture::list_windows().unwrap_or_default();
    let mics: Vec<String> = qcapture_audio::list_input_devices()
        .unwrap_or_default()
        .into_iter()
        .map(|d| d.name)
        .collect();

    let app = WidgetApp {
        screens,
        windows,
        target: TargetSel::Screen(0),
        window_text: String::new(),
        region_screen: 0,
        mic_name: None,
        mic_options: mics,
        audio_on: true,
        levels: SharedLevels::new(qcapture_audio::MixerLevels::default()),
        adv: Arc::new(Mutex::new(AdvCfg::default())),
        adv_open: Arc::new(AtomicBool::new(false)),
        rec: None,
        last_msg: String::new(),
        vu: 0.0,
        pick: Arc::new(Mutex::new(PickState::default())),
        pending_doc: None,
        annotate: Arc::new(Mutex::new(AnnotateState::default())),
        draw_live: false,
        cursor_highlight: false,
        cursor_ripple: false,
        draw_ui: Arc::new(Mutex::new(None)),
        draw_open: Arc::new(AtomicBool::new(false)),
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("QCapture")
        .with_inner_size([360.0, 470.0])
        .with_min_inner_size([320.0, 400.0])
        .with_always_on_top()
        .with_active(true);

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "qcapture-widget",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| format!("widget failed: {e}"))?;
    Ok(())
}

#[derive(Default)]
struct PickState {
    pending: bool,
    result: Option<Option<(u32, Rect)>>, // screen + rect, or None = cancelled
}

/// Live drawing session (take-2): video-texture panel in a deferred viewport.
/// The record thread owns `live_rx` + `preview_tx`; the UI thread owns this
/// state (panel + other ends). Norm coords survive feed-size self-correction.
struct DrawUiState {
    feed_w: u32,
    feed_h: u32,
    events_tx: flume::Sender<qcapture_annotate::DrawEvent>,
    preview_rx: flume::Receiver<qcapture_capture::ffmpeg_cap::PreviewFrame>,
    panel: Option<super::draw_panel::DrawPanel>,
    /// Output path for sidecar saving when recording ends.
    output: String,
}

/// Capture-side ends of the draw channels, moved into the record thread.
struct DrawCaptureEnds {
    live_rx: flume::Receiver<qcapture_annotate::DrawEvent>,
    preview_tx: flume::Sender<qcapture_capture::ffmpeg_cap::PreviewFrame>,
}

struct WidgetApp {
    screens: Vec<DisplayInfo>,
    windows: Vec<WindowInfo>,
    target: TargetSel,
    window_text: String,
    region_screen: u32,
    mic_name: Option<String>,
    mic_options: Vec<String>,
    audio_on: bool,
    levels: Arc<SharedLevels>,
    adv: Arc<Mutex<AdvCfg>>,
    /// Advanced window visibility. Arc because the deferred-viewport close
    /// handler (`'static` callback) must clear it when the user hits [X].
    adv_open: Arc<AtomicBool>,
    rec: Option<ActiveRec>,
    last_msg: String,
    vu: f32,
    pick: Arc<Mutex<PickState>>,
    /// Doc staged by the Annotate flow; burned in on the next ffmpeg recording.
    pending_doc: Option<qcapture_annotate::AnnotateDoc>,
    annotate: Arc<Mutex<AnnotateState>>,
    /// Live drawing overlay during the next recording (ffmpeg path only).
    draw_live: bool,
    /// Cursor highlight ring + click ripple (ffmpeg path only, like drawing).
    cursor_highlight: bool,
    cursor_ripple: bool,
    draw_ui: Arc<Mutex<Option<DrawUiState>>>,
    /// Draw viewport visibility (closing it keeps recording; preview drops).
    draw_open: Arc<AtomicBool>,
}

#[derive(Default)]
struct AnnotateState {
    pending: bool,
    result: Option<Option<AnnotateResult>>,
}

struct AnnotateResult {
    screen: u32,
    rect: qcapture_core::Rect,
    doc: qcapture_annotate::AnnotateDoc,
}

impl Drop for WidgetApp {
    fn drop(&mut self) {
        // Closing the widget must not orphan a recording: the record thread
        // polls this flag and finalizes the file in the background.
        if let Some(r) = &self.rec {
            r.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn default_output() -> String {
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    format!("qcapture_{ts}.mp4")
}

impl WidgetApp {
    fn recording(&self) -> bool {
        self.rec.is_some()
    }

    fn poll_done(&mut self) {
        let finished = self
            .rec
            .as_ref()
            .and_then(|r| r.done.lock().ok().and_then(|mut g| g.take()));
        if let Some(done) = finished {
            self.last_msg = done.message;
            // Hybrid sidecar for live strokes (norm coords survive resize).
            let draw_doc = self
                .draw_ui
                .lock()
                .ok()
                .and_then(|mut g| g.take())
                .and_then(|mut s| s.panel.take().map(|p| (s.output.clone(), p.doc())));
            if let Some((output, doc)) = draw_doc {
                if !doc.strokes.is_empty() {
                    // Merge with any staged scripted doc for re-editing.
                    let mut merged =
                        self.pending_doc
                            .clone()
                            .unwrap_or(qcapture_annotate::AnnotateDoc {
                                version: 1,
                                canvas_w: doc.canvas_w,
                                canvas_h: doc.canvas_h,
                                strokes: Vec::new(),
                                watermarks: Vec::new(),
                                burn_in: true,
                            });
                    for s in doc.strokes {
                        let _ = merged.add_stroke(s);
                    }
                    let sidecar = qcapture_annotate::AnnotateDoc::sidecar_path_for(&output);
                    match merged.save(&sidecar) {
                        Ok(()) => {
                            self.last_msg.push_str(&format!(
                                " (+{} live strokes → {sidecar})",
                                merged.strokes.len()
                            ));
                        }
                        Err(e) => {
                            self.last_msg.push_str(&format!(" (sidecar failed: {e})"));
                        }
                    }
                }
            }
            self.draw_open.store(false, Ordering::Relaxed);
            self.rec = None;
        }
    }

    fn poll_pick(&mut self) {
        let take = self
            .pick
            .lock()
            .ok()
            .and_then(|mut p| {
                p.pending = false;
                p.result.take()
            })
            .flatten();
        if let Some((screen, rect)) = take {
            self.region_screen = screen;
            self.target = TargetSel::Region { screen, rect };
            self.last_msg = format!("region {},{},{},{}", rect.x, rect.y, rect.w, rect.h);
        }
    }

    fn poll_annotate(&mut self) {
        let take = self
            .annotate
            .lock()
            .ok()
            .and_then(|mut p| {
                p.pending = false;
                p.result.take()
            })
            .flatten();
        if let Some(res) = take {
            self.region_screen = res.screen;
            self.target = TargetSel::Region {
                screen: res.screen,
                rect: res.rect,
            };
            self.pending_doc = Some(res.doc);
            let r = res.rect;
            self.last_msg = format!(
                "annotated region {},{},{},{} ({} strokes)",
                r.x,
                r.y,
                r.w,
                r.h,
                self.pending_doc
                    .as_ref()
                    .map(|d| d.strokes.len())
                    .unwrap_or(0)
            );
        }
    }

    fn start_recording(&mut self) {
        if self.rec.is_some() {
            return;
        }
        // Live-draw feed size must resolve on the UI thread (it owns the
        // screen list). Window targets can't be tracked live yet.
        let draw_feed = if self.draw_live {
            match self.draw_feed_size() {
                Ok(wh) => Some(wh),
                Err(e) => {
                    self.last_msg = e;
                    return;
                }
            }
        } else {
            None
        };
        let adv = self.adv.lock().map(|g| g.clone()).unwrap_or_default();
        let output = default_output();
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(Mutex::new(None));
        let target = self.target.clone();
        let levels = self.levels.clone();
        let mic_name = if self.audio_on {
            self.mic_name.clone()
        } else {
            None
        };
        let audio_on = self.audio_on;
        let out_path = output.clone();
        let stop_t = stop.clone();
        let done_t = done.clone();
        let annotate = self.pending_doc.clone();
        let cursor_fx = qcapture_core::CursorFx::opt(self.cursor_highlight, self.cursor_ripple);
        // Draw channels: UI keeps tx+rx+panel, capture gets rx+tx.
        let draw_caps = if let Some((fw, fh)) = draw_feed {
            let (draw_tx, draw_rx) = flume::bounded::<qcapture_annotate::DrawEvent>(256);
            let (preview_tx, preview_rx) =
                flume::bounded::<qcapture_capture::ffmpeg_cap::PreviewFrame>(4);
            let state = DrawUiState {
                feed_w: (fw & !1).max(64),
                feed_h: (fh & !1).max(64),
                events_tx: draw_tx,
                preview_rx,
                panel: None,
                output: out_path.clone(),
            };
            if let Ok(mut g) = self.draw_ui.lock() {
                *g = Some(state);
            }
            self.draw_open.store(true, Ordering::Relaxed);
            Some(DrawCaptureEnds {
                live_rx: draw_rx,
                preview_tx,
            })
        } else {
            None
        };

        std::thread::Builder::new()
            .name("qcapture-widget-record".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    record_thread_body(
                        &target, &adv, &out_path, mic_name, audio_on, levels, annotate, draw_caps,
                        cursor_fx, stop_t,
                    )
                }));
                let msg = match result {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => format!("record failed: {e}"),
                    Err(_) => "record thread panicked".to_string(),
                };
                if let Ok(mut g) = done_t.lock() {
                    *g = Some(RecDone { message: msg });
                }
            })
            .ok();

        self.rec = Some(ActiveRec {
            started: Instant::now(),
            output,
            stop,
            done,
        });
        self.last_msg.clear();
    }

    fn stop_recording(&mut self) {
        if let Some(r) = &self.rec {
            r.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            self.last_msg = "finalizing…".to_string();
        }
    }

    /// Feed size guess for the live draw panel (preview self-corrects
    /// align/clamp and window-size drift; strokes are norm coords).
    /// Screen values follow the dropdown order (0 = primary).
    fn draw_feed_size(&self) -> Result<(u32, u32), String> {
        let screen_of = |idx: u32| {
            self.screens
                .get(idx as usize)
                .ok_or_else(|| format!("screen {idx} not in the display list — reopen the widget"))
        };
        match &self.target {
            TargetSel::Screen(s) => {
                let d = screen_of(*s)?;
                Ok((d.width, d.height))
            }
            TargetSel::Region { screen, rect } => {
                let _ = screen_of(*screen)?;
                Ok((rect.w, rect.h))
            }
            TargetSel::Window(title) => {
                // Window resize mid-record hits the fixed-canvas rule (frames
                // adapted center crop/pad, encoder never re-inits); panel
                // stays on the initial geometry. Prefer the WGC size (what
                // the encoder inits with) over the xcap list (client area
                // without borders — ~16px smaller each way).
                if let Ok((w, h)) = qcapture_capture::window_feed_size(title) {
                    Ok((w, h))
                } else if let Some(w) = self.windows.iter().find(|w| w.title == *title) {
                    Ok((w.width.max(64) & !1, w.height.max(64) & !1))
                } else {
                    Ok((1280, 720))
                }
            }
        }
    }

    fn launch_pick_region(&mut self, ctx: &egui::Context) {
        {
            let mut p = match self.pick.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if p.pending {
                return;
            }
            p.pending = true;
            p.result = None;
        }
        let screen = match &self.target {
            TargetSel::Screen(s) => *s as usize,
            TargetSel::Region { screen, .. } => *screen as usize,
            TargetSel::Window(_) => self.region_screen as usize,
        };
        let ctx2 = ctx.clone();
        let pick = self.pick.clone();
        // Run the overlay in a thread: it blocks, and the modal fullscreen
        // window covers us anyway. Minimize first so there's no always-on-top
        // fight (restored when the overlay exits).
        std::thread::spawn(move || {
            ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            // Small delay so the minimize lands before the overlay opens.
            std::thread::sleep(Duration::from_millis(250));
            let exe = std::env::current_exe().unwrap_or_else(|_| "qcapture".into());
            let out = std::process::Command::new(exe)
                .arg("pick-region")
                .arg("--screen")
                .arg(screen.to_string())
                .output();
            ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            let parsed = out.ok().and_then(|o| {
                if !o.status.success() {
                    return None;
                }
                let s = String::from_utf8_lossy(&o.stdout);
                parse_region_str(s.trim()).map(|r| (screen as u32, r))
            });
            if let Ok(mut p) = pick.lock() {
                p.result = Some(parsed);
            }
        });
    }

    /// Visual annotate flow: minimize, run `annotate` overlay subprocess,
    /// load the saved doc + region on return.
    fn launch_annotate(&mut self, ctx: &egui::Context) {
        {
            let mut p = match self.annotate.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if p.pending {
                return;
            }
            p.pending = true;
            p.result = None;
        }
        let screen = match &self.target {
            TargetSel::Screen(s) => *s as usize,
            TargetSel::Region { screen, .. } => *screen as usize,
            TargetSel::Window(_) => self.region_screen as usize,
        };
        let doc_path = std::env::temp_dir()
            .join("qcapture_widget_annotate.qcap.json")
            .to_string_lossy()
            .into_owned();
        let ctx2 = ctx.clone();
        let slot = self.annotate.clone();
        std::thread::spawn(move || {
            ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            std::thread::sleep(Duration::from_millis(250));
            let exe = std::env::current_exe().unwrap_or_else(|_| "qcapture".into());
            let out = std::process::Command::new(&exe)
                .arg("annotate")
                .arg("--screen")
                .arg(screen.to_string())
                .arg("--save")
                .arg(&doc_path)
                .output();
            ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            let parsed = out.ok().and_then(|o| {
                if !o.status.success() {
                    return None;
                }
                let s = String::from_utf8_lossy(&o.stdout);
                let rect = parse_region_str(s.trim())?;
                let doc = qcapture_annotate::AnnotateDoc::load(&doc_path).ok()?;
                Some(AnnotateResult {
                    screen: screen as u32,
                    rect,
                    doc,
                })
            });
            if let Ok(mut p) = slot.lock() {
                p.result = Some(parsed);
            }
        });
    }
}

fn parse_region_str(s: &str) -> Option<Rect> {
    let p: Vec<&str> = s.split(',').collect();
    if p.len() != 4 {
        return None;
    }
    let (x, y, w, h) = (
        p[0].trim().parse().ok()?,
        p[1].trim().parse().ok()?,
        p[2].trim().parse().ok()?,
        p[3].trim().parse().ok()?,
    );
    let r = Rect::new(x, y, w, h);
    (!r.is_empty()).then_some(r)
}

/// Runs on the record thread. Returns the human "Saved …" line.
#[allow(clippy::too_many_arguments)]
fn record_thread_body(
    target: &TargetSel,
    adv: &AdvCfg,
    output: &str,
    mic_name: Option<String>,
    audio_on: bool,
    levels: Arc<SharedLevels>,
    annotate: Option<qcapture_annotate::AnnotateDoc>,
    draw_caps: Option<DrawCaptureEnds>,
    cursor_fx: Option<qcapture_core::CursorFx>,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    use qcapture_capture::win_record as rec;

    // Staged annotations, live drawing AND cursor fx need the ffmpeg byte
    // path. Auto-switch a native encoder rather than failing: NVENC
    // availability is probed inside the ffmpeg branch.
    let mut adv_owned = adv.clone();
    let mut switched_note = String::new();
    if (annotate.is_some() || draw_caps.is_some() || cursor_fx.is_some())
        && !adv_owned.encoder.is_ffmpeg()
    {
        adv_owned.encoder = EncoderSel::Nvenc;
        switched_note = " (auto-switched to NVENC for annotations/drawing/cursor-fx)".to_string();
    }
    // MF is CBR-only; a staged non-CBR mode with an MF encoder is a loud error
    // (no silent fallback to a different quality contract).
    if !adv_owned.encoder.is_ffmpeg() && adv_owned.rc != RateSel::Cbr {
        return Err(
            "rate modes (VBR/CQP/CRF) need an ffmpeg encoder (NVENC/AMF/QSV/x264)".to_string(),
        );
    }
    let adv = &adv_owned;

    // FFmpeg HW path first (audio via named pipe since 5b).
    if adv.encoder.is_ffmpeg() {
        let kind = match adv.encoder {
            EncoderSel::Nvenc => qcapture_core::EncoderKind::H264Nvenc,
            EncoderSel::Amf => qcapture_core::EncoderKind::H264Amf,
            EncoderSel::Qsv => qcapture_core::EncoderKind::H264Qsv,
            EncoderSel::X264 => qcapture_core::EncoderKind::LibX264,
            _ => unreachable!("is_ffmpeg gate"),
        };
        return record_thread_ffmpeg(
            target, adv, output, kind, mic_name, audio_on, levels, annotate, draw_caps, cursor_fx,
            stop,
        )
        .map(|m| format!("{m}{switched_note}"));
    }
    if annotate.is_some() || draw_caps.is_some() || cursor_fx.is_some() {
        return Err(
            "annotations / live drawing / cursor-fx need an ffmpeg encoder (NVENC/AMF/QSV/x264)"
                .to_string(),
        );
    }

    let audio = if audio_on {
        let cfg = qcapture_audio::win_audio::WinAudioConfig {
            capture_system: true,
            mic_query: mic_name.clone(),
            levels: levels.clone(),
        };
        match qcapture_audio::win_audio::start_pipeline(cfg) {
            Ok(p) => Some(p),
            Err(e) => {
                if mic_name.is_some() {
                    return Err(format!("audio pipeline: {e}"));
                }
                None // system-only failure is soft; video continues
            }
        }
    } else {
        None
    };
    let audio_rx = audio.as_ref().map(|p| p.mixed_rx());
    let t0 = Instant::now();

    let use_hevc = adv.encoder == EncoderSel::Hevc;
    let res = match target {
        TargetSel::Screen(s) => {
            let idx = qcapture_capture::wgc_monitor_index(*s);
            rec::record_monitor(
                idx,
                output.to_string(),
                adv.fps,
                adv.bitrate_kbps,
                adv.canvas,
                adv.show_cursor,
                use_hevc,
                audio_rx,
                None,
                stop,
            )
        }
        TargetSel::Window(title) => rec::record_window_title(
            title,
            output.to_string(),
            adv.fps,
            adv.bitrate_kbps,
            adv.show_cursor,
            audio_rx,
            None,
            stop,
        ),
        TargetSel::Region { screen, rect } => {
            let idx = qcapture_capture::wgc_monitor_index(*screen);
            rec::record_region(
                idx,
                rect.x.max(0) as u32,
                rect.y.max(0) as u32,
                rect.w,
                rect.h,
                output.to_string(),
                adv.fps,
                adv.bitrate_kbps,
                adv.show_cursor,
                audio_rx,
                None,
                stop,
            )
        }
    };
    if let Err(e) = res {
        if let Some(p) = audio {
            let _ = p.shutdown();
        }
        return Err(e.to_string());
    }

    let el = t0.elapsed();
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    let audio_note = match audio {
        Some(p) => {
            let s = p.shutdown();
            format!(
                " + AAC {} quanta ({} sys/{} mic underruns)",
                s.quanta_emitted, s.sys_underruns, s.mic_underruns
            )
        }
        None => String::new(),
    };
    Ok(format!(
        "Saved {output} — {:.1}s, {:.2} MB{audio_note}",
        el.as_secs_f64(),
        size as f64 / 1_000_000.0
    ))
}

/// FFmpeg HW branch of [`record_thread_body`]: probe, availability check,
/// byte-frame capture into the rawvideo pipe, AAC through the named pipe.
#[allow(clippy::too_many_arguments)]
fn record_thread_ffmpeg(
    target: &TargetSel,
    adv: &AdvCfg,
    output: &str,
    kind: qcapture_core::EncoderKind,
    mic_name: Option<String>,
    audio_on: bool,
    levels: Arc<SharedLevels>,
    annotate: Option<qcapture_annotate::AnnotateDoc>,
    draw_caps: Option<DrawCaptureEnds>,
    cursor_fx: Option<qcapture_core::CursorFx>,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    use qcapture_capture::ffmpeg_cap as fc;
    use qcapture_encode::audio_pipe as ap;
    let info = qcapture_encode::probe_ffmpeg().map_err(|e| e.to_string())?;
    if !qcapture_encode::supports(kind, &info) {
        return Err(format!(
            "this ffmpeg has no {} — pick another encoder",
            fc::encoder_name(kind)
        ));
    }
    // Same dry-run as CLI: fail before threads start.
    qcapture_encode::rate_control_args(
        fc::encoder_name(kind),
        &rate_from_adv(adv).map_err(|e| e.to_string())?,
        adv.fps,
    )
    .map_err(|e| e.to_string())?;
    // Same pipeline as the MF path; chunks forward into the ffmpeg pipe.
    // Fail-soft like CLI: system-only trouble -> video-only, mic typo -> loud.
    let audio = if audio_on {
        let cfg = qcapture_audio::win_audio::WinAudioConfig {
            capture_system: true,
            mic_query: mic_name.clone(),
            levels: levels.clone(),
        };
        match qcapture_audio::win_audio::start_pipeline(cfg) {
            Ok(p) => Some(p),
            Err(e) => {
                if mic_name.is_some() {
                    return Err(format!("audio pipeline: {e}"));
                }
                None
            }
        }
    } else {
        None
    };
    let pipe = if audio.is_some() {
        Some(ap::FfmpegAudioPipe::create().map_err(|e| e.to_string())?)
    } else {
        None
    };
    if let (Some(a), Some(p)) = (audio.as_ref(), pipe.as_ref()) {
        ap::forward_to_pipe(a.mixed_rx(), p.sender());
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
    let t0 = Instant::now();
    let rate = rate_from_adv(adv).map_err(|e| e.to_string())?;
    // Take-2 drawing: in-process channels to the embedded draw viewport
    // (same UI thread, same event loop) — no child process, no overlay.
    let (mut draw_rx, preview_tx) = match draw_caps {
        Some(c) => (Some(c.live_rx), Some(c.preview_tx)),
        None => (None, None),
    };
    // Window HWND for cursor mapping (fail fast on a stale title).
    let cursor_hwnd = match target {
        TargetSel::Window(title) if cursor_fx.is_some() => Some(
            qcapture_capture::resolve_window(title)
                .map(|r| r.hwnd)
                .map_err(|e| e.to_string())?,
        ),
        _ => None,
    };
    let job = |canvas: Option<(u32, u32)>| fc::FfmpegJob {
        fps: adv.fps,
        encoder: kind,
        rate,
        canvas,
        show_cursor: adv.show_cursor,
        output: output.to_string(),
        duration: None, // widget stops via the flag (Stop button / close)
        stop_flag: stop.clone(),
        annotate: annotate.clone(),
        cursor_fx,
        cursor_window: cursor_hwnd,
        audio_pipe: audio_pipe_name.clone(),
        preview_tx: preview_tx.clone(),
    };
    let stats = match target {
        TargetSel::Screen(s) => {
            let idx = qcapture_capture::wgc_monitor_index(*s);
            fc::run_ffmpeg_monitor(idx, job(adv.canvas), on_end, draw_rx.take())
        }
        TargetSel::Window(title) => fc::run_ffmpeg_window(title, job(None), on_end, draw_rx.take()),
        TargetSel::Region { screen, rect } => {
            let idx = qcapture_capture::wgc_monitor_index(*screen);
            fc::run_ffmpeg_region(
                idx,
                rect.x.max(0) as u32,
                rect.y.max(0) as u32,
                rect.w,
                rect.h,
                job(None),
                on_end,
                draw_rx.take(),
            )
        }
    }
    .map_err(|e| e.to_string())?;
    if let Some(p) = pipe {
        if let Err(e) = p.finish() {
            eprintln!("warning: audio pipe: {e}");
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
    Ok(format!(
        "Saved {output} — {} frames in {:.1}s, {:.2} MB via {}{}",
        stats.written,
        el.as_secs_f64(),
        size as f64 / 1_000_000.0,
        fc::encoder_name(kind),
        audio_note
    ))
}

impl eframe::App for WidgetApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_done();
        self.poll_pick();
        self.poll_annotate();

        if self.recording() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // VU decay.
        let peak = self.levels.take_peak();
        self.vu = peak.max(self.vu * 0.92);

        // Detached Advanced window (Phase 5 will grow it: HW encoder matrix).
        // The [X] close is honored via adv_open so the window stays shut.
        if self.adv_open.load(Ordering::Relaxed) {
            let adv = self.adv.clone();
            let open = self.adv_open.clone();
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of("qcapture-advanced"),
                egui::ViewportBuilder::default()
                    .with_title("QCapture — Advanced")
                    .with_inner_size([300.0, 400.0])
                    .with_always_on_top(),
                move |ctx, _| {
                    if let Ok(mut a) = adv.lock() {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            ui.heading("Advanced");
                            ui.add(egui::Slider::new(&mut a.fps, 15..=120).text("fps"));
                            ui.add(
                                egui::Slider::new(&mut a.bitrate_kbps, 1000..=50000)
                                    .text("bitrate kbps (CBR/VBR target)"),
                            );
                            egui::ComboBox::from_label("Rate control")
                                .selected_text(a.rc.label())
                                .show_ui(ui, |ui| {
                                    for r in
                                        [RateSel::Cbr, RateSel::Vbr, RateSel::Cqp, RateSel::Crf]
                                    {
                                        ui.selectable_value(&mut a.rc, r, r.label());
                                    }
                                });
                            match a.rc {
                                RateSel::Cbr => {}
                                RateSel::Vbr => {
                                    ui.add(
                                        egui::Slider::new(&mut a.maxrate_kbps, 1000..=80000)
                                            .text("maxrate kbps"),
                                    );
                                }
                                RateSel::Cqp => {
                                    ui.add(
                                        egui::Slider::new(&mut a.qp, 0..=51)
                                            .text("QP (lower=better)"),
                                    );
                                    ui.label("CQP: NVENC/AMF/QSV (x264 uses CRF).");
                                }
                                RateSel::Crf => {
                                    ui.add(
                                        egui::Slider::new(&mut a.crf, 0..=51)
                                            .text("CRF (lower=better)"),
                                    );
                                    ui.label("CRF: x264 only.");
                                }
                            }
                            ui.label("VBR/CQP/CRF need an ffmpeg encoder; MF is CBR-only.");
                            egui::ComboBox::from_label("Canvas")
                                .selected_text(match a.canvas {
                                    None => "Native".to_string(),
                                    Some((w, h)) => format!("{w}x{h}"),
                                })
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut a.canvas, None, "Native");
                                    ui.selectable_value(
                                        &mut a.canvas,
                                        Some((1280, 720)),
                                        "1280x720",
                                    );
                                    ui.selectable_value(
                                        &mut a.canvas,
                                        Some((1920, 1080)),
                                        "1920x1080",
                                    );
                                });
                            egui::ComboBox::from_label("Encoder")
                                .selected_text(a.encoder.label())
                                .show_ui(ui, |ui| {
                                    for e in [
                                        EncoderSel::Auto,
                                        EncoderSel::H264,
                                        EncoderSel::Hevc,
                                        EncoderSel::Nvenc,
                                        EncoderSel::Amf,
                                        EncoderSel::Qsv,
                                        EncoderSel::X264,
                                    ] {
                                        ui.selectable_value(&mut a.encoder, e, e.label());
                                    }
                                });
                            ui.checkbox(&mut a.show_cursor, "Capture cursor");
                            ui.label("ffmpeg entries mix AAC via named pipe.");
                        });
                    }
                    if ctx.input(|i| i.viewport().close_requested()) {
                        open.store(false, Ordering::Relaxed);
                    }
                },
            );
        }

        // Embedded draw viewport (take-2): live video texture + tools, same
        // event loop as the widget. Closing it keeps recording (preview
        // drops on backpressure); Stop ends capture + saves the sidecar.
        if self.draw_open.load(Ordering::Relaxed) {
            let draw_ui = self.draw_ui.clone();
            let draw_open = self.draw_open.clone();
            // Initial size from the feed guess; the panel aspect-fits inside.
            let (init_w, init_h) = self
                .draw_ui
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|s| (s.feed_w, s.feed_h)))
                .unwrap_or((960, 600));
            let s = (960.0 / init_w.max(64) as f32)
                .min(600.0 / init_h.max(64) as f32)
                .clamp(0.15, 1.0);
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of("qcapture-draw"),
                egui::ViewportBuilder::default()
                    .with_title("QCapture — draw live (close keeps recording)")
                    .with_inner_size([init_w as f32 * s, init_h as f32 * s]),
                move |ctx, _| {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                    let mut guard = match draw_ui.lock() {
                        Ok(g) => g,
                        Err(_) => return,
                    };
                    let Some(state) = guard.as_mut() else {
                        return;
                    };
                    if state.panel.is_none() {
                        state.panel = Some(super::draw_panel::DrawPanel::new(
                            state.feed_w,
                            state.feed_h,
                            state.events_tx.clone(),
                        ));
                    }
                    let panel = match state.panel.as_mut() {
                        Some(p) => p,
                        None => return,
                    };
                    panel.drain_previews(ctx, &state.preview_rx);
                    egui::CentralPanel::default()
                        .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                        .show(ctx, |ui| {
                            panel.show(ctx, ui);
                        });
                    if ctx.input(|i| i.viewport().close_requested()) {
                        draw_open.store(false, Ordering::Relaxed);
                    }
                },
            );
        }

        let rec = self.recording();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("QCapture");
            ui.separator();

            // ---- target ----
            ui.label("Target:");
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!rec, |ui| {
                    if ui
                        .selectable_label(matches!(self.target, TargetSel::Screen(_)), "Screen")
                        .clicked()
                    {
                        self.target = TargetSel::Screen(0);
                    }
                    if ui
                        .selectable_label(matches!(self.target, TargetSel::Window(_)), "Window")
                        .clicked()
                    {
                        self.target = TargetSel::Window(self.window_text.clone());
                    }
                    let _ = ui.selectable_label(
                        matches!(self.target, TargetSel::Region { .. }),
                        "Region",
                    );
                });
            });

            // Region pick is launched after the match (it needs &mut self).
            let mut want_pick: Option<u32> = None;
            ui.add_enabled_ui(!rec, |ui| match &mut self.target {
                TargetSel::Screen(s) => {
                    let labels: Vec<String> = self
                        .screens
                        .iter()
                        .enumerate()
                        .map(|(i, d)| {
                            let tag = if d.is_primary { "Primary" } else { "Monitor" };
                            format!("{tag} #{i} — {} {}x{}", d.name, d.width, d.height)
                        })
                        .collect();
                    let mut idx = (*s as usize).min(labels.len().saturating_sub(1));
                    egui::ComboBox::from_id_salt("screen-pick")
                        .selected_text(labels.get(idx).cloned().unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for (i, l) in labels.iter().enumerate() {
                                ui.selectable_value(&mut idx, i, l);
                            }
                        });
                    *s = idx as u32;
                }
                TargetSel::Window(title) => {
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.window_text)
                            .on_hover_text("Substring of the window title");
                        if ui
                            .button("⟳")
                            .on_hover_text("Refresh window list")
                            .clicked()
                        {
                            self.windows = qcapture_capture::list_windows().unwrap_or_default();
                        }
                    });
                    egui::ComboBox::from_id_salt("window-pick")
                        .selected_text(if title.is_empty() {
                            "…"
                        } else {
                            title.as_str()
                        })
                        .show_ui(ui, |ui| {
                            for w in &self.windows {
                                let label = format!(
                                    "{} — {}",
                                    w.app_name,
                                    w.title.chars().take(48).collect::<String>()
                                );
                                if ui.selectable_label(w.title == *title, label).clicked() {
                                    *title = w.title.clone();
                                    self.window_text = w.title.clone();
                                }
                            }
                        });
                    *title = self.window_text.clone();
                }
                TargetSel::Region { screen, rect } => {
                    ui.label(format!(
                        "screen {screen}: {},{} {}x{}",
                        rect.x, rect.y, rect.w, rect.h
                    ));
                    if ui.button("⌖ Select region…").clicked() {
                        want_pick = Some(*screen);
                    }
                }
            });
            if let Some(s) = want_pick {
                self.region_screen = s;
                self.launch_pick_region(ctx);
            }
            if matches!(self.target, TargetSel::Screen(_))
                && !rec
                && ui
                    .small_button("⌖ …or pick a region on this screen")
                    .clicked()
            {
                if let TargetSel::Screen(s) = self.target {
                    self.region_screen = s;
                }
                self.launch_pick_region(ctx);
            }
            // Pick-region progress (set by the background thread).
            if self.pick.lock().map(|p| p.pending).unwrap_or(false) {
                ui.label("selecting region… (overlay open)");
            }

            ui.separator();

            // ---- audio ----
            ui.horizontal(|ui| {
                ui.label("Audio:");
                let mut on = self.audio_on;
                ui.add_enabled_ui(!rec, |ui| {
                    ui.checkbox(&mut on, "system");
                });
                self.audio_on = on;
            });
            ui.add_enabled_ui(!rec, |ui| {
                // System row.
                ui.horizontal(|ui| {
                    let mut m = self.levels.muted(false);
                    if ui
                        .selectable_label(m, if m { "🔇" } else { "🔊" })
                        .on_hover_text("Mute system audio")
                        .clicked()
                    {
                        m = !m;
                        self.levels.set_muted(false, m);
                    }
                    let mut db = self.levels.gain_db(false);
                    ui.label("Sys");
                    if ui
                        .add(egui::Slider::new(&mut db, -60.0..=12.0).show_value(false))
                        .changed()
                    {
                        self.levels.set_gain_db(false, db);
                    }
                });
                // Mic row.
                ui.horizontal(|ui| {
                    let mut m = self.levels.muted(true);
                    if ui
                        .selectable_label(m, if m { "🎙🚫" } else { "🎙" })
                        .on_hover_text("Mute mic")
                        .clicked()
                    {
                        m = !m;
                        self.levels.set_muted(true, m);
                    }
                    let mut sel = self.mic_name.clone().unwrap_or_default();
                    egui::ComboBox::from_id_salt("mic-pick")
                        .selected_text(if sel.is_empty() { "mic off" } else { &sel })
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(sel.is_empty(), "mic off").clicked() {
                                sel.clear();
                            }
                            for name in &self.mic_options {
                                if ui.selectable_label(sel == *name, name).clicked() {
                                    sel = name.clone();
                                }
                            }
                        });
                    self.mic_name = if sel.is_empty() { None } else { Some(sel) };
                    let mut db = self.levels.gain_db(true);
                    if ui
                        .add(
                            egui::Slider::new(&mut db, -60.0..=12.0)
                                .show_value(false)
                                .text("Mic"),
                        )
                        .changed()
                    {
                        self.levels.set_gain_db(true, db);
                    }
                });
            });
            // VU meter (mixed output peak, drained per frame with decay).
            ui.horizontal(|ui| {
                ui.label("Mix");
                let bar = egui::ProgressBar::new(self.vu).desired_width(140.0);
                ui.add(bar);
                ui.label(format!("{:.0} dB", 20.0 * self.vu.max(1e-3).log10()));
            });

            ui.separator();

            // ---- record ----
            if !rec {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.draw_live, "✏ Draw live")
                        .on_hover_text(
                            "Live video + pen/shapes/text in a second window (ffmpeg encoders; closing it keeps recording)",
                        );
                });
                ui.horizontal(|ui| {
                    ui.label("Cursor fx:");
                    ui.checkbox(&mut self.cursor_highlight, "highlight").on_hover_text(
                        "Yellow ring around the cursor, burned into the video (ffmpeg encoders)",
                    );
                    ui.checkbox(&mut self.cursor_ripple, "clicks").on_hover_text(
                        "White ripple on mouse clicks, burned into the video (ffmpeg encoders)",
                    );
                });
                if ui
                    .add_sized([340.0, 36.0], egui::Button::new("●  Record"))
                    .clicked()
                {
                    // Validate window target early for a loud error instead of a
                    // background failure.
                    if let TargetSel::Window(t) = &self.target {
                        if t.trim().is_empty() {
                            self.last_msg =
                                "pick a window first (substring of its title)".to_string();
                        } else {
                            self.start_recording();
                        }
                    } else {
                        self.start_recording();
                    }
                }
            } else if ui
                .add_sized([340.0, 36.0], egui::Button::new("■  Stop"))
                .clicked()
            {
                self.stop_recording();
            }

            if let Some(r) = &self.rec {
                let el = r.started.elapsed();
                let size = std::fs::metadata(&r.output).map(|m| m.len()).unwrap_or(0);
                ui.label(format!(
                    "⏺ {:.0}s — {:.2} MB\n{}",
                    el.as_secs_f64(),
                    size as f64 / 1_000_000.0,
                    r.output
                ));
                if self.last_msg == "finalizing…" {
                    ui.spinner();
                }
            }
            if !self.last_msg.is_empty() && self.rec.is_none() {
                ui.label(&self.last_msg);
                if self.last_msg.starts_with("Saved ") && ui.small_button("Open folder").clicked() {
                    if let Some(path) = self.last_saved_path() {
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{path}"))
                            .spawn();
                    }
                }
            }

            ui.separator();
            ui.horizontal(|ui| {
                let open = self.adv_open.load(Ordering::Relaxed);
                if ui
                    .small_button(if open {
                        "⚙ Close advanced"
                    } else {
                        "⚙ Advanced"
                    })
                    .clicked()
                {
                    self.adv_open.store(!open, Ordering::Relaxed);
                }
                if ui.small_button("Pick region…").clicked() {
                    self.launch_pick_region(ctx);
                }
                if ui
                    .small_button("Annotate…")
                    .on_hover_text("Select a region, draw on it, record with burn-in")
                    .clicked()
                {
                    self.launch_annotate(ctx);
                }
            });
            if self.annotate.lock().map(|p| p.pending).unwrap_or(false) {
                ui.label("annotating… (editor open)");
            }
            if self.pending_doc.is_some() && self.rec.is_none() {
                ui.label(format!(
                    "staged annotations ({} strokes) — burns in on next record (ffmpeg encoder)",
                    self.pending_doc
                        .as_ref()
                        .map(|d| d.strokes.len())
                        .unwrap_or(0)
                ));
            }
        });
    }
}

impl WidgetApp {
    fn last_saved_path(&self) -> Option<String> {
        self.last_msg
            .strip_prefix("Saved ")
            .and_then(|s| s.split(" — ").next())
            .map(|s| s.to_string())
    }
}
