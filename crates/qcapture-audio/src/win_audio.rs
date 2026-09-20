//! Windows audio pipeline (Phase 3a): WASAPI loopback (system) + cpal mic,
//! mixed to one continuous i16 stereo 48 kHz stream for the AAC encoder.
//!
//! Threading: the mixer thread owns the WASAPI loopback client and drains a
//! cpal mic stream (created on its own thread for COM-apartment isolation).
//! Every 10 ms it emits exactly one 480-frame quantum — zero-padding gaps —
//! so the encoder's monotonic audio clock never stalls and A/V stay in sync.
//! Silence (idle desktop, muted sources) is real zeros, not gaps.

use super::{AudioError, MixerLevels};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const OUT_RATE: u32 = 48_000;
pub const QUANTUM_FRAMES: usize = 480; // 10 ms @ 48 kHz
const QUANTUM_BYTES: usize = QUANTUM_FRAMES * 2 * 2; // stereo i16
const MAX_FIFO_FRAMES: usize = OUT_RATE as usize * 2; // 2 s backlog cap

/// What to capture. Levels are shared (`Arc`) so a UI can move sliders
/// mid-recording; the mixer snapshots them once per 10 ms quantum.
#[derive(Debug, Clone)]
pub struct WinAudioConfig {
    pub capture_system: bool,
    pub mic_query: Option<String>,
    pub levels: std::sync::Arc<SharedLevels>,
}

/// Live gain/mute state shared between UI and mixer thread.
/// Gains stored as f32 bit patterns for lock-free atomic access.
#[derive(Debug, Default)]
pub struct SharedLevels {
    sys_gain_bits: AtomicU32,
    mic_gain_bits: AtomicU32,
    sys_mute: AtomicBool,
    mic_mute: AtomicBool,
    /// Mixed-output peak (0.0..1.0 bits), updated per quantum with max().
    /// UI drains via [`SharedLevels::take_peak`] and applies its own decay.
    peak_bits: AtomicU32,
}

impl SharedLevels {
    pub fn new(levels: MixerLevels) -> Arc<Self> {
        Arc::new(Self {
            sys_gain_bits: AtomicU32::new(levels.system_gain.to_bits()),
            mic_gain_bits: AtomicU32::new(levels.mic_gain.to_bits()),
            sys_mute: AtomicBool::new(levels.system_muted),
            mic_mute: AtomicBool::new(levels.mic_muted),
            peak_bits: AtomicU32::new(0),
        })
    }

    pub fn snapshot(&self) -> MixerLevels {
        MixerLevels {
            system_gain: f32::from_bits(self.sys_gain_bits.load(Ordering::Relaxed)),
            mic_gain: f32::from_bits(self.mic_gain_bits.load(Ordering::Relaxed)),
            system_muted: self.sys_mute.load(Ordering::Relaxed),
            mic_muted: self.mic_mute.load(Ordering::Relaxed),
        }
    }

    pub fn set_gain_db(&self, mic: bool, db: f32) {
        let bits = MixerLevels::db_to_linear(db).to_bits();
        if mic {
            self.mic_gain_bits.store(bits, Ordering::Relaxed);
        } else {
            self.sys_gain_bits.store(bits, Ordering::Relaxed);
        }
    }

    pub fn gain_db(&self, mic: bool) -> f32 {
        let lin = if mic {
            f32::from_bits(self.mic_gain_bits.load(Ordering::Relaxed))
        } else {
            f32::from_bits(self.sys_gain_bits.load(Ordering::Relaxed))
        };
        if lin <= 0.0 {
            -60.0
        } else {
            20.0 * lin.log10()
        }
    }

    pub fn set_muted(&self, mic: bool, muted: bool) {
        if mic {
            self.mic_mute.store(muted, Ordering::Relaxed);
        } else {
            self.sys_mute.store(muted, Ordering::Relaxed);
        }
    }

    pub fn muted(&self, mic: bool) -> bool {
        if mic {
            self.mic_mute.load(Ordering::Relaxed)
        } else {
            self.sys_mute.load(Ordering::Relaxed)
        }
    }

    pub fn observe_peak(&self, peak: f32) {
        self.peak_bits
            .fetch_max(peak.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Drain the accumulated peak (returns 0.0 if the mixer added nothing).
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed))
    }
}

/// Runtime stats for the final log line / future VU meters.
#[derive(Debug, Default)]
pub struct AudioStats {
    pub quanta_emitted: u64,
    pub sys_underruns: u64,
    pub mic_underruns: u64,
    pub mic_packets_dropped: u64,
}

