//! Downbeat detection via beat_this (Foroughmand et al. 2024), exported
//! to ONNX. Replaces the bar-phase-blind phase search in the v1
//! spectral-flux pipeline.
//!
//! Pipeline:
//!   1. Audio → mono → 22050 Hz (caller's job; v1 does block-average).
//!   2. Log-mel spectrogram (128 mel, 1024 fft, 441 hop, fmin=30, fmax=11000).
//!   3. Chunk into 1500-frame windows with border_size=6 keep-first overlap.
//!   4. ONNX inference per chunk → (beat, downbeat) logit curves.
//!   5. Peak-pick beats (±70 ms = ±3 frames at 50 fps).
//!   6. Global bar offset: try the 4 candidate bar phases, pick the one
//!      where Σ downbeat-logit at predicted "1"s is highest.
//!
//! See docs/notes/downbeat_detection.md for the spike validation, model
//! re-generation steps, and the Python reference implementation.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use ndarray::Array3;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Tensor;

/// Frames-per-second of the model's input + output. beat_this is fixed
/// at 22050 Hz audio with 441-sample hop → exactly 50 fps.
pub const MODEL_FPS: f32 = 50.0;

/// One 1500-frame chunk = the model's training receptive field. We
/// re-export at a static (1, 1500, 128) shape; chunk-stitching in
/// `infer_track` handles tracks longer than this.
pub const CHUNK: usize = 1500;
const N_MELS: usize = 128;

/// Number of mel frames in a single inference chunk. Re-exported so
/// callers like `analyse` can size their log-mel buffer without
/// pulling the constant by name.
pub const fn chunk_n_frames() -> usize {
    CHUNK
}
/// Frames per chunk side that are discarded by the model's training-time
/// max-pool loss. We mirror beat_this' `split_predict_aggregate` and
/// drop these from each chunk's contribution to the full-track logits,
/// except at the very start / very end of the piece.
const BORDER: usize = 6;

/// On-disk location for the bundled beat_this ONNX weights. Honours
/// `$XDG_CACHE_HOME` then falls back to `$HOME/.cache/`. The user runs
/// `python downbeat-spike/export_onnx.py` once to drop the 84 MB file
/// here; future versions will auto-download.
pub fn model_path() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    cache.join("dj").join("model_final0.onnx")
}

/// Lazily-loaded ONNX Runtime session. The first call pays ~1 s of
/// graph parse + CUDA EP init; the compiled session is then reused
/// across the worker thread + any synchronous loads. `Session` is
/// `Send + Sync`, so a single shared instance is fine.
fn model() -> Result<&'static Session> {
    static MODEL: OnceLock<Session> = OnceLock::new();
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let path = model_path();
    if !path.exists() {
        bail!(
            "beat_this ONNX model not found at {}. \
             Run `python downbeat-spike/export_onnx.py` and copy \
             model_final0_inline.onnx to that path (see docs/notes/downbeat_detection.md).",
            path.display()
        );
    }
    // Try CUDA first; ort silently falls back to CPU if the CUDA EP
    // can't initialise (no CUDA libs, no compatible GPU). We log the
    // chosen path once so the user knows what they got.
    let session = Session::builder()
        .context("ort Session::builder()")?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_execution_providers([CUDAExecutionProvider::default().build()])?
        .commit_from_file(&path)
        .with_context(|| format!("loading ONNX model at {}", path.display()))?;
    eprintln!("analysis: ort session ready (CUDA EP requested)");
    Ok(MODEL.get_or_init(|| session))
}

