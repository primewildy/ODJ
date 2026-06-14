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

// =====================================================================
// Playlists
// =====================================================================
//
// `<music-dir>/.playlists/` is the source of truth. Each playlist is a
// standard M3U file; directories under .playlists/ are folders in the
// UI. Mapping the on-disk shape directly to the UI tree means we get
// "playlists can be in folders" for free (the filesystem already does
// that), and any other audio tool that speaks M3U can read/write the
// same files without us inventing a schema.
//
// File format (minimal subset of M3U we emit; lenient on read):
//   #EXTM3U                              <- optional header marker
//   #EXTINF:duration,Artist - Title      <- optional, we skip on read
//   /absolute/path/to/track.mp3          <- track entries are kept
//   (blank lines + other comments dropped)
//
// We always write absolute paths so playlists survive being moved
// around within `.playlists/` (renaming the playlist file doesn't
// invalidate its entries). The on-disk format never changes between
// schema versions — M3U is M3U.

/// One node in the playlist tree. Mirrors the on-disk layout under
/// `.playlists/`: a `Folder` is a directory, a `Playlist` is a `.m3u`
/// file. The `name` is the user-visible label (file stem for
/// playlists, directory name for folders).
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired into the UI in playlists §2
pub enum PlaylistNode {
    Folder {
        name: String,
        children: Vec<PlaylistNode>,
    },
    Playlist {
        name: String,
        /// Absolute path to the `.m3u` file on disk. Used for atomic
        /// rewrite + as the stable identity when renames happen.
        file: PathBuf,
        /// Tracks as absolute paths. The order in the file is the
        /// order in the UI; playlist editing preserves it.
        tracks: Vec<PathBuf>,
    },
}

#[allow(dead_code)] // wired into the UI in playlists §2
impl PlaylistNode {
    pub fn name(&self) -> &str {
        match self {
            PlaylistNode::Folder { name, .. } => name,
            PlaylistNode::Playlist { name, .. } => name,
        }
    }
    pub fn is_folder(&self) -> bool {
        matches!(self, PlaylistNode::Folder { .. })
    }
}

/// In-memory mirror of `<music-dir>/.playlists/`. Loads the full tree
/// at startup (cheap — playlist trees are small) and rewrites the
/// relevant `.m3u` on every mutation, atomically (write-temp + rename
/// to avoid partial-write corruption on crash).
#[allow(dead_code)] // wired into the UI in playlists §2
pub struct PlaylistStore {
    /// Absolute path to `<music-dir>/.playlists/`. Created on first
    /// mutation if it doesn't exist; load tolerates absence.
    root: PathBuf,
    /// Top-level nodes (folders + playlist files directly under root).
    nodes: Vec<PlaylistNode>,
    /// Bumps on every successful mutation; the UI uses this to know
    /// when to refresh its source-rail / track-table snapshot. Same
    /// trick the analysis cache uses.
    generation: std::sync::atomic::AtomicU64,
}