/// Live pipeline. `mixed_rx` yields consecutive 1920-byte i16-stereo-48k chunks.
/// Call [`WinAudioPipeline::shutdown`] after recording to join threads.
pub struct WinAudioPipeline {
    mixed_rx: flume::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    stats: Arc<StatsInner>,
}

#[derive(Default)]
struct StatsInner {
    quanta: AtomicU64,
    sys_underruns: AtomicU64,
    mic_underruns: AtomicU64,
    mic_dropped: AtomicU64,
}

impl WinAudioPipeline {
    pub fn mixed_rx(&self) -> flume::Receiver<Vec<u8>> {
        self.mixed_rx.clone()
    }

    pub fn stats(&self) -> AudioStats {
        AudioStats {
            quanta_emitted: self.stats.quanta.load(Ordering::Relaxed),
            sys_underruns: self.stats.sys_underruns.load(Ordering::Relaxed),
            mic_underruns: self.stats.mic_underruns.load(Ordering::Relaxed),
            mic_packets_dropped: self.stats.mic_dropped.load(Ordering::Relaxed),
        }
    }

    pub fn shutdown(mut self) -> AudioStats {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.stats()
    }
}

/// Raw mic packet from the cpal callback thread (device rate/channels, f32).
struct MicPacket {
    data: Vec<f32>,
    channels: usize,
    rate: u32,
}

/// Start the pipeline. Returns an error if a *requested* source fails
/// (mic not found, mixer thread spawn) — the caller decides fail-soft vs bail.
/// Note: loopback *runtime* loss is always soft (mixer warns and goes silent).
pub fn start_pipeline(config: WinAudioConfig) -> Result<WinAudioPipeline, AudioError> {
    if !config.capture_system && config.mic_query.is_none() {
        return Err(AudioError::Backend("no audio source selected".into()));
    }
    // Fail fast on an explicit mic typo: the mic thread itself can only
    // warn-and-exit (it runs detached), which would silently drop the mic.
    // NOTE: this validation runs on a helper thread, NEVER the caller's:
    // cpal initializes COM on the calling thread and that poisons WinRT/WGC
    // init later on the same thread ("Failed to initialize WinRT" / hangs).
    if let Some(query) = config.mic_query.clone() {
        thread::Builder::new()
            .name("qcapture-mic-validate".into())
            .spawn(move || check_mic_exists(&query))
            .map_err(|e| AudioError::Backend(format!("validator spawn: {e}")))?
            .join()
            .map_err(|_| AudioError::Backend("mic validator panicked".into()))??;
    }

    // Mic packets flow callback-thread -> mixer-thread.
    let (mic_tx, mic_rx) = flume::bounded::<MicPacket>(64);
    let mic_dropped = Arc::new(AtomicU64::new(0));
    let mic_stop = Arc::new(AtomicBool::new(false));

    // cpal stream must live on its own thread; keep the JoinHandle out of the
    // mixer so a stuck device doesn't block shutdown (we only signal + detach).
    let mic_query = config.mic_query.clone();
    if let Some(query) = mic_query {
        let tx = mic_tx.clone();
        let stop = mic_stop.clone();
        let dropped = mic_dropped.clone();
        thread::Builder::new()
            .name("qcapture-mic".into())
            .spawn(move || {
                if let Err(e) = mic_thread_main(&query, tx, stop, dropped) {
                    tracing::warn!("mic thread exited: {e}");
                }
            })
            .map_err(|e| AudioError::Backend(format!("mic thread spawn: {e}")))?;
    }
    drop(mic_tx);

    // Unbounded: the capture handler drains at WGC frame rate (100+/s) while we
    // produce 100 chunks/s, so backlog only grows if the encoder itself stalls —
    // and dropping would corrupt the monotonic audio clock. ~190 KB/s worst case.
    let (mixed_tx, mixed_rx) = flume::unbounded::<Vec<u8>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StatsInner::default());
    // Seed mic-drop counter so mixer stats include callback drops.
    stats
        .mic_dropped
        .store(mic_dropped.load(Ordering::Relaxed), Ordering::Relaxed);

    let thread_cfg = config.clone();
    let thread_stop = stop.clone();
    let thread_stats = stats.clone();
    let handle = thread::Builder::new()
        .name("qcapture-audio-mix".into())
        .spawn(move || {
            mixer_thread_main(
                thread_cfg,
                mic_rx,
                mic_stop,
                mixed_tx,
                thread_stop,
                &thread_stats,
            );
        })
        .map_err(|e| AudioError::Backend(format!("mixer thread spawn: {e}")))?;

    Ok(WinAudioPipeline {
        mixed_rx,
        stop,
        handle: Some(handle),
        stats,
    })
}

