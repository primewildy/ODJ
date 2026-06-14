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

    /// Snapshot of the inner set. Cloned into the auto-mix shared
    /// state for cross-thread access.
    pub fn paths(&self) -> &HashSet<PathBuf> {
        &self.paths
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
    /// Track length in seconds. `None` for legacy v1/v2 entries that
    /// pre-date the duration column; the UI falls back to a beats-
    /// derived heuristic in that case. v3+ entries always carry it.
    pub duration_secs: Option<f64>,
}

pub struct AnalysisCache {
    file: PathBuf,
    entries: Mutex<HashMap<PathBuf, CachedAnalysis>>,
    /// Bumped every time `insert` lands. Lets the track-table cache
    /// invalidate after a background analysis tick adds a new entry —
    /// new BPM / key data may change the sort order. (Field can't be
    /// named `gen` — that's a reserved keyword in Rust 2024.)
    cache_gen: std::sync::atomic::AtomicU64,
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
            cache_gen: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Monotonically-increasing generation counter. Bumped on every
    /// `insert` so consumers (e.g. the track-table filter/sort cache)
    /// can detect that the cache contents may have changed.
    pub fn generation(&self) -> u64 {
        self.cache_gen.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn get(&self, path: &Path) -> Option<CachedAnalysis> {
        self.entries.lock().ok()?.get(path).cloned()
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
        self.cache_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // Format: "v3|path|bpm|tonic|is_minor|beats|downbeats|duration".
        // v3 added the trailing duration field; parsers fall back to
        // v2/v1 for older entries on disk. Duration writes as 0.000
        // when we don't know it (worker should always supply it; this
        // is just defensive).
        let duration = a.duration_secs.unwrap_or(0.0);
        writeln!(
            f,
            "v3|{}|{:.3}|{}|{}|{}|{}|{:.3}",
            path_str, a.bpm, tonic, is_minor, beats, downbeats, duration
        )
    }
}

fn parse_line(line: &str) -> Option<(PathBuf, CachedAnalysis)> {
    // Try newest first. Older variants stay loadable so the cache
    // file isn't invalidated when the schema rolls — the worker
    // upgrades anything below CACHE_VERSION on the next pass for
    // schema-breaking changes; additive ones (like the v3 duration
    // column) simply read as None and the UI falls back.
    if let Some(rest) = line.strip_prefix("v3|") {
        return parse_v3(rest);
    }
    if let Some(rest) = line.strip_prefix("v2|") {
        return parse_v2(rest);
    }
    if let Some(rest) = line.strip_prefix("v1|") {
        return parse_v1(rest);
    }
    parse_v1(line)
}

fn parse_v3(rest: &str) -> Option<(PathBuf, CachedAnalysis)> {
    let mut parts = rest.splitn(7, '|');
    let path = parts.next()?;
    let bpm = parts.next()?.parse::<f32>().ok()?;
    let tonic = parts.next()?.parse::<i32>().ok()?;
    let is_minor = parts.next()?.parse::<bool>().ok()?;
    let beats_str = parts.next()?;
    let downbeats_str = parts.next()?;
    let duration_str = parts.next()?;
    let beats = parse_f64_csv(beats_str);
    let downbeats = parse_u32_csv(downbeats_str);
    let duration = duration_str.parse::<f64>().ok()
        .filter(|d| d.is_finite() && *d > 0.0);
    let key = make_key(tonic, is_minor);
    Some((
        PathBuf::from(path),
        CachedAnalysis { bpm, key, beats, downbeats, version: 3, duration_secs: duration },
    ))
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
        CachedAnalysis { bpm, key, beats, downbeats, version: 2, duration_secs: None },
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
        CachedAnalysis { bpm, key, beats, downbeats: Vec::new(), version: 1, duration_secs: None },
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

/// Per-track *user-authored* metadata, persisted across sessions in
/// `.track-meta` in the music dir. **Not** the analysis cache:
/// that file is disposable and worker-regenerated, while this one
/// holds data the user touched and must never be lost.
///
/// Today carries only hot cues (§3); designed to grow with the
/// beat-grid override (§2) and saved loops as those features land —
/// fields the parser doesn't recognise are skipped silently so old
/// app versions don't blow away newer entries.
///
/// File format (one line per track):
///     `v1|<absolute_path>|<key>=<value>;<key>=<value>;…`
///
/// Known keys:
/// * `hot_cues` — eight comma-separated f64 seconds (empty = unset)
/// * `hot_cue_labels` — eight `:`-separated labels (empty = unlabelled).
///   Any `:`, `;`, `|`, `,`, `\n`, `\r` in the user's label is replaced
///   with an underscore on write — keeps the field-and-line format
///   simple. Labels are forward-compat: older versions ignore them.
/// * `hot_cue_colours` — eight comma-separated 6-digit RRGGBB hex
///   (empty = use default `pal.hot_cue`).
/// * `grid_bpm` — single f32 BPM for the manual override.
/// * `grid_beats` — comma-separated beat times in seconds for the
///   override; presence of this field marks the track as
///   "manually-gridded" and suppresses the kick-align + refined
///   `UpdateAnalysis` sends on load.
/// * `grid_downbeats` — comma-separated u32 indices into `grid_beats`.
#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    /// 8 hot-cue slot positions in **seconds**, `None` when unset.
    /// Seconds (not frames) so the file is sample-rate-independent.
    pub hot_cues: [Option<f64>; 8],
    /// Optional user-facing label per slot ("Intro", "Drop", …).
    pub hot_cue_labels: [Option<String>; 8],
    /// Optional RRGGBB packed into a u32 (top byte unused).
    pub hot_cue_colours: [Option<u32>; 8],
    /// Manual beat-grid override. When `Some`, this grid replaces the
    /// analyser's result on every load — kick-align and refined-model
    /// updates are also suppressed so this stays the source of truth.
    pub grid_override: Option<GridOverride>,
}

/// Stored beat grid for a track. Frozen point-in-time snapshot; if the
/// analyser later improves, the user can `Reset to analysis` from the
/// grid-adjust panel and the override is removed.
#[derive(Debug, Clone)]
pub struct GridOverride {
    pub bpm: f32,
    /// Beat times in seconds from the start of the track.
    pub beat_grid: Vec<f64>,
    /// Indices into `beat_grid` of bar-1 downbeats.
    pub downbeats: Vec<u32>,
}

impl TrackMeta {
    /// True iff this entry holds anything worth persisting.
    pub fn is_empty(&self) -> bool {
        self.hot_cues.iter().all(Option::is_none)
            && self.hot_cue_labels.iter().all(Option::is_none)
            && self.hot_cue_colours.iter().all(Option::is_none)
            && self.grid_override.is_none()
    }
}

pub struct TrackMetaStore {
    file: PathBuf,
    entries: HashMap<PathBuf, TrackMeta>,
}

impl TrackMetaStore {
    pub fn load(dir: &Path) -> Self {
        let file = dir.join(".track-meta");
        let mut entries: HashMap<PathBuf, TrackMeta> = HashMap::new();
        if let Ok(f) = File::open(&file) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some((path, meta)) = parse_track_meta_line(&line) {
                    entries.insert(path, meta);
                }
            }
        }
        Self { file, entries }
    }

    pub fn get(&self, path: &Path) -> Option<&TrackMeta> {
        self.entries.get(path)
    }

    /// Overwrite the hot-cue slots for a track and persist. Removes
    /// the entry entirely when the resulting `TrackMeta` is empty so
    /// the file doesn't accrete dead rows for cleared cues.
    pub fn set_hot_cues(&mut self, path: &Path, hot_cues: [Option<f64>; 8]) {
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        entry.hot_cues = hot_cues;
        // Clearing a slot also clears its label / colour — they're
        // attached to the slot, not the track.
        for (i, slot) in hot_cues.iter().enumerate() {
            if slot.is_none() {
                entry.hot_cue_labels[i] = None;
                entry.hot_cue_colours[i] = None;
            }
        }
        if entry.is_empty() {
            self.entries.remove(path);
        }
        let _ = self.save();
    }

    /// Update the label on a single slot. `None` removes the label.
    pub fn set_hot_cue_label(&mut self, path: &Path, slot: usize, label: Option<String>) {
        if slot >= 8 { return; }
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        entry.hot_cue_labels[slot] = label.filter(|s| !s.is_empty());
        if entry.is_empty() {
            self.entries.remove(path);
        }
        let _ = self.save();
    }

    /// Update the slot colour. `None` reverts to the palette default.
    pub fn set_hot_cue_colour(&mut self, path: &Path, slot: usize, rgb: Option<u32>) {
        if slot >= 8 { return; }
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        entry.hot_cue_colours[slot] = rgb;
        if entry.is_empty() {
            self.entries.remove(path);
        }
        let _ = self.save();
    }

    /// Store (or remove with `None`) a manual beat-grid override. The
    /// load worker checks this on every load — when present, the
    /// grid wins over both the cache entry and the refined analyser.
    pub fn set_grid_override(&mut self, path: &Path, grid: Option<GridOverride>) {
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        entry.grid_override = grid;
        if entry.is_empty() {
            self.entries.remove(path);
        }
        let _ = self.save();
    }

    /// Atomic rewrite — write-temp + rename. Same approach the
    /// other persistence stores use; protects against partial
    /// writes if the process is killed mid-save.
    fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.file.with_extension("track-meta.tmp");
        {
            let mut f = File::create(&tmp)?;
            for (path, meta) in &self.entries {
                let path_str = path.to_string_lossy();
                if path_str.contains('|') {
                    // Same compromise as the analysis cache: pipe
                    // in a path means we can't round-trip; skip.
                    continue;
                }
                let mut fields = Vec::new();
                if meta.hot_cues.iter().any(Option::is_some) {
                    let csv: Vec<String> = meta.hot_cues.iter()
                        .map(|o| o.map(|v| format!("{v:.4}")).unwrap_or_default())
                        .collect();
                    fields.push(format!("hot_cues={}", csv.join(",")));
                }
                if meta.hot_cue_labels.iter().any(Option::is_some) {
                    let csv: Vec<String> = meta.hot_cue_labels.iter()
                        .map(|o| o.as_deref().map(sanitise_label).unwrap_or_default())
                        .collect();
                    fields.push(format!("hot_cue_labels={}", csv.join(":")));
                }
                if meta.hot_cue_colours.iter().any(Option::is_some) {
                    let csv: Vec<String> = meta.hot_cue_colours.iter()
                        .map(|o| o.map(|v| format!("{:06X}", v & 0xFFFFFF)).unwrap_or_default())
                        .collect();
                    fields.push(format!("hot_cue_colours={}", csv.join(",")));
                }
                if let Some(g) = &meta.grid_override {
                    fields.push(format!("grid_bpm={:.4}", g.bpm));
                    let beats_csv: Vec<String> = g.beat_grid.iter()
                        .map(|b| format!("{b:.4}"))
                        .collect();
                    fields.push(format!("grid_beats={}", beats_csv.join(",")));
                    let db_csv: Vec<String> = g.downbeats.iter()
                        .map(|i| i.to_string())
                        .collect();
                    fields.push(format!("grid_downbeats={}", db_csv.join(",")));
                }
                if fields.is_empty() { continue; }
                writeln!(f, "v1|{}|{}", path_str, fields.join(";"))?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.file)
    }
}

