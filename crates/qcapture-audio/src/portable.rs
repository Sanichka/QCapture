//! Portable audio pipeline (Linux/macOS): cpal loopback and/or mic mixed to
//! one continuous i16 stereo 48 kHz stream, same chunk protocol as Windows.
//!
//! - Linux: system loopback opens the cpal input device whose name contains
//!   "monitor" (PipeWire/ALSA monitor sources). Missing monitor + no mic is
//!   a loud error so the caller can fail soft to video-only; a requested mic
//!   that vanishes is always loud.
//! - macOS: no system loopback API exists (would need a BlackHole-style
//!   driver). `capture_system` is ignored with a warning; mic-only or
//!   video-only.
//!
//! The mixer mirrors `win_audio` (10 ms quanta, zero-padded gaps, tanh
//! soft-clip, VU peak) except the system fifo tracks its own device rate
//! (WASAPI is fixed 48 kHz; cpal loopback is whatever the device runs).

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

/// What to capture. Mirrors [`super::win_audio::WinAudioConfig`] so callers
/// switch pipelines by cfg only.
#[derive(Debug, Clone)]
pub struct PortAudioConfig {
    pub capture_system: bool,
    pub mic_query: Option<String>,
    pub levels: Arc<SharedLevels>,
}

/// Live pipeline. `mixed_rx` yields consecutive 1920-byte i16-stereo-48k chunks.
/// Call [`PortAudioPipeline::shutdown`] after recording to join threads.
pub struct PortAudioPipeline {
    mixed_rx: flume::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    sys_stop: Arc<AtomicBool>,
    mic_stop: Arc<AtomicBool>,
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

impl PortAudioPipeline {
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
        self.sys_stop.store(true, Ordering::SeqCst);
        self.mic_stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.stats()
    }
}

/// Start the pipeline. Loud errors (mic typo, no source at all) let the
/// caller fail soft to video-only; runtime loss stays soft inside the mixer.
pub fn start_pipeline(config: PortAudioConfig) -> Result<PortAudioPipeline, AudioError> {
    if !config.capture_system && config.mic_query.is_none() {
        return Err(AudioError::Backend("no audio source selected".into()));
    }
    // Fail fast on an explicit mic typo (helper thread, never the caller:
    // cpal can init per-thread state we don't want on capture threads).
    if let Some(query) = config.mic_query.clone() {
        thread::Builder::new()
            .name("qcapture-mic-validate".into())
            .spawn(move || cpal_in::check_mic_exists(&query))
            .map_err(|e| AudioError::Backend(format!("validator spawn: {e}")))?
            .join()
            .map_err(|_| AudioError::Backend("mic validator panicked".into()))??;
    }

    let (mic_rx, mic_stop, mic_dropped) = match config.mic_query.clone() {
        Some(query) => cpal_in::spawn_packet_thread(&query, "qcapture-mic")?,
        None => {
            let (_, rx) = flume::bounded::<MicPacket>(64);
            (
                rx,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicU64::new(0)),
            )
        }
    };

    // System loopback: Linux monitor source only; macOS has no OS API
    // (would need a BlackHole-style driver).
    let (sys_rx, sys_stop, sys_rate) = open_system_source(config.capture_system);
    if sys_rx.is_none() && config.mic_query.is_none() {
        return Err(AudioError::Backend(
            "no audio source available (no loopback device, no mic)".into(),
        ));
    }

    let (mixed_tx, mixed_rx) = flume::unbounded::<Vec<u8>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StatsInner::default());
    stats
        .mic_dropped
        .store(mic_dropped.load(Ordering::Relaxed), Ordering::Relaxed);

    let thread_cfg = config.clone();
    let thread_stop = stop.clone();
    let thread_stats = stats.clone();
    let thread_mic_stop = mic_stop.clone();
    let handle = thread::Builder::new()
        .name("qcapture-audio-mix".into())
        .spawn(move || {
            mixer_thread_main(
                thread_cfg,
                sys_rx,
                sys_rate,
                mic_rx,
                thread_mic_stop,
                mixed_tx,
                thread_stop,
                &thread_stats,
            );
        })
        .map_err(|e| AudioError::Backend(format!("mixer thread spawn: {e}")))?;

    Ok(PortAudioPipeline {
        mixed_rx,
        stop,
        sys_stop,
        mic_stop,
        handle: Some(handle),
        stats,
    })
}

type SysSource = (Option<flume::Receiver<MicPacket>>, Arc<AtomicBool>, u32);

