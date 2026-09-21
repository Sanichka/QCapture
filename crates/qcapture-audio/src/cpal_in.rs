//! Portable cpal input: device lookup by substring, typed stream builders and
//! the packet-thread main loop. Used by the mic path on every OS and by the
//! Linux loopback path (a PipeWire/ALSA monitor source is just another cpal
//! input device). No WASAPI, no COM — safe on any thread.

use super::AudioError;
use cpal::traits::DeviceTrait;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

/// Raw input packet from a cpal callback thread (device rate/channels, f32).
pub struct MicPacket {
    pub data: Vec<f32>,
    pub channels: usize,
    pub rate: u32,
}

/// Packet receiver + stop flag + drop counter handed to a mixer thread.
pub(crate) type PacketEnds = (flume::Receiver<MicPacket>, Arc<AtomicBool>, Arc<AtomicU64>);

/// Find an input device by substring, case-insensitive (`"default"` or empty
/// = system default). Same matching rule everywhere so `--mic` behaves
/// identically on all OSes.
pub fn find_input_device(query: &str) -> Result<cpal::Device, AudioError> {
    use cpal::traits::HostTrait;
    let host = cpal::default_host();
    if query.eq_ignore_ascii_case("default") || query.is_empty() {
        return host
            .default_input_device()
            .ok_or_else(|| AudioError::NoHost("no default input device".into()));
    }
    let lower = query.to_lowercase();
    host.input_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?
        .find(|d| d.to_string().to_lowercase().contains(&lower))
        .ok_or_else(|| {
            AudioError::Backend(format!(
                "input '{query}' not found — see `qcapture --list-audio`"
            ))
        })
}

/// Synchronous mic lookup with the same matching rule as the capture thread.
/// Called at pipeline start so an explicit `--mic typo` bails instead of
/// silently recording without the mic.
pub fn check_mic_exists(query: &str) -> Result<(), AudioError> {
    find_input_device(query).map(|_| ())
}

pub(crate) fn mic_thread_main(
    query: &str,
    tx: flume::Sender<MicPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<(), AudioError> {
    let device = find_input_device(query)?;
    packet_thread_main(device, "input device", tx, stop, dropped)
}

/// Packet-thread main for an already-resolved device (loopback sources
/// resolve by role, not by user query).
pub(crate) fn packet_thread_main(
    device: cpal::Device,
    role: &str,
    tx: flume::Sender<MicPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<(), AudioError> {
    tracing::info!("{role}: {}", device.to_string());

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
        .ok_or_else(|| AudioError::Backend("input has no usable config".into()))?;
    let rate = 48000u32.clamp(range.min_sample_rate(), range.max_sample_rate());
    let cfg = range.with_sample_rate(rate);
    tracing::info!("input config: {cfg:?}");

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
                |e| tracing::warn!("input stream error: {e}"),
                None,
            )
            .map_err(|e| AudioError::Backend(e.to_string()))?,
        cpal::SampleFormat::I16 => {
            let send = send_f32;
            device
                .build_input_stream(
                    config,
                    move |data: &[i16], _| send(data.iter().map(|&s| s as f32 / 32768.0).collect()),
                    |e| tracing::warn!("input stream error: {e}"),
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
                    |e| tracing::warn!("input stream error: {e}"),
                    None,
                )
                .map_err(|e| AudioError::Backend(e.to_string()))?
        }
        _ => {
            return Err(AudioError::Backend(
                "unsupported input sample format".into(),
            ))
        }
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

/// Spawn a detached packet thread for an already-resolved query.
/// Returns the packet receiver, stop flag and drop counter for the mixer.
pub(crate) fn spawn_packet_thread(
    query: &str,
    thread_name: &str,
) -> Result<PacketEnds, AudioError> {
    let (tx, rx) = flume::bounded::<MicPacket>(64);
    let stop = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let q = query.to_string();
    let tx2 = tx.clone();
    let stop2 = stop.clone();
    let dropped2 = dropped.clone();
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = mic_thread_main(&q, tx2, stop2, dropped2) {
                tracing::warn!("input thread exited: {e}");
            }
        })
        .map_err(|e| AudioError::Backend(format!("input thread spawn: {e}")))?;
    drop(tx);
    Ok((rx, stop, dropped))
}

/// Spawn a detached packet thread for an already-resolved device at a
/// pre-picked rate. Returns receiver, stop flag, drop counter and the rate
/// the mixer should resample from. Loopback path only (non-Windows).
#[cfg(not(windows))]
pub(crate) fn spawn_packet_thread_for(
    device: cpal::Device,
    rate: u32,
    role: &'static str,
    thread_name: &str,
) -> Result<PacketEnds, AudioError> {
    let (tx, rx) = flume::bounded::<MicPacket>(64);
    let stop = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let tx2 = tx.clone();
    let stop2 = stop.clone();
    let dropped2 = dropped.clone();
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            if let Err(e) = packet_thread_main_for(device, rate, role, tx2, stop2, dropped2) {
                tracing::warn!("input thread exited: {e}");
            }
        })
        .map_err(|e| AudioError::Backend(format!("input thread spawn: {e}")))?;
    drop(tx);
    Ok((rx, stop, dropped))
}

/// Packet-thread main for a device with an explicit rate (loopback path).
#[cfg(not(windows))]
fn packet_thread_main_for(
    device: cpal::Device,
    rate: u32,
    role: &str,
    tx: flume::Sender<MicPacket>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> Result<(), AudioError> {
    use cpal::traits::DeviceTrait;
    tracing::info!("{role}: {}", device.to_string());
    let supported = device
        .supported_input_configs()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    // Prefer a range containing the rate, else closest min/max.
    let mut best: Option<cpal::SupportedStreamConfig> = None;
    for r in supported {
        if rate < r.min_sample_rate() || rate > r.max_sample_rate() {
            continue;
        }
        let cfg = r.with_sample_rate(rate);
        let score = match cfg.sample_format() {
            cpal::SampleFormat::F32 => 0,
            cpal::SampleFormat::I16 => 1,
            cpal::SampleFormat::U16 => 2,
            _ => 3,
        } * 1000
            + cfg.channels() as i32;
        if best.as_ref().is_none_or(|b| {
            let bs = match b.sample_format() {
                cpal::SampleFormat::F32 => 0,
                cpal::SampleFormat::I16 => 1,
                cpal::SampleFormat::U16 => 2,
                _ => 3,
            } * 1000
                + b.channels() as i32;
            score < bs
        }) {
            best = Some(cfg);
        }
    }
    let cfg = best.ok_or_else(|| AudioError::Backend("input has no usable config".into()))?;
    build_typed_stream(&device, &cfg, tx, stop, dropped)
}
