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

#[derive(Clone, Debug)]
pub struct CachedAnalysis {
    pub bpm: f32,
    pub key: Option<MusicalKey>,
    pub beats: Vec<f64>,
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
        writeln!(
            f,
            "{}|{:.3}|{}|{}|{}",
            path_str, a.bpm, tonic, is_minor, beats
        )
    }
}

fn parse_line(line: &str) -> Option<(PathBuf, CachedAnalysis)> {
    let mut parts = line.splitn(5, '|');
    let path = parts.next()?;
    let bpm = parts.next()?.parse::<f32>().ok()?;
    let tonic = parts.next()?.parse::<i32>().ok()?;
    let is_minor = parts.next()?.parse::<bool>().ok()?;
    let beats_str = parts.next()?;
    let beats: Vec<f64> = if beats_str.is_empty() {
        Vec::new()
    } else {
        beats_str
            .split(',')
            .filter_map(|s| s.parse::<f64>().ok())
            .collect()
    };
    let key = if (0..12).contains(&tonic) {
        Some(MusicalKey {
            tonic: tonic as u8,
            is_minor,
        })
    } else {
        None
    };
    Some((
        PathBuf::from(path),
        CachedAnalysis { bpm, key, beats },
    ))
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
