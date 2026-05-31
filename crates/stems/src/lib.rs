//! HTDemucs stem separation, pure Rust + GPU via `ort` + CUDA.
//!
//! The model file lives at `$XDG_CACHE_HOME/dj/htdemucs.onnx`. Build
//! with `stem-spike/export_demucs_onnx.py` (see
//! docs/notes/htdemucs_onnx_export.md). The session cache holds an
//! `Arc<TrackStems>` per-track for the lifetime of the process; the
//! audio is never written to disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use control::{TrackBuffer, TrackStems};
use ndarray::Array4;
use ort::execution_providers::CUDAExecutionProvider;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Tensor;

/// HTDemucs operates on 7.8 s segments at 44.1 kHz stereo.
const MODEL_SR: u32 = 44_100;
const MODEL_CHUNK: usize = 343_980; // == int(39/5 * 44100)

/// Per-process session cache. In-memory `Arc<TrackStems>` keyed by
/// the input track's path. No disk persistence — stems-per-track at
/// CD quality are ~200 MB so caching to disk would blow up storage
/// fast and the user explicitly asked to keep this session-only.
pub struct SessionCache {
    entries: Mutex<HashMap<PathBuf, Arc<TrackStems>>>,
}

impl SessionCache {
    pub fn new() -> Result<Self> {
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn get(&self, path: &Path) -> Option<Arc<TrackStems>> {
        self.entries.lock().ok()?.get(path).cloned()
    }

    /// Decode, resample, run the model, stitch outputs. Long-running
    /// (~5-15 s for a 5-min track depending on GPU) — call from a
    /// background thread.
    pub fn separate(&self, input_path: &Path) -> Result<Arc<TrackStems>> {
        if let Some(s) = self.get(input_path) {
            return Ok(s);
        }
        let stems = run_htdemucs(input_path)?;
        let arc = Arc::new(stems);
        if let Ok(mut m) = self.entries.lock() {
            m.insert(input_path.to_path_buf(), Arc::clone(&arc));
        }
        Ok(arc)
    }
}

/// `~/.cache/dj/htdemucs.onnx`. The user generates this once via
/// `stem-spike/export_demucs_onnx.py` and the path stays put. Future
/// versions could auto-download from a hosted URL.
pub fn model_path() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    cache.join("dj").join("htdemucs.onnx")
}

type Plan = Session;

/// Lazily-loaded ONNX Runtime session, same pattern as
/// `crates/analysis::downbeat::model()`. CUDA requested
/// unconditionally; ort falls back to CPU if the EP can't init.
fn model() -> Result<&'static Plan> {
    static MODEL: OnceLock<Plan> = OnceLock::new();
    if let Some(m) = MODEL.get() {
        return Ok(m);
    }
    let path = model_path();
    if !path.exists() {
        bail!(
            "htdemucs ONNX model not found at {}. \
             Run `python stem-spike/export_demucs_onnx.py` and copy \
             htdemucs_inline.onnx to that path (see \
             docs/notes/htdemucs_onnx_export.md).",
            path.display()
        );
    }
    let session = Session::builder()
        .context("ort Session::builder()")?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_execution_providers([CUDAExecutionProvider::default().build()])?
        .commit_from_file(&path)
        .with_context(|| format!("loading ONNX model at {}", path.display()))?;
    eprintln!("stems: htdemucs ONNX ready (CUDA EP requested)");
    Ok(MODEL.get_or_init(|| session))
}