fn parse_track_meta_line(line: &str) -> Option<(PathBuf, TrackMeta)> {
    // splitn(3) so the third field keeps any `|` it might contain
    // (none today, but future-proof).
    let mut parts = line.splitn(3, '|');
    if parts.next()? != "v1" {
        return None;
    }
    let path_str = parts.next()?;
    let rest = parts.next().unwrap_or("");
    let mut meta = TrackMeta::default();
    for field in rest.split(';') {
        let Some((key, value)) = field.split_once('=') else { continue };
        match key {
            "hot_cues" => {
                for (i, v) in value.split(',').take(8).enumerate() {
                    let v = v.trim();
                    if v.is_empty() { continue; }
                    if let Ok(secs) = v.parse::<f64>() {
                        if secs.is_finite() && secs >= 0.0 {
                            meta.hot_cues[i] = Some(secs);
                        }
                    }
                }
            }
            "hot_cue_labels" => {
                for (i, v) in value.split(':').take(8).enumerate() {
                    if v.is_empty() { continue; }
                    meta.hot_cue_labels[i] = Some(v.to_string());
                }
            }
            "hot_cue_colours" => {
                for (i, v) in value.split(',').take(8).enumerate() {
                    let v = v.trim();
                    if v.is_empty() { continue; }
                    if let Ok(c) = u32::from_str_radix(v, 16) {
                        meta.hot_cue_colours[i] = Some(c & 0xFFFFFF);
                    }
                }
            }
            "grid_bpm" => {
                if let Ok(b) = value.trim().parse::<f32>() {
                    if b.is_finite() && b > 0.0 {
                        meta.grid_override
                            .get_or_insert_with(GridOverride::empty)
                            .bpm = b;
                    }
                }
            }
            "grid_beats" => {
                let beats: Vec<f64> = value.split(',')
                    .filter_map(|s| s.trim().parse::<f64>().ok())
                    .filter(|b| b.is_finite() && *b >= 0.0)
                    .collect();
                if !beats.is_empty() {
                    meta.grid_override
                        .get_or_insert_with(GridOverride::empty)
                        .beat_grid = beats;
                }
            }
            "grid_downbeats" => {
                let dbs: Vec<u32> = value.split(',')
                    .filter_map(|s| s.trim().parse::<u32>().ok())
                    .collect();
                meta.grid_override
                    .get_or_insert_with(GridOverride::empty)
                    .downbeats = dbs;
            }
            _ => {} // unknown field — leave room for saved loops
        }
    }
    // A partial grid override (grid_beats but no grid_bpm, say) isn't
    // meaningful — drop it so we don't divide by zero downstream.
    if let Some(g) = &meta.grid_override {
        if g.bpm <= 0.0 || g.beat_grid.is_empty() {
            meta.grid_override = None;
        }
    }
    Some((PathBuf::from(path_str), meta))
}