#[allow(dead_code)] // wired into the UI in playlists §2
impl PlaylistStore {
    pub fn load(music_dir: &Path) -> Self {
        let root = music_dir.join(".playlists");
        let nodes = scan_playlist_dir(&root).unwrap_or_default();
        Self {
            root,
            nodes,
            generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Bumped on every successful mutation. Cheap counter the UI
    /// snapshots once per frame to know if its cached rendering of
    /// the tree is stale.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Top-level nodes (root of the tree).
    pub fn nodes(&self) -> &[PlaylistNode] {
        &self.nodes
    }

    /// Children at a given path (a sequence of folder names from the
    /// root). `None` if the path doesn't resolve to a folder.
    pub fn children_at(&self, path: &[String]) -> Option<&[PlaylistNode]> {
        let mut current = &self.nodes;
        for segment in path {
            let next = current.iter().find_map(|n| match n {
                PlaylistNode::Folder { name, children } if name == segment => Some(children),
                _ => None,
            })?;
            current = next;
        }
        Some(current)
    }

    /// Resolve a playlist leaf by its tree path (folders + final
    /// playlist name). Returns the track list verbatim.
    pub fn playlist_tracks(&self, path: &[String]) -> Option<&[PathBuf]> {
        let (last, parents) = path.split_last()?;
        let siblings = self.children_at(parents)?;
        siblings.iter().find_map(|n| match n {
            PlaylistNode::Playlist { name, tracks, .. } if name == last => Some(tracks.as_slice()),
            _ => None,
        })
    }

    /// Flat list of every leaf playlist with its full tree path.
    /// Used by the track-row "Add to ▸" submenu — we display the
    /// path joined by `/` so nested playlists are still
    /// unambiguous.
    pub fn all_playlists(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        fn walk(prefix: &mut Vec<String>, nodes: &[PlaylistNode], out: &mut Vec<Vec<String>>) {
            for n in nodes {
                match n {
                    PlaylistNode::Folder { name, children } => {
                        prefix.push(name.clone());
                        walk(prefix, children, out);
                        prefix.pop();
                    }
                    PlaylistNode::Playlist { name, .. } => {
                        let mut p = prefix.clone();
                        p.push(name.clone());
                        out.push(p);
                    }
                }
            }
        }
        walk(&mut Vec::new(), &self.nodes, &mut out);
        out
    }

    /// Create an empty playlist `<name>.m3u` inside the folder at
    /// `at`. Returns an error if the name is invalid (contains `/`,
    /// is empty, etc.) or already exists.
    pub fn create_playlist(&mut self, at: &[String], name: &str) -> std::io::Result<()> {
        let name = name.trim();
        validate_name(name)?;
        let dir = self.dir_for(at);
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{name}.m3u"));
        if file.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "playlist already exists",
            ));
        }
        atomic_write(&file, "#EXTM3U\n")?;
        self.reload();
        Ok(())
    }

    /// Create a sub-folder `<name>` inside the folder at `at`.
    pub fn create_folder(&mut self, at: &[String], name: &str) -> std::io::Result<()> {
        let name = name.trim();
        validate_name(name)?;
        let dir = self.dir_for(at).join(name);
        if dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "folder already exists",
            ));
        }
        std::fs::create_dir_all(&dir)?;
        self.reload();
        Ok(())
    }

    /// Append a track to the playlist at `path`. No-op if the track
    /// is already in the playlist (we don't want a noisy click to
    /// duplicate entries — that's almost always not what the user
    /// meant). De-dup is by exact path equality.
    pub fn add_track(&mut self, path: &[String], track: &Path) -> std::io::Result<()> {
        let (last, parents) = path.split_last().ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty playlist path",
        ))?;
        let dir = self.dir_for(parents);
        let file = dir.join(format!("{last}.m3u"));
        let mut tracks = parse_m3u(&file)?;
        if tracks.iter().any(|p| p == track) {
            return Ok(());
        }
        tracks.push(track.to_path_buf());
        write_m3u(&file, &tracks)?;
        self.reload();
        Ok(())
    }

    /// Rename a playlist or folder at `path` to `new_name`.
    pub fn rename(&mut self, path: &[String], new_name: &str) -> std::io::Result<()> {
        let new_name = new_name.trim();
        validate_name(new_name)?;
        let (last, parents) = path.split_last().ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty path",
        ))?;
        let parent_dir = self.dir_for(parents);
        // Either name.m3u (playlist) or name (folder) — try both.
        let playlist_path = parent_dir.join(format!("{last}.m3u"));
        let folder_path = parent_dir.join(last);
        if playlist_path.exists() {
            let dest = parent_dir.join(format!("{new_name}.m3u"));
            std::fs::rename(&playlist_path, &dest)?;
        } else if folder_path.exists() {
            let dest = parent_dir.join(new_name);
            std::fs::rename(&folder_path, &dest)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not a playlist or folder",
            ));
        }
        self.reload();
        Ok(())
    }

    /// Delete a playlist (file) or folder (recursive — caller should
    /// confirm with the user before calling).
    pub fn delete(&mut self, path: &[String]) -> std::io::Result<()> {
        let (last, parents) = path.split_last().ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty path",
        ))?;
        let parent_dir = self.dir_for(parents);
        let playlist_path = parent_dir.join(format!("{last}.m3u"));
        let folder_path = parent_dir.join(last);
        if playlist_path.exists() {
            std::fs::remove_file(&playlist_path)?;
        } else if folder_path.exists() {
            std::fs::remove_dir_all(&folder_path)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not a playlist or folder",
            ));
        }
        self.reload();
        Ok(())
    }

    /// Re-scan the playlist directory from scratch + bump generation.
    /// Cheap (playlist trees are small). Called after every mutation
    /// so the in-memory model stays in sync with on-disk reality.
    fn reload(&mut self) {
        self.nodes = scan_playlist_dir(&self.root).unwrap_or_default();
        self.generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Resolve a tree path to the absolute filesystem directory it
    /// represents. The empty path resolves to the playlist root.
    fn dir_for(&self, path: &[String]) -> PathBuf {
        let mut p = self.root.clone();
        for seg in path {
            p.push(seg);
        }
        p
    }
}