/// Run a single (batch=1, time=CHUNK, mel=N_MELS) input through the
/// model. Returns (beat_logits, downbeat_logits), each of length CHUNK.
pub fn infer_chunk(mel_chunk: &[f32]) -> Result<(Vec<f32>, Vec<f32>)> {
    assert_eq!(mel_chunk.len(), CHUNK * N_MELS);
    let session = model()?;
    let input: Array3<f32> = Array3::from_shape_vec((1, CHUNK, N_MELS), mel_chunk.to_vec())?;
    let outputs = session.run(ort::inputs!["spect" => Tensor::from_array(input)?]?)?;
    let beat = outputs["beat"]
        .try_extract_tensor::<f32>()?
        .as_slice()
        .context("beat output not contiguous")?
        .to_vec();
    let downbeat = outputs["downbeat"]
        .try_extract_tensor::<f32>()?
        .as_slice()
        .context("downbeat output not contiguous")?
        .to_vec();
    Ok((beat, downbeat))
}

/// Result of running the full pipeline on a track: beat times in
/// seconds, plus indices into `beats` that are bar-position-1
/// downbeats (i.e. `beats[downbeats[i]]` is a downbeat time).
#[derive(Debug, Clone, Default)]
pub struct DownbeatResult {
    pub beats: Vec<f64>,
    pub downbeats: Vec<u32>,
}

/// Pipeline entry point: given a flat row-major log-mel of shape
/// `(n_frames, N_MELS)`, returns beat times + downbeat indices.
pub fn infer_track(logmel: &[f32]) -> Result<DownbeatResult> {
    if logmel.is_empty() {
        return Ok(DownbeatResult::default());
    }
    assert_eq!(logmel.len() % N_MELS, 0, "logmel buffer not (n, {N_MELS}) shape");
    let n_frames = logmel.len() / N_MELS;

    let (beat_logits, downbeat_logits) = run_chunked(logmel, n_frames)?;
    let beat_frames = peak_pick(&beat_logits, 3, -1.0);
    let bar_off = global_bar_offset(&beat_frames, &downbeat_logits);

    let beats: Vec<f64> = beat_frames
        .iter()
        .map(|&f| f as f64 / MODEL_FPS as f64)
        .collect();
    let downbeats: Vec<u32> = (0..beat_frames.len())
        .filter(|i| ((*i + 4 - bar_off) % 4) == 0)
        .map(|i| i as u32)
        .collect();

    Ok(DownbeatResult { beats, downbeats })
}

/// Single-chunk pipeline: run the model on exactly one 1500-frame
/// log-mel window. Returns the offset (0..4) of the first beat in the
/// window that is bar-position-1, plus the time-in-window (seconds)
/// at which that beat lands. Caller back-projects this onto the
/// DSP-derived global beat_grid via the constant-BPM assumption.
pub fn infer_window_bar_phase(logmel_chunk: &[f32]) -> Result<WindowBarPhase> {
    assert_eq!(
        logmel_chunk.len(),
        CHUNK * N_MELS,
        "expected exactly CHUNK*N_MELS log-mel frames"
    );
    let (beat_logits, downbeat_logits) = infer_chunk(logmel_chunk)?;
    let beat_frames = peak_pick(&beat_logits, 3, -1.0);
    if beat_frames.len() < 4 {
        return Ok(WindowBarPhase {
            first_downbeat_secs: None,
            bar_offset: 0,
            local_beat_count: beat_frames.len(),
            scores: [f32::NEG_INFINITY; 4],
        });
    }
    let (bar_off, scores) = global_bar_offset_with_scores(&beat_frames, &downbeat_logits);
    let first_db_frame = beat_frames[bar_off];
    Ok(WindowBarPhase {
        first_downbeat_secs: Some(first_db_frame as f64 / MODEL_FPS as f64),
        bar_offset: bar_off,
        local_beat_count: beat_frames.len(),
        scores,
    })
}

/// Variant of `global_bar_offset` that also returns the 4 raw scores
/// (one per candidate bar offset) for diagnostics + multi-window voting.
fn global_bar_offset_with_scores(
    beat_frames: &[usize],
    downbeat_logits: &[f32],
) -> (usize, [f32; 4]) {
    let mut scores = [f32::NEG_INFINITY; 4];
    if beat_frames.len() < 4 {
        return (0, scores);
    }
    for off in 0..4 {
        let mut sum = 0.0f32;
        let mut i = off;
        while i < beat_frames.len() {
            sum += downbeat_logits[beat_frames[i]];
            i += 4;
        }
        scores[off] = sum;
    }
    let mut best_off = 0usize;
    let mut best = f32::NEG_INFINITY;
    for (i, &s) in scores.iter().enumerate() {
        if s > best {
            best = s;
            best_off = i;
        }
    }
    (best_off, scores)
}

