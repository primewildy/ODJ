//! Per-track BPM + beat-grid detection (v1 — DSP only, no ML).
//!
//! Pipeline:
//!   1. Block-average to mono at ~11025 Hz
//!   2. STFT (1024 frame, 512 hop, Hann)
//!   3. Spectral-flux onset envelope (positive differences only)
//!   4. Subtract local mean (median-like detrending)
//!   5. Autocorrelate envelope; peak in lag range = beat period
//!   6. Phase: try N sub-frame offsets, pick offset maximising envelope sum
//!      at predicted beat positions
//!   7. Generate beat times in seconds
//!
//! Assumes 4/4-ish steady tempo (true for ~all dance music). Bad for
//! deliberate tempo changes or non-percussive material — handle in v1.5+.

use std::f32::consts::PI;

use control::{MusicalKey, TrackBuffer};
use rustfft::{FftPlanner, num_complex::Complex};

pub mod downbeat;

pub struct AnalysisResult {
    pub bpm: f32,
    /// Beat times in seconds from the start of the track.
    pub beat_grid: Vec<f64>,
    pub key: Option<MusicalKey>,
}

/// Krumhansl-Kessler key profiles (major + minor in C, rotated for other tonics).
const PROFILE_MAJOR: [f32; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const PROFILE_MINOR: [f32; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];

const TARGET_RATE: u32 = 11025;
const N_FFT: usize = 1024;
const HOP: usize = 512;
const MIN_BPM: f32 = 60.0;
const MAX_BPM: f32 = 200.0;
const N_PHASES: usize = 32;

pub fn analyse(buf: &TrackBuffer) -> AnalysisResult {
    let sr = buf.sample_rate;
    let ch = buf.channels.max(1) as usize;
    let total = buf.frames();
    if total < sr as usize {
        return AnalysisResult {
            bpm: 0.0,
            beat_grid: Vec::new(),
            key: None,
        };
    }

    // 1. Decimate to ~TARGET_RATE mono via block average.
    let decim = (sr / TARGET_RATE).max(1) as usize;
    let actual_rate = sr / decim as u32;
    let n_out = total / decim;
    let mut mono = Vec::<f32>::with_capacity(n_out);
    for out_i in 0..n_out {
        let in_start = out_i * decim;
        let mut sum = 0.0f32;
        for k in 0..decim {
            let frame_idx = (in_start + k) * ch;
            for c in 0..ch {
                sum += buf.samples[frame_idx + c];
            }
        }
        mono.push(sum / (decim * ch) as f32);
    }

    if mono.len() < N_FFT * 2 {
        return AnalysisResult {
            bpm: 0.0,
            beat_grid: Vec::new(),
            key: None,
        };
    }

    // 2-3. Spectral-flux onset envelope + chroma accumulation (for key
    // detection in step 7).
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(N_FFT);
    let window: Vec<f32> = (0..N_FFT)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / (N_FFT - 1) as f32).cos())
        .collect();

    // Precompute bin → pitch class table for the chroma accumulator.
    // Skip very low (rumble) and very high (cymbal sheen) bins where pitch
    // class is unreliable.
    let bin_to_pc: Vec<Option<usize>> = (0..(N_FFT / 2))
        .map(|k| {
            let freq = k as f32 * actual_rate as f32 / N_FFT as f32;
            if !(60.0..=4000.0).contains(&freq) {
                return None;
            }
            // MIDI note number 69 = A4 = 440 Hz. Pitch class = MIDI % 12,
            // where 0 = C.
            let midi = 69.0 + 12.0 * (freq / 440.0).log2();
            let pc = midi.round().rem_euclid(12.0) as usize;
            Some(pc)
        })
        .collect();
    let mut chroma = [0.0f32; 12];

    let mut flux = Vec::<f32>::new();
    let mut prev_mag = vec![0.0f32; N_FFT / 2];
    let mut cur_mag = vec![0.0f32; N_FFT / 2];
    let mut scratch = vec![Complex::<f32>::new(0.0, 0.0); N_FFT];

    let mut start = 0usize;
    while start + N_FFT <= mono.len() {
        for i in 0..N_FFT {
            scratch[i] = Complex::new(mono[start + i] * window[i], 0.0);
        }
        fft.process(&mut scratch);
        for k in 0..(N_FFT / 2) {
            cur_mag[k] = scratch[k].norm();
        }
        let mut sf = 0.0f32;
        for k in 0..(N_FFT / 2) {
            let d = cur_mag[k] - prev_mag[k];
            if d > 0.0 {
                sf += d;
            }
            if let Some(pc) = bin_to_pc[k] {
                chroma[pc] += cur_mag[k];
            }
        }
        flux.push(sf);
        std::mem::swap(&mut prev_mag, &mut cur_mag);
        start += HOP;
    }

    // 4. Subtract local mean (~1 sec window) to detrend.
    let frame_rate = actual_rate as f32 / HOP as f32; // ~21.5 Hz
    let half_win = (frame_rate * 0.5) as usize; // ~10 frames each side
    let env: Vec<f32> = (0..flux.len())
        .map(|i| {
            let s = i.saturating_sub(half_win);
            let e = (i + half_win + 1).min(flux.len());
            let avg = flux[s..e].iter().sum::<f32>() / (e - s) as f32;
            (flux[i] - avg).max(0.0)
        })
        .collect();

    // 5. Autocorrelation: find peak in lag range = beat period.
    //    Need neighbours of the peak for sub-frame refinement, so store the
    //    whole autocorrelation curve over the search range.
    let min_lag = ((frame_rate * 60.0 / MAX_BPM).round() as usize).max(2);
    let max_lag = ((frame_rate * 60.0 / MIN_BPM).round() as usize).max(min_lag + 1);
    let mut autocorr = vec![0.0f32; max_lag + 2];
    for lag in min_lag..=max_lag {
        let mut s = 0.0f32;
        for i in lag..env.len() {
            s += env[i] * env[i - lag];
        }
        autocorr[lag] = s;
    }
    let mut best_lag = min_lag;
    let mut best_score = f32::MIN;
    for lag in min_lag..=max_lag {
        if autocorr[lag] > best_score {
            best_score = autocorr[lag];
            best_lag = lag;
        }
    }

    // Parabolic interpolation around the peak for sub-frame precision.
    // Without this, BPM is quantised to 60 * frame_rate / N integer lags,
    // which means a 127 BPM track snaps to 129.2 BPM (lag 10) and the grid
    // drifts ~1.7%.
    let refined_lag = if best_lag > min_lag && best_lag < max_lag {
        let y0 = autocorr[best_lag - 1];
        let y1 = autocorr[best_lag];
        let y2 = autocorr[best_lag + 1];
        let denom = y0 - 2.0 * y1 + y2;
        if denom.abs() > 1e-9 {
            let delta = 0.5 * (y0 - y2) / denom;
            best_lag as f32 + delta.clamp(-1.0, 1.0)
        } else {
            best_lag as f32
        }
    } else {
        best_lag as f32
    };

    // Defensive half/double tempo bias: most dance music sits in [90, 180]
    // BPM. If autocorr picked the half/double, shift it.
    let mut rough_bpm = 60.0 * frame_rate / refined_lag;
    while rough_bpm < 80.0 {
        rough_bpm *= 2.0;
    }
    while rough_bpm > 180.0 {
        rough_bpm /= 2.0;
    }

    // 5b. Fine BPM refinement via brute-force phase-aligned scoring.
    //
    // Autocorrelation + parabolic interp gives the rough period, but the
    // autocorr curve around the peak isn't a clean parabola so the refined
    // value can still be off by ~1 BPM (observed: 127 BPM track reading as
    // 128.4). Direct phase scoring is the right objective: for each
    // candidate BPM, compute the best phase score (mean envelope at
    // predicted beat positions) and pick the candidate with the highest
    // mean. Search ±5 BPM around the rough estimate at 0.05 BPM resolution.
    //
    // Cost: ~200 BPMs × N_PHASES × beats_in_track ≈ a few million ops.
    let search_window = 5.0_f32;
    let bpm_step = 0.05_f32;
    let lo = (rough_bpm - search_window).max(MIN_BPM);
    let hi = (rough_bpm + search_window).min(MAX_BPM);
    let n_steps = ((hi - lo) / bpm_step).ceil() as usize;

    let mut bpm = rough_bpm;
    let mut best_refine_score = f32::MIN;
    for step in 0..=n_steps {
        let candidate_bpm = lo + step as f32 * bpm_step;
        let candidate_period = 60.0 * frame_rate / candidate_bpm;
        // Best phase score for this BPM, normalised by beat count.
        let mut best_phase_score = 0.0f32;
        for ph in 0..N_PHASES {
            let offset = (ph as f32 / N_PHASES as f32) * candidate_period;
            let mut sum = 0.0f32;
            let mut count = 0u32;
            let mut t = offset;
            while (t as usize) < env.len() {
                sum += env[t as usize];
                t += candidate_period;
                count += 1;
            }
            if count > 0 {
                let mean = sum / count as f32;
                if mean > best_phase_score {
                    best_phase_score = mean;
                }
            }
        }
        if best_phase_score > best_refine_score {
            best_refine_score = best_phase_score;
            bpm = candidate_bpm;
        }
    }
    let period = 60.0 * frame_rate / bpm;

    // 6. Final phase search at the refined period.
    let mut best_offset = 0.0f32;
    let mut best_phase_score = f32::MIN;
    for p in 0..N_PHASES {
        let offset = (p as f32 / N_PHASES as f32) * period;
        let mut score = 0.0f32;
        let mut t = offset;
        while (t as usize) < env.len() {
            score += env[t as usize];
            t += period;
        }
        if score > best_phase_score {
            best_phase_score = score;
            best_offset = offset;
        }
    }

    // 7. Beat times in seconds.
    let frame_to_secs = 1.0 / frame_rate as f64;
    let track_secs = mono.len() as f64 / actual_rate as f64;
    let mut beats = Vec::<f64>::new();
    let mut t = best_offset as f64 * frame_to_secs;
    let dt = period as f64 * frame_to_secs;
    while t <= track_secs {
        if t >= 0.0 {
            beats.push(t);
        }
        t += dt;
    }

    // 7. Key detection: correlate normalised chroma with all 24 key profiles.
    let key = detect_key(&chroma);

    AnalysisResult {
        bpm,
        beat_grid: beats,
        key,
    }
}

