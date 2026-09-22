//! Windows audio pipeline (Phase 3a): WASAPI loopback (system) + cpal mic,
//! mixed to one continuous i16 stereo 48 kHz stream for the AAC encoder.
//!
//! Threading: the mixer thread owns the WASAPI loopback client and drains a
//! cpal mic stream (created on its own thread for COM-apartment isolation).
//! Every 10 ms it emits exactly one 480-frame quantum — zero-padding gaps —
//! so the encoder's monotonic audio clock never stalls and A/V stay in sync.
//! Silence (idle desktop, muted sources) is real zeros, not gaps.

use super::cpal_in::{self, MicPacket};
use super::dsp::{
    pull_resampled, push_channels_as_stereo, truncate_fifo, MAX_FIFO_FRAMES, OUT_RATE,
    QUANTUM_BYTES, QUANTUM_FRAMES,
};
use super::{AudioError, AudioStats, SharedLevels};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// What to capture. Levels are shared (`Arc`) so a UI can move sliders
/// mid-recording; the mixer snapshots them once per 10 ms quantum.
#[derive(Debug, Clone)]
pub struct WinAudioConfig {
    pub capture_system: bool,
    pub mic_query: Option<String>,
    pub levels: std::sync::Arc<SharedLevels>,
    /// Pause flag shared with the video backend: while set the mixer emits
    /// nothing and discards inputs, freezing the audio clock in step with
    /// the frozen video clock.
    pub pause: Arc<AtomicBool>,
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
            .spawn(move || cpal_in::check_mic_exists(&query))
            .map_err(|e| AudioError::Backend(format!("validator spawn: {e}")))?
            .join()
            .map_err(|_| AudioError::Backend("mic validator panicked".into()))??;
    }

    // Mic packets flow callback-thread -> mixer-thread. The cpal stream must
    // live on its own thread; the JoinHandle stays out of the mixer so a
    // stuck device doesn't block shutdown (we only signal + detach).
    let (mic_rx, mic_stop, mic_dropped) = match config.mic_query.clone() {
        Some(query) => {
            let (rx, stop, dropped) = cpal_in::spawn_packet_thread(&query, "qcapture-mic")?;
            (rx, stop, dropped)
        }
        None => {
            let (_, rx) = flume::bounded::<MicPacket>(64);
            (
                rx,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicU64::new(0)),
            )
        }
    };

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
    let pause = config.pause.clone();

    while !stop.load(Ordering::SeqCst) {
        // --- paused: freeze the audio clock (discard inputs, emit nothing).
        // The deadline is left stale so the stall-resync below re-arms
        // pacing on resume; fifos are cleared so no burst plays.
        if pause.load(Ordering::Relaxed) {
            if let Some(lb) = loopback.as_mut() {
                let _ = lb.drain_into(&mut sys_fifo);
            }
            sys_fifo.clear();
            while mic_rx.try_recv().is_ok() {}
            mic_fifo.clear();
            thread::sleep(Duration::from_millis(10));
            continue;
        }
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
