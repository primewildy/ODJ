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
pub mod logmel;

pub struct AnalysisResult {
    pub bpm: f32,
    /// Beat times in seconds from the start of the track.
    pub beat_grid: Vec<f64>,
    /// Indices into `beat_grid` of bar-position-1 downbeats. Populated
    /// by the beat_this ONNX model when the cached weights are present;
    /// empty when the model is unavailable (caller falls back to
    /// `i % 4 == 0`).
    pub downbeats: Vec<u32>,
    pub key: Option<MusicalKey>,
    /// Schema version of this result. Bumped when we change what we
    /// compute (1 = DSP only; 2 = adds model-derived downbeats).
    pub analysis_version: u32,
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
            downbeats: Vec::new(),
            key: None,
            analysis_version: 1,
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
            downbeats: Vec::new(),
            key: None,
            analysis_version: 1,
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

    // 8. Downbeat detection via beat_this. Rather than running the
    //    model over the entire track (slow on CPU, and most of the
    //    track is redundant for bar-phase purposes), we pick the
    //    busiest 30 s window (1 chunk @ 50 fps) using the existing
    //    spectral-flux envelope as a beat-density score, run a single
    //    forward pass there, then back-project the bar phase across
    //    the full DSP beat grid under the 4/4 + constant-BPM
    //    assumption (good for ~all dance music).
    match run_downbeat_window(buf, &env, frame_rate, &beats) {
        Ok(downbeats) => AnalysisResult {
            bpm,
            beat_grid: beats,
            downbeats,
            key,
            analysis_version: 2,
        },
        Err(e) => {
            // Log once. Pre-format so the worker thread's stderr isn't
            // chatty about the same missing file for every track.
            log_model_unavailable(&e);
            AnalysisResult {
                bpm,
                beat_grid: beats,
                downbeats: Vec::new(),
                key,
                analysis_version: 1,
            }
        }
    }
}

/// Pick the busiest 30 s window in the track (using the DSP envelope
/// as a beat-density score so we land on a kick-snare section instead
/// of an intro / breakdown / outro), run a single model forward pass
/// there to find the bar phase, then back-project across the DSP
/// beat grid to derive downbeat indices for the whole track.
fn run_downbeat_window(
    buf: &TrackBuffer,
    env: &[f32],
    env_fps: f32,
    beats: &[f64],
) -> anyhow::Result<Vec<u32>> {
    if beats.is_empty() {
        return Ok(Vec::new());
    }

    // Phantom-beat short circuit: the DSP picks a constant tempo + a
    // global phase, which means the beat grid is extended backwards
    // into any silent intro. The "first" few beats can sit in audible
    // silence — phantom positions where the period happens to land.
    // Whenever we see this, the FIRST audible beat is by overwhelming
    // convention bar position 1, so the bar offset is just the count
    // of silent leading beats modulo 4. No model needed.
    let n_silent = leading_silent_beats(beats, env, env_fps);
    if n_silent > 0 {
        let mod_offset = n_silent % 4;
        eprintln!(
            "analysis: trimming {} silent leading beats (env at beat[0..{}] near zero), mod_offset = {}",
            n_silent, n_silent, mod_offset
        );
        return Ok((0..beats.len())
            .filter(|i| i % 4 == mod_offset)
            .map(|i| i as u32)
            .collect());
    }

    const WINDOW_SECS: f64 = (downbeat::CHUNK as f64) / 50.0; // CHUNK frames at 50 fps = 30 s

    let window_start_secs = best_window_start(env, env_fps, WINDOW_SECS);

    let ch = buf.channels.max(1) as usize;
    let n_frames_buf = buf.frames();
    let sr = buf.sample_rate as f64;
    let start_sample = ((window_start_secs * sr) as usize).min(n_frames_buf);
    let end_sample =
        (((window_start_secs + WINDOW_SECS) * sr) as usize).min(n_frames_buf);
    let window_audio = &buf.samples[start_sample * ch..end_sample * ch];
    let resampled = resample_to_22050_mono(window_audio, buf.sample_rate, buf.channels);

    // Log-mel — pad / truncate to exactly CHUNK frames so the static
    // ONNX input shape (1, 1500, 128) is satisfied.
    let lm = logmel::LogMel::new();
    let mut mel = lm.compute(&resampled);
    let mel_target = downbeat::chunk_n_frames() * logmel::N_MELS;
    if mel.len() < mel_target {
        mel.resize(mel_target, 0.0);
    } else if mel.len() > mel_target {
        mel.truncate(mel_target);
    }

    let phase = downbeat::infer_window_bar_phase(&mel)?;
    let Some(secs_into_window) = phase.first_downbeat_secs else {
        // Sparse window — model couldn't lock a bar phase. Bail to
        // legacy i%4 behaviour rather than guess.
        return Ok(Vec::new());
    };
    let downbeat_global_secs = window_start_secs + secs_into_window;

    // Find the DSP beat index whose time is closest to the model's
    // first detected downbeat. Its `% 4` gives the bar phase for the
    // whole track.
    let mut nearest = 0usize;
    let mut best = f64::INFINITY;
    for (i, &t) in beats.iter().enumerate() {
        let d = (t - downbeat_global_secs).abs();
        if d < best {
            best = d;
            nearest = i;
        }
    }
    let mod_offset = nearest % 4;

    // The model has a known failure mode on dance music where it
    // confidently picks the half-bar (kicks-on-2-and-4 misread as
    // kicks-on-1-and-3), giving an off-by-two result. For tracks where
    // the FIRST beat of the grid is a strong onset — i.e. beat[0]
    // sits on a kick, not a pickup hit — DJs almost always treat
    // beat[0] as bar position 1. Override the model in exactly that
    // case.
    //
    // The check: if beat[0]'s spectral-flux envelope value is among
    // the top 30 % of envelope values in the track, AND the model
    // picked a non-zero offset, snap back to offset 0. Moonlight's
    // anacrustic intro has a quiet beat[0] (env is mid-pack) so the
    // override doesn't fire there.
    // Phrase-boundary cross-check intentionally disabled: my
    // re-entry detector finds the *climb-back* of the smoothed
    // envelope, which lands one beat after the actual downbeat,
    // so the vote consistently points to off=N+1 instead of N.
    // Net record so far: 0 true positives, 1 false positive
    // (Space Arp; model picked 0, vote said 1). Until the
    // re-entry frame is found more precisely (low-band onset
    // peak, not envelope ramp), trust the model + beat[0] override.
    let beat0_strong = first_beat_is_strong_onset(beats, env, env_fps);

    let final_offset = if mod_offset != 0 && beat0_strong {
        eprintln!(
            "analysis: half-bar override (beat[0] is a strong kick); model wanted offset {}",
            mod_offset
        );
        0
    } else {
        mod_offset
    };
    let mod_offset = final_offset;

    Ok((0..beats.len())
        .filter(|i| i % 4 == mod_offset)
        .map(|i| i as u32)
        .collect())
}

