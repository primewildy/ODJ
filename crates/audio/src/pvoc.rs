//! Streaming phase vocoder for tempo-only (key-lock) playback.
//!
//! Standard short-time-Fourier PV:
//! - Hann window applied at both analysis and synthesis (Hann² OLA gain 1.5
//!   at hop = N/4)
//! - N_FFT = 1024, HOP_S = 256 (synthesis hop)
//! - HOP_A (analysis hop) varies with speed: HOP_A = HOP_S * speed_ratio
//! - Fractional HOP_A accumulator avoids long-term drift
//! - Phase: dphase = wrap(observed - expected); true_freq = (expected+dphase)/hop_a;
//!   synth_phase += true_freq * HOP_S
//!
//! Designed to run in the audio thread: no allocations in process/consume,
//! all buffers pre-allocated to MAX_HOP_A.

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex};

pub const N_FFT: usize = 1024;
pub const HOP_S: usize = N_FFT / 4; // 256
pub const MAX_HOP_A: usize = 512;

pub struct PhaseVocoder {
    pub channels: usize,
    fft_fwd: Arc<dyn Fft<f32>>,
    fft_inv: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Reusable scratch for FFT in/out (size N_FFT).
    scratch: Vec<Complex<f32>>,

    // Per-channel state
    in_history: Vec<Vec<f32>>,  // [ch][0..N_FFT]
    last_phase: Vec<Vec<f32>>,  // [ch][bin], len N_FFT/2 + 1
    synth_phase: Vec<Vec<f32>>, // [ch][bin], len N_FFT/2 + 1
    ola: Vec<Vec<f32>>,         // [ch][0..N_FFT] OLA accumulator

    /// Per-frame scratch for identity phase locking. Pre-allocated to N/2+1
    /// to avoid per-callback allocation.
    mag: Vec<f32>,
    phase_in: Vec<f32>,
    nearest_peak: Vec<usize>,

    /// Caller writes hop_a samples per channel here before process_frame.
    pub input_buf: Vec<Vec<f32>>, // [ch][0..MAX_HOP_A]

    /// Output samples ready to consume per channel, at ola[ch][HOP_S - ready..HOP_S].
    ready: usize,

    /// Fractional accumulator for hop_a, so non-integer speeds don't drift.
    hop_a_accum: f64,
}

/// For each bin, write the index of the nearest *magnitude peak* (local
/// max) into `out`. A peak bin's own entry is itself. Pre-allocated `out`
/// of length n_bins; no allocation.
fn map_to_nearest_peak(mag: &[f32], out: &mut [usize]) {
    let n = mag.len();
    debug_assert_eq!(out.len(), n);
    if n == 0 {
        return;
    }
    if n == 1 {
        out[0] = 0;
        return;
    }

    // Forward sweep: each bin inherits the most recent peak seen.
    let mut last_peak: usize = 0;
    let mut have_peak = false;
    for k in 0..n {
        let is_peak = if k == 0 {
            mag[0] > mag[1]
        } else if k == n - 1 {
            mag[n - 1] > mag[n - 2]
        } else {
            mag[k] > mag[k - 1] && mag[k] > mag[k + 1]
        };
        if is_peak {
            last_peak = k;
            have_peak = true;
        }
        out[k] = if have_peak { last_peak } else { k };
    }
    // If no peak was seen at all (uniformly-zero / monotonic spectrum), bail.
    if !have_peak {
        return;
    }

    // Backward sweep: a later peak might be closer for bins to the left
    // of any peak. Replace `out[k]` if the right-side peak is nearer.
    let mut last_peak_r: usize = out[n - 1];
    for k in (0..n).rev() {
        // Detect peaks again (same condition); reuse the existing test.
        let is_peak = if k == 0 {
            mag[0] > mag[1]
        } else if k == n - 1 {
            mag[n - 1] > mag[n - 2]
        } else {
            mag[k] > mag[k - 1] && mag[k] > mag[k + 1]
        };
        if is_peak {
            last_peak_r = k;
        }
        let forward = out[k];
        let dist_forward = (k as isize - forward as isize).unsigned_abs();
        let dist_back = (k as isize - last_peak_r as isize).unsigned_abs();
        if dist_back < dist_forward {
            out[k] = last_peak_r;
        }
    }
}

