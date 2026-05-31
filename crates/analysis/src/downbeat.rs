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
use tract_onnx::prelude::*;

/// Frames-per-second of the model's input + output. beat_this is fixed
/// at 22050 Hz audio with 441-sample hop → exactly 50 fps.
pub const MODEL_FPS: f32 = 50.0;

/// One 1500-frame chunk = the model's training receptive field. We
/// re-export at a static (1, 1500, 128) shape; chunk-stitching in
/// `infer_track` handles tracks longer than this.
const CHUNK: usize = 1500;
const N_MELS: usize = 128;

type Plan = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

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

/// Lazily-compiled model. Loading + tract's graph optimisation takes a
/// minute, do it once per process. The compiled plan is immutable so
/// it's safe to share across the analysis worker's threads.
fn model() -> Result<&'static Plan> {
    static MODEL: OnceLock<Plan> = OnceLock::new();
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
    let m = tract_onnx::onnx()
        .model_for_path(&path)
        .with_context(|| format!("parsing ONNX model at {}", path.display()))?
        .with_input_fact(0, f32::fact(&[1, CHUNK, N_MELS]).into())?
        .into_optimized()
        .context("optimising tract graph")?
        .into_runnable()
        .context("compiling tract plan")?;
    Ok(MODEL.get_or_init(|| m))
}

/// Run a single (batch=1, time=CHUNK, mel=N_MELS) input through the
/// model. Returns (beat_logits, downbeat_logits), each of length CHUNK.
pub fn infer_chunk(mel_chunk: &[f32]) -> Result<(Vec<f32>, Vec<f32>)> {
    assert_eq!(mel_chunk.len(), CHUNK * N_MELS);
    let plan = model()?;
    let input = tract_ndarray::Array3::from_shape_vec(
        (1, CHUNK, N_MELS),
        mel_chunk.to_vec(),
    )?
    .into_tensor();
    let outputs = plan.run(tvec!(input.into()))?;
    let beat = outputs[0]
        .to_array_view::<f32>()?
        .as_slice()
        .context("beat output not contiguous")?
        .to_vec();
    let downbeat = outputs[1]
        .to_array_view::<f32>()?
        .as_slice()
        .context("downbeat output not contiguous")?
        .to_vec();
    Ok((beat, downbeat))
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