impl GridOverride {
    fn empty() -> Self {
        Self { bpm: 0.0, beat_grid: Vec::new(), downbeats: Vec::new() }
    }
}

/// Replace any of `:`, `;`, `|`, `,`, `\n`, `\r` with `_` so the label
/// can't break the line / field / sub-field format. Leading/trailing
/// whitespace is trimmed so a stray space doesn't show up after a
/// round-trip.
fn sanitise_label(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            ':' | ';' | '|' | ',' | '\n' | '\r' => '_',
            other => other,
        })
        .collect()
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

#[cfg(test)]
mod tests {
    //! Regression tests for the pure helpers used by the track-table
    //! sort/filter pipeline and the on-disk cache parser. Targets
    //! exactly the behaviour the rest of the app relies on (and the
    //! pieces that must survive an eframe upgrade unchanged).

    use super::*;

    fn key(tonic: u8, is_minor: bool) -> MusicalKey {
        MusicalKey { tonic, is_minor }
    }

    // ---- make_key -------------------------------------------------

    #[test]
    fn make_key_rejects_negative_tonic() {
        assert!(make_key(-1, false).is_none());
        assert!(make_key(-1, true).is_none());
    }

    #[test]
    fn make_key_accepts_valid_range() {
        for t in 0..12 {
            assert_eq!(make_key(t, false), Some(key(t as u8, false)));
            assert_eq!(make_key(t, true), Some(key(t as u8, true)));
        }
    }