fn run_htdemucs(input_path: &Path) -> Result<TrackStems> {
    let buf = decode::load_to_buffer(input_path)?;
    // Demucs operates on 44.1 kHz stereo. Resample / channel-fix as needed.
    let audio = prepare_audio_for_model(&buf);
    let n_frames = audio.len() / 2;

    let session = model()?;
    let t0 = std::time::Instant::now();

    // Per-chunk processing: pad to a multiple of MODEL_CHUNK, run
    // each chunk through the model, stitch outputs. No overlap-add
    // for v1 — slight artefacts at chunk boundaries, fix later.
    let n_chunks = n_frames.div_ceil(MODEL_CHUNK);
    let padded_len = n_chunks * MODEL_CHUNK;

    // Output buffers per stem, sized to the FULL padded length (we'll
    // truncate to n_frames at the end). Each holds interleaved stereo.
    let mut drums = vec![0.0_f32; padded_len * 2];
    let mut bass = vec![0.0_f32; padded_len * 2];
    let mut other = vec![0.0_f32; padded_len * 2];
    let mut vocals = vec![0.0_f32; padded_len * 2];

    // De-interleaved chunk buffer reused per inference call.
    let mut chunk_in = vec![0.0_f32; 2 * MODEL_CHUNK];

    for ci in 0..n_chunks {
        let src_start = ci * MODEL_CHUNK;
        // Build (1, 2, MODEL_CHUNK) — channels-first, contiguous.
        chunk_in.fill(0.0);
        for i in 0..MODEL_CHUNK {
            let src_i = src_start + i;
            if src_i < n_frames {
                chunk_in[i] = audio[src_i * 2];                  // left
                chunk_in[MODEL_CHUNK + i] = audio[src_i * 2 + 1]; // right
            }
        }
        let input = Array4::from_shape_vec(
            (1, 2, MODEL_CHUNK, 1),
            chunk_in.clone(),
        )?
        .into_shape_with_order((1, 2, MODEL_CHUNK))?;

        let outputs = session
            .run(ort::inputs!["audio" => Tensor::from_array(input)?]?)?;
        let stems_out = outputs["stems"].try_extract_tensor::<f32>()?;
        // Shape (1, 4, 2, MODEL_CHUNK) — drums, bass, other, vocals.
        let slice = stems_out.as_slice().context("stems output not contiguous")?;
        // Interleave each stem into its output buffer.
        for (stem_idx, out_buf) in [
            (0usize, &mut drums),
            (1, &mut bass),
            (2, &mut other),
            (3, &mut vocals),
        ] {
            let base = stem_idx * 2 * MODEL_CHUNK;
            for i in 0..MODEL_CHUNK {
                let dst = (src_start + i) * 2;
                if dst + 1 < out_buf.len() {
                    out_buf[dst] = slice[base + i];
                    out_buf[dst + 1] = slice[base + MODEL_CHUNK + i];
                }
            }
        }
    }

    // Truncate to the original frame count.
    drums.truncate(n_frames * 2);
    bass.truncate(n_frames * 2);
    other.truncate(n_frames * 2);
    vocals.truncate(n_frames * 2);

    eprintln!(
        "stems: {} chunks ({} s) in {:.1}s",
        n_chunks,
        n_frames as f32 / MODEL_SR as f32,
        t0.elapsed().as_secs_f32()
    );

    Ok(TrackStems {
        drums,
        bass,
        vocals,
        other,
        channels: 2,
        sample_rate: MODEL_SR,
    })
}

/// Linear-interp resample to 44.1 kHz stereo and de-interleave to
/// interleaved [L, R, L, R, ...] in that order. Cheap and good
/// enough — the model is robust to small spectral artefacts.
fn prepare_audio_for_model(buf: &TrackBuffer) -> Vec<f32> {
    let src_ch = buf.channels.max(1) as usize;
    let src_sr = buf.sample_rate;
    let src_n = buf.samples.len() / src_ch;
    if src_n == 0 {
        return Vec::new();
    }
    let ratio = src_sr as f64 / MODEL_SR as f64;
    let dst_n = ((src_n as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(dst_n * 2);
    for i in 0..dst_n {
        let src_pos = i as f64 * ratio;
        let lo = src_pos.floor() as usize;
        let hi = (lo + 1).min(src_n - 1);
        let t = (src_pos - lo as f64) as f32;
        let (left, right) = match src_ch {
            1 => {
                let s = buf.samples[lo] * (1.0 - t) + buf.samples[hi] * t;
                (s, s)
            }
            _ => {
                let l =
                    buf.samples[lo * src_ch] * (1.0 - t) + buf.samples[hi * src_ch] * t;
                let r = buf.samples[lo * src_ch + 1] * (1.0 - t)
                    + buf.samples[hi * src_ch + 1] * t;
                (l, r)
            }
        };
        out.push(left);
        out.push(right);
    }
    out
}
