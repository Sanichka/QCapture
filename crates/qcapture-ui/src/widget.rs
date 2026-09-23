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
use qcapture_audio::SharedLevels;
use qcapture_core::{DisplayInfo, Rect, WindowInfo};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

/// Advanced capture settings, shared with the detached Advanced window.
fn default_hl_rgba() -> [u8; 4] {
    [255, 210, 0, 255]
}

fn default_ripple_rgba() -> [u8; 4] {
    [255, 255, 255, 255]
}

/// Accept settings files written before alpha existed ([u8;3] → opaque).
fn de_rgba<'de, D>(d: D) -> Result<[u8; 4], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Vec<u8> = serde::Deserialize::deserialize(d)?;
    match v.as_slice() {
        [r, g, b] => Ok([*r, *g, *b, 255]),
        [r, g, b, a] => Ok([*r, *g, *b, *a]),
        _ => Err(serde::de::Error::custom(
            "color must be [r,g,b] or [r,g,b,a]",
        )),
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Cursor fx style (serde defaults keep pre-style settings files loading;
    /// `de_rgba` additionally accepts the pre-alpha [r,g,b] arrays).
    #[serde(default = "default_hl_rgba", deserialize_with = "de_rgba")]
    pub cursor_color: [u8; 4],
    #[serde(default = "default_cursor_size")]
    pub cursor_size: f32,
    #[serde(default = "default_ripple_rgba", deserialize_with = "de_rgba")]
    pub ripple_color: [u8; 4],
    #[serde(default = "default_ripple_size")]
    pub ripple_size: f32,
    #[serde(default = "default_ripple_ms")]
    pub ripple_ms: u32,
    /// Additive glow instead of normal blending (egui's picker "Additive"
    /// radio can't survive u8 storage, so glow is an explicit flag).
    #[serde(default)]
    pub additive: bool,
}

fn default_cursor_size() -> f32 {
    14.0
}

fn default_ripple_size() -> f32 {
    42.0
}

fn default_ripple_ms() -> u32 {
    600
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// Every option the Advanced dropdown offers (UI renders from this so
    /// tests and UI can't drift apart).
    pub const fn all() -> [Self; 4] {
        [Self::Cbr, Self::Vbr, Self::Cqp, Self::Crf]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// Every option the Advanced dropdown offers (UI renders from this).
    pub const fn all() -> [Self; 7] {
        [
            Self::Auto,
            Self::H264,
            Self::Hevc,
            Self::Nvenc,
            Self::Amf,
            Self::Qsv,
            Self::X264,
        ]
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
            cursor_color: default_hl_rgba(),
            cursor_size: default_cursor_size(),
            ripple_color: default_ripple_rgba(),
            ripple_size: default_ripple_size(),
            ripple_ms: default_ripple_ms(),
            additive: false,
        }
    }
}

/// Canvas choices the Advanced dropdown offers (None = native feed size).
pub const CANVAS_OPTIONS: [Option<(u32, u32)>; 3] = [None, Some((1280, 720)), Some((1920, 1080))];

impl AdvCfg {
    /// Slider ranges, shared by the UI and the tests below.
    pub const FPS_RANGE: std::ops::RangeInclusive<u32> = 15..=120;
    pub const BITRATE_RANGE: std::ops::RangeInclusive<u32> = 1000..=50000;
    pub const MAXRATE_RANGE: std::ops::RangeInclusive<u32> = 1000..=80000;
    pub const QP_RANGE: std::ops::RangeInclusive<u8> = 0..=51;
    pub const GAIN_DB_RANGE: std::ops::RangeInclusive<f32> = -60.0..=12.0;
    pub const CURSOR_RADIUS_RANGE: std::ops::RangeInclusive<f32> = 6.0..=40.0;
    pub const RIPPLE_RADIUS_RANGE: std::ops::RangeInclusive<f32> = 12.0..=120.0;
    pub const RIPPLE_MS_RANGE: std::ops::RangeInclusive<u32> = 100..=3000;

    /// Clamp every numeric field into its slider range. Applied to settings
    /// loaded from disk (hand-edited files can hold anything); the live
    /// sliders can never leave range on their own.
    pub fn sanitize(&mut self) {
        self.fps = self
            .fps
            .clamp(*Self::FPS_RANGE.start(), *Self::FPS_RANGE.end());
        self.bitrate_kbps = self
            .bitrate_kbps
            .clamp(*Self::BITRATE_RANGE.start(), *Self::BITRATE_RANGE.end());
        self.maxrate_kbps = self
            .maxrate_kbps
            .clamp(*Self::MAXRATE_RANGE.start(), *Self::MAXRATE_RANGE.end());
        self.qp = self
            .qp
            .clamp(*Self::QP_RANGE.start(), *Self::QP_RANGE.end());
        self.crf = self
            .crf
            .clamp(*Self::QP_RANGE.start(), *Self::QP_RANGE.end());
        self.cursor_size = self.cursor_size.clamp(
            *Self::CURSOR_RADIUS_RANGE.start(),
            *Self::CURSOR_RADIUS_RANGE.end(),
        );
        self.ripple_size = self.ripple_size.clamp(
            *Self::RIPPLE_RADIUS_RANGE.start(),
            *Self::RIPPLE_RADIUS_RANGE.end(),
        );
        self.ripple_ms = self
            .ripple_ms
            .clamp(*Self::RIPPLE_MS_RANGE.start(), *Self::RIPPLE_MS_RANGE.end());
        if let Some((w, h)) = self.canvas {
            if w < 64 || h < 64 {
                self.canvas = None;
            }
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

/// Widget capture-target selection (also persisted across restarts).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum TargetSel {
    Screen(u32),
    Window(String),
    Region { screen: u32, rect: Rect },
}

struct ActiveRec {
    started: Instant,
    output: String,
    stop: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    done: Arc<Mutex<Option<RecDone>>>,
    /// Wall time spent paused so far + ongoing pause start. The readout
    /// shows content time (what lands in the file), not session time.
    paused_total: Duration,
    pause_began: Option<Instant>,
}

impl ActiveRec {
    /// Content clock: session time minus paused spans (including an ongoing
    /// pause, so the readout freezes while paused).
    fn content_elapsed(&self) -> Duration {
        let paused = self.paused_total + self.pause_began.map(|b| b.elapsed()).unwrap_or_default();
        self.started.elapsed().saturating_sub(paused)
    }

    fn set_paused(&mut self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
        if paused {
            self.pause_began = Some(Instant::now());
        } else if let Some(began) = self.pause_began.take() {
            self.paused_total += began.elapsed();
        }
    }
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

    // Restore last session (target, encoder, audio, toggles, folder).
    // Missing/corrupt file = fresh defaults; out-of-range screens clamp.
    let saved = super::settings::load();
    let n_screens = screens.len();
    let clamp_screen = |s: u32| {
        if (s as usize) < n_screens.max(1) {
            s
        } else {
            0
        }
    };
    let target = saved
        .as_ref()
        .and_then(|s| s.target.clone())
        .map(|t| match t {
            TargetSel::Screen(s) => TargetSel::Screen(clamp_screen(s)),
            TargetSel::Region { screen, rect } => TargetSel::Region {
                screen: clamp_screen(screen),
                rect,
            },
            w @ TargetSel::Window(_) => w,
        })
        .unwrap_or(TargetSel::Screen(0));
    let region_screen = clamp_screen(saved.as_ref().map(|s| s.region_screen).unwrap_or(0));
    let levels = SharedLevels::new(qcapture_audio::MixerLevels {
        system_gain: qcapture_audio::MixerLevels::db_to_linear(
            saved.as_ref().map(|s| s.sys_gain_db).unwrap_or(0.0),
        ),
        mic_gain: qcapture_audio::MixerLevels::db_to_linear(
            saved.as_ref().map(|s| s.mic_gain_db).unwrap_or(0.0),
        ),
        system_muted: saved.as_ref().map(|s| s.sys_muted).unwrap_or(false),
        mic_muted: saved.as_ref().map(|s| s.mic_muted).unwrap_or(false),
    });
    // Hand-edited settings can hold anything — clamp back into slider range.
    let mut adv = saved
        .as_ref()
        .and_then(|s| s.adv.clone())
        .unwrap_or_default();
    adv.sanitize();

    let app = WidgetApp {
        screens,
        windows,
        target,
        window_text: saved
            .as_ref()
            .map(|s| s.window_text.clone())
            .unwrap_or_default(),
        region_screen,
        mic_name: saved.as_ref().and_then(|s| s.mic_name.clone()),
        mic_options: mics,
        audio_on: saved.as_ref().map(|s| s.audio_on).unwrap_or(true),
        levels,
        adv: Arc::new(Mutex::new(adv)),
        adv_open: Arc::new(AtomicBool::new(false)),
        rec: None,
        last_msg: String::new(),
        vu: 0.0,
        pick: Arc::new(Mutex::new(PickState::default())),
        pending_doc: None,
        annotate: Arc::new(Mutex::new(AnnotateState::default())),
        draw_live: saved.as_ref().map(|s| s.draw_live).unwrap_or(false),
        cursor_highlight: saved.as_ref().map(|s| s.cursor_highlight).unwrap_or(false),
        cursor_ripple: saved.as_ref().map(|s| s.cursor_ripple).unwrap_or(false),
        output_dir: saved
            .as_ref()
            .map(|s| s.output_dir.clone())
            .unwrap_or_default(),
        countdown_enabled: saved.as_ref().map(|s| s.countdown_enabled).unwrap_or(false),
        countdown_until: None,
        draw_ui: Arc::new(Mutex::new(None)),
        draw_open: Arc::new(AtomicBool::new(false)),
    };

    let viewport = egui::ViewportBuilder::default()
        .with_title("QCapture")
        .with_icon(super::app_icon())
        .with_inner_size([430.0, 480.0])
        .with_min_inner_size([360.0, 420.0])
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
    preview_rx: flume::Receiver<qcapture_capture::pump::PreviewFrame>,
    panel: Option<super::draw_panel::DrawPanel>,
    /// Output path for sidecar saving when recording ends.
    output: String,
}

/// Capture-side ends of the draw channels, moved into the record thread.
struct DrawCaptureEnds {
    live_rx: flume::Receiver<qcapture_annotate::DrawEvent>,
    preview_tx: flume::Sender<qcapture_capture::pump::PreviewFrame>,
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
    /// Output folder (empty = current folder). Persisted; editable below.
    output_dir: String,
    /// 3-second countdown before recording. Persisted toggle.
    countdown_enabled: bool,
    /// Countdown deadline once the user hits Record (None = idle).
    /// Display-only overlay; cancelling happens in the main window.
    countdown_until: Option<Instant>,
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
        self.save_settings();
    }
}

/// Timestamped filename, optionally inside `dir` (empty = current folder).
/// Creates the folder so a typo fails fast at record time, not mid-encode.
fn default_output_in(dir: &str) -> Result<String, String> {
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let name = format!("qcapture_{ts}.mp4");
    if dir.trim().is_empty() {
        return Ok(name);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("output folder '{dir}': {e}"))?;
    Ok(std::path::Path::new(dir)
        .join(name)
        .to_string_lossy()
        .into_owned())
}

impl WidgetApp {
    fn recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Snapshot everything worth remembering for the next launch.
    fn snapshot_settings(&self) -> super::settings::PersistedSettings {
        super::settings::PersistedSettings {
            version: super::settings::SETTINGS_VERSION,
            target: Some(self.target.clone()),
            region_screen: self.region_screen,
            window_text: self.window_text.clone(),
            mic_name: self.mic_name.clone(),
            audio_on: self.audio_on,
            sys_gain_db: self.levels.gain_db(false),
            sys_muted: self.levels.muted(false),
            mic_gain_db: self.levels.gain_db(true),
            mic_muted: self.levels.muted(true),
            adv: self.adv.lock().ok().map(|g| g.clone()),
            draw_live: self.draw_live,
            cursor_highlight: self.cursor_highlight,
            cursor_ripple: self.cursor_ripple,
            output_dir: self.output_dir.clone(),
            countdown_enabled: self.countdown_enabled,
        }
    }

    /// Best-effort persist (a failed save warns; recording never depends on it).
    /// Called on close (Drop) and at record start so a crash mid-record
    /// still keeps the setup that launched it.
    fn save_settings(&self) {
        if let Err(e) = super::settings::save(&self.snapshot_settings()) {
            eprintln!("warning: {e}");
        }
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
        let take = self.annotate.lock().ok().and_then(|mut p| {
            p.pending = false;
            p.result.take()
        });
        match take {
            // Subprocess finished with no usable result (cancelled, crashed,
            // bad output): say so instead of going silently idle.
            Some(None) => {
                self.last_msg = "annotate finished without a result — see log file".to_string();
            }
            None => {}
            Some(Some(res)) => {
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
    }

    /// Countdown length before a recording starts.
    const COUNTDOWN_SECS: u64 = 3;

    /// Pure deadline check (unit-testable without spawning a recording).
    fn countdown_finished(deadline: Instant) -> bool {
        Instant::now() >= deadline
    }

    /// Short target line for the countdown overlay.
    fn target_desc(&self) -> String {
        match &self.target {
            TargetSel::Screen(s) => format!("Screen #{s}"),
            TargetSel::Window(t) if t.trim().is_empty() => "Window […]".to_string(),
            TargetSel::Window(t) => format!("Window '{t}'"),
            TargetSel::Region { screen, rect } => {
                format!(
                    "Region {},{},{},{} on screen {screen}",
                    rect.x, rect.y, rect.w, rect.h
                )
            }
        }
    }

    /// Take an expired countdown deadline (clearing it), if any. Split out
    /// so tests can verify expiry without spawning a real recording.
    fn take_expired_countdown(&mut self) -> bool {
        match self.countdown_until {
            Some(d) if Self::countdown_finished(d) => {
                self.countdown_until = None;
                true
            }
            _ => false,
        }
    }

    /// Fire the recording once the countdown expires. Runs at the top of
    /// every update; starting goes through the normal path so behavior
    /// matches an immediate Record press.
    fn poll_countdown(&mut self) {
        if self.take_expired_countdown() {
            self.start_recording();
        }
    }

    fn start_recording(&mut self) {
        if self.rec.is_some() {
            return;
        }
        // Cursor fx is Windows-only (no portable cursor position API yet).
        #[cfg(not(windows))]
        if self.cursor_highlight || self.cursor_ripple {
            self.last_msg = "cursor fx is Windows-only in this release".to_string();
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
        let output = match default_output_in(&self.output_dir) {
            Ok(o) => o,
            Err(e) => {
                self.last_msg = e;
                return;
            }
        };
        self.save_settings();
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
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
        let pause_t = pause.clone();
        let done_t = done.clone();
        let annotate = self.pending_doc.clone();
        let style = self
            .adv
            .lock()
            .map(|a| qcapture_core::CursorStyle {
                hl_rgba: a.cursor_color,
                hl_radius: a.cursor_size,
                ripple_rgba: a.ripple_color,
                ripple_radius: a.ripple_size,
                ripple_ms: a.ripple_ms,
                additive: a.additive,
            })
            .unwrap_or_default();
        let cursor_fx =
            qcapture_core::CursorFx::opt_with(self.cursor_highlight, self.cursor_ripple, style);
        let screens_t = self.screens.clone();
        // Draw channels: UI keeps tx+rx+panel, capture gets rx+tx.
        let draw_caps = if let Some((fw, fh)) = draw_feed {
            let (draw_tx, draw_rx) = flume::bounded::<qcapture_annotate::DrawEvent>(256);
            let (preview_tx, preview_rx) =
                flume::bounded::<qcapture_capture::pump::PreviewFrame>(4);
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
                        cursor_fx, &screens_t, pause_t, stop_t,
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
            pause,
            done,
            paused_total: Duration::ZERO,
            pause_began: None,
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
                // stays on the initial geometry. Prefer the capture size
                // (what the encoder inits with) over the list size.
                #[cfg(windows)]
                if let Ok((w, h)) = qcapture_capture::window_feed_size(title) {
                    return Ok((w, h));
                }
                if let Some(w) = self.windows.iter().find(|w| w.title == *title) {
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
            let exe = qcapture_core::cli_helper_exe();
            let mut pick_cmd = std::process::Command::new(exe);
            qcapture_core::hide_child_console(&mut pick_cmd);
            let out = pick_cmd
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
            let exe = qcapture_core::cli_helper_exe();
            let mut annotate_cmd = std::process::Command::new(&exe);
            qcapture_core::hide_child_console(&mut annotate_cmd);
            let out = annotate_cmd
                .arg("annotate")
                .arg("--screen")
                .arg(screen.to_string())
                .arg("--save")
                .arg(&doc_path)
                .output();
            ctx2.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            let parsed = out
                .map_err(|e| {
                    eprintln!("annotate subprocess failed to spawn: {e}");
                    tracing::warn!("annotate subprocess failed to spawn: {e}");
                })
                .ok()
                .and_then(|o| {
                    if !o.status.success() {
                        eprintln!(
                            "annotate subprocess exited {}: {}",
                            o.status,
                            String::from_utf8_lossy(&o.stderr).trim()
                        );
                        tracing::warn!(
                            "annotate subprocess exited {}: {}",
                            o.status,
                            String::from_utf8_lossy(&o.stderr).trim()
                        );
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

/// Reveal a saved file in the OS file manager (select it when supported).
fn open_in_folder(path: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{path}"))
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .args(["-R", path])
        .spawn();
    #[cfg(target_os = "linux")]
    {
        // No ubiquitous select-in-folder on Linux: open the parent dir.
        let parent = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string());
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
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

/// Look up a widget screen index (0 = primary) for the portable capture
/// path (same rule as the CLI `--screen` flag). Non-Windows only (Windows
/// maps through the 1-based WGC index instead).
#[cfg(not(windows))]
fn display_of(screens: &[DisplayInfo], s: u32) -> Result<DisplayInfo, String> {
    let idx = s as usize;
    if idx == 0 {
        screens
            .iter()
            .find(|d| d.is_primary)
            .or(screens.first())
            .cloned()
            .ok_or_else(|| "no displays detected".to_string())
    } else {
        screens
            .get(idx)
            .cloned()
            .ok_or_else(|| format!("screen {s} not in the display list — reopen the widget"))
    }
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
    _screens: &[DisplayInfo],
    pause: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    // Live drawing AND cursor fx need the ffmpeg byte path. Auto-switch a
    // native encoder rather than failing: NVENC availability is probed
    // inside the ffmpeg branch. Timed annotations burn into MF directly.
    let mut adv_owned = adv.clone();
    let mut switched_note = String::new();
    if (draw_caps.is_some() || cursor_fx.is_some()) && !adv_owned.encoder.is_ffmpeg() {
        adv_owned.encoder = EncoderSel::Nvenc;
        switched_note = " (auto-switched to NVENC for drawing/cursor-fx)".to_string();
    }
    // MF is CBR-only; a staged non-CBR mode with an MF encoder is a loud error
    // (no silent fallback to a different quality contract).
    if !adv_owned.encoder.is_ffmpeg() && adv_owned.rc != RateSel::Cbr {
        return Err(
            "rate modes (VBR/CQP/CRF) need an ffmpeg encoder (NVENC/AMF/QSV/x264)".to_string(),
        );
    }
    let adv = &adv_owned;

    // FFmpeg path: vendor encoders everywhere, plus everything off Windows
    // (no MediaFoundation there — probe-backed resolve, fails loudly).
    let ffmpeg_kind: Option<qcapture_core::EncoderKind> = match adv.encoder {
        EncoderSel::Nvenc => Some(qcapture_core::EncoderKind::H264Nvenc),
        EncoderSel::Amf => Some(qcapture_core::EncoderKind::H264Amf),
        EncoderSel::Qsv => Some(qcapture_core::EncoderKind::H264Qsv),
        EncoderSel::X264 => Some(qcapture_core::EncoderKind::LibX264),
        #[cfg(not(windows))]
        EncoderSel::Auto | EncoderSel::H264 | EncoderSel::Hevc => {
            let info = qcapture_encode::probe_ffmpeg().map_err(|e| e.to_string())?;
            let kind = match adv.encoder {
                EncoderSel::Auto => qcapture_encode::resolve_auto_encoder(&info),
                EncoderSel::H264 => qcapture_encode::best_h264(&info)
                    .ok_or_else(|| "this ffmpeg has no H.264 encoder".to_string())?,
                _ => qcapture_encode::best_hevc(&info)
                    .ok_or_else(|| "this ffmpeg has no HEVC encoder".to_string())?,
            };
            switched_note = format!(" (using {kind:?} — no MediaFoundation off Windows)");
            Some(kind)
        }
        #[cfg(windows)]
        EncoderSel::Auto | EncoderSel::H264 | EncoderSel::Hevc => None,
    };
    if let Some(kind) = ffmpeg_kind {
        return record_thread_ffmpeg(
            target, adv, output, kind, mic_name, audio_on, levels, annotate, draw_caps, cursor_fx,
            _screens, pause, stop,
        )
        .map(|m| format!("{m}{switched_note}"));
    }
    if draw_caps.is_some() || cursor_fx.is_some() {
        return Err(
            "live drawing / cursor-fx need an ffmpeg encoder (NVENC/AMF/QSV/x264)".to_string(),
        );
    }

    // Native MediaFoundation path below is Windows-only (off Windows every
    // encoder resolves to ffmpeg above and never reaches here).
    #[cfg(not(windows))]
    {
        let _ = (&mic_name, &audio_on, &levels);
        Err("internal: native encoder off Windows".to_string())
    }

    #[cfg(windows)]
    let audio = if audio_on {
        let cfg = qcapture_audio::win_audio::WinAudioConfig {
            capture_system: true,
            mic_query: mic_name.clone(),
            levels: levels.clone(),
            pause: pause.clone(),
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
    #[cfg(windows)]
    {
        use qcapture_capture::win_record as rec;
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
                    pause.clone(),
                    annotate.clone(),
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
                pause.clone(),
                annotate.clone(),
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
                    pause.clone(),
                    annotate.clone(),
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
    _screens: &[DisplayInfo],
    pause: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<String, String> {
    #[cfg(windows)]
    use qcapture_capture::ffmpeg_cap as fc;
    use qcapture_capture::pump as pc;
    #[cfg(not(windows))]
    use qcapture_capture::xcap_cap as xc;
    use qcapture_encode::audio_pipe as ap;
    let info = qcapture_encode::probe_ffmpeg().map_err(|e| e.to_string())?;
    if !qcapture_encode::supports(kind, &info) {
        return Err(format!(
            "this ffmpeg has no {} — pick another encoder",
            pc::encoder_name(kind)
        ));
    }
    // Same dry-run as CLI: fail before threads start.
    qcapture_encode::rate_control_args(
        pc::encoder_name(kind),
        &rate_from_adv(adv).map_err(|e| e.to_string())?,
        adv.fps,
    )
    .map_err(|e| e.to_string())?;
    // Same pipeline as the MF path; chunks forward into the ffmpeg pipe.
    // Fail-soft like CLI: system-only trouble -> video-only, mic typo -> loud.
    // Backend per OS: WASAPI loopback on Windows, cpal monitor on Linux,
    // mic-only on macOS.
    let audio = if audio_on {
        #[cfg(windows)]
        let started = {
            let cfg = qcapture_audio::win_audio::WinAudioConfig {
                capture_system: true,
                mic_query: mic_name.clone(),
                levels: levels.clone(),
                pause: pause.clone(),
            };
            qcapture_audio::win_audio::start_pipeline(cfg)
                .map(qcapture_audio::AnyAudioPipeline::Win)
        };
        #[cfg(not(windows))]
        let started = {
            let cfg = qcapture_audio::portable::PortAudioConfig {
                capture_system: true,
                mic_query: mic_name.clone(),
                levels: levels.clone(),
                pause: pause.clone(),
            };
            qcapture_audio::portable::start_pipeline(cfg)
                .map(qcapture_audio::AnyAudioPipeline::Port)
        };
        match started {
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
    // Window HWND for cursor mapping (Windows; fail fast on a stale title).
    // Other OSes map nothing (cursor fx is Windows-only for now).
    #[cfg(windows)]
    let cursor_hwnd = match target {
        TargetSel::Window(title) if cursor_fx.is_some() => Some(
            qcapture_capture::resolve_window(title)
                .map(|r| r.hwnd)
                .map_err(|e| e.to_string())?,
        ),
        _ => None,
    };
    #[cfg(not(windows))]
    let cursor_hwnd = None;
    let job = |canvas: Option<(u32, u32)>| pc::FfmpegJob {
        fps: adv.fps,
        encoder: kind,
        rate,
        canvas,
        show_cursor: adv.show_cursor,
        output: output.to_string(),
        duration: None, // widget stops via the flag (Stop button / close)
        stop_flag: stop.clone(),
        pause_flag: pause.clone(),
        annotate: annotate.clone(),
        cursor_fx,
        cursor_window: cursor_hwnd,
        audio_pipe: audio_pipe_name.clone(),
        preview_tx: preview_tx.clone(),
    };
    // Cursor fx is Windows-only (no portable cursor position API yet).
    #[cfg(not(windows))]
    if cursor_fx.is_some() {
        return Err("cursor fx is Windows-only in this release".to_string());
    }
    let stats = match target {
        TargetSel::Screen(s) => {
            #[cfg(windows)]
            let r = {
                let idx = qcapture_capture::wgc_monitor_index(*s);
                fc::run_ffmpeg_monitor(idx, job(adv.canvas), on_end, draw_rx.take())
            };
            #[cfg(not(windows))]
            let r = {
                let d = display_of(_screens, *s)?;
                xc::run_xcap_monitor(&d, job(adv.canvas), on_end, draw_rx.take())
            };
            r
        }
        TargetSel::Window(title) => {
            #[cfg(windows)]
            let r = fc::run_ffmpeg_window(title, job(None), on_end, draw_rx.take());
            #[cfg(not(windows))]
            let r = xc::run_xcap_window(title, job(None), on_end, draw_rx.take());
            r
        }
        TargetSel::Region { screen, rect } => {
            #[cfg(windows)]
            let r = {
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
            };
            #[cfg(not(windows))]
            let r = {
                let d = display_of(_screens, *screen)?;
                xc::run_xcap_region(
                    &d,
                    rect.x.max(0) as u32,
                    rect.y.max(0) as u32,
                    rect.w,
                    rect.h,
                    job(None),
                    on_end,
                    draw_rx.take(),
                )
            };
            r
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
        pc::encoder_name(kind),
        audio_note
    ))
}

/// Advanced window body: fps/bitrate sliders, rate-control/canvas/encoder
/// dropdowns, cursor toggle. Free function (not a method) so headless UI
/// tests drive the exact same controls the production viewport shows.
fn show_advanced_panel(ui: &mut egui::Ui, a: &mut AdvCfg) {
    ui.heading("Advanced");
    ui.add(egui::Slider::new(&mut a.fps, AdvCfg::FPS_RANGE).text("fps"));
    ui.add(
        egui::Slider::new(&mut a.bitrate_kbps, AdvCfg::BITRATE_RANGE)
            .text("bitrate kbps (CBR/VBR target)"),
    );
    egui::ComboBox::from_label("Rate control")
        .selected_text(a.rc.label())
        .show_ui(ui, |ui| {
            for r in RateSel::all() {
                ui.selectable_value(&mut a.rc, r, r.label());
            }
        });
    match a.rc {
        RateSel::Cbr => {}
        RateSel::Vbr => {
            ui.add(
                egui::Slider::new(&mut a.maxrate_kbps, AdvCfg::MAXRATE_RANGE).text("maxrate kbps"),
            );
        }
        RateSel::Cqp => {
            ui.add(egui::Slider::new(&mut a.qp, AdvCfg::QP_RANGE).text("QP (lower=better)"));
            ui.label("CQP: NVENC/AMF/QSV (x264 uses CRF).");
        }
        RateSel::Crf => {
            ui.add(egui::Slider::new(&mut a.crf, AdvCfg::QP_RANGE).text("CRF (lower=better)"));
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
            for c in CANVAS_OPTIONS {
                let label = match c {
                    None => "Native".to_string(),
                    Some((w, h)) => format!("{w}x{h}"),
                };
                ui.selectable_value(&mut a.canvas, c, label);
            }
        });
    egui::ComboBox::from_label("Encoder")
        .selected_text(a.encoder.label())
        .show_ui(ui, |ui| {
            for e in EncoderSel::all() {
                ui.selectable_value(&mut a.encoder, e, e.label());
            }
        });
    ui.checkbox(&mut a.show_cursor, "Capture cursor");
    ui.separator();
    ui.heading("Cursor fx style");
    ui.label("Burned in when highlight/clicks are on (ffmpeg encoders).");
    ui.horizontal(|ui| {
        ui.label("Ring color");
        let mut c = egui::Color32::from_rgba_unmultiplied(
            a.cursor_color[0],
            a.cursor_color[1],
            a.cursor_color[2],
            a.cursor_color[3],
        );
        if super::pick_color_no_additive(ui, &mut c) {
            a.cursor_color = [c.r(), c.g(), c.b(), c.a()];
        }
        ui.label("Ripple color");
        let mut r = egui::Color32::from_rgba_unmultiplied(
            a.ripple_color[0],
            a.ripple_color[1],
            a.ripple_color[2],
            a.ripple_color[3],
        );
        if super::pick_color_no_additive(ui, &mut r) {
            a.ripple_color = [r.r(), r.g(), r.b(), r.a()];
        }
    });
    ui.checkbox(&mut a.additive, "Additive glow")
        .on_hover_text("Add light instead of blending over");
    ui.add(
        egui::Slider::new(&mut a.cursor_size, AdvCfg::CURSOR_RADIUS_RANGE).text("ring radius px"),
    );
    ui.add(
        egui::Slider::new(&mut a.ripple_size, AdvCfg::RIPPLE_RADIUS_RANGE).text("ripple radius px"),
    );
    ui.add(egui::Slider::new(&mut a.ripple_ms, AdvCfg::RIPPLE_MS_RANGE).text("ripple lifetime ms"));
    ui.label("ffmpeg entries mix AAC via named pipe.");
}

impl eframe::App for WidgetApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_done();
        self.poll_pick();
        self.poll_annotate();
        self.poll_countdown();

        if self.recording() || self.countdown_until.is_some() {
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
                            show_advanced_panel(ui, &mut a);
                        });
                    }
                    if ctx.input(|i| i.viewport().close_requested()) {
                        open.store(false, Ordering::Relaxed);
                    }
                },
            );
        }

        // Countdown overlay: display-only big number (same event loop).
        // Cancelling happens in the main window; expiry fires the normal
        // record path via poll_countdown above.
        if let Some(deadline) = self.countdown_until {
            let remaining = deadline.saturating_duration_since(Instant::now()).as_secs() + 1;
            let desc = self.target_desc();
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of("qcapture-countdown"),
                egui::ViewportBuilder::default()
                    .with_title("QCapture — starting…")
                    .with_inner_size([280.0, 170.0])
                    .with_resizable(false)
                    .with_always_on_top(),
                move |ctx, _| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.heading(format!("Recording {desc}"));
                            ui.label(
                                egui::RichText::new(format!("{remaining}"))
                                    .size(72.0)
                                    .strong(),
                            );
                            ui.label("Cancel in the main window");
                        });
                    });
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
                    if ui
                        .selectable_label(
                            matches!(self.target, TargetSel::Region { .. }),
                            "Region",
                        )
                        .clicked()
                        && !matches!(self.target, TargetSel::Region { .. })
                    {
                        // Select mode only: whole current screen as the initial
                        // rect (immediately recordable); refine via Select region.
                        let screen = match &self.target {
                            TargetSel::Screen(s) => *s,
                            TargetSel::Region { screen, .. } => *screen,
                            TargetSel::Window(_) => self.region_screen,
                        };
                        let (w, h) = self
                            .screens
                            .get(screen as usize)
                            .map(|d| (d.width, d.height))
                            .unwrap_or((1920, 1080));
                        self.region_screen = screen;
                        self.target = TargetSel::Region {
                            screen,
                            rect: Rect::new(0, 0, w, h),
                        };
                        self.last_msg = format!("region 0,0 {w}x{h} (full screen)");
                    }
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
                        .add(
                            egui::Slider::new(&mut db, AdvCfg::GAIN_DB_RANGE)
                                .show_value(false),
                        )
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
                            egui::Slider::new(&mut db, AdvCfg::GAIN_DB_RANGE)
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
                    ui.checkbox(&mut self.countdown_enabled, "⏳ Countdown").on_hover_text(
                        "3-second countdown overlay before recording starts",
                    );
                });
                // Cursor fx is Windows-only (no portable cursor position API).
                #[cfg(windows)]
                ui.horizontal(|ui| {
                    ui.label("Cursor fx:");
                    ui.checkbox(&mut self.cursor_highlight, "highlight").on_hover_text(
                        "Yellow ring around the cursor, burned into the video (ffmpeg encoders)",
                    );
                    ui.checkbox(&mut self.cursor_ripple, "clicks").on_hover_text(
                        "White ripple on mouse clicks, burned into the video (ffmpeg encoders)",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Folder:");
                    ui.text_edit_singleline(&mut self.output_dir)
                        .on_hover_text("Output folder for recordings (empty = current folder)");
                    if ui
                        .small_button("…")
                        .on_hover_text("Pick output folder…")
                        .clicked()
                    {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("QCapture output folder")
                            .pick_folder()
                        {
                            self.output_dir = dir.to_string_lossy().into_owned();
                        }
                    }
                });
                if self.countdown_until.is_some() {
                    // Countdown running: Record becomes Cancel.
                    if ui
                        .add_sized([390.0, 36.0], egui::Button::new("✕  Cancel countdown"))
                        .clicked()
                    {
                        self.countdown_until = None;
                        self.last_msg = "countdown cancelled.".to_string();
                    }
                } else if ui
                    .add_sized([390.0, 36.0], egui::Button::new("●  Record"))
                    .clicked()
                {
                    // Validate window target early for a loud error instead of a
                    // background failure.
                    if matches!(&self.target, TargetSel::Window(t) if t.trim().is_empty()) {
                        self.last_msg =
                            "pick a window first (substring of its title)".to_string();
                    } else if self.countdown_enabled {
                        self.countdown_until = Some(
                            Instant::now() + Duration::from_secs(Self::COUNTDOWN_SECS),
                        );
                        self.last_msg.clear();
                    } else {
                        self.start_recording();
                    }
                }
            } else if ui
                .add_sized([390.0, 36.0], egui::Button::new("■  Stop"))
                .clicked()
            {
                self.stop_recording();
            }

            if let Some(r) = &mut self.rec {
                let el = r.content_elapsed();
                let size = std::fs::metadata(&r.output).map(|m| m.len()).unwrap_or(0);
                let paused = r.pause.load(Ordering::Relaxed);
                ui.horizontal(|ui| {
                    if ui
                        .small_button(if paused { "▶ Resume" } else { "⏸ Pause" })
                        .on_hover_text("Freeze both A/V clocks; paused spans are cut")
                        .clicked()
                    {
                        r.set_paused(!paused);
                    }
                    ui.label(format!(
                        "{} {:.0}s — {:.2} MB\n{}",
                        if paused { "⏸" } else { "⏺" },
                        el.as_secs_f64(),
                        size as f64 / 1_000_000.0,
                        r.output
                    ));
                });
                if self.last_msg == "finalizing…" {
                    ui.spinner();
                }
            }
            if !self.last_msg.is_empty() && self.rec.is_none() {
                ui.label(&self.last_msg);
                if self.last_msg.starts_with("Saved ") && ui.small_button("Open folder").clicked() {
                    if let Some(path) = self.last_saved_path() {
                        open_in_folder(&path);
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
                    "staged annotations ({} strokes) — burns in on next record",
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

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, Modifiers, PointerButton, Pos2};
    use egui_kittest::{kittest::Queryable, Harness};

    fn test_screens() -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                id: 0,
                name: "TestA".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 1.0,
                is_primary: true,
                refresh_hz: Some(60),
            },
            DisplayInfo {
                id: 1,
                name: "TestB".into(),
                x: 1920,
                y: 0,
                width: 1280,
                height: 720,
                scale_factor: 1.0,
                is_primary: false,
                refresh_hz: Some(60),
            },
        ]
    }

    fn test_app() -> WidgetApp {
        WidgetApp {
            screens: test_screens(),
            windows: vec![WindowInfo {
                id: 7,
                title: "Notepad doc".into(),
                app_name: "np".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                minimized: false,
            }],
            target: TargetSel::Screen(0),
            window_text: String::new(),
            region_screen: 0,
            mic_name: None,
            mic_options: vec!["Mic A".into(), "USB Mic".into()],
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
            output_dir: String::new(),
            countdown_enabled: false,
            countdown_until: None,
            draw_ui: Arc::new(Mutex::new(None)),
            draw_open: Arc::new(AtomicBool::new(false)),
        }
    }

    fn main_harness() -> Harness<'static, WidgetApp> {
        Harness::builder()
            .with_size(eframe::egui::Vec2::new(420.0, 760.0))
            .build_eframe(|_cc| test_app())
    }

    fn adv_harness() -> Harness<'static, AdvCfg> {
        Harness::builder()
            .with_size(eframe::egui::Vec2::new(420.0, 560.0))
            .build_ui_state(
                |ui: &mut eframe::egui::Ui, adv: &mut AdvCfg| show_advanced_panel(ui, adv),
                AdvCfg::default(),
            )
    }

    /// Pointer drag across frames (press, move, release each land on their
    /// own frame so egui registers a drag, not a click).
    fn drag<State>(h: &mut Harness<'_, State>, from: Pos2, to: Pos2) {
        h.input_mut().events.push(Event::PointerMoved(from));
        h.input_mut().events.push(Event::PointerButton {
            pos: from,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::default(),
        });
        h.step();
        h.input_mut().events.push(Event::PointerMoved(to));
        h.step();
        h.input_mut().events.push(Event::PointerButton {
            pos: to,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Modifiers::default(),
        });
        h.step();
        h.run();
    }

    /// Drag a slider node to a horizontal fraction of its own rect
    /// (0.0 = left edge, 1.0 = right edge). Sliders share their accesskit
    /// label with a SpinButton twin, so the Slider role disambiguates.
    fn drag_slider_to<State>(
        h: &mut Harness<'_, State>,
        label: &str,
        frac: f32,
    ) -> eframe::egui::Rect {
        use eframe::egui::accesskit::Role;
        let rect = h.get_by_role_and_label(Role::Slider, label).rect();
        let y = rect.center().y;
        drag(
            h,
            Pos2::new(rect.min.x + 2.0, y),
            Pos2::new(rect.min.x + rect.width() * frac.clamp(0.0, 1.0), y),
        );
        rect
    }

    /// Open a combo box (queried by role + `from_label` label — the label
    /// twin and the selected value itself are not clickable) and pick a
    /// popup option.
    fn combo_pick<State>(h: &mut Harness<'_, State>, combo: &str, option: &str) {
        use eframe::egui::accesskit::Role;
        h.get_by_role_and_label(Role::ComboBox, combo).click();
        h.run();
        h.get_by_label(option).click();
        h.run();
    }

    // ------------------------------------------------------------ sanitize ---

    #[test]
    fn sanitize_clamps_every_slider_range() {
        // Below range, inside range, above range for every numeric field.
        let mut a = AdvCfg {
            fps: 0,
            bitrate_kbps: 0,
            maxrate_kbps: 0,
            qp: 0,
            crf: 0,
            cursor_size: 0.0,
            ripple_size: 0.0,
            ripple_ms: 0,
            ..Default::default()
        };
        a.sanitize();
        assert_eq!(a.fps, 15);
        assert_eq!(a.bitrate_kbps, 1000);
        assert_eq!(a.maxrate_kbps, 1000);
        assert_eq!(a.cursor_size, 6.0);
        assert_eq!(a.ripple_size, 12.0);
        assert_eq!(a.ripple_ms, 100);

        let mut a = AdvCfg {
            fps: 30,
            bitrate_kbps: 8000,
            maxrate_kbps: 12000,
            qp: 23,
            crf: 23,
            cursor_size: 20.0,
            ripple_size: 60.0,
            ripple_ms: 900,
            ..Default::default()
        };
        a.sanitize();
        assert_eq!(
            (a.fps, a.bitrate_kbps, a.maxrate_kbps, a.qp, a.crf),
            (30, 8000, 12000, 23, 23)
        );
        assert_eq!(
            (a.cursor_size, a.ripple_size, a.ripple_ms),
            (20.0, 60.0, 900)
        );

        let mut a = AdvCfg {
            fps: 999,
            bitrate_kbps: 999_999,
            maxrate_kbps: 999_999,
            qp: 255,
            crf: 255,
            cursor_size: 999.0,
            ripple_size: 999.0,
            ripple_ms: 99999,
            ..Default::default()
        };
        a.sanitize();
        assert_eq!(a.fps, 120);
        assert_eq!(a.bitrate_kbps, 50000);
        assert_eq!(a.maxrate_kbps, 80000);
        assert_eq!(a.qp, 51);
        assert_eq!(a.crf, 51);
        assert_eq!(a.cursor_size, 40.0);
        assert_eq!(a.ripple_size, 120.0);
        assert_eq!(a.ripple_ms, 3000);
    }

    #[test]
    fn sanitize_rejects_tiny_canvas() {
        let mut a = AdvCfg {
            canvas: Some((10, 10)),
            ..Default::default()
        };
        a.sanitize();
        assert_eq!(a.canvas, None);
        let mut a = AdvCfg {
            canvas: Some((1280, 720)),
            ..Default::default()
        };
        a.sanitize();
        assert_eq!(a.canvas, Some((1280, 720)));
    }

    // ------------------------------------------------------------ dropdowns ---

    #[test]
    fn dropdown_options_cover_everything() {
        assert_eq!(RateSel::all().len(), 4);
        assert_eq!(EncoderSel::all().len(), 7);
        assert_eq!(CANVAS_OPTIONS.len(), 3);
        assert_eq!(CANVAS_OPTIONS[0], None);
        let mut labels: Vec<_> = RateSel::all().iter().map(|r| r.label()).collect();
        labels.extend(EncoderSel::all().iter().map(|e| e.label()));
        assert!(labels.iter().all(|l| !l.is_empty()));
        assert_eq!(
            labels.len(),
            labels
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
        );
    }

    #[test]
    fn encoder_ffmpeg_split_matches_paths() {
        for e in [EncoderSel::Auto, EncoderSel::H264, EncoderSel::Hevc] {
            assert!(!e.is_ffmpeg(), "{e:?} must stay native");
        }
        for e in [
            EncoderSel::Nvenc,
            EncoderSel::Amf,
            EncoderSel::Qsv,
            EncoderSel::X264,
        ] {
            assert!(e.is_ffmpeg(), "{e:?} must use ffmpeg");
        }
    }

    #[test]
    fn rate_mapping_covers_every_mode() {
        let adv = |rc| AdvCfg {
            rc,
            ..Default::default()
        };
        assert!(matches!(
            rate_from_adv(&adv(RateSel::Cbr)).unwrap(),
            qcapture_core::RateControl::Cbr { .. }
        ));
        assert!(matches!(
            rate_from_adv(&adv(RateSel::Vbr)).unwrap(),
            qcapture_core::RateControl::Vbr { .. }
        ));
        assert!(matches!(
            rate_from_adv(&adv(RateSel::Cqp)).unwrap(),
            qcapture_core::RateControl::Cqp { .. }
        ));
        assert!(matches!(
            rate_from_adv(&adv(RateSel::Crf)).unwrap(),
            qcapture_core::RateControl::Crf { .. }
        ));
    }

    // ------------------------------------------------------------ main screen ---

    #[test]
    fn target_labels_switch_modes() {
        let mut h = main_harness();
        h.get_by_label("Window").click();
        h.run();
        assert!(matches!(h.state().target, TargetSel::Window(_)));

        h.get_by_label("Region").click();
        h.run();
        match &h.state().target {
            TargetSel::Region { screen, rect } => {
                assert_eq!(*screen, 0);
                assert_eq!((rect.w, rect.h), (1920, 1080));
            }
            other => panic!("expected Region, got {other:?}"),
        }

        h.get_by_label("Screen").click();
        h.run();
        assert!(matches!(h.state().target, TargetSel::Screen(0)));
    }

    #[test]
    fn quick_toggles_flip() {
        let mut h = main_harness();
        // Cursor fx row is Windows-only (no portable cursor position API).
        #[cfg(windows)]
        let labels = ["✏ Draw live", "highlight", "clicks", "system"];
        #[cfg(not(windows))]
        let labels = ["✏ Draw live", "system"];
        for label in labels {
            h.get_by_label(label).click();
            h.run();
        }
        let s = h.state();
        assert!(s.draw_live && !s.audio_on);
        #[cfg(windows)]
        assert!(s.cursor_highlight && s.cursor_ripple);
        // Clicking again flips back.
        h.get_by_label("✏ Draw live").click();
        h.run();
        assert!(!h.state().draw_live);
    }

    #[test]
    fn gain_sliders_drive_mixer() {
        let mut h = main_harness();
        let sys: Vec<_> = h
            .get_all_by_role(eframe::egui::accesskit::Role::Slider)
            .collect();
        assert_eq!(sys.len(), 2, "Sys + Mic gain sliders");
        let before = h.state().levels.gain_db(false);
        let r0 = sys[0].rect();
        drop(sys);
        drag(
            &mut h,
            Pos2::new(r0.min.x + 2.0, r0.center().y),
            Pos2::new(r0.max.x - 2.0, r0.center().y),
        );
        assert!(
            h.state().levels.gain_db(false) > before,
            "dragging Sys right raises gain"
        );
    }

    #[test]
    fn mic_combo_selects_and_folder_types() {
        use eframe::egui::accesskit::Role;
        let mut h = main_harness();
        // Unlabelled combos in layout order: screen-pick, then mic-pick.
        let combos: Vec<_> = h.get_all_by_role(Role::ComboBox).collect();
        assert_eq!(combos.len(), 2);
        combos[1].click();
        drop(combos);
        h.run();
        h.get_by_label("Mic A").click();
        h.run();
        assert_eq!(h.state().mic_name.as_deref(), Some("Mic A"));

        // Single text field on the Screen target = output folder.
        let fields: Vec<_> = h
            .get_all_by_role(eframe::egui::accesskit::Role::TextInput)
            .collect();
        assert_eq!(fields.len(), 1);
        fields[0].focus();
        fields[0].type_text("D:\\Vids");
        drop(fields);
        h.run();
        assert_eq!(h.state().output_dir, "D:\\Vids");
    }

    #[test]
    fn screen_combo_selects_second_monitor() {
        use eframe::egui::accesskit::Role;
        let mut h = main_harness();
        let combos: Vec<_> = h.get_all_by_role(Role::ComboBox).collect();
        assert_eq!(combos.len(), 2);
        combos[0].click();
        drop(combos);
        h.run();
        h.get_by_label_contains("TestB").click();
        h.run();
        assert!(matches!(h.state().target, TargetSel::Screen(1)));
    }

    #[test]
    fn window_combo_selects_listed_window() {
        use eframe::egui::accesskit::Role;
        let mut h = main_harness();
        h.get_by_label("Window").click();
        h.run();
        // Window target shows window-pick first, mic-pick second.
        let combos: Vec<_> = h.get_all_by_role(Role::ComboBox).collect();
        assert_eq!(combos.len(), 2);
        combos[0].click();
        drop(combos);
        h.run();
        h.get_by_label_contains("Notepad").click();
        h.run();
        match &h.state().target {
            TargetSel::Window(t) => assert_eq!(t, "Notepad doc"),
            other => panic!("expected Window, got {other:?}"),
        }
    }

    #[test]
    fn pause_button_freezes_and_resumes() {
        let mut app = test_app();
        // Fake an in-progress recording (no threads — only the flag matters).
        app.rec = Some(ActiveRec {
            started: Instant::now(),
            output: "test.mp4".into(),
            stop: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            done: Arc::new(Mutex::new(None)),
            paused_total: Duration::ZERO,
            pause_began: None,
        });
        let mut h = Harness::builder()
            .with_size(eframe::egui::Vec2::new(420.0, 760.0))
            .build_eframe(|_cc| app);
        // A live recording repaints continuously, so run() never settles —
        // fixed steps process the queued click events just as well.
        assert!(!h
            .state()
            .rec
            .as_ref()
            .unwrap()
            .pause
            .load(Ordering::Relaxed));
        h.get_by_label("⏸ Pause").click();
        h.run_steps(6);
        let rec = h.state().rec.as_ref().unwrap();
        assert!(rec.pause.load(Ordering::Relaxed));
        assert!(rec.pause_began.is_some(), "pausing starts the clock");
        // Button flips to Resume; clicking again clears the flag and banks
        // the paused span, so the content clock excludes it.
        h.get_by_label("▶ Resume").click();
        h.run_steps(6);
        let rec = h.state().rec.as_ref().unwrap();
        assert!(!rec.pause.load(Ordering::Relaxed));
        assert!(rec.pause_began.is_none());
        assert!(
            rec.content_elapsed() <= rec.started.elapsed(),
            "content clock never exceeds session clock"
        );
    }

    #[test]
    fn countdown_finished_checks_deadline() {
        use std::time::Duration;
        assert!(WidgetApp::countdown_finished(
            Instant::now() - Duration::from_secs(1)
        ));
        assert!(!WidgetApp::countdown_finished(
            Instant::now() + Duration::from_secs(60)
        ));
    }

    #[test]
    fn countdown_toggle_arms_and_cancels() {
        let mut h = main_harness();
        // Toggle on, then Record arms the countdown instead of recording.
        h.get_by_label("⏳ Countdown").click();
        h.run();
        assert!(h.state().countdown_enabled);
        h.get_by_label("●  Record").click();
        // Counting down repaints continuously, so run() never settles —
        // fixed steps process the queued clicks just as well.
        h.run_steps(6);
        assert!(h.state().rec.is_none());
        assert!(h.state().countdown_until.is_some());
        // Record button is replaced by Cancel while counting down.
        h.get_by_label("✕  Cancel countdown").click();
        h.run_steps(6);
        assert!(h.state().countdown_until.is_none());
        assert!(h.state().rec.is_none());
    }

    #[test]
    fn countdown_expiry_takes_once() {
        let mut app = test_app();
        // Future deadline: kept, not taken.
        app.countdown_until = Some(Instant::now() + Duration::from_secs(60));
        assert!(!app.take_expired_countdown());
        assert!(app.countdown_until.is_some());
        // Past deadline: taken exactly once (poll never double-fires).
        app.countdown_until = Some(Instant::now() - Duration::from_secs(1));
        assert!(app.take_expired_countdown());
        assert!(app.countdown_until.is_none());
        assert!(!app.take_expired_countdown());
        // Nothing armed: no-op.
        assert!(!app.take_expired_countdown());
    }

    #[test]
    fn record_with_empty_window_title_is_loud() {
        let mut h = main_harness();
        h.get_by_label("Window").click();
        h.run();
        h.get_by_label("●  Record").click();
        h.run();
        assert!(h.state().rec.is_none());
        assert!(h.state().last_msg.contains("pick a window first"));
    }

    // ------------------------------------------------------------ advanced ---

    #[test]
    fn advanced_fps_slider_hits_min_mid_max() {
        let mut h = adv_harness();
        drag_slider_to(&mut h, "fps", 1.0);
        assert_eq!(h.state().fps, 120);
        drag_slider_to(&mut h, "fps", 0.0);
        assert_eq!(h.state().fps, 15);
        drag_slider_to(&mut h, "fps", 0.5);
        let mid = h.state().fps;
        assert!(
            (40..=95).contains(&mid),
            "mid drag lands mid-range, got {mid}"
        );
    }

    #[test]
    fn advanced_bitrate_slider_hits_min_and_max() {
        let mut h = adv_harness();
        drag_slider_to(&mut h, "bitrate kbps (CBR/VBR target)", 1.0);
        assert_eq!(h.state().bitrate_kbps, 50000);
        drag_slider_to(&mut h, "bitrate kbps (CBR/VBR target)", 0.0);
        assert_eq!(h.state().bitrate_kbps, 1000);
    }

    #[test]
    fn color_picker_has_no_additive_trap() {
        use eframe::egui::accesskit::Role;
        let mut h = adv_harness();
        // The two swatches are ColorWell nodes (no text of their own).
        let wells: Vec<_> = h.get_all_by_role(Role::ColorWell).collect();
        assert_eq!(wells.len(), 2);
        wells[0].click();
        drop(wells);
        h.run();
        // The picker must not offer the Normal/Additive toggle: additive is
        // unrepresentable in u8 storage (negative alpha collapses to 0, so
        // every click looked broken). Glow is an explicit checkbox instead.
        // (Translucency itself works through the alpha slider + 8-digit hex;
        // covered by unit tests — the slider carries hover-text, not a label.)
        assert!(h.query_by_label("Additive").is_none());
        assert!(h.query_by_label("Blending:").is_none());
    }

    #[test]
    fn advanced_cursor_sliders_hit_min_and_max() {
        let mut h = adv_harness();
        drag_slider_to(&mut h, "ring radius px", 1.0);
        assert_eq!(h.state().cursor_size, 40.0);
        drag_slider_to(&mut h, "ring radius px", 0.0);
        assert_eq!(h.state().cursor_size, 6.0);
        drag_slider_to(&mut h, "ripple radius px", 1.0);
        assert_eq!(h.state().ripple_size, 120.0);
        drag_slider_to(&mut h, "ripple radius px", 0.0);
        assert_eq!(h.state().ripple_size, 12.0);
        drag_slider_to(&mut h, "ripple lifetime ms", 1.0);
        assert_eq!(h.state().ripple_ms, 3000);
        drag_slider_to(&mut h, "ripple lifetime ms", 0.0);
        assert_eq!(h.state().ripple_ms, 100);
    }

    #[test]
    fn advanced_mode_sliders_appear_per_rate_mode() {
        let mut h = adv_harness();
        // CBR shows neither conditional slider.
        assert!(h.query_by_label("maxrate kbps").is_none());
        assert!(h.query_by_label("QP (lower=better)").is_none());

        combo_pick(&mut h, "Rate control", "VBR");
        assert_eq!(h.state().rc, RateSel::Vbr);
        drag_slider_to(&mut h, "maxrate kbps", 1.0);
        assert_eq!(h.state().maxrate_kbps, 80000);
        drag_slider_to(&mut h, "maxrate kbps", 0.0);
        assert_eq!(h.state().maxrate_kbps, 1000);

        combo_pick(&mut h, "Rate control", "CQP");
        assert_eq!(h.state().rc, RateSel::Cqp);
        drag_slider_to(&mut h, "QP (lower=better)", 1.0);
        assert_eq!(h.state().qp, 51);
        drag_slider_to(&mut h, "QP (lower=better)", 0.0);
        assert_eq!(h.state().qp, 0);

        combo_pick(&mut h, "Rate control", "CRF");
        assert_eq!(h.state().rc, RateSel::Crf);
        drag_slider_to(&mut h, "CRF (lower=better)", 0.5);
        let mid = h.state().crf;
        assert!(
            (10..=45).contains(&mid),
            "mid CRF drag lands mid-range, got {mid}"
        );
    }

    #[test]
    fn advanced_encoder_dropdown_covers_all_options() {
        let mut h = adv_harness();
        for (label, sel) in [
            ("NVENC (ffmpeg)", EncoderSel::Nvenc),
            ("AMF (ffmpeg)", EncoderSel::Amf),
            ("QSV (ffmpeg)", EncoderSel::Qsv),
            ("x264 (ffmpeg)", EncoderSel::X264),
            ("HEVC", EncoderSel::Hevc),
            ("H264", EncoderSel::H264),
            ("Auto (H264)", EncoderSel::Auto),
        ] {
            combo_pick(&mut h, "Encoder", label);
            assert_eq!(h.state().encoder, sel);
        }
    }

    #[test]
    fn advanced_canvas_dropdown_covers_all_options() {
        let mut h = adv_harness();
        combo_pick(&mut h, "Canvas", "1280x720");
        assert_eq!(h.state().canvas, Some((1280, 720)));
        combo_pick(&mut h, "Canvas", "1920x1080");
        assert_eq!(h.state().canvas, Some((1920, 1080)));
        combo_pick(&mut h, "Canvas", "Native");
        assert_eq!(h.state().canvas, None);
    }

    #[test]
    fn advanced_rate_dropdown_covers_all_options() {
        let mut h = adv_harness();
        for (label, sel) in [
            ("VBR", RateSel::Vbr),
            ("CQP", RateSel::Cqp),
            ("CRF", RateSel::Crf),
            ("CBR", RateSel::Cbr),
        ] {
            combo_pick(&mut h, "Rate control", label);
            assert_eq!(h.state().rc, sel);
        }
    }

    #[test]
    fn advanced_cursor_checkbox_toggles() {
        let mut h = adv_harness();
        assert!(h.state().show_cursor);
        h.get_by_label("Capture cursor").click();
        h.run();
        assert!(!h.state().show_cursor);
    }

    // ------------------------------------------------------------ pure helpers ---

    #[test]
    fn region_string_parses_and_rejects() {
        let r = parse_region_str("10,20,640,480").unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (10, 20, 640, 480));
        assert!(parse_region_str("10,20,0,480").is_none());
        assert!(parse_region_str("nope").is_none());
    }

    #[test]
    fn output_path_resolves_folder_or_cwd() {
        let cwd = default_output_in("").unwrap();
        assert!(cwd.starts_with("qcapture_") && cwd.ends_with(".mp4"));
        assert!(!cwd.contains(std::path::MAIN_SEPARATOR));

        let dir = std::env::temp_dir().join("qcapture-out-test");
        let _ = std::fs::remove_dir_all(&dir);
        let out = default_output_in(dir.to_str().unwrap()).unwrap();
        assert!(out.starts_with(dir.to_str().unwrap()));
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