/// Returns `Some(off)` (0..4) when phrase-boundary detection finds at
/// least two break-and-re-entry pairs in the track AND ≥75 % of them
/// agree on the same DSP-grid `% 4`. Returns `None` when the signal
/// isn't strong enough (no clear breaks, or re-entries disagree).
///
/// Currently unused — kept for the future, see the
/// "phrase-boundary cross-check intentionally disabled" comment in
/// `run_downbeat_window` for the reason.
#[allow(dead_code)]
///
/// The premise: in dance music every breakdown ends on a "1". A
/// breakdown shows up in the spectral-flux envelope as a sustained
/// quiet patch (kicks gone). The frame where the envelope shoots
/// back up is the re-entry. The DSP beat closest to that frame is
/// therefore a bar-position-1 downbeat; its index modulo 4 is the
/// global bar phase.
fn phrase_boundary_offset(beats: &[f64], env: &[f32], env_fps: f32) -> Option<usize> {
    if beats.is_empty() || env.is_empty() {
        return None;
    }
    // Smooth the env over ~1 bar (2 s at 120 BPM) to suppress
    // intra-bar variation; we want section-level activity, not
    // beat-level activity.
    let smooth_half = (env_fps * 1.0) as usize;
    let smoothed: Vec<f32> = (0..env.len())
        .map(|i| {
            let s = i.saturating_sub(smooth_half);
            let e = (i + smooth_half + 1).min(env.len());
            env[s..e].iter().sum::<f32>() / (e - s) as f32
        })
        .collect();

    // Reference level from the active sections: take the mean of the
    // top 50 % of smoothed values so quiet breaks don't pull it down.
    let mut sorted = smoothed.clone();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let active_n = (sorted.len() / 2).max(1);
    let active_mean = sorted[..active_n].iter().sum::<f32>() / active_n as f32;
    if active_mean <= 1e-6 {
        return None;
    }

    // Break = ≥2 s under 25 % of active level. Re-entry = the first
    // frame after a break that climbs back above 60 % of active.
    let break_thresh = 0.25 * active_mean;
    let entry_thresh = 0.6 * active_mean;
    let min_break_frames = (env_fps * 2.0) as usize;

    let mut re_entry_times: Vec<f64> = Vec::new();
    let mut in_break = false;
    let mut break_start = 0usize;
    for i in 0..smoothed.len() {
        if !in_break {
            if smoothed[i] < break_thresh {
                in_break = true;
                break_start = i;
            }
        } else if smoothed[i] > entry_thresh {
            if i - break_start >= min_break_frames {
                re_entry_times.push(i as f64 / env_fps as f64);
            }
            in_break = false;
        }
    }
    if re_entry_times.len() < 2 {
        return None;
    }

    // Vote on `% 4` of the nearest DSP beat for each re-entry.
    let mut votes = [0u32; 4];
    for &t in &re_entry_times {
        let mut nearest = 0usize;
        let mut best = f64::INFINITY;
        for (i, &b) in beats.iter().enumerate() {
            let d = (b - t).abs();
            if d < best {
                best = d;
                nearest = i;
            }
        }
        votes[nearest % 4] += 1;
    }
    let total: u32 = votes.iter().sum();
    let (winner, &winning_count) = votes.iter().enumerate().max_by_key(|&(_, &v)| v).unwrap();
    let ratio = winning_count as f32 / total as f32;
    if ratio >= 0.75 {
        Some(winner)
    } else {
        None
    }
}