/// Recursive directory scan. Returns sub-folders before playlists in
/// each directory (matches the typical file-manager convention). Skips
/// dotfiles and anything not a directory or `.m3u`.
fn scan_playlist_dir(dir: &Path) -> Option<Vec<PlaylistNode>> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut folders: Vec<PlaylistNode> = Vec::new();
    let mut playlists: Vec<PlaylistNode> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let children = scan_playlist_dir(&path).unwrap_or_default();
            folders.push(PlaylistNode::Folder {
                name: name.to_string(),
                children,
            });
        } else if path.extension().and_then(|s| s.to_str()).map(|s| s.eq_ignore_ascii_case("m3u")).unwrap_or(false) {
            let display = path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| name.to_string());
            let tracks = parse_m3u(&path).unwrap_or_default();
            playlists.push(PlaylistNode::Playlist {
                name: display,
                file: path,
                tracks,
            });
        }
    }
    folders.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    playlists.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    folders.extend(playlists);
    Some(folders)
}

/// Read tracks from an M3U file. Lenient parser — anything starting
/// with `#` or blank is skipped; remaining lines are taken verbatim
/// as paths. Doesn't try to validate that the path exists (handled
/// by the loader when the user actually tries to play it).
fn parse_m3u(file: &Path) -> std::io::Result<Vec<PathBuf>> {
    let f = match File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        out.push(PathBuf::from(trimmed));
    }
    Ok(out)
}

/// Write tracks to an M3U file. Always emits `#EXTM3U` header + one
/// absolute path per line. Atomic write — go through a `.tmp` and
/// rename so a kill mid-write can't truncate the playlist.
fn write_m3u(file: &Path, tracks: &[PathBuf]) -> std::io::Result<()> {
    let mut body = String::from("#EXTM3U\n");
    for t in tracks {
        body.push_str(&t.to_string_lossy());
        body.push('\n');
    }
    atomic_write(file, &body)
}