// ---------------------------------------------------------------------------
// Mic (cpal) thread
// ---------------------------------------------------------------------------

/// Synchronous mic lookup with the same matching rule as the capture thread.
/// Called at pipeline start so an explicit `--mic typo` bails instead of
/// silently recording without the mic.
pub fn check_mic_exists(query: &str) -> Result<(), AudioError> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    if query.eq_ignore_ascii_case("default") || query.is_empty() {
        return host
            .default_input_device()
            .map(|_| ())
            .ok_or_else(|| AudioError::NoHost("no default input device".into()));
    }
    let lower = query.to_lowercase();
    let found = host
        .input_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?
        .any(|d| d.to_string().to_lowercase().contains(&lower));
    if found {
        Ok(())
    } else {
        Err(AudioError::Backend(format!(
            "mic '{query}' not found — see `qcapture --list-audio`"
        )))
    }
}

fn mic_thread_main(
    query: &str,
    tx: flume::Sender<MicPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<(), AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let device = if query.eq_ignore_ascii_case("default") || query.is_empty() {
        host.default_input_device()
            .ok_or_else(|| AudioError::NoHost("no default input device".into()))?
    } else {
        let lower = query.to_lowercase();
        host.input_devices()
            .map_err(|e| AudioError::Backend(e.to_string()))?
            .find(|d| d.to_string().to_lowercase().contains(&lower))
            .ok_or_else(|| {
                AudioError::Backend(format!(
                    "mic '{query}' not found — see `qcapture --list-audio`"
                ))
            })?
    };
    tracing::info!("mic device: {}", device.to_string());

    // Pick config: F32 > I16 > U16, rate closest to 48 k, stereo preferred.
    let mut best: Option<(i32, cpal::SupportedStreamConfigRange)> = None;
    let ranges = device
        .supported_input_configs()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    for r in ranges {
        let fmt_score = match r.sample_format() {
            cpal::SampleFormat::F32 => 0,
            cpal::SampleFormat::I16 => 1,
            cpal::SampleFormat::U16 => 2,
            _ => continue,
        };
        let rate = 48000u32.clamp(r.min_sample_rate(), r.max_sample_rate());
        let rate_dist = rate.abs_diff(48000) as i32;
        let ch_pen = match r.channels() {
            2 => 0,
            1 => 500,
            _ => 2000,
        };
        let score = fmt_score * 1_000_000 + rate_dist + ch_pen;
        if best.as_ref().is_none_or(|(s, _)| score < *s) {
            best = Some((score, r));
        }
    }
    let range = best
        .map(|(_, r)| r)
        .ok_or_else(|| AudioError::Backend("mic has no usable input config".into()))?;
    let rate = 48000u32.clamp(range.min_sample_rate(), range.max_sample_rate());
    let cfg = range.with_sample_rate(rate);
    tracing::info!("mic config: {cfg:?}");

    // cpal 0.18 selects the callback sample type at compile time per format,
    // so dispatch explicitly (see build_typed_stream).
    build_typed_stream(&device, &cfg, tx, stop, dropped)
}