/// Counts how many leading DSP beats fall in silence — env value at the
/// beat's time is below 10 % of the track-wide mean envelope. Used to
/// detect tracks where the DSP autocorr propagates the beat period
/// *backwards* into a silent intro, generating phantom beats before the
/// real music begins. The first non-silent beat is the actual bar-1
/// downbeat, so its index modulo 4 is the correct bar phase — no
/// model call needed.
fn leading_silent_beats(beats: &[f64], env: &[f32], env_fps: f32) -> usize {
    if env.is_empty() {
        return 0;
    }
    let mean = env.iter().sum::<f32>() / env.len() as f32;
    let threshold = mean * 0.10;
    let radius = (env_fps * 0.1) as usize;
    let mut n = 0;
    for &b in beats {
        let idx = (b as f32 * env_fps).round() as usize;
        let lo = idx.saturating_sub(radius);
        let hi = (idx + radius + 1).min(env.len());
        let local_max = env[lo..hi]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        if local_max < threshold {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// True iff `beats[0]`'s position in the spectral-flux envelope is in
/// the top 30 % of the track's envelope values — i.e. there's a
/// strong onset at the very first detected beat. Used to spot dance
/// tracks where beat[0] is the real bar-position-1 kick (the
/// overwhelming majority) versus anacrustic intros where beat[0] is
/// a quiet pickup hit (Moonlight et al.). Cheap: one O(n log n) sort
/// per call, but this runs once per track.
fn first_beat_is_strong_onset(beats: &[f64], env: &[f32], env_fps: f32) -> bool {
    let Some(&beat0) = beats.first() else {
        return false;
    };
    let env_idx = (beat0 as f32 * env_fps).round() as usize;
    // Take the max env value across the ~1-beat window centred on
    // beat[0]'s time. The DSP beat positions are sub-frame accurate
    // so the strongest onset may not sit on the nearest env frame.
    let radius = (env_fps * 0.15) as usize; // ±150 ms
    let lo = env_idx.saturating_sub(radius);
    let hi = (env_idx + radius + 1).min(env.len());
    let beat0_onset = env[lo..hi]
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);

    let mut sorted: Vec<f32> = env.iter().copied().collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let top_third = sorted[sorted.len() / 3];
    beat0_onset >= top_third
}

/// Sliding-window scan over the spectral-flux envelope. Returns the
/// start (in seconds) of the `window_secs`-long span with the highest
/// mean envelope — i.e. the section with the most onset energy, which
/// is the bit of the track with the clearest beat to lock onto.
fn best_window_start(env: &[f32], env_fps: f32, window_secs: f64) -> f64 {
    let window_frames = (window_secs as f32 * env_fps).round() as usize;
    if env.len() <= window_frames || window_frames == 0 {
        return 0.0;
    }
    // Step 1 s — finer resolution doesn't change the chosen section
    // and 1 s of slop in the start is irrelevant for a 30 s window.
    let step = (env_fps as usize).max(1);
    let mut running: f32 = env[..window_frames].iter().sum();
    let mut best_score = running;
    let mut best_start = 0usize;
    let mut i = 0;
    while i + window_frames + step <= env.len() {
        // Slide by `step` frames; subtract leaving, add entering.
        for k in 0..step {
            running -= env[i + k];
            running += env[i + window_frames + k];
        }
        i += step;
        if running > best_score {
            best_score = running;
            best_start = i;
        }
    }
    best_start as f64 / env_fps as f64
}

/// Linear-interpolation resampler to mono @ 22050 Hz. Good enough for
/// the analysis pass — model is robust to small spectral artefacts.
fn resample_to_22050_mono(samples: &[f32], src_sr: u32, src_ch: u16) -> Vec<f32> {
    let ch = src_ch.max(1) as usize;
    let n_in = samples.len() / ch;
    if n_in == 0 {
        return Vec::new();
    }
    if src_sr == logmel::SR && ch == 1 {
        return samples.to_vec();
    }
    let ratio = src_sr as f64 / logmel::SR as f64;
    let n_out = ((n_in as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let src_pos = i as f64 * ratio;
        let lo = src_pos.floor() as usize;
        let hi = (lo + 1).min(n_in - 1);
        let t = (src_pos - lo as f64) as f32;
        let mut a = 0.0f32;
        let mut b = 0.0f32;
        for c in 0..ch {
            a += samples[lo * ch + c];
            b += samples[hi * ch + c];
        }
        out.push(((1.0 - t) * a + t * b) / ch as f32);
    }
    out
}

/// Rate-limited stderr warning for a missing/broken model. Worker
/// scans hundreds of tracks; we don't want one warning per track.
fn log_model_unavailable(err: &anyhow::Error) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::SeqCst) {
        eprintln!(
            "analysis: downbeat model unavailable, falling back to DSP grid: {err:#}"
        );
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
