//! On-disk persistence for favourites + the analysis cache.
//!
//! Two simple line-based text files in the project directory:
//! - `.favourites` — one absolute path per line.
//! - `.analysis-cache` — one entry per line, pipe-delimited:
//!     `path|bpm|tonic|is_minor|beat0,beat1,beat2,...`
//!
//! Line-based formats keep the implementation tiny (no XML/JSON parser
//! dependency). Pipes are uncommon in audio filenames; if the user ever
//! has one we skip the offending line.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use control::MusicalKey;

pub struct Favourites {
    file: PathBuf,
    paths: HashSet<PathBuf>,
}

impl Favourites {
    pub fn load(dir: &Path) -> Self {
        let file = dir.join(".favourites");
        let mut paths = HashSet::new();
        if let Ok(f) = File::open(&file) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    paths.insert(PathBuf::from(trimmed));
                }
            }
        }
        Self { file, paths }
    }

    pub fn contains(&self, p: &Path) -> bool {
        self.paths.contains(p)
    }

    pub fn toggle(&mut self, p: &Path) {
        if !self.paths.remove(p) {
            self.paths.insert(p.to_path_buf());
        }
        self.save();
    }

    fn save(&self) {
        // Rewrite the whole file — small enough (a few hundred lines max).
        let Ok(mut f) = File::create(&self.file) else {
            return;
        };
        for p in &self.paths {
            let _ = writeln!(f, "{}", p.display());
        }
    }
}

/// Latest schema version we accept from `.analysis-cache`. Entries
/// older than this get dropped at load time (and re-analysed by the
/// background worker on the next launch).
pub const CACHE_VERSION: u32 = 2;

#[derive(Clone, Debug)]
pub struct CachedAnalysis {
    pub bpm: f32,
    pub key: Option<MusicalKey>,
    pub beats: Vec<f64>,
    /// Indices into `beats` of bar-position-1 downbeats. Empty for
    /// pre-v2 entries (the worker re-analyses them on next launch).
    pub downbeats: Vec<u32>,
    /// Schema version that produced this entry. Anything below
    /// `CACHE_VERSION` is treated by the worker as "needs re-analysis"
    /// (e.g. legacy entries with no downbeats; once the user installs
    /// the model the worker upgrades them on the next launch).
    pub version: u32,
}

pub struct AnalysisCache {
    file: PathBuf,
    entries: Mutex<HashMap<PathBuf, CachedAnalysis>>,
}

impl AnalysisCache {
    pub fn load(dir: &Path) -> Self {
        let file = dir.join(".analysis-cache");
        let mut entries = HashMap::new();
        if let Ok(f) = File::open(&file) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some((path, a)) = parse_line(&line) {
                    entries.insert(path, a);
                }
            }
        }
        Self {
            file,
            entries: Mutex::new(entries),
        }
    }

    pub fn get(&self, path: &Path) -> Option<CachedAnalysis> {
        self.entries.lock().ok()?.get(path).cloned()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.entries
            .lock()
            .map(|m| m.contains_key(path))
            .unwrap_or(false)
    }

    /// True iff we have an entry for `path` *and* it was produced by
    /// the current schema (or newer). Worker uses this to decide
    /// whether to re-analyse a track that has a stale legacy entry.
    pub fn is_current(&self, path: &Path) -> bool {
        self.entries
            .lock()
            .ok()
            .and_then(|m| m.get(path).map(|c| c.version >= CACHE_VERSION))
            .unwrap_or(false)
    }

    /// Insert + append to disk. Cheap: appends one line.
    pub fn insert(&self, path: PathBuf, analysis: CachedAnalysis) {
        if let Ok(mut m) = self.entries.lock() {
            m.insert(path.clone(), analysis.clone());
        }
        let _ = self.append_line(&path, &analysis);
    }

    pub fn count(&self) -> usize {
        self.entries.lock().map(|m| m.len()).unwrap_or(0)
    }

    fn append_line(&self, path: &Path, a: &CachedAnalysis) -> std::io::Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)?;
        let path_str = path.to_string_lossy();
        if path_str.contains('|') {
            // Pipe in path — can't roundtrip; skip writing rather than corrupt.
            return Ok(());
        }
        let tonic = a.key.map(|k| k.tonic as i32).unwrap_or(-1);
        let is_minor = a.key.map(|k| k.is_minor).unwrap_or(false);
        let beats: String = a
            .beats
            .iter()
            .map(|b| format!("{b:.4}"))
            .collect::<Vec<_>>()
            .join(",");
        let downbeats: String = a
            .downbeats
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // Format: "v<version>|path|bpm|tonic|is_minor|beats|downbeats".
        // The downbeats column is only meaningful at v2+; older
        // versions write an empty string there.
        writeln!(
            f,
            "v{}|{}|{:.3}|{}|{}|{}|{}",
            a.version, path_str, a.bpm, tonic, is_minor, beats, downbeats
        )
    }
}