fn build_typed_stream(
    device: &cpal::Device,
    cfg: &cpal::SupportedStreamConfig,
    tx: flume::Sender<MicPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<(), AudioError> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    let channels = cfg.channels() as usize;
    let rate = cfg.sample_rate();
    let config: cpal::StreamConfig = (*cfg).into();

    let stop_cb = stop.clone();
    let send_f32 = move |samples: Vec<f32>| {
        if stop_cb.load(Ordering::Relaxed) {
            return;
        }
        if tx
            .try_send(MicPacket {
                data: samples,
                channels,
                rate,
            })
            .is_err()
        {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    };

    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_input_stream(
                config,
                move |data: &[f32], _| send_f32(data.to_vec()),
                |e| tracing::warn!("mic stream error: {e}"),
                None,
            )
            .map_err(|e| AudioError::Backend(e.to_string()))?,
        cpal::SampleFormat::I16 => {
            let send = send_f32;
            device
                .build_input_stream(
                    config,
                    move |data: &[i16], _| send(data.iter().map(|&s| s as f32 / 32768.0).collect()),
                    |e| tracing::warn!("mic stream error: {e}"),
                    None,
                )
                .map_err(|e| AudioError::Backend(e.to_string()))?
        }
        cpal::SampleFormat::U16 => {
            let send = send_f32;
            device
                .build_input_stream(
                    config,
                    move |data: &[u16], _| {
                        send(data.iter().map(|&s| s as f32 / 32768.0 - 1.0).collect())
                    },
                    |e| tracing::warn!("mic stream error: {e}"),
                    None,
                )
                .map_err(|e| AudioError::Backend(e.to_string()))?
        }
        _ => return Err(AudioError::Backend("unsupported mic sample format".into())),
    };
    stream
        .play()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    // Park until shutdown; the cpal callback threads do the work.
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mixer thread (owns WASAPI loopback)
// ---------------------------------------------------------------------------

fn mixer_thread_main(
    config: WinAudioConfig,
    mic_rx: flume::Receiver<MicPacket>,
    mic_stop: Arc<AtomicBool>,
    mixed_tx: flume::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    stats: &StatsInner,
) {
    let mut loopback = if config.capture_system {
        match LoopbackReader::start() {
            Ok(lb) => {
                tracing::info!("system loopback capturing (f32 stereo 48 kHz)");
                Some(lb)
            }
            Err(e) => {
                tracing::warn!("system loopback unavailable, continuing without it: {e}");
                None
            }
        }
    } else {
        None
    };
    // If loopback failed and no mic was requested, emit silence so the encoder
    // still gets a continuous clock (video-only fallback is the caller's call,
    // but a silent track keeps MP4 duration correct).
    if loopback.is_none() && config.mic_query.is_none() {
        tracing::warn!("no audio source active — emitting silence");
    }

    let mut sys_fifo: VecDeque<f32> = VecDeque::with_capacity(MAX_FIFO_FRAMES * 2);
    let mut mic_fifo: VecDeque<f32> = VecDeque::with_capacity(MAX_FIFO_FRAMES * 2);
    let mut sys_pos = 0.0f64;
    let mut mic_pos = 0.0f64;
    let mut mic_rate = OUT_RATE;
    let mut mic_seen = false;
    let mut out_lr = vec![0.0f32; QUANTUM_FRAMES * 2];
    let mut out_bytes = vec![0u8; QUANTUM_BYTES];

    let quantum = Duration::from_nanos(1_000_000_000 / (OUT_RATE as u64 / QUANTUM_FRAMES as u64));
    let mut deadline = Instant::now() + quantum;

    while !stop.load(Ordering::SeqCst) {
        // --- pull system audio (non-blocking drain) ---
        if let Some(lb) = loopback.as_mut() {
            match lb.drain_into(&mut sys_fifo) {
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("loopback lost mid-record, continuing silent: {e}");
                    loopback = None;
                }
            }
            truncate_fifo(&mut sys_fifo);
        }
        // --- pull mic packets ---
        while let Ok(pkt) = mic_rx.try_recv() {
            mic_seen = true;
            mic_rate = pkt.rate;
            push_channels_as_stereo(&mut mic_fifo, &pkt.data, pkt.channels);
            truncate_fifo(&mut mic_fifo);
        }
        let mic_active = config.mic_query.is_some();

        // --- resample each source to one 10 ms quantum ---
        let sys_used = pull_resampled(&mut sys_fifo, &mut sys_pos, OUT_RATE, &mut out_lr, 0);
        if config.capture_system && loopback.is_some() && !sys_used {
            stats.sys_underruns.fetch_add(1, Ordering::Relaxed);
        }
        // out_lr currently holds SYSTEM in both channels; mix mic on top.
        if mic_active {
            let mut mic_q = vec![0.0f32; QUANTUM_FRAMES * 2];
            let mic_used = pull_resampled(&mut mic_fifo, &mut mic_pos, mic_rate, &mut mic_q, 1);
            // Only count starvation after the stream has delivered at least once
            // (startup priming is expected, not an underrun).
            if !mic_used && mic_seen {
                stats.mic_underruns.fetch_add(1, Ordering::Relaxed);
            }
            let lv = config.levels.snapshot();
            for i in 0..QUANTUM_FRAMES * 2 {
                let m = if lv.mic_muted {
                    0.0
                } else {
                    mic_q[i] * lv.mic_gain
                };
                out_lr[i] = (out_lr[i] + m).tanh();
            }
        } else {
            // System-only: soft-clip after gain.
            let lv = config.levels.snapshot();
            if lv.system_muted {
                out_lr.fill(0.0);
            } else if (lv.system_gain - 1.0).abs() > f32::EPSILON {
                for s in out_lr.iter_mut() {
                    *s = (*s * lv.system_gain).tanh();
                }
            } else {
                for s in out_lr.iter_mut() {
                    *s = s.tanh();
                }
            }
        }

        // --- f32 -> i16 LE (+ peak observe for VU meters) ---
        let mut peak = 0.0f32;
        for s in out_lr.iter() {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
        }
        config.levels.observe_peak(peak);
        for (i, s) in out_lr.iter().enumerate() {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            out_bytes[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        if mixed_tx.send(out_bytes.clone()).is_err() {
            break; // encoder gone — stop the pipeline
        }
        stats.quanta.fetch_add(1, Ordering::Relaxed);

        // --- pace ---
        let now = Instant::now();
        if now < deadline {
            thread::sleep(deadline - now);
        } else if now - deadline > Duration::from_millis(100) {
            deadline = Instant::now(); // resync after a stall, don't pile up
        }
        deadline += quantum;
    }

    mic_stop.store(true, Ordering::SeqCst);
    tracing::info!(
        "audio mixer exiting: {} quanta",
        stats.quanta.load(Ordering::Relaxed)
    );
}

fn truncate_fifo(fifo: &mut VecDeque<f32>) {
    let cap = MAX_FIFO_FRAMES * 2;
    if fifo.len() > cap {
        let drop_n = fifo.len() - cap;
        fifo.drain(..drop_n);
    }
}

/// Push device-channel audio as interleaved stereo f32.
fn push_channels_as_stereo(fifo: &mut VecDeque<f32>, data: &[f32], channels: usize) {
    if channels == 0 {
        return;
    }
    if channels == 1 {
        for &s in data {
            fifo.push_back(s);
            fifo.push_back(s);
        }
    } else {
        for frame in data.chunks(channels) {
            fifo.push_back(frame[0]);
            fifo.push_back(frame.get(1).copied().unwrap_or(frame[0]));
        }
    }
}

/// Pull exactly `QUANTUM_FRAMES` stereo frames from `fifo` (device rate
/// `in_rate`), linear-resampling into `out` (48 kHz). Zero-pads shortfall and
/// returns false on underrun (caller counts). `pos` carries fractional phase
/// across quanta for click-free continuity.
fn pull_resampled(
    fifo: &mut VecDeque<f32>,
    pos: &mut f64,
    in_rate: u32,
    out: &mut [f32],
    _ch: usize,
) -> bool {
    debug_assert_eq!(out.len(), QUANTUM_FRAMES * 2);
    let step = in_rate as f64 / OUT_RATE as f64;
    let avail_frames = fifo.len() / 2;
    // Last output frame reads index `pos + step*(N-1)` plus one lookahead for lerp.
    let need_frames = (*pos + step * (QUANTUM_FRAMES - 1) as f64).ceil() as usize + 1;
    let underrun = avail_frames < need_frames;

    for i in 0..QUANTUM_FRAMES {
        let idx = *pos as usize;
        let frac = (*pos - idx as f64) as f32;
        let l0 = fifo.get(idx * 2).copied().unwrap_or(0.0);
        let r0 = fifo.get(idx * 2 + 1).copied().unwrap_or(0.0);
        let l1 = fifo.get(idx * 2 + 2).copied().unwrap_or(0.0);
        let r1 = fifo.get(idx * 2 + 3).copied().unwrap_or(0.0);
        out[i * 2] = l0 + (l1 - l0) * frac;
        out[i * 2 + 1] = r0 + (r1 - r0) * frac;
        *pos += step;
    }
    // Drop consumed frames; on underrun reset phase to self-heal.
    if underrun {
        fifo.clear();
        *pos = 0.0;
        false
    } else {
        let consumed = (*pos as usize).min(avail_frames);
        fifo.drain(..consumed * 2);
        *pos -= consumed as f64;
        true
    }
}

// ---------------------------------------------------------------------------
// WASAPI loopback reader (render endpoint, shared-events, autoconvert to
// f32 stereo 48 kHz so no resampling is needed on the hot path)
// ---------------------------------------------------------------------------

struct LoopbackReader {
    client: wasapi::AudioClient,
    capture: wasapi::AudioCaptureClient,
    event: wasapi::Handle,
    scratch: Vec<u8>,
}

impl LoopbackReader {
    fn start() -> Result<Self, AudioError> {
        let _ = wasapi::initialize_mta();
        let enumerator =
            wasapi::DeviceEnumerator::new().map_err(|e| AudioError::Backend(e.to_string()))?;
        let device = enumerator
            .get_default_device(&wasapi::Direction::Render)
            .map_err(|e| AudioError::Backend(format!("no default render device: {e}")))?;
        let mut client = device
            .get_iaudioclient()
            .map_err(|e| AudioError::Backend(e.to_string()))?;

        let format = wasapi::WaveFormat::new(32, 32, &wasapi::SampleType::Float, 48000, 2, None);
        if format.get_blockalign() != 8 {
            return Err(AudioError::Backend("unexpected loopback blockalign".into()));
        }
        let (_def, min) = client
            .get_device_period()
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        // Render device + Capture direction = AUDCLNT_STREAMFLAGS_LOOPBACK.
        client
            .initialize_client(
                &format,
                &wasapi::Direction::Capture,
                &wasapi::StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: min,
                },
            )
            .map_err(|e| AudioError::Backend(format!("loopback init: {e}")))?;
        let event = client
            .set_get_eventhandle()
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        client
            .start_stream()
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        Ok(Self {
            client,
            capture,
            event,
            scratch: vec![0u8; 480 * 8 * 4],
        })
    }

    /// Drain all pending packets as stereo f32 into `fifo` (zeros when silent).
    fn drain_into(&mut self, fifo: &mut VecDeque<f32>) -> Result<(), AudioError> {
        // Short poll: never block the 10 ms quantum.
        let _ = self.event.wait_for_event(5);
        loop {
            let frames = self
                .capture
                .get_next_packet_size()
                .map_err(|e| AudioError::Backend(e.to_string()))?;
            let frames = match frames {
                Some(n) if n > 0 => n as usize,
                _ => break,
            };
            let need = frames * 8;
            if self.scratch.len() < need {
                self.scratch.resize(need, 0);
            }
            let (read, info) = self
                .capture
                .read_from_device(&mut self.scratch[..need])
                .map_err(|e| AudioError::Backend(e.to_string()))?;
            if info.flags.silent {
                for _ in 0..read as usize {
                    fifo.push_back(0.0);
                    fifo.push_back(0.0);
                }
            } else {
                let (chunks, _) = self.scratch[..read as usize * 8].as_chunks::<4>();
                for chunk in chunks {
                    fifo.push_back(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
            }
        }
        Ok(())
    }
}

impl Drop for LoopbackReader {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_convert_mono_dup() {
        let mut fifo = VecDeque::new();
        push_channels_as_stereo(&mut fifo, &[0.5, -0.5], 1);
        assert_eq!(fifo.len(), 4);
        assert_eq!([fifo[0], fifo[1], fifo[2], fifo[3]], [0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn stereo_convert_multichannel_takes_lr() {
        let mut fifo = VecDeque::new();
        push_channels_as_stereo(&mut fifo, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3);
        assert_eq!(fifo.len(), 4);
        assert_eq!([fifo[0], fifo[1], fifo[2], fifo[3]], [1.0, 2.0, 4.0, 5.0]);
    }

    #[test]
    fn resample_passthrough_48k() {
        let data: Vec<f32> = (0..960).map(|i| i as f32 / 960.0).collect();
        let mut fifo: VecDeque<f32> = data.into();
        let mut pos = 0.0;
        let mut out = vec![0.0; QUANTUM_FRAMES * 2];
        assert!(pull_resampled(&mut fifo, &mut pos, 48000, &mut out, 0));
        // step=1.0: output frame i == input frame i (frac 0).
        // Stereo layout: frame i = samples (2i, 2i+1) = (2i/960, (2i+1)/960).
        assert!((out[0] - 0.0).abs() < 1e-5);
        assert!((out[2] - 2.0 / 960.0).abs() < 1e-4);
        assert!((out[3] - 3.0 / 960.0).abs() < 1e-4);
    }

    #[test]
    fn resample_underrun_zero_pads_and_heals() {
        let mut fifo: VecDeque<f32> = VecDeque::new();
        let mut pos = 0.0;
        let mut out = vec![0.0; QUANTUM_FRAMES * 2];
        assert!(!pull_resampled(&mut fifo, &mut pos, 48000, &mut out, 0));
        assert!(out.iter().all(|&s| s == 0.0));
        // Healed: new data flows normally.
        let data: Vec<f32> = vec![0.25; 960 + 4];
        fifo.extend(data);
        assert!(pull_resampled(&mut fifo, &mut pos, 48000, &mut out, 0));
        assert!((out[0] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn db_gains_match_mixer() {
        assert!((MixerLevels::db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
    }
}
