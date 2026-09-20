//! qcapture-audio: device enumeration + single-track mixer types.
//! cpal covers mics everywhere. System loopback is per-OS:
//! Win=WASAPI loopback (`win_audio`, Phase 3), Linux=PipeWire, macOS=SCKit track.
//! Mixing math lives here so it can be unit-tested without hardware.

#[cfg(windows)]
pub mod win_audio;

use cpal::traits::HostTrait;
use qcapture_core::AudioDeviceInfo;

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