/// Open the system loopback source when requested. Linux: first cpal input
/// device with "monitor" in its name (PipeWire/ALSA monitor sources).
/// Other OSes: no API — warn once and continue mic-only/silent.
#[cfg(target_os = "linux")]
fn open_system_source(want: bool) -> SysSource {
    if !want {
        return (None, Arc::new(AtomicBool::new(false)), OUT_RATE);
    }
    match open_monitor_source() {
        Ok((rx, stop, rate)) => (Some(rx), stop, rate),
        Err(e) => {
            tracing::warn!("system loopback unavailable, continuing without it: {e}");
            (None, Arc::new(AtomicBool::new(false)), OUT_RATE)
        }
    }
}

/// Other OSes (macOS): no loopback API exists.
#[cfg(not(target_os = "linux"))]
fn open_system_source(want: bool) -> SysSource {
    if want {
        tracing::warn!("system loopback has no OS API here — recording mic-only (or silent)");
    }
    (None, Arc::new(AtomicBool::new(false)), OUT_RATE)
}

/// First cpal input device with "monitor" in its name.
#[cfg(target_os = "linux")]
fn open_monitor_source() -> Result<(flume::Receiver<MicPacket>, Arc<AtomicBool>, u32), AudioError> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    let device = host
        .input_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?
        .find(|d| d.to_string().to_lowercase().contains("monitor"))
        .ok_or_else(|| {
            AudioError::Backend("no loopback monitor device (need PipeWire/ALSA monitor)".into())
        })?;
    let rate = best_rate_of(&device);
    let (rx, stop, _dropped) =
        cpal_in::spawn_packet_thread_for(device, rate, "loopback device", "qcapture-loop")?;
    Ok((rx, stop, rate))
}

/// Best-effort rate pick for the loopback device (closest to 48 k).
#[cfg(target_os = "linux")]
fn best_rate_of(device: &cpal::Device) -> u32 {
    use cpal::traits::DeviceTrait;
    device
        .supported_input_configs()
        .map(|ranges| {
            ranges
                .map(|r| 48000u32.clamp(r.min_sample_rate(), r.max_sample_rate()))
                .min_by_key(|r| r.abs_diff(48000))
                .unwrap_or(48000)
        })
        .unwrap_or(48000)
}

#[allow(clippy::too_many_arguments)]
fn mixer_thread_main(
    config: PortAudioConfig,
    sys_rx: Option<flume::Receiver<MicPacket>>,
    mut sys_rate: u32,
    mic_rx: flume::Receiver<MicPacket>,
    mic_stop: Arc<AtomicBool>,
    mixed_tx: flume::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    stats: &StatsInner,
) {
    let mut sys_fifo: VecDeque<f32> = VecDeque::with_capacity(MAX_FIFO_FRAMES * 2);
    let mut mic_fifo: VecDeque<f32> = VecDeque::with_capacity(MAX_FIFO_FRAMES * 2);
    let mut sys_pos = 0.0f64;
    let mut mic_pos = 0.0f64;
    let mut mic_rate = OUT_RATE;
    let mut mic_seen = false;
    let mut sys_seen = false;
    let mut out_lr = vec![0.0f32; QUANTUM_FRAMES * 2];
    let mut out_bytes = vec![0u8; QUANTUM_BYTES];

    let quantum = Duration::from_nanos(1_000_000_000 / (OUT_RATE as u64 / QUANTUM_FRAMES as u64));
    let mut deadline = Instant::now() + quantum;

    while !stop.load(Ordering::SeqCst) {
        // --- pull system packets (device rate, stereo-ized) ---
        if let Some(rx) = sys_rx.as_ref() {
            while let Ok(pkt) = rx.try_recv() {
                sys_seen = true;
                sys_rate = pkt.rate;
                push_channels_as_stereo(&mut sys_fifo, &pkt.data, pkt.channels);
                truncate_fifo(&mut sys_fifo);
            }
        }
        // --- pull mic packets ---
        while let Ok(pkt) = mic_rx.try_recv() {
            mic_seen = true;
            mic_rate = pkt.rate;
            push_channels_as_stereo(&mut mic_fifo, &pkt.data, pkt.channels);
            truncate_fifo(&mut mic_fifo);
        }
        let mic_active = config.mic_query.is_some();
        let sys_active = sys_rx.is_some();

        // --- resample each source to one 10 ms quantum ---
        let sys_used = if sys_active {
            pull_resampled(&mut sys_fifo, &mut sys_pos, sys_rate, &mut out_lr, 0)
        } else {
            out_lr.fill(0.0);
            false
        };
        if sys_active && !sys_used && sys_seen {
            stats.sys_underruns.fetch_add(1, Ordering::Relaxed);
        }
        // out_lr holds SYSTEM in both channels; mix mic on top.
        if mic_active {
            let mut mic_q = vec![0.0f32; QUANTUM_FRAMES * 2];
            let mic_used = pull_resampled(&mut mic_fifo, &mut mic_pos, mic_rate, &mut mic_q, 1);
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