    // ---- camelot_compatible --------------------------------------

    #[test]
    fn camelot_identical_is_compatible() {
        let k = key(0, true);
        assert!(camelot_compatible(k, k));
    }

    #[test]
    fn camelot_relative_major_minor_is_compatible() {
        // C minor (tonic=0, minor) ↔ Eb major (tonic=3, major)
        // — both at wheel position 5.
        assert!(camelot_compatible(key(0, true), key(3, false)));
        assert!(camelot_compatible(key(3, false), key(0, true)));
    }

    #[test]
    fn camelot_plus_minus_one_same_letter_is_compatible() {
        // C major (8B) → G major (9B): +1, same letter.
        assert!(camelot_compatible(key(0, false), key(7, false)));
        // C major (8B) → F major (7B): -1, same letter.
        assert!(camelot_compatible(key(0, false), key(5, false)));
        // A minor (8A) → E minor (9A): +1, same letter.
        assert!(camelot_compatible(key(9, true), key(4, true)));
    }

    #[test]
    fn camelot_distant_keys_incompatible() {
        // C major (8B) and F# major (2B) — opposite the wheel.
        assert!(!camelot_compatible(key(0, false), key(6, false)));
        // Tritone — fundamentally clashy.
        assert!(!camelot_compatible(key(0, false), key(6, true)));
    }