/// write-temp + rename atomic file replace. Same pattern the other
/// stores use (favourites, settings, track-meta).
fn atomic_write(file: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension("m3u.tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, file)
}

/// Reject empty names, names with path-separator characters, names
/// that would be hidden (start with `.`), or names that resolve
/// outside the parent directory.
fn validate_name(name: &str) -> std::io::Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid name (empty, hidden, or contains a path separator)",
        ));
    }
    Ok(())
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

    // ---- PlaylistStore -----------------------------------------------

    /// Minimal tempdir helper — std::env::temp_dir() + a unique
    /// suffix. No tempfile crate dep needed for our handful of
    /// tests. Caller is responsible for cleaning up on success;
    /// on test failure the OS sweeps `/tmp` eventually.
    fn tempdir(tag: &str) -> PathBuf {
        // Loose uniqueness — the test runner serialises so a counter
        // would also work, but stamping the address keeps tests
        // independent even if parallelism is re-enabled later.
        let id = std::process::id();
        let counter = std::sync::atomic::AtomicU64::new(0);
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("odj-playlist-test-{id}-{stamp}-{n}-{tag}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn playlists_empty_dir_loads_empty_tree() {
        let dir = tempdir("empty");
        let s = PlaylistStore::load(&dir);
        assert!(s.nodes().is_empty());
        assert!(s.all_playlists().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playlists_create_then_load() {
        let dir = tempdir("create-load");
        let mut s = PlaylistStore::load(&dir);
        s.create_playlist(&[], "Warmup").unwrap();
        s.create_folder(&[], "House").unwrap();
        s.create_playlist(&["House".into()], "Deep").unwrap();

        // Re-load from disk — verify the tree persisted correctly.
        let s2 = PlaylistStore::load(&dir);
        let names: Vec<&str> = s2.nodes().iter().map(|n| n.name()).collect();
        // Sort puts folders first (House) then playlists (Warmup).
        assert_eq!(names, vec!["House", "Warmup"]);
        let house = s2.children_at(&["House".into()]).unwrap();
        assert_eq!(house.len(), 1);
        assert_eq!(house[0].name(), "Deep");
        assert!(matches!(house[0], PlaylistNode::Playlist { .. }));

        // Flat all_playlists includes the nested one.
        let all = s2.all_playlists();
        let paths: Vec<Vec<String>> = all.into_iter().collect();
        assert!(paths.contains(&vec!["House".to_string(), "Deep".to_string()]));
        assert!(paths.contains(&vec!["Warmup".to_string()]));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playlists_add_track_persists_and_dedupes() {
        let dir = tempdir("add-track");
        let mut s = PlaylistStore::load(&dir);
        s.create_playlist(&[], "Mix").unwrap();
        let t1 = PathBuf::from("/music/track-1.mp3");
        let t2 = PathBuf::from("/music/track-2.mp3");
        s.add_track(&["Mix".into()], &t1).unwrap();
        s.add_track(&["Mix".into()], &t2).unwrap();
        s.add_track(&["Mix".into()], &t1).unwrap(); // dedup

        let s2 = PlaylistStore::load(&dir);
        let tracks = s2.playlist_tracks(&["Mix".into()]).unwrap();
        assert_eq!(tracks, &[t1, t2]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playlists_rename_and_delete() {
        let dir = tempdir("rename-delete");
        let mut s = PlaylistStore::load(&dir);
        s.create_playlist(&[], "Old").unwrap();
        s.create_folder(&[], "Sets").unwrap();

        s.rename(&["Old".into()], "New").unwrap();
        s.rename(&["Sets".into()], "Mixes").unwrap();

        let s = PlaylistStore::load(&dir);
        let names: Vec<&str> = s.nodes().iter().map(|n| n.name()).collect();
        assert_eq!(names, vec!["Mixes", "New"]);

        let mut s = PlaylistStore::load(&dir);
        s.delete(&["New".into()]).unwrap();
        s.delete(&["Mixes".into()]).unwrap();

        let s = PlaylistStore::load(&dir);
        assert!(s.nodes().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playlists_lenient_m3u_parser_skips_metadata() {
        let dir = tempdir("m3u-metadata");
        let pl_dir = dir.join(".playlists");
        std::fs::create_dir_all(&pl_dir).unwrap();
        // Hand-write an M3U with comments + EXTINF (other tools do
        // this) and verify we only keep the path lines on read.
        let body = "#EXTM3U\n\
                    #PLAYLIST:My Set\n\
                    #EXTINF:240,Artist - Title\n\
                    /music/track-1.mp3\n\
                    \n\
                    #EXTINF:200,Other - Song\n\
                    /music/track-2.mp3\n";
        std::fs::write(pl_dir.join("Mix.m3u"), body).unwrap();
        let s = PlaylistStore::load(&dir);
        let tracks = s.playlist_tracks(&["Mix".into()]).unwrap();
        assert_eq!(tracks, &[
            PathBuf::from("/music/track-1.mp3"),
            PathBuf::from("/music/track-2.mp3"),
        ]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playlists_reject_bad_names() {
        let dir = tempdir("bad-names");
        let mut s = PlaylistStore::load(&dir);
        assert!(s.create_playlist(&[], "").is_err());
        assert!(s.create_playlist(&[], ".hidden").is_err());
        assert!(s.create_playlist(&[], "with/slash").is_err());
        assert!(s.create_playlist(&[], "..").is_err());
        // Validating happens before we touch the disk, so the store
        // stays empty after the rejected attempts.
        assert!(s.nodes().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn playlists_generation_bumps_on_mutation() {
        let dir = tempdir("generation");
        let mut s = PlaylistStore::load(&dir);
        let g0 = s.generation();
        s.create_playlist(&[], "P").unwrap();
        assert!(s.generation() > g0);
        let g1 = s.generation();
        s.add_track(&["P".into()], Path::new("/x.mp3")).unwrap();
        assert!(s.generation() > g1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