#[derive(Debug, Clone)]
pub struct WindowBarPhase {
    /// Seconds into the window at which the first detected bar-1
    /// downbeat lands. `None` when fewer than 4 beats were picked
    /// inside the window (unusual — implies a very sparse section).
    pub first_downbeat_secs: Option<f64>,
    /// Which of the first four detected beats was bar-position-1
    /// (`bar_offset` ∈ 0..4). Kept for diagnostics.
    pub bar_offset: usize,
    /// Number of beats peak-picked inside the window. Useful for
    /// scoring whether the chosen window was actually beat-heavy.
    pub local_beat_count: usize,
    /// Raw 4-way downbeat-logit sums (one per candidate bar offset).
    /// Useful for diagnostics + future multi-window voting.
    pub scores: [f32; 4],
}

/// Run the model over a track that may be longer (or shorter) than a
/// single chunk. Ports beat_this' `split_predict_aggregate` with
/// `border_size=6`, `overlap_mode="keep_first"`, `avoid_short_end=true`.
/// Returns `(beat_logits, downbeat_logits)` each of length `n_frames`.
fn run_chunked(logmel: &[f32], n_frames: usize) -> Result<(Vec<f32>, Vec<f32>)> {
    let starts = chunk_starts(n_frames);
    // -1000 sentinel mirrors the Python code; the loss-side max-pool
    // never sees these because the gap is filled by an adjacent chunk
    // (and at start/end we use the chunk's full edge).
    let mut beat_full = vec![-1000.0_f32; n_frames];
    let mut downbeat_full = vec![-1000.0_f32; n_frames];

    // Walk starts in REVERSE so earlier chunks overwrite later ones in
    // the overlap region — beat_this' "keep_first" semantics.
    let mut chunk_buf = vec![0.0_f32; CHUNK * N_MELS];
    for &start in starts.iter().rev() {
        zeropad_chunk(logmel, n_frames, start, &mut chunk_buf);
        let (beat, downbeat) = infer_chunk(&chunk_buf)?;
        let is_first = start <= -(BORDER as isize);
        let is_last = (start + CHUNK as isize) >= n_frames as isize + BORDER as isize;
        // Drop the per-side border unless we're at the piece edge.
        let lo = if is_first { 0 } else { BORDER };
        let hi = if is_last { CHUNK } else { CHUNK - BORDER };
        for k in lo..hi {
            let pos_signed = start + k as isize;
            if pos_signed < 0 {
                continue;
            }
            let pos = pos_signed as usize;
            if pos >= n_frames {
                continue;
            }
            beat_full[pos] = beat[k];
            downbeat_full[pos] = downbeat[k];
        }
    }
    Ok((beat_full, downbeat_full))
}

/// Mirror of beat_this' `split_piece`. Returns the start position (in
/// frames, signed because the first chunk starts at -BORDER) of each
/// chunk. With `avoid_short_end=true` the final chunk is shifted left
/// so it ends at `n_frames + BORDER` instead of running short.
fn chunk_starts(n_frames: usize) -> Vec<isize> {
    let stride = (CHUNK - 2 * BORDER) as isize;
    let mut starts: Vec<isize> = Vec::new();
    let mut s = -(BORDER as isize);
    let end_threshold = n_frames as isize - BORDER as isize;
    while s < end_threshold {
        starts.push(s);
        s += stride;
    }
    if starts.is_empty() {
        starts.push(-(BORDER as isize));
    }
    if n_frames as isize > stride {
        // avoid_short_end: anchor the last chunk to the right edge.
        let last = (n_frames as isize) - (CHUNK as isize - BORDER as isize);
        *starts.last_mut().unwrap() = last;
    }
    starts
}