    #[test]
    fn camelot_plus_one_with_different_letter_incompatible() {
        // C major (8B) vs Bm (10A) — +/- 1 but different letter
        // → only same-letter step counts as harmonic.
        let cmaj = key(0, false); // 8B
        let bm = key(11, true);    // ... wheel-pos depends on math; the point is the rule.
        let same_step_diff_letter =
            camelot_compatible(cmaj, bm) && camelot_number(cmaj) != camelot_number(bm);
        assert!(
            !same_step_diff_letter,
            "+/- 1 with different letter must be REJECTED"
        );
    }

    #[test]
    fn camelot_is_symmetric_and_reflexive() {
        // Spot-check 12 sample pairs — compat(a,b) == compat(b,a),
        // and compat(a,a) is always true.
        for t in 0..12u8 {
            for &m in &[false, true] {
                let k = key(t, m);
                assert!(camelot_compatible(k, k));
            }
        }
        let a = key(0, false);
        let b = key(7, false);
        assert_eq!(camelot_compatible(a, b), camelot_compatible(b, a));
    }

    // ---- cache parse_line (v1, v2, legacy) -----------------------

    #[test]
    fn parse_line_v2_roundtrip() {
        // v2|path|bpm|tonic|is_minor|beats|downbeats
        let line = "v2|/tmp/song.mp3|128.000|0|false|0.5,1.0,1.5|0,4";
        let (path, c) = parse_line(line).expect("parse v2");
        assert_eq!(path.to_string_lossy(), "/tmp/song.mp3");
        assert!((c.bpm - 128.0).abs() < 0.01);
        assert_eq!(c.key, Some(key(0, false)));
        assert_eq!(c.beats, vec![0.5, 1.0, 1.5]);
        assert_eq!(c.downbeats, vec![0, 4]);
        assert_eq!(c.version, 2);
    }

    #[test]
    fn parse_line_v1_has_no_downbeats() {
        // v1|path|bpm|tonic|is_minor|beats
        let line = "v1|/tmp/song.mp3|128.000|0|false|0.5,1.0,1.5";
        let (_, c) = parse_line(line).expect("parse v1");
        assert!(c.downbeats.is_empty());
        assert_eq!(c.version, 1);
    }

    #[test]
    fn parse_line_rejects_garbage() {
        assert!(parse_line("").is_none());
        assert!(parse_line("not a cache line").is_none());
        assert!(parse_line("v2|/p|notabpm|0|false||").is_none());
    }

    #[test]
    fn parse_line_minor_key_roundtrip() {
        let line = "v2|/tmp/m.mp3|120.0|9|true||";
        let (_, c) = parse_line(line).unwrap();
        assert_eq!(c.key, Some(key(9, true)));
    }

    // ---- TrackMeta ------------------------------------------------

    #[test]
    fn track_meta_parses_full_hot_cues() {
        let line = "v1|/tmp/song.mp3|hot_cues=1.5,12.0,,45.3,,,78.9,";
        let (path, meta) = parse_track_meta_line(line).unwrap();
        assert_eq!(path.to_string_lossy(), "/tmp/song.mp3");
        assert_eq!(meta.hot_cues[0], Some(1.5));
        assert_eq!(meta.hot_cues[1], Some(12.0));
        assert_eq!(meta.hot_cues[2], None);
        assert_eq!(meta.hot_cues[3], Some(45.3));
        assert_eq!(meta.hot_cues[4], None);
        assert_eq!(meta.hot_cues[5], None);
        assert_eq!(meta.hot_cues[6], Some(78.9));
        assert_eq!(meta.hot_cues[7], None);
    }

    #[test]
    fn track_meta_skips_unknown_fields() {
        // Forward-compat: a future `beat_grid=…` from a newer build
        // shouldn't blow away the hot cues on load.
        let line = "v1|/p.mp3|hot_cues=2.0,,,,,,,;beat_grid=0.0,0.5,1.0;loops=1.0:2.0";
        let (_, meta) = parse_track_meta_line(line).unwrap();
        assert_eq!(meta.hot_cues[0], Some(2.0));
    }

    #[test]
    fn track_meta_rejects_garbage() {
        assert!(parse_track_meta_line("").is_none());
        assert!(parse_track_meta_line("vXX|/p|hot_cues=").is_none());
        // Missing fields field is ok — empty meta with that path.
        let (_, m) = parse_track_meta_line("v1|/p|").unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn track_meta_is_empty_check() {
        let mut m = TrackMeta::default();
        assert!(m.is_empty());
        m.hot_cues[3] = Some(5.0);
        assert!(!m.is_empty());
    }
}