fn parse_line(line: &str) -> Option<(PathBuf, CachedAnalysis)> {
    // Try v2 first, then v1, then the unprefixed legacy format. The
    // worker upgrades anything below CACHE_VERSION on the next pass.
    if let Some(rest) = line.strip_prefix("v2|") {
        return parse_v2(rest);
    }
    if let Some(rest) = line.strip_prefix("v1|") {
        return parse_v1(rest);
    }
    parse_v1(line)
}

fn parse_v2(rest: &str) -> Option<(PathBuf, CachedAnalysis)> {
    let mut parts = rest.splitn(6, '|');
    let path = parts.next()?;
    let bpm = parts.next()?.parse::<f32>().ok()?;
    let tonic = parts.next()?.parse::<i32>().ok()?;
    let is_minor = parts.next()?.parse::<bool>().ok()?;
    let beats_str = parts.next()?;
    let downbeats_str = parts.next()?;
    let beats = parse_f64_csv(beats_str);
    let downbeats = parse_u32_csv(downbeats_str);
    let key = make_key(tonic, is_minor);
    Some((
        PathBuf::from(path),
        CachedAnalysis { bpm, key, beats, downbeats, version: 2 },
    ))
}

fn parse_v1(rest: &str) -> Option<(PathBuf, CachedAnalysis)> {
    let mut parts = rest.splitn(5, '|');
    let path = parts.next()?;
    let bpm = parts.next()?.parse::<f32>().ok()?;
    let tonic = parts.next()?.parse::<i32>().ok()?;
    let is_minor = parts.next()?.parse::<bool>().ok()?;
    let beats_str = parts.next()?;
    let beats = parse_f64_csv(beats_str);
    let key = make_key(tonic, is_minor);
    Some((
        PathBuf::from(path),
        CachedAnalysis { bpm, key, beats, downbeats: Vec::new(), version: 1 },
    ))
}

fn parse_f64_csv(s: &str) -> Vec<f64> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(',').filter_map(|x| x.parse().ok()).collect()
    }
}

fn parse_u32_csv(s: &str) -> Vec<u32> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(',').filter_map(|x| x.parse().ok()).collect()
    }
}

fn make_key(tonic: i32, is_minor: bool) -> Option<MusicalKey> {
    if (0..12).contains(&tonic) {
        Some(MusicalKey {
            tonic: tonic as u8,
            is_minor,
        })
    } else {
        None
    }
}

/// Camelot Wheel compatibility (harmonic mixing). Two keys are compatible iff:
/// - identical, or
/// - same number, different letter (relative major/minor), or
/// - ±1 number on the wheel with same letter (perfect 5th up/down).
pub fn camelot_compatible(a: MusicalKey, b: MusicalKey) -> bool {
    if a == b {
        return true;
    }
    let an = camelot_number(a);
    let bn = camelot_number(b);
    if an == bn {
        // Same number, any letter (relative).
        return true;
    }
    if a.is_minor == b.is_minor {
        // Same letter — ±1 on the wheel counts as harmonic.
        let diff = (an as i32 - bn as i32).rem_euclid(12);
        let d = diff.min(12 - diff);
        if d == 1 {
            return true;
        }
    }
    false
}

fn camelot_number(k: MusicalKey) -> u8 {
    const MAJOR: [u8; 12] = [8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6, 1];
    let major_tonic = if k.is_minor {
        (k.tonic + 3) % 12
    } else {
        k.tonic % 12
    };
    MAJOR[major_tonic as usize]
}
