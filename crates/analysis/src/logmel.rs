//! Log-mel spectrogram matching beat_this' `torchaudio.MelSpectrogram`
//! configuration bit-for-bit (within FP rounding). The settings —
//! Slaney mel scale, periodic Hann window, frame-length normalisation,
//! `log1p(1000 * mel)` — are what the model was trained on, so any
//! drift here silently degrades downbeat detection.
//!
//! The match is validated by `tests/logmel_matches_torchaudio.rs` which
//! diffs against a Python-dumped reference for the first 5 s of
//! Moonlight. See `downbeat-spike/dump_reference.py` for the dumper.

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex};

pub const SR: u32 = 22050;
pub const N_FFT: usize = 1024;
pub const HOP: usize = 441;
pub const N_MELS: usize = 128;
const F_MIN: f32 = 30.0;
const F_MAX: f32 = 11000.0;
const LOG_MULT: f32 = 1000.0;

/// Log-mel spectrogram computer. Construct once at startup (it builds
/// the FFT plan + mel filterbank) and reuse for many `compute` calls.
pub struct LogMel {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Sparse mel filterbank: `fb[m]` is the list of `(bin, weight)`
    /// pairs that contribute to mel band `m`. Skipping zero-weight bins
    /// is ~4× faster than dense multiplication for typical FFT sizes.
    fb: Vec<Vec<(usize, f32)>>,
    /// `1 / sqrt(n_fft)` — torchaudio's `normalized="frame_length"`
    /// scales each complex STFT value by this. Since magnitude is
    /// linear, we apply it post-hoc to the summed mel band.
    mag_norm: f32,
}

impl Default for LogMel {
    fn default() -> Self {
        Self::new()
    }
}

impl LogMel {
    pub fn new() -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(N_FFT);
        // Periodic Hann (torch.hann_window default) — divisor is N, not N-1.
        let window: Vec<f32> = (0..N_FFT)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / N_FFT as f32).cos())
            .collect();
        let n_freqs = N_FFT / 2 + 1;
        let dense = build_slaney_mel_fb(SR, N_FFT, N_MELS, F_MIN, F_MAX);
        let mut fb: Vec<Vec<(usize, f32)>> = vec![Vec::new(); N_MELS];
        for k in 0..n_freqs {
            for m in 0..N_MELS {
                let w = dense[k * N_MELS + m];
                if w > 0.0 {
                    fb[m].push((k, w));
                }
            }
        }
        Self {
            fft,
            window,
            fb,
            mag_norm: 1.0 / (N_FFT as f32).sqrt(),
        }
    }

    /// Compute the log-mel spectrogram with `center=True` reflection
    /// padding. Returns a flat row-major `(n_frames, N_MELS)` buffer.
    pub fn compute(&self, samples: &[f32]) -> Vec<f32> {
        let pad = N_FFT / 2;
        let total_len = samples.len() + 2 * pad;
        if total_len < N_FFT {
            return Vec::new();
        }
        let n_frames = (total_len - N_FFT) / HOP + 1;
        let mut out = vec![0.0_f32; n_frames * N_MELS];
        let mut scratch = vec![Complex::new(0.0, 0.0); N_FFT];

        for fi in 0..n_frames {
            let start = fi * HOP; // position in (virtually) padded signal
            for i in 0..N_FFT {
                let s = self.padded_sample(samples, start + i, pad);
                scratch[i] = Complex::new(s * self.window[i], 0.0);
            }
            self.fft.process(&mut scratch);
            for m in 0..N_MELS {
                let mut sum = 0.0_f32;
                for &(k, w) in &self.fb[m] {
                    sum += scratch[k].norm() * w;
                }
                let mag = sum * self.mag_norm;
                out[fi * N_MELS + m] = (LOG_MULT * mag).ln_1p();
            }
        }
        out
    }

    /// PyTorch-style reflection padding. For left padding of width
    /// `pad`, the padded array at position `k` (`k < pad`) returns
    /// `samples[pad - k]` — mirror across `samples[0]`, excluding it.
    /// Right padding is the same trick mirrored at the end.
    fn padded_sample(&self, samples: &[f32], idx: usize, pad: usize) -> f32 {
        let len = samples.len();
        if idx < pad {
            samples[pad - idx]
        } else if idx < pad + len {
            samples[idx - pad]
        } else {
            // Right-side reflection across samples[len - 1].
            let off = idx - pad - len + 1;
            samples[len - 1 - off]
        }
    }
}

/// Triangular Slaney-scale mel filterbank. Returns a dense row-major
/// `(n_freqs, n_mels)` matrix.
fn build_slaney_mel_fb(
    sr: u32,
    n_fft: usize,
    n_mels: usize,
    f_min: f32,
    f_max: f32,
) -> Vec<f32> {
    let n_freqs = n_fft / 2 + 1;
    let mel_lo = hz_to_slaney_mel(f_min);
    let mel_hi = hz_to_slaney_mel(f_max);
    let mel_pts: Vec<f32> = (0..n_mels + 2)
        .map(|i| mel_lo + (mel_hi - mel_lo) * (i as f32) / (n_mels as f32 + 1.0))
        .collect();
    let hz_pts: Vec<f32> = mel_pts.iter().map(|&m| slaney_mel_to_hz(m)).collect();
    let mut fb = vec![0.0_f32; n_freqs * n_mels];
    for k in 0..n_freqs {
        let f = k as f32 * sr as f32 / n_fft as f32;
        for m in 0..n_mels {
            let lower = hz_pts[m];
            let center = hz_pts[m + 1];
            let upper = hz_pts[m + 2];
            let w = if f >= lower && f <= center && center > lower {
                (f - lower) / (center - lower)
            } else if f > center && f <= upper && upper > center {
                (upper - f) / (upper - center)
            } else {
                0.0
            };
            if w > 0.0 {
                fb[k * n_mels + m] = w;
            }
        }
    }
    fb
}

const F_SP: f32 = 200.0 / 3.0;
const MIN_LOG_HZ: f32 = 1000.0;

fn hz_to_slaney_mel(f: f32) -> f32 {
    let min_log_mel = MIN_LOG_HZ / F_SP;
    let logstep = (6.4_f32).ln() / 27.0;
    if f >= MIN_LOG_HZ {
        min_log_mel + (f / MIN_LOG_HZ).ln() / logstep
    } else {
        f / F_SP
    }
}

fn slaney_mel_to_hz(m: f32) -> f32 {
    let min_log_mel = MIN_LOG_HZ / F_SP;
    let logstep = (6.4_f32).ln() / 27.0;
    if m >= min_log_mel {
        MIN_LOG_HZ * ((m - min_log_mel) * logstep).exp()
    } else {
        F_SP * m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slaney_mel_roundtrip() {
        for &hz in &[0.0_f32, 100.0, 500.0, 999.0, 1000.0, 5000.0, 11000.0] {
            let m = hz_to_slaney_mel(hz);
            let back = slaney_mel_to_hz(m);
            assert!((back - hz).abs() < 1e-3, "roundtrip {hz} -> {m} -> {back}");
        }
    }
}