/// Copy `logmel[max(start,0) .. min(start+CHUNK, n_frames)]` into
/// `dst`, zero-padding either side as needed. `dst` must be sized
/// `CHUNK * N_MELS`.
fn zeropad_chunk(logmel: &[f32], n_frames: usize, start: isize, dst: &mut [f32]) {
    debug_assert_eq!(dst.len(), CHUNK * N_MELS);
    dst.fill(0.0);
    let src_start = start.max(0) as usize;
    let src_end = (start + CHUNK as isize).min(n_frames as isize).max(0) as usize;
    if src_end <= src_start {
        return;
    }
    let dst_start = (src_start as isize - start) as usize;
    let n_copy = src_end - src_start;
    dst[dst_start * N_MELS..(dst_start + n_copy) * N_MELS]
        .copy_from_slice(&logmel[src_start * N_MELS..src_end * N_MELS]);
}

/// Sliding-window argmax — keep frame `i` iff its logit is the maximum
/// over `[i - half_win, i + half_win]` and above `thresh`. Mirrors the
/// Python spike's `peak_pick`.
fn peak_pick(logits: &[f32], half_win: usize, thresh: f32) -> Vec<usize> {
    let mut peaks = Vec::new();
    let n = logits.len();
    for i in 0..n {
        let v = logits[i];
        if v <= thresh {
            continue;
        }
        let lo = i.saturating_sub(half_win);
        let hi = (i + half_win + 1).min(n);
        let mut is_peak = true;
        for j in lo..hi {
            if logits[j] > v {
                is_peak = false;
                break;
            }
        }
        if is_peak {
            peaks.push(i);
        }
    }
    peaks
}

/// Among the four candidate bar offsets `0..4` (i.e. which of the
/// first four beats is the true "1"), pick the one where the sum of
/// downbeat-logit at predicted downbeat frames is maximum.
fn global_bar_offset(beat_frames: &[usize], downbeat_logits: &[f32]) -> usize {
    if beat_frames.len() < 4 {
        return 0;
    }
    let mut best_off = 0usize;
    let mut best_score = f32::NEG_INFINITY;
    for off in 0..4 {
        let mut sum = 0.0_f32;
        let mut i = off;
        while i < beat_frames.len() {
            sum += downbeat_logits[beat_frames[i]];
            i += 4;
        }
        if sum > best_score {
            best_score = sum;
            best_off = off;
        }
    }
    best_off
}

#[cfg(test)]
mod chunk_tests {
    use super::*;

    #[test]
    fn chunk_starts_short_piece() {
        // ≤ stride → single chunk anchored at -BORDER.
        let s = chunk_starts(100);
        assert_eq!(s, vec![-(BORDER as isize)]);
    }

    #[test]
    fn chunk_starts_two_chunks() {
        // n_frames slightly > stride → two chunks, last anchored to end.
        let n = 1500;
        let s = chunk_starts(n);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], -(BORDER as isize));
        // last chunk ends at n + BORDER, so starts at n - (CHUNK - BORDER).
        assert_eq!(s[1], n as isize - (CHUNK - BORDER) as isize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke check: load + run on pseudo-data. Skipped automatically
    /// when the model isn't present (e.g. CI without the cached file).
    #[test]
    fn smoke_load_and_run() {
        if !model_path().exists() {
            eprintln!("skipping smoke test: model not present at {}", model_path().display());
            return;
        }
        let mut mel = vec![0.0_f32; CHUNK * N_MELS];
        for (i, x) in mel.iter_mut().enumerate() {
            *x = ((i as f32 * 0.013).sin() * 0.5) - 2.0;
        }
        let (beat, downbeat) = infer_chunk(&mel).expect("inference");
        assert_eq!(beat.len(), CHUNK);
        assert_eq!(downbeat.len(), CHUNK);
        for v in beat.iter().chain(downbeat.iter()) {
            assert!(v.is_finite(), "non-finite logit {v}");
        }
    }
}
