//! HTDemucs stem separation via the spike's Python venv, with an
//! in-process session cache. The output files persist on disk only as
//! long as the process is alive (under `/tmp/dj-stems-<pid>/`).
//!
//! Approach: shell out to `python -m demucs.separate --device cuda`.
//! That's the same path the spike validated at ~15 s/track on an
//! RTX 4060. A pure-Rust ONNX export of HTDemucs would be ideal but
//! it doesn't exist yet (Mixxx GSoC 2025 is working on it); the
//! subprocess buys us a working stack today without blocking on that.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use control::TrackStems;

/// Per-process session cache. Holds:
///   - in-memory `Arc<TrackStems>` for tracks already separated this run,
///   - a tempdir on disk for the raw WAV outputs (cleaned up at process exit).
///
/// Cheap to clone: behaviour is `Arc<inner>`. UI keeps one
/// `Arc<SessionCache>` and hands clones to each background worker.
pub struct SessionCache {
    entries: Mutex<HashMap<PathBuf, Arc<TrackStems>>>,
    /// Output dir for the demucs subprocess. Lives at
    /// `$TMPDIR/dj-stems-<pid>/` so concurrent dj instances don't
    /// collide; removed on Drop. We don't try to share between
    /// sessions — the spec is explicit that stems are session-only.
    cache_dir: PathBuf,
}

impl SessionCache {
    pub fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("dj-stems-{}", std::process::id()));
        std::fs::create_dir_all(&dir).context("creating session stem cache dir")?;
        eprintln!("stems: session cache dir {}", dir.display());
        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            cache_dir: dir,
        })
    }

    /// Return cached stems if we've already separated this track in
    /// this session. Cheap — single mutex lock.
    pub fn get(&self, path: &Path) -> Option<Arc<TrackStems>> {
        self.entries.lock().ok()?.get(path).cloned()
    }

    /// Run demucs (or hit the cache) and return the four stem buffers.
    /// Long-running on a cache miss (~15 s/track on GPU); call from a
    /// background thread.
    pub fn separate(&self, input_path: &Path) -> Result<Arc<TrackStems>> {
        if let Some(s) = self.get(input_path) {
            return Ok(s);
        }
        let stems = run_demucs(input_path, &self.cache_dir)?;
        let arc = Arc::new(stems);
        if let Ok(mut m) = self.entries.lock() {
            m.insert(input_path.to_path_buf(), Arc::clone(&arc));
        }
        Ok(arc)
    }
}

impl Drop for SessionCache {
    fn drop(&mut self) {
        // Best-effort cleanup. Don't panic if the dir is gone.
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}

fn run_demucs(input_path: &Path, cache_dir: &Path) -> Result<TrackStems> {
    let stem_name = input_path
        .file_stem()
        .ok_or_else(|| anyhow!("input path has no file stem"))?
        .to_string_lossy()
        .into_owned();

    // demucs lays out output as <out>/htdemucs/<basename>/{drums,bass,
    // vocals,other}.wav. We use a per-track subdir under cache_dir so
    // demucs doesn't dump intermediate stems on top of each other if
    // two tracks happen to share a basename.
    let per_track_out = cache_dir.join(&stem_name);
    let htdemucs_dir = per_track_out.join("htdemucs").join(&stem_name);

    let stems_present = ["drums", "bass", "vocals", "other"]
        .iter()
        .all(|n| htdemucs_dir.join(format!("{n}.wav")).exists());

    if !stems_present {
        let python = find_python()?;
        let t0 = std::time::Instant::now();
        eprintln!("stems: separating {stem_name}…");
        let status = Command::new(&python)
            .args(["-m", "demucs.separate", "--device", "cuda", "-o"])
            .arg(&per_track_out)
            .arg(input_path)
            .status()
            .context("spawning demucs subprocess")?;
        if !status.success() {
            bail!("demucs exited with {status} for {stem_name}");
        }
        eprintln!("stems: separated {stem_name} in {:.1}s", t0.elapsed().as_secs_f32());
    }

    let drums = load_into_samples(&htdemucs_dir.join("drums.wav"))?;
    let bass = load_into_samples(&htdemucs_dir.join("bass.wav"))?;
    let vocals = load_into_samples(&htdemucs_dir.join("vocals.wav"))?;
    let other = load_into_samples(&htdemucs_dir.join("other.wav"))?;

    let sr = drums.sample_rate;
    let ch = drums.channels;
    if bass.sample_rate != sr
        || vocals.sample_rate != sr
        || other.sample_rate != sr
        || bass.channels != ch
        || vocals.channels != ch
        || other.channels != ch
    {
        bail!("stem files disagree on sample-rate / channels");
    }

    Ok(TrackStems {
        drums: drums.samples,
        bass: bass.samples,
        vocals: vocals.samples,
        other: other.samples,
        channels: ch,
        sample_rate: sr,
    })
}

struct LoadedSamples {
    samples: Vec<f32>,
    channels: u16,
    sample_rate: u32,
}

fn load_into_samples(path: &Path) -> Result<LoadedSamples> {
    let arc = decode::load_to_buffer(path)?;
    // decode returns Arc; since we just created it nobody else holds it.
    // try_unwrap avoids a 50 MB clone.
    let buf = Arc::try_unwrap(arc).unwrap_or_else(|a| (*a).clone());
    Ok(LoadedSamples {
        samples: buf.samples,
        channels: buf.channels,
        sample_rate: buf.sample_rate,
    })
}

fn find_python() -> Result<PathBuf> {
    // Honour an override first so the user can point at a different
    // venv without rebuilding.
    if let Ok(p) = std::env::var("DJ_DEMUCS_PYTHON") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        eprintln!("stems: DJ_DEMUCS_PYTHON={} doesn't exist, falling back", p.display());
    }
    // Spike's venv, relative to CWD or absolute (for when the binary
    // is launched directly).
    let candidates = [
        PathBuf::from("stem-spike/venv/bin/python"),
        PathBuf::from("/home/ben/Documents/DJ/stem-spike/venv/bin/python"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    bail!(
        "no demucs python venv found. \
         Expected stem-spike/venv/bin/python; \
         set DJ_DEMUCS_PYTHON to override."
    )
}

// TrackBuffer doesn't implement Clone — give it one locally via a
// wrapper Vec clone (control crate doesn't depend on us so we can't
// add a Clone impl there from here, but try_unwrap should almost
// always succeed in this code path anyway).
trait TrackBufferLocalClone {
    fn clone(&self) -> control::TrackBuffer;
}

impl TrackBufferLocalClone for control::TrackBuffer {
    fn clone(&self) -> control::TrackBuffer {
        control::TrackBuffer {
            samples: self.samples.clone(),
            channels: self.channels,
            sample_rate: self.sample_rate,
        }
    }
}
