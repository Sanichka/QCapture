//! Portable audio DSP: quantum pacing constants, channel conversion and
//! linear resampling shared by the Windows WASAPI mixer and the portable
//! cpal mixer. Pure math, no OS calls — unit-tested without hardware.

use std::collections::VecDeque;

pub const OUT_RATE: u32 = 48_000;
pub const QUANTUM_FRAMES: usize = 480; // 10 ms @ 48 kHz
pub const QUANTUM_BYTES: usize = QUANTUM_FRAMES * 2 * 2; // stereo i16
pub const MAX_FIFO_FRAMES: usize = OUT_RATE as usize * 2; // 2 s backlog cap

pub fn truncate_fifo(fifo: &mut VecDeque<f32>) {
    let cap = MAX_FIFO_FRAMES * 2;
    if fifo.len() > cap {
        let drop_n = fifo.len() - cap;
        fifo.drain(..drop_n);
    }
}

/// Push device-channel audio as interleaved stereo f32.
pub fn push_channels_as_stereo(fifo: &mut VecDeque<f32>, data: &[f32], channels: usize) {
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
pub fn pull_resampled(
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
}
