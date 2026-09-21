//! qcapture-audio: device enumeration + single-track mixer types.
//! cpal covers mics everywhere. System loopback is per-OS: Win=WASAPI
//! loopback (`win_audio`), Linux=PipeWire/ALSA monitor source via cpal
//! (`portable`), macOS has no loopback API (mic-only, fail-soft).
//! Mixing math lives in `dsp` so it can be unit-tested without hardware.

/// Portable cpal input (mic everywhere, loopback source lookup).
pub mod cpal_in;
/// Portable DSP: quanta, channel conversion, resampling.
pub mod dsp;
/// Portable pipeline (non-Windows): cpal loopback/mic mixer.
#[cfg(not(windows))]
pub mod portable;
#[cfg(windows)]
pub mod win_audio;

use cpal::traits::HostTrait;
use qcapture_core::AudioDeviceInfo;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio host available: {0}")]
    NoHost(String),
    #[error("device query failed: {0}")]
    Backend(String),
}

/// List input (mic) devices via cpal. Loopback devices are enumerated
/// by OS-specific code in Phase 3; here is_loopback=false for all.
pub fn list_input_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    // cpal 0.18: Device name via Display (`to_string()`); disconnected devices
    // surface as empty strings — filter them so UI pickers stay clean.
    Ok(devices
        .map(|d| d.to_string())
        .filter(|name| !name.is_empty())
        .map(|name| AudioDeviceInfo {
            name,
            is_input: true,
            is_loopback: false,
        })
        .collect())
}

/// List output devices (for future loopback pickers). Not loopback capture itself.
pub fn list_output_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    Ok(devices
        .map(|d| d.to_string())
        .filter(|name| !name.is_empty())
        .map(|name| AudioDeviceInfo {
            name,
            is_input: false,
            is_loopback: false,
        })
        .collect())
}

/// Single-track mixer state (UI thread owns gains/mutes as atomics in production;
/// this struct is the testable math core: gain -> mix -> soft clip).
#[derive(Debug, Clone)]
pub struct MixerLevels {
    pub mic_gain: f32,
    pub system_gain: f32,
    pub mic_muted: bool,
    pub system_muted: bool,
}

impl Default for MixerLevels {
    fn default() -> Self {
        Self {
            mic_gain: 1.0,
            system_gain: 1.0,
            mic_muted: false,
            system_muted: false,
        }
    }
}

impl MixerLevels {
    /// Mix one mono sample pair. Gains are linear (1.0 = 0dB).
    /// Soft-clips with tanh to avoid hard digital clipping on hot mics.
    pub fn mix_mono(&self, mic: f32, system: f32) -> f32 {
        let m = if self.mic_muted {
            0.0
        } else {
            mic * self.mic_gain
        };
        let s = if self.system_muted {
            0.0
        } else {
            system * self.system_gain
        };
        (m + s).tanh()
    }

    /// dB -> linear helper for sliders (-60dB..+12dB).
    pub fn db_to_linear(db: f32) -> f32 {
        10.0_f32.powf(db / 20.0)
    }
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

/// Runtime stats for the final log line / VU meters. Shared by both
/// pipelines so callers report identically on every OS.
#[derive(Debug, Default)]
pub struct AudioStats {
    pub quanta_emitted: u64,
    pub sys_underruns: u64,
    pub mic_underruns: u64,
    pub mic_packets_dropped: u64,
}

/// Either OS pipeline behind one interface, so record flows (CLI + widget)
/// stay single-path across platforms. Construction still differs per OS
/// (different config types); consumption is identical.
pub enum AnyAudioPipeline {
    #[cfg(windows)]
    Win(win_audio::WinAudioPipeline),
    #[cfg(not(windows))]
    Port(portable::PortAudioPipeline),
}

impl AnyAudioPipeline {
    pub fn mixed_rx(&self) -> flume::Receiver<Vec<u8>> {
        match self {
            #[cfg(windows)]
            Self::Win(p) => p.mixed_rx(),
            #[cfg(not(windows))]
            Self::Port(p) => p.mixed_rx(),
        }
    }

    pub fn shutdown(self) -> AudioStats {
        match self {
            #[cfg(windows)]
            Self::Win(p) => p.shutdown(),
            #[cfg(not(windows))]
            Self::Port(p) => p.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_works() {
        let mix = MixerLevels {
            mic_muted: true,
            ..Default::default()
        };
        let out = mix.mix_mono(0.9, 0.1);
        assert!(out < 0.5, "muted mic should not dominate");
    }

    #[test]
    fn soft_clip_bounded() {
        let mix = MixerLevels::default();
        let out = mix.mix_mono(10.0, 10.0);
        assert!(out.abs() <= 1.0);
    }

    #[test]
    fn db_conversion() {
        assert!((MixerLevels::db_to_linear(0.0) - 1.0).abs() < 1e-5);
        assert!((MixerLevels::db_to_linear(-20.0) - 0.1).abs() < 1e-4);
    }
}