impl PhaseVocoder {
    pub fn new(channels: usize) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(N_FFT);
        let fft_inv = planner.plan_fft_inverse(N_FFT);
        let window: Vec<f32> = (0..N_FFT)
            .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (N_FFT - 1) as f32).cos())
            .collect();
        let n_bins = N_FFT / 2 + 1;
        Self {
            channels,
            fft_fwd,
            fft_inv,
            window,
            scratch: vec![Complex::new(0.0, 0.0); N_FFT],
            in_history: (0..channels).map(|_| vec![0.0; N_FFT]).collect(),
            last_phase: (0..channels).map(|_| vec![0.0; n_bins]).collect(),
            synth_phase: (0..channels).map(|_| vec![0.0; n_bins]).collect(),
            ola: (0..channels).map(|_| vec![0.0; N_FFT]).collect(),
            mag: vec![0.0; n_bins],
            phase_in: vec![0.0; n_bins],
            nearest_peak: vec![0usize; n_bins],
            input_buf: (0..channels).map(|_| vec![0.0; MAX_HOP_A]).collect(),
            ready: 0,
            hop_a_accum: 0.0,
        }
    }

    /// Clear all state. Used when toggling pitch_lock so stale OLA / phase
    /// from a prior session doesn't leak.
    pub fn reset(&mut self) {
        for ch in 0..self.channels {
            self.in_history[ch].fill(0.0);
            self.last_phase[ch].fill(0.0);
            self.synth_phase[ch].fill(0.0);
            self.ola[ch].fill(0.0);
        }
        self.ready = 0;
        self.hop_a_accum = 0.0;
    }

    /// Compute the next hop_a value given the current speed ratio. Tracks
    /// the fractional residual so the long-term average matches `speed * HOP_S`.
    pub fn next_hop_a(&mut self, speed: f32) -> usize {
        self.hop_a_accum += HOP_S as f64 * speed as f64;
        let h = self.hop_a_accum.floor() as i64;
        let h = h.clamp(1, MAX_HOP_A as i64) as usize;
        self.hop_a_accum -= h as f64;
        h
    }

    /// Number of output samples per channel currently ready to consume.
    pub fn ready(&self) -> usize {
        self.ready
    }

    /// Run one analysis-synthesis frame using `input_buf[ch][0..hop_a]`.
    /// Produces HOP_S new ready output samples (silently overwrites any
    /// unread output — caller should drain before calling).
    pub fn process_frame(&mut self, hop_a: usize) {
        debug_assert!(hop_a > 0 && hop_a <= MAX_HOP_A);
        let two_pi = 2.0 * PI;
        let n_bins = N_FFT / 2 + 1;
        let hop_a_f = hop_a as f32;
        let expected_per_bin = two_pi * hop_a_f / N_FFT as f32;
        let norm = 1.0 / (N_FFT as f32 * 1.5);

        for ch in 0..self.channels {
            // Slide in_history left by hop_a, append the new input.
            self.in_history[ch].copy_within(hop_a..N_FFT, 0);
            let keep_start = N_FFT - hop_a;
            self.in_history[ch][keep_start..N_FFT]
                .copy_from_slice(&self.input_buf[ch][..hop_a]);

            // Window + forward FFT
            for i in 0..N_FFT {
                self.scratch[i] = Complex::new(
                    self.in_history[ch][i] * self.window[i],
                    0.0,
                );
            }
            self.fft_fwd.process(&mut self.scratch);

            // Pass 1: extract magnitudes and input phases for the positive
            // freq half. Reusing pre-allocated buffers — no allocation here.
            for k in 0..n_bins {
                let c = self.scratch[k];
                self.mag[k] = (c.re * c.re + c.im * c.im).sqrt();
                self.phase_in[k] = c.im.atan2(c.re);
            }

            // Pass 2: identity phase locking — for each bin, find the
            // nearest *magnitude peak* (local max). Then advance only
            // peak phases via the PV formula and lock non-peak bins to
            // track their nearest peak, preserving the relative input
            // phase. This stops adjacent bins drifting apart, which is
            // what causes the chorusy "amplitude wobble" — especially
            // audible in the high end where bins per critical band is
            // largest.
            map_to_nearest_peak(&self.mag, &mut self.nearest_peak);

            for k in 0..n_bins {
                if self.nearest_peak[k] != k {
                    continue; // non-peak — handled below
                }
                let expected = expected_per_bin * k as f32;
                let mut dphase = self.phase_in[k] - self.last_phase[ch][k] - expected;
                dphase -= two_pi * (dphase / two_pi).round();
                let true_freq = (expected + dphase) / hop_a_f;

                self.synth_phase[ch][k] += true_freq * HOP_S as f32;
                let sp = self.synth_phase[ch][k];
                self.synth_phase[ch][k] = sp - two_pi * (sp / two_pi).floor();
            }

            for k in 0..n_bins {
                let p = self.nearest_peak[k];
                if k != p {
                    // Lock to nearest peak, preserving the *input* phase
                    // relationship between this bin and its peak.
                    self.synth_phase[ch][k] =
                        self.synth_phase[ch][p] + (self.phase_in[k] - self.phase_in[p]);
                }
                self.last_phase[ch][k] = self.phase_in[k];

                let sp = self.synth_phase[ch][k];
                self.scratch[k] = Complex::new(self.mag[k] * sp.cos(), self.mag[k] * sp.sin());
            }
            // Hermitian symmetry for the negative-frequency half so iFFT
            // yields a real signal.
            for k in 1..N_FFT / 2 {
                let m = N_FFT - k;
                self.scratch[m] = self.scratch[k].conj();
            }
            // DC and Nyquist must be real.
            self.scratch[0] = Complex::new(self.scratch[0].re, 0.0);
            self.scratch[N_FFT / 2] = Complex::new(self.scratch[N_FFT / 2].re, 0.0);

            self.fft_inv.process(&mut self.scratch);

            // Shift OLA left by HOP_S; zero new tail; add windowed iFFT.
            self.ola[ch].copy_within(HOP_S..N_FFT, 0);
            let tail = N_FFT - HOP_S;
            for i in tail..N_FFT {
                self.ola[ch][i] = 0.0;
            }
            for i in 0..N_FFT {
                self.ola[ch][i] += self.scratch[i].re * self.window[i] * norm;
            }
        }

        self.ready = HOP_S;
    }

    /// Mix up to `want` ready samples (per channel) into `out` (interleaved
    /// across `out_channels`), applying linear `gain`. Returns the number
    /// of samples per channel written.
    pub fn consume(&mut self, out: &mut [f32], out_channels: usize, gain: f32, want: usize) -> usize {
        let take = want.min(self.ready);
        if take == 0 {
            return 0;
        }
        let start = HOP_S - self.ready;
        for i in 0..take {
            for c in 0..out_channels {
                let src_c = c.min(self.channels.saturating_sub(1));
                out[i * out_channels + c] += self.ola[src_c][start + i] * gain;
            }
        }
        self.ready -= take;
        take
    }
}