fn detect_key(chroma_raw: &[f32; 12]) -> Option<MusicalKey> {
    let total: f32 = chroma_raw.iter().sum();
    if total <= 1e-6 {
        return None;
    }
    let chroma: [f32; 12] = std::array::from_fn(|i| chroma_raw[i] / total);

    let mut best: Option<(u8, bool, f32)> = None;
    for is_minor in [false, true] {
        let profile = if is_minor { &PROFILE_MINOR } else { &PROFILE_MAJOR };
        for tonic in 0..12u8 {
            let r = pearson_rotated(&chroma, profile, tonic);
            if best.map(|(_, _, s)| r > s).unwrap_or(true) {
                best = Some((tonic, is_minor, r));
            }
        }
    }
    best.map(|(tonic, is_minor, _)| MusicalKey { tonic, is_minor })
}

/// Pearson correlation between `x` and `profile` rotated so its tonic is at
/// position `tonic`.
fn pearson_rotated(x: &[f32; 12], profile: &[f32; 12], tonic: u8) -> f32 {
    let n = 12;
    let mut sum_xy = 0.0;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut sum_x2 = 0.0;
    let mut sum_y2 = 0.0;
    for i in 0..n {
        let xi = x[i];
        let yi = profile[(i + 12 - tonic as usize) % 12];
        sum_xy += xi * yi;
        sum_x += xi;
        sum_y += yi;
        sum_x2 += xi * xi;
        sum_y2 += yi * yi;
    }
    let nf = n as f32;
    let num = nf * sum_xy - sum_x * sum_y;
    let denom = ((nf * sum_x2 - sum_x * sum_x) * (nf * sum_y2 - sum_y * sum_y)).sqrt();
    if denom > 1e-9 { num / denom } else { 0.0 }
}
