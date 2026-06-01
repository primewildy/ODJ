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

/// Overlap-add stride: chunks step forward by this many samples
/// (rather than the full chunk length). The model produces a CHUNK
/// of output centred on each step, the OVERLAP regions on either
/// side get blended into the neighbour. demucs uses ~1 s of overlap
/// each side; we use 22050 samples = 0.5 s as a compromise (cheaper,
/// fewer inferences, still kills the boundary artefacts).
const OVERLAP: usize = 22_050; // 0.5 s
const STRIDE: usize = MODEL_CHUNK - 2 * OVERLAP; // 299,880

fn run_htdemucs(input_path: &Path) -> Result<TrackStems> {
    let buf = decode::load_to_buffer(input_path)?;
    let audio = prepare_audio_for_model(&buf);
    let n_frames = audio.len() / 2;

    let session = model()?;
    let t0 = std::time::Instant::now();

    // Overlap-add layout. Each chunk is centred on a STRIDE-aligned
    // origin, padded with OVERLAP on each side. Output is summed into
    // the corresponding `n_frames + OVERLAP` slice via a triangular
    // window so adjacent chunks crossfade in the OVERLAP regions.
    // This matches demucs.apply.apply_model's behaviour and eliminates
    // the "click every 7.8 s" artefact we had with hard cuts.
    let n_chunks = (n_frames + STRIDE - 1) / STRIDE;
    let mut drums = vec![0.0_f32; n_frames * 2];
    let mut bass = vec![0.0_f32; n_frames * 2];
    let mut other = vec![0.0_f32; n_frames * 2];
    let mut vocals = vec![0.0_f32; n_frames * 2];
    // Per-sample weight sum — divide once at the end to normalise.
    let mut weight_sum = vec![0.0_f32; n_frames];

    // Pre-build the triangular window: ramps up over OVERLAP, plateaus
    // for STRIDE, ramps down over OVERLAP.
    let mut window = vec![0.0_f32; MODEL_CHUNK];
    for i in 0..OVERLAP {
        let w = (i + 1) as f32 / OVERLAP as f32;
        window[i] = w;
        window[MODEL_CHUNK - 1 - i] = w;
    }
    for i in OVERLAP..MODEL_CHUNK - OVERLAP {
        window[i] = 1.0;
    }

    let mut chunk_in = vec![0.0_f32; 2 * MODEL_CHUNK];
    let mut t_prep = std::time::Duration::ZERO;
    let mut t_run = std::time::Duration::ZERO;
    let mut t_extract = std::time::Duration::ZERO;
    for ci in 0..n_chunks {
        // Chunk centre in samples; the actual chunk reads OVERLAP
        // samples before and (MODEL_CHUNK - OVERLAP) after.
        let centre = (ci * STRIDE) as isize;
        let start = centre - OVERLAP as isize;
        let tp = std::time::Instant::now();
        chunk_in.fill(0.0);
        for i in 0..MODEL_CHUNK {
            let src_i = start + i as isize;
            if src_i >= 0 && (src_i as usize) < n_frames {
                let si = src_i as usize;
                chunk_in[i] = audio[si * 2];
                chunk_in[MODEL_CHUNK + i] = audio[si * 2 + 1];
            }
        }
        let input = Array4::from_shape_vec(
            (1, 2, MODEL_CHUNK, 1),
            chunk_in.clone(),
        )?
        .into_shape_with_order((1, 2, MODEL_CHUNK))?;
        t_prep += tp.elapsed();

        let tr = std::time::Instant::now();
        let outputs = session
            .run(ort::inputs!["audio" => Tensor::from_array(input)?]?)?;
        t_run += tr.elapsed();

        let te = std::time::Instant::now();
        let stems_out = outputs["stems"].try_extract_tensor::<f32>()?;
        let slice = stems_out.as_slice().context("stems output not contiguous")?;
        for (stem_idx, out_buf) in [
            (0usize, &mut drums),
            (1, &mut bass),
            (2, &mut other),
            (3, &mut vocals),
        ] {
            let base = stem_idx * 2 * MODEL_CHUNK;
            for i in 0..MODEL_CHUNK {
                let dst_i = start + i as isize;
                if dst_i < 0 || (dst_i as usize) >= n_frames {
                    continue;
                }
                let dst = dst_i as usize * 2;
                let w = window[i];
                out_buf[dst] += slice[base + i] * w;
                out_buf[dst + 1] += slice[base + MODEL_CHUNK + i] * w;
            }
        }
        // Accumulate weight only on the first stem pass; same for all 4.
        for i in 0..MODEL_CHUNK {
            let dst_i = start + i as isize;
            if dst_i < 0 || (dst_i as usize) >= n_frames {
                continue;
            }
            weight_sum[dst_i as usize] += window[i];
        }
        t_extract += te.elapsed();
    }

    // Normalise — divide accumulated weighted sums by the window sum.
    for f in 0..n_frames {
        let w = weight_sum[f].max(1e-6);
        for buf in [&mut drums, &mut bass, &mut other, &mut vocals] {
            buf[f * 2] /= w;
            buf[f * 2 + 1] /= w;
        }
    }

    eprintln!(
        "stems: {} chunks ({:.1} s) in {:.1}s — per-chunk avg: prep {:.0} ms, run {:.0} ms, extract {:.0} ms",
        n_chunks,
        n_frames as f32 / MODEL_SR as f32,
        t0.elapsed().as_secs_f32(),
        t_prep.as_secs_f32() * 1000.0 / n_chunks as f32,
        t_run.as_secs_f32() * 1000.0 / n_chunks as f32,
        t_extract.as_secs_f32() * 1000.0 / n_chunks as f32,
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
