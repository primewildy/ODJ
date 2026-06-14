//! egui/eframe GUI: track picker on the left, two deck panels stacked on
//! the right. Each deck has: overview waveform, scrolling 16-beat zoom view
//! with beat grid, transport (Play/Pause + CUE), pitch slider, quantize.

mod auto_mix;
pub mod fonts;
mod grid_edit;
mod history;
mod network_output;
pub mod palette;
mod persistence;
pub mod settings;
// Smart-playlist data model + evaluator. Spike — not wired into the
// UI yet; see the module doc comment for the rollout plan. Allow
// dead_code at module level so the public surface (which exists for
// the future wizard) doesn't generate noise during the wire-in
// transition.
#[allow(dead_code)]
mod smart_playlist;
pub mod theme;
mod upnp;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender as StdSender, channel};

use audio::{DeckTelemetry, Engine, Sender};
use control::{DeckCommand, DeckId, MusicalKey, TrackAnalysis, TrackBuffer};
use eframe::egui;
use egui::{Color32, Pos2, Sense, Stroke, Vec2};
use persistence::{AnalysisCache, CachedAnalysis, Favourites};

pub(crate) use auto_mix::{
    AutoMixController, AutoMixShared, AutoMixState, ArmedState, DeckMeta, spawn_load_worker,
};

/// One-shot diagnostic for the elusive "grid moves but waveform
/// disappears" state. Logs the deck's hires/beat_grid/loaded_path
/// status once per deck per anomaly run so the log isn't spammed.
fn log_waveform_anomaly(deck: DeckId, d: &DeckUi) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static LOGGED_A: AtomicBool = AtomicBool::new(false);
    static LOGGED_B: AtomicBool = AtomicBool::new(false);
    let logged = match deck { DeckId::A => &LOGGED_A, DeckId::B => &LOGGED_B };
    if logged.swap(true, Ordering::Relaxed) { return; }
    eprintln!(
        "waveform-anomaly deck={:?} loaded_path={:?} hires_len={} samples_per_hires={} total_frames={} sample_rate={} beat_grid_len={} downbeats_len={}",
        deck,
        d.loaded_path.as_ref().and_then(|p| p.file_name()).and_then(|s| s.to_str()),
        d.hires.len(),
        d.samples_per_hires,
        d.total_frames,
        d.sample_rate,
        d.beat_grid.len(),
        d.downbeats.len(),
    );
}

fn snapshot_into(d: &DeckUi, m: &mut DeckMeta) {
    m.loaded_path = d.loaded_path.clone();
    m.bpm = d.bpm;
    m.beat_grid = d.beat_grid.clone();
    m.downbeats = d.downbeats.clone();
    m.total_frames = d.total_frames;
    m.sample_rate = d.sample_rate;
    m.key = d.key;
}

const SUPPORTED_EXTS: &[&str] = &["mp3", "flac", "wav", "m4a", "aac", "ogg"];
pub(crate) const OVERVIEW_BUCKETS: usize = 2000;
/// Hi-res peaks density (peaks per source-sample). Smaller = more memory but
/// finer resolution in the scrolling view. 64 ≈ 690 peaks/sec at 44.1k.
pub(crate) const HIRES_SAMPLES_PER_PEAK: usize = 64;
const PITCH_MIN: f32 = 0.92;
const PITCH_MAX: f32 = 1.08;
const ZOOM_BEATS: f64 = 16.0;
const ZOOM_PLAYHEAD_FRAC: f32 = 0.33;
// 3-stem overlay colours. Alpha ~50 % so overlapping columns blend
// rather than mask each other. Drums = warm red (impact / energy),
// Stem colours now come from `palette::Palette` (drums red, vocals
// green, instruments blue — matches the design handoff). Legacy
// STEM_COLOR_* constants removed; call sites use `palette::for_ui(ui).stem_*`.

/// Background worker that fills the analysis cache by decoding +
/// analysing each track that's not already in the cache. Spawned once at
/// app start; exits when the tracks list is exhausted. Off the UI thread.
fn spawn_analysis_worker(
    tracks: Vec<PathBuf>,
    cache: Arc<AnalysisCache>,
    progress: Arc<AtomicUsize>,
) {
    if tracks.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        // Nice ourselves down. ort+CUDA inference + symphonia decode
        // are both CPU-hungry on top of GPU work; without this they
        // happily compete with the audio callback thread and trigger
        // snd_pcm_recover underruns. +10 is enough headroom on a
        // multi-core laptop without making the library scan feel
        // glacial.
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, 0, 10);
        }
        for path in tracks {
            if cache.is_current(&path) {
                continue;
            }
            let display_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?");
            let buffer = match decode::load_to_buffer(&path) {
                Ok(b) => b,
                Err(e) => {
                    // Loud — this is the path you came looking for when
                    // the "library: N tracks · N-1 analysed" counter
                    // sits one short. Without the log there's no way
                    // to know which file the decoder choked on.
                    eprintln!(
                        "analyse: decode failed for {} ({e}) — skipping",
                        path.display(),
                    );
                    progress.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };
            eprintln!("analysing: {display_name}");
            // analyse() is pure DSP but built from third-party FFT /
            // matrix code that can panic on pathological inputs
            // (NaN-bearing buffers, zero-length channels, …). Without
            // catch_unwind, a single bad track kills the whole
            // background worker — every subsequent track silently
            // never gets analysed, which is exactly the off-by-N
            // counter symptom. Catching here keeps the rest going
            // and prints the offender's path.
            let buffer_for_analyse = Arc::clone(&buffer);
            let duration_secs = buffer.duration_secs();
            let outcome = std::panic::catch_unwind(
                std::panic::AssertUnwindSafe(move || analysis::analyse(&buffer_for_analyse)),
            );
            match outcome {
                Ok(r) => {
                    cache.insert(
                        path.clone(),
                        CachedAnalysis {
                            bpm: r.bpm,
                            key: r.key,
                            beats: r.beat_grid,
                            downbeats: r.downbeats,
                            version: r.analysis_version,
                            duration_secs: Some(duration_secs),
                        },
                    );
                }
                Err(_) => {
                    eprintln!(
                        "analyse: PANIC analysing {} — skipping, worker continues",
                        path.display(),
                    );
                }
            }
            progress.fetch_add(1, Ordering::Relaxed);
        }
    });
}

pub fn scan_music_dir(dir: &Path) -> Vec<TrackMeta> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if !p.is_file() {
                continue;
            }
            let ext = p
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase());
            let Some(ext) = ext else { continue };
            if !SUPPORTED_EXTS.iter().any(|e| *e == ext) {
                continue;
            }
            // Canonicalise so the in-memory path is identical regardless of
            // whether music_dir was passed in as relative ("music") or
            // absolute ("/home/.../music"). Without this, favourites and
            // cache lookups (which key on PathBuf) silently miss after a
            // change in CLI invocation.
            let p = p.canonicalize().unwrap_or(p);
            let filename = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let stem = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&filename);
            let (parsed_artist, parsed_title) = parse_track_name(stem);
            // Prefer the file's embedded tags (more accurate than the
            // filename heuristic); fall back to the filename parse when a
            // tag is missing.
            let tags = decode::read_tags(&p).unwrap_or_default();
            let title = tags.title.unwrap_or(parsed_title);
            let artist = tags.artist.unwrap_or(parsed_artist);
            let genre = tags.genre.unwrap_or_default();
            out.push(TrackMeta {
                path: p,
                filename,
                title,
                artist,
                genre,
            });
        }
    }
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    out
}

#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub path: PathBuf,
    /// Full filename (for substring filter).
    pub filename: String,
    /// Display title — prefers the file's embedded tag, falls back to a
    /// best-effort filename parse.
    pub title: String,
    /// Artist — same source preference as `title`.
    pub artist: String,
    /// Genre tag (empty when the file doesn't carry one).
    pub genre: String,
}

/// Best-effort split of a filename stem into (artist, title).
/// Strategy: strip any leading "01 ", "01. ", "01 - " track-number prefix,
/// then split on " - ". 2 parts → artist | title. 3+ parts → artist | rest
/// joined as title. No separator → empty artist, whole stem as title.
fn parse_track_name(stem: &str) -> (String, String) {
    let stripped = strip_leading_track_number(stem);
    let parts: Vec<&str> = stripped.splitn(2, " - ").collect();
    match parts.as_slice() {
        [only] => (String::new(), only.trim().to_string()),
        [artist, title] => (artist.trim().to_string(), title.trim().to_string()),
        _ => (String::new(), stripped.to_string()),
    }
}

fn strip_leading_track_number(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return s;
    }
    // Optional "." or "-" right after the digits.
    let mut j = i;
    if j < bytes.len() && (bytes[j] == b'.' || bytes[j] == b'-') {
        j += 1;
    }
    // Must be followed by whitespace to count as a track-number prefix.
    if j < bytes.len() && bytes[j].is_ascii_whitespace() {
        return s[j..].trim_start();
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Title,
    Artist,
    Genre,
    Key,
    Bpm,
    Length,
    Plays,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortState {
    column: SortColumn,
    ascending: bool,
}

pub(crate) fn compute_overview(buf: &TrackBuffer, num_buckets: usize) -> Vec<f32> {
    compute_overview_from(&buf.samples, buf.channels, num_buckets)
}

pub(crate) fn compute_overview_from(samples: &[f32], channels: u16, num_buckets: usize) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let total = samples.len() / ch;
    if total == 0 {
        return vec![0.0; num_buckets];
    }
    let bucket_size = (total + num_buckets - 1) / num_buckets;
    (0..num_buckets)
        .map(|i| {
            let start = i * bucket_size;
            let end = ((i + 1) * bucket_size).min(total);
            if start >= end {
                return 0.0;
            }
            let mut peak = 0.0f32;
            for f in start..end {
                let i0 = f * ch;
                for c in 0..ch {
                    let v = samples[i0 + c].abs();
                    if v > peak {
                        peak = v;
                    }
                }
            }
            peak
        })
        .collect()
}

pub(crate) fn compute_hires_peaks(buf: &TrackBuffer, samples_per_peak: usize) -> Vec<f32> {
    compute_hires_peaks_from(&buf.samples, buf.channels, samples_per_peak)
}

pub(crate) fn compute_hires_peaks_from(samples: &[f32], channels: u16, samples_per_peak: usize) -> Vec<f32> {
    let ch = channels.max(1) as usize;
    let total = samples.len() / ch;
    if total == 0 || samples_per_peak == 0 {
        return Vec::new();
    }
    let n = (total + samples_per_peak - 1) / samples_per_peak;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * samples_per_peak;
        let end = ((i + 1) * samples_per_peak).min(total);
        let mut peak = 0.0f32;
        for f in start..end {
            let i0 = f * ch;
            for c in 0..ch {
                let v = samples[i0 + c].abs();
                if v > peak {
                    peak = v;
                }
            }
        }
        out.push(peak);
    }
    out
}

pub(crate) enum LoadEvent {
    /// First message after decode + waveform compute. Carries the
    /// `path` so the deck can identify any subsequent `Refined`.
    /// Beats/downbeats may be empty when the track wasn't cached and
    /// the background analyser hasn't finished yet.
    Initial(LoadInitial),
    /// Second message, only sent for tracks that missed the cache.
    /// Carries the refined beat grid + downbeats once the model has
    /// completed. The UI drops it if `path` no longer matches the
    /// deck's current track.
    Refined(AnalysisRefined),
    /// Stem peak arrays for the 3-colour overlay. Sent by the stem
    /// worker after demucs finishes (~15 s post-load). Dropped if
    /// the deck has moved on to another track.
    Stems(StemPeaks),
    /// Decode failed for this path. Auto-mix uses this to clear its
    /// `pre_load_pending` latch and try another track on the next
    /// tick — otherwise a single broken file would silently freeze
    /// auto-mix until restart.
    Failed { deck: DeckId, path: PathBuf },
    /// Kick-trough alignment finished on a worker thread. Carries the
    /// shifted beat grid; the UI applies it and sends an
    /// `UpdateAnalysis`. Done off the UI thread because the alignment
    /// pass is O(track-length) DSP and was the main cause of the
    /// "Application Not Responding" dialog on Wayland.
    KickAligned {
        deck: DeckId,
        path: PathBuf,
        beat_grid: Vec<f64>,
        offset_secs: f64,
    },
}

pub(crate) struct LoadInitial {
    pub(crate) deck: DeckId,
    pub(crate) path: PathBuf,
    pub(crate) title: String,
    pub(crate) overview: Vec<f32>,
    pub(crate) hires: Vec<f32>,
    pub(crate) samples_per_hires: usize,
    pub(crate) total_frames: u64,
    pub(crate) sample_rate: u32,
    pub(crate) bpm: f32,
    pub(crate) beat_grid: Vec<f64>,
    pub(crate) downbeats: Vec<u32>,
    pub(crate) key: Option<MusicalKey>,
}

pub(crate) struct AnalysisRefined {
    pub(crate) deck: DeckId,
    pub(crate) path: PathBuf,
    pub(crate) bpm: f32,
    pub(crate) beat_grid: Vec<f64>,
    pub(crate) downbeats: Vec<u32>,
    pub(crate) key: Option<MusicalKey>,
}

pub(crate) struct StemPeaks {
    pub(crate) deck: DeckId,
    pub(crate) path: PathBuf,
    /// Stems audio. Routed through the UI's drain loop (gated on
    /// `path` matching the deck's current track) so the engine never
    /// gets stems for a track the user already moved on from.
    pub(crate) stems: Arc<control::TrackStems>,
    pub(crate) overview_drums: Vec<f32>,
    pub(crate) overview_vocals: Vec<f32>,
    pub(crate) overview_instr: Vec<f32>,
    pub(crate) hires_drums: Vec<f32>,
    pub(crate) hires_vocals: Vec<f32>,
    pub(crate) hires_instr: Vec<f32>,
}

struct DeckUi {
    title: Option<String>,
    /// Filesystem path of whatever's currently on the deck. Used to
    /// validate that a late-arriving `Refined` event is still
    /// relevant — if the user loaded a different track in the
    /// meantime, the refined grid is dropped on the floor.
    loaded_path: Option<PathBuf>,
    overview: Vec<f32>,
    hires: Vec<f32>,
    /// Per-stem peak arrays for the 3-colour overlay. Empty until the
    /// stem worker finishes. Indexed the same way as `overview`/`hires`
    /// so the renderers can stride over them in parallel.
    stem_overview_drums: Vec<f32>,
    stem_overview_vocals: Vec<f32>,
    stem_overview_instr: Vec<f32>,
    stem_hires_drums: Vec<f32>,
    stem_hires_vocals: Vec<f32>,
    stem_hires_instr: Vec<f32>,
    samples_per_hires: usize,
    total_frames: u64,
    sample_rate: u32,
    bpm: f32,
    beat_grid: Vec<f64>,
    /// Indices into `beat_grid` of bar-position-1 downbeats. Empty
    /// when the cache entry pre-dates v2 — the waveform renderer then
    /// falls back to `i % 4 == 0`.
    downbeats: Vec<u32>,
    key: Option<MusicalKey>,
    telemetry: DeckTelemetry,
    quantize: bool,
    cue_held: bool,
    loading: bool,
    /// Local UI mirror of the deck's FX state. Each knob drag sends
    /// a `SetFx*` command and updates the mirror immediately for
    /// snappy redraw; the engine is the source of truth, the UI
    /// just displays.
    fx_kind: control::FxKindId,
    fx_on: bool,
    fx_colour: f32,
    fx_time: f32,
    fx_mix: f32,
    fx_beats: f32,
    /// Set true by `hot_cue_row` whenever a hot cue is set or cleared
    /// from the UI. The parent `App::ui` checks this after
    /// `deck_controls` returns and calls `sync_hot_cues_to_meta` once
    /// per frame per deck — keeps the `&mut self` requirement of the
    /// meta sync out of the inner widget functions.
    hot_cue_meta_dirty: bool,
    /// UI-only mirror of the slot labels + colours from TrackMeta.
    /// Loaded on LoadEvent::Initial and edited via the context menu.
    /// The TrackMetaStore on `DjApp` is the source of truth — these
    /// fields exist so the renderers (button row, waveform ticks) can
    /// read them without a TrackMeta lookup per frame.
    hot_cue_labels: [Option<String>; 8],
    hot_cue_colours: [Option<u32>; 8],
}

impl DeckUi {
    fn new(telemetry: DeckTelemetry) -> Self {
        Self {
            title: None,
            loaded_path: None,
            overview: Vec::new(),
            hires: Vec::new(),
            stem_overview_drums: Vec::new(),
            stem_overview_vocals: Vec::new(),
            stem_overview_instr: Vec::new(),
            stem_hires_drums: Vec::new(),
            stem_hires_vocals: Vec::new(),
            stem_hires_instr: Vec::new(),
            samples_per_hires: HIRES_SAMPLES_PER_PEAK,
            total_frames: 0,
            sample_rate: 0,
            bpm: 0.0,
            beat_grid: Vec::new(),
            downbeats: Vec::new(),
            key: None,
            telemetry,
            quantize: true,
            cue_held: false,
            loading: false,
            fx_kind: control::FxKindId::Echo,
            fx_on: false,
            fx_colour: 0.45,
            fx_time: 0.5,
            fx_mix: 0.35,
            fx_beats: 0.5,
            hot_cue_meta_dirty: false,
            hot_cue_labels: Default::default(),
            hot_cue_colours: Default::default(),
        }
    }

    fn playhead_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.telemetry.playhead_frames() as f64 / self.sample_rate as f64
        }
    }
}

pub struct DjApp {
    #[allow(dead_code)]
    engine: Engine,
    sender: Sender,
    #[allow(dead_code)]
    music_dir: PathBuf,
    tracks: Vec<TrackMeta>,
    filter: String,
    /// Selected genre filter. `None` = no filter; `Some("Techno")` = only
    /// tracks whose genre tag exactly equals "Techno".
    genre_filter: Option<String>,
    deck_a: DeckUi,
    deck_b: DeckUi,
    load_rx: Receiver<LoadEvent>,
    load_tx: StdSender<LoadEvent>,
    midi_status: String,
    analysis_cache: Arc<AnalysisCache>,
    /// Session-only stem cache. None if construction failed (in which
    /// case stem separation is silently disabled for the session).
    stem_cache: Option<Arc<stems::SessionCache>>,
    favourites: Favourites,
    favourites_only: bool,
    harmonic_filter: Option<DeckId>,
    analysis_progress: Arc<AtomicUsize>,
    analysis_total: usize,
    sort: SortState,
    /// Headphone bus state — the global "🎧 vol" and "CUE↔MASTER" mix knobs
    /// in the top bar. Maintained locally; sent to the engine on change.
    cue_gain: f32,
    cue_mix: f32,
    /// Master-bus gain. Mirrors the engine value; UI-owned, sent via
    /// `SetMasterGain` on change.
    master_gain: f32,
    /// Auto-mix shared state, owned by the background controller thread
    /// AND read/written by the UI thread (button toggle, drain_loads
    /// meta sync, picker sync, user-touch cancel). See `auto_mix`
    /// module for the full state machine.
    auto_mix: Arc<Mutex<AutoMixShared>>,
    /// When true, the hardware controller's EQ pots (CC 7/8/10) re-route
    /// to stem gains instead of EQ shelves. Read by the MIDI handler in
    /// `src/midi.rs`; the UI toggles it via the "🎛 → stems" button in
    /// the top bar.
    stem_mode: Arc<std::sync::atomic::AtomicBool>,
    /// User-editable preferences (settings.toml). Mostly startup
    /// defaults; the live engine state is *not* mirrored here.
    settings: settings::Settings,
    /// Shared with the MIDI thread; flipped from the settings window
    /// to toggle MIDI stderr logging on/off without restarting.
    log_midi: Arc<std::sync::atomic::AtomicBool>,
    /// Effective values main.rs resolved for this run — shown as
    /// placeholders in the settings window so empty overrides
    /// communicate "using the default" rather than "no value".
    effective: settings::EffectiveDefaults,
    /// Settings window open/closed (egui::Window's `open` flag).
    settings_open: bool,
    /// Background SSDP discovery for UPnP MediaRenderers on the LAN.
    /// Populates the "Stream to room" dropdown in the settings window;
    /// the actual stream wiring lands in the next commit.
    upnp_discovery: upnp::DiscoveryHandle,
    /// Local HTTP server that serves the post-master mix as raw L16
    /// PCM to a UPnP renderer. Spawned lazily on first selection (or
    /// on app start if a renderer is already persisted). Stays alive
    /// for the rest of the session — toggling the "Stream to room"
    /// setting just flips its `enabled` gate.
    network_output: Option<network_output::NetworkOutput>,
    /// The UDN we last successfully told to Play, with the control
    /// URL we used. Set when we send SetAVTransportURI+Play, cleared
    /// after Stop. Per-frame logic in `sync_network_play_state`
    /// compares this against the user's pinned selection (plus its
    /// live discovery status) and fires Stop/Play SOAP calls on
    /// transitions only — never per frame.
    network_active: Option<(String, String)>,
    /// Cached filtered+sorted track index list, with a fingerprint of
    /// the inputs used to produce it. Re-using the cache on frames
    /// where nothing changed avoids the per-frame
    /// `Vec<.to_lowercase()>` sort cost, which gets expensive with
    /// thousands of tracks (the comparator allocates on every
    /// compare). Invalidated when the fingerprint changes.
    filter_sort_cache: Option<(FilterSortKey, Vec<usize>)>,
    /// Append-only log of "deck became audible" events. Persisted in
    /// `.history` next to the analysis cache; rendered as the History
    /// tab in the left panel.
    history: history::HistoryStore,
    /// User-authored per-track metadata — hot cues today, beat-grid
    /// overrides and saved loops later. Persisted in `.track-meta`
    /// next to the analysis cache.
    track_meta: persistence::TrackMetaStore,
    /// Playlist tree (mirrors `<music-dir>/.playlists/`). Read on the
    /// UI thread, modified by right-click menus in § 5. The store's
    /// `generation()` counter invalidates the filter/sort cache so a
    /// freshly-added track lands in the table immediately.
    playlists: persistence::PlaylistStore,
    /// The leaf playlist whose contents currently fill the track
    /// table. `None` = normal library filter via `library_source`.
    /// Set when the user clicks a playlist in the source rail;
    /// cleared by selecting any non-playlist source.
    active_playlist: Option<Vec<String>>,
    /// Modal in flight for playlist editing (new / rename / delete-
    /// confirm). `None` outside of a dialog. Rendered as a small
    /// `egui::Window` at the bottom of the frame.
    pending_playlist_dialog: Option<PlaylistDialog>,
    /// Per-deck: the loaded_path we already logged for the current
    /// LoadTrack. None means "nothing logged for whatever's loaded
    /// now"; a Some(path) acts as a sticky latch — we won't log
    /// again until LoadTrack replaces the deck's track (which we
    /// detect as `loaded_path != logged_path` below). This gives us
    /// hysteresis for free: dipping the fader below the audible
    /// threshold and back up doesn't re-log, because the latch
    /// is keyed on the load, not the audibility edge.
    deck_logged_path: [Option<PathBuf>; 2],
    /// Which tab the left panel shows: the track browser or the
    /// session history list.
    browser_tab: BrowserTab,
    /// Active filter source from the left source rail. Persisted
    /// for the session only — not saved across launches (Settings
    /// holds preferences, not transient view state).
    library_source: LibrarySource,
    /// Source rail collapsed state (icons only, narrower).
    source_rail_collapsed: bool,
    /// Drill-down state for the left source rail. `Root` shows the
    /// top-level items (All / Playlists / Genres / Favourites / …).
    /// `PlaylistsAt` shows the playlist tree at `path` (empty path =
    /// `.playlists/` root). `Genres` shows the unique-genre list.
    /// All non-Root views render a "← Back" affordance that pops one
    /// level (or returns to Root from the top-level drill-in).
    source_rail_view: SourceRailView,
    /// Which deck the Grid Adjust panel is targeting.
    grid_edit_deck: DeckId,
    /// Edits gated until the user unlocks. Resets to locked on app
    /// start so an accidental tab-click can't mangle a grid.
    grid_edit_unlocked: bool,
}

/// Modal dialog states for playlist editing. The store API is
/// synchronous + fallible so the dialog is the natural place to
/// collect a name / confirm a destructive action before calling
/// through. Cleared on accept, cancel, or any error (with a stderr
/// log).
#[derive(Debug, Clone)]
enum PlaylistDialog {
    /// Create a new empty playlist at the given folder path.
    NewPlaylist { at: Vec<String>, draft: String },
    /// Create a new folder at the given folder path.
    NewFolder { at: Vec<String>, draft: String },
    /// Rename the playlist or folder at `at` to `draft`.
    Rename { at: Vec<String>, draft: String },
    /// Confirm-delete prompt. `is_folder` controls the warning
    /// wording (recursive vs single-file).
    ConfirmDelete { at: Vec<String>, is_folder: bool },
}

/// Drill-down state of the left source rail. Most views are "Root +
/// a filter" (one level), but Playlists is genuinely tree-shaped so
/// the rail walks into it. Persisted in memory only — every launch
/// starts at `Root` so the user always sees the familiar top-level
/// chrome first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceRailView {
    Root,
    /// Folder path within `.playlists/`. Empty = root of the playlist
    /// tree (so clicking a playlist or descending into a folder
    /// updates this path).
    PlaylistsAt { path: Vec<String> },
    /// Flat list of unique genre strings derived from TrackMeta.
    Genres,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum BrowserTab {
    Tracks,
    History,
    GridEdit,
}

/// Left source rail selection. Drives the primary filter on the
/// track list (and, for History, swaps the browser to the History
/// tab content). Other variants are placeholders today — the design
/// shows them but Playlists / Similar aren't features yet.
#[derive(PartialEq, Eq, Clone, Copy)]
enum LibrarySource {
    AllTracks,
    Playlists,
    Genres,
    Favourites,
    Similar,
    History,
    GridEdit,
}

impl LibrarySource {
    fn label(self) -> &'static str {
        match self {
            Self::AllTracks => "All tracks",
            Self::Playlists => "Playlists",
            Self::Genres => "Genres",
            Self::Favourites => "Favourites",
            Self::Similar => "Similar",
            Self::History => "History",
            Self::GridEdit => "Grid Adjust",
        }
    }
    fn icon(self) -> &'static str {
        // Unicode placeholders for the 15 px line icons the design
        // calls for. Bundling proper SVG icons can come later;
        // these read clearly enough for v1 and don't pull in deps.
        match self {
            Self::AllTracks => "≡",
            Self::Playlists => "♫",
            Self::Genres => "🏷",
            Self::Favourites => "★",
            Self::Similar => "⇄",
            Self::History => "🕓",
            Self::GridEdit => "⌗",
        }
    }
}

#[derive(PartialEq, Eq, Clone)]
struct FilterSortKey {
    filter_lower: String,
    favs_only: bool,
    genre_filter: Option<String>,
    harmonic_target: Option<control::MusicalKey>,
    sort: SortState,
    tracks_len: usize,
    favourites_len: usize,
    analysis_gen: u64,
    history_gen: u64,
    /// When `Some`, the table renders that playlist's tracks in
    /// playlist order rather than the standard filtered library. The
    /// path identifies the leaf; the playlists_gen bump ensures the
    /// cache invalidates if the underlying file changes.
    active_playlist: Option<Vec<String>>,
    playlists_gen: u64,
}

impl DjApp {
    pub fn new(
        engine: Engine,
        music_dir: PathBuf,
        midi_status: String,
        stem_mode: Arc<std::sync::atomic::AtomicBool>,
        settings: settings::Settings,
        log_midi: Arc<std::sync::atomic::AtomicBool>,
        effective: settings::EffectiveDefaults,
    ) -> Self {
        let tracks = scan_music_dir(&music_dir);
        let (load_tx, load_rx) = channel();
        let deck_a = DeckUi::new(engine.telemetry(DeckId::A));
        let deck_b = DeckUi::new(engine.telemetry(DeckId::B));
        let sender = engine.sender();

        let analysis_cache = Arc::new(AnalysisCache::load(&music_dir));
        let stem_cache = match stems::SessionCache::new() {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                eprintln!("stems: session cache unavailable, stems disabled: {e:#}");
                None
            }
        };
        let favourites = Favourites::load(&music_dir);
        let history = history::HistoryStore::load(&music_dir);
        let track_meta = persistence::TrackMetaStore::load(&music_dir);
        let playlists = persistence::PlaylistStore::load(&music_dir);
        let analysis_progress = Arc::new(AtomicUsize::new(analysis_cache.count()));
        let analysis_total = tracks.len();

        // Background worker: decodes + analyses any tracks not in the cache,
        // appends each result to the cache file. Doesn't block the UI.
        let worker_paths: Vec<PathBuf> = tracks.iter().map(|t| t.path.clone()).collect();
        spawn_analysis_worker(
            worker_paths,
            Arc::clone(&analysis_cache),
            Arc::clone(&analysis_progress),
        );

        let auto_mix = Arc::new(Mutex::new(AutoMixShared::new()));
        // Spawn the auto-mix controller thread. It polls `auto_mix`
        // shared state at 20 Hz and drives blends entirely independent
        // of the UI thread, so it keeps working when the egui window
        // is on another Wayland workspace and not receiving frames.
        let telemetry_a = engine.telemetry(DeckId::A);
        let telemetry_b = engine.telemetry(DeckId::B);
        AutoMixController {
            shared: Arc::clone(&auto_mix),
            sender: sender.clone(),
            telemetry_a,
            telemetry_b,
            load_tx: load_tx.clone(),
            analysis_cache: Arc::clone(&analysis_cache),
            stem_cache: stem_cache.as_ref().map(Arc::clone),
        }
        .spawn();

        // Spawn the network-output HTTP server eagerly. URL is then
        // stable for the whole session and survives the user
        // toggling renderer selections (we just flip its `enabled`
        // gate). If the audio engine's network consumer was already
        // taken (defensive — shouldn't happen) or the listener bind
        // fails (rare; privileged-port-only systems, etc.), the
        // feature stays inert and the settings dropdown still works
        // for the discovery aspect — it just won't make sound.
        let network_output = engine.take_network_consumer().and_then(|consumer| {
            match network_output::NetworkOutput::spawn(
                consumer,
                engine.sample_rate(),
                engine.out_channels(),
            ) {
                Ok(no) => Some(no),
                Err(e) => {
                    eprintln!("network-output: spawn failed: {e}");
                    None
                }
            }
        });
        // Honour persisted "Stream to room" — if settings.toml has
        // a renderer pinned, flip the gate on right away so the
        // feature is live as soon as the engine starts pushing.
        // (§3 will additionally fire the SOAP Play command when it
        // sees the renderer come up in discovery.)
        if settings.network_renderer_udn.is_some() {
            if let Some(no) = network_output.as_ref() {
                no.enable();
            }
        }

        // Take the renderer URL cache out of settings before we move
        // `settings` into the struct literal below. The SSDP
        // discovery thread direct-probes each of these every sweep,
        // so any device we've ever discovered stays in the picker
        // even when it's SSDP-silent (typical Qute post-standby).
        // Includes the pinned descriptor URL as a hot path.
        let mut upnp_seeds: Vec<String> = settings.known_renderer_urls.clone();
        if let Some(pinned) = &settings.network_renderer_descriptor_url {
            if !upnp_seeds.contains(pinned) {
                upnp_seeds.push(pinned.clone());
            }
        }

        Self {
            engine,
            sender,
            music_dir,
            tracks,
            filter: String::new(),
            genre_filter: None,
            deck_a,
            deck_b,
            load_rx,
            load_tx,
            midi_status,
            analysis_cache,
            stem_cache,
            favourites,
            favourites_only: false,
            harmonic_filter: None,
            analysis_progress,
            analysis_total,
            sort: SortState {
                column: SortColumn::Title,
                ascending: true,
            },
            cue_gain: 0.15,
            cue_mix: 1.0,
            master_gain: 1.0,
            auto_mix,
            stem_mode,
            filter_sort_cache: None,
            history,
            track_meta,
            playlists,
            active_playlist: None,
            pending_playlist_dialog: None,
            deck_logged_path: [None, None],
            browser_tab: BrowserTab::Tracks,
            library_source: LibrarySource::AllTracks,
            grid_edit_deck: DeckId::A,
            grid_edit_unlocked: false,
            source_rail_collapsed: false,
            source_rail_view: SourceRailView::Root,
            settings,
            log_midi,
            effective,
            settings_open: false,
            upnp_discovery: upnp::DiscoveryHandle::spawn(upnp_seeds),
            network_output,
            network_active: None,
        }
    }

    /// Per-frame state reconciliation for network output. Fires SOAP
    /// commands only on a state change (pin/unpin, renderer comes
    /// online, IP changes) — never per frame on steady state.
    ///
    /// Target state: `Some(udn)` iff the user has pinned a renderer
    /// AND that renderer is currently visible in discovery AND we
    /// have an HTTP server URL we can hand it. Active state mirrors
    /// what we've actually told a renderer to Play.
    ///
    ///   target == active        → do nothing
    ///   target.is_some() & diff → Stop old (if any), Play new
    ///   target.is_none() & some → Stop old, clear active
    ///
    /// All SOAP calls fire-and-forget (their own threads); a slow
    /// renderer can't hang the UI. Failures self-heal next frame
    /// the moment any input changes (active stays untouched on
    /// failed Play, so we retry; stays untouched on failed Stop,
    /// so we just give up and the renderer's session times out
    /// naturally).
    fn sync_network_play_state(&mut self) {
        let Some(no) = self.network_output.as_ref() else { return };
        // MIME we serve over HTTP — DLNA-standard big-endian L16
        // PCM. Naim plays this natively (no transcoding), so the
        // renderer-side latency is just buffering, not codec decode.
        let mime = format!(
            "audio/L16;rate={};channels={}",
            self.engine.sample_rate(),
            self.engine.out_channels(),
        );
        let renderers = self.upnp_discovery.renderers();
        let target = self.settings.network_renderer_udn.as_ref()
            .and_then(|udn| renderers.iter().find(|r| &r.udn == udn))
            .and_then(|r| {
                let url = no.lan_url_for(r.address)?;
                Some((r.udn.clone(), r.av_transport_control.clone(), r.name.clone(), url))
            });

        match (&self.network_active, target) {
            (None, None) => { /* idle */ }
            (Some(_), None) => {
                // Unpinned, or pinned renderer went offline. Stop.
                if let Some((_, ctrl)) = self.network_active.take() {
                    upnp::stop(ctrl, "(deselected)".into());
                }
            }
            (None, Some((udn, ctrl, name, url))) => {
                upnp::play_url(ctrl.clone(), url, mime.clone(), name);
                self.network_active = Some((udn, ctrl));
            }
            (Some((cur_udn, _cur_ctrl)), Some((udn, ctrl, name, url))) if cur_udn != &udn => {
                // Different renderer pinned. Stop the old, play the
                // new. Take here so we don't double-borrow.
                if let Some((_, old_ctrl)) = self.network_active.take() {
                    upnp::stop(old_ctrl, "(switching)".into());
                }
                upnp::play_url(ctrl.clone(), url, mime.clone(), name);
                self.network_active = Some((udn, ctrl));
            }
            (Some((cur_udn, cur_ctrl)), Some((udn, ctrl, name, url))) if cur_udn == &udn && cur_ctrl != &ctrl => {
                // Same UDN but new control URL — IP changed under us
                // (DHCP). Re-issue Play at the new URL.
                upnp::play_url(ctrl.clone(), url, mime.clone(), name);
                self.network_active = Some((udn, ctrl));
            }
            (Some(_), Some(_)) => { /* already playing on the right target */ }
        }
    }
    fn deck_mut(&mut self, deck: DeckId) -> &mut DeckUi {
        match deck {
            DeckId::A => &mut self.deck_a,
            DeckId::B => &mut self.deck_b,
        }
    }

    fn start_load(&mut self, path: PathBuf, deck: DeckId) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.deck_mut(deck).loading = true;
        self.deck_mut(deck).title = Some(format!("loading: {name}"));
        spawn_load_worker(
            path,
            deck,
            self.sender.clone(),
            self.load_tx.clone(),
            Arc::clone(&self.analysis_cache),
            self.stem_cache.as_ref().map(Arc::clone),
        );
    }

    /// Left source rail — six filter sources stacked vertically,
    /// with a header overline + chevron toggle for collapse. Active
    /// selection paints in accent-blue tint; hover paints the
    /// `raised` surface. Picking "Favourites" or "History" wires
    /// the existing flags so the rest of the app doesn't need to
    /// know about the rail.
    fn render_source_rail(&mut self, ui: &mut egui::Ui) {
        let pal = palette::for_ui(ui);
        let collapsed = self.source_rail_collapsed;
        // Header: "SOURCE" overline + chevron toggle. Chevron flips
        // direction so the user knows which way the click expands.
        ui.horizontal(|ui| {
            if !collapsed {
                ui.colored_label(pal.faint, egui::RichText::new("SOURCE").small());
            }
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    let glyph = if collapsed { "›" } else { "‹" };
                    if ui.small_button(glyph).clicked() {
                        self.source_rail_collapsed = !self.source_rail_collapsed;
                    }
                },
            );
        });
        ui.separator();

        // Dispatch to the active drill-down view. Each branch is
        // responsible for its own back-button + header + list.
        // Cloning the view here keeps borrow rules quiet — the user
        // may transition it during a click handler.
        match self.source_rail_view.clone() {
            SourceRailView::Root => self.render_rail_root(ui, collapsed),
            SourceRailView::PlaylistsAt { path } => self.render_rail_playlists(ui, collapsed, &path),
            SourceRailView::Genres => self.render_rail_genres(ui, collapsed),
        }
    }

    /// Root-level rail: the 7 top-level items. Clicking Playlists /
    /// Genres drills into the corresponding view; the rest behave
    /// the same as before (toggle filter, swap browser tab).
    fn render_rail_root(&mut self, ui: &mut egui::Ui, collapsed: bool) {
        const SOURCES: [LibrarySource; 7] = [
            LibrarySource::AllTracks,
            LibrarySource::Playlists,
            LibrarySource::Genres,
            LibrarySource::Favourites,
            LibrarySource::Similar,
            LibrarySource::History,
            LibrarySource::GridEdit,
        ];
        for src in SOURCES {
            let selected = self.library_source == src;
            if source_rail_item(
                ui,
                src.icon(),
                src.label(),
                selected,
                collapsed,
            ).clicked() {
                self.library_source = src;
                // Any root-level source switch unpins the active
                // playlist (otherwise it'd still filter the track
                // table). Genres/Playlists drill-down handles its
                // own pin/unpin separately.
                if !matches!(src, LibrarySource::Playlists) {
                    self.active_playlist = None;
                }
                match src {
                    LibrarySource::AllTracks => {
                        self.favourites_only = false;
                        self.browser_tab = BrowserTab::Tracks;
                    }
                    LibrarySource::Favourites => {
                        self.favourites_only = true;
                        self.browser_tab = BrowserTab::Tracks;
                    }
                    LibrarySource::History => {
                        self.browser_tab = BrowserTab::History;
                    }
                    LibrarySource::GridEdit => {
                        self.browser_tab = BrowserTab::GridEdit;
                    }
                    LibrarySource::Playlists => {
                        // Drill in. Children (folders + leaf
                        // playlists) render in the next frame.
                        self.source_rail_view = SourceRailView::PlaylistsAt { path: Vec::new() };
                        self.browser_tab = BrowserTab::Tracks;
                        self.favourites_only = false;
                    }
                    LibrarySource::Genres => {
                        self.source_rail_view = SourceRailView::Genres;
                        self.browser_tab = BrowserTab::Tracks;
                        self.favourites_only = false;
                    }
                    LibrarySource::Similar => {
                        // Placeholder — feature not built yet.
                        self.favourites_only = false;
                        self.browser_tab = BrowserTab::Tracks;
                    }
                }
            }
        }
        // Self-heal a stale active_playlist (deleted via § 5, etc.).
        if let Some(p) = self.active_playlist.clone() {
            if self.playlists.playlist_tracks(&p).is_none() {
                self.active_playlist = None;
            }
        }
    }

    /// Playlists drill-down at `path`. Renders the children at that
    /// path: sub-folders (📁) drill in, leaf playlists (♫) become
    /// the active filter for the track table.
    fn render_rail_playlists(&mut self, ui: &mut egui::Ui, collapsed: bool, path: &[String]) {
        let pal = palette::for_ui(ui);
        // Back-out affordance. Empty path → Root; non-empty pops a
        // segment.
        if source_rail_item(ui, "‹", "Back", false, collapsed).clicked() {
            self.source_rail_view = if path.is_empty() {
                SourceRailView::Root
            } else {
                let mut parent = path.to_vec();
                parent.pop();
                SourceRailView::PlaylistsAt { path: parent }
            };
            return;
        }
        if !collapsed {
            let label = if path.is_empty() {
                "Playlists".to_string()
            } else {
                format!("Playlists / {}", path.last().unwrap())
            };
            ui.colored_label(pal.muted, egui::RichText::new(label).small().strong());
            ui.add_space(2.0);
        }

        // Snapshot the children at the current path. Cloning the
        // shape we need keeps borrow rules quiet — the iteration's
        // click handlers want `&mut self`.
        let children: Vec<(String, bool)> = match self.playlists.children_at(path) {
            Some(nodes) => nodes.iter()
                .map(|n| (n.name().to_string(), n.is_folder()))
                .collect(),
            None => {
                if !collapsed {
                    ui.colored_label(
                        pal.faint,
                        egui::RichText::new("(folder no longer exists)").small(),
                    );
                }
                return;
            }
        };

        if children.is_empty() {
            if !collapsed {
                let msg = if path.is_empty() {
                    "(no playlists yet — right-click to create)"
                } else {
                    "(empty folder)"
                };
                ui.colored_label(pal.faint, egui::RichText::new(msg).small());
            }
            return;
        }

        // Render folders + playlists. Each row supports right-click
        // for rename / delete; the action is deferred into
        // `pending_playlist_dialog` so the modal can render outside
        // this borrow.
        for (name, is_folder) in children {
            let icon = if is_folder { "📁" } else { "♫" };
            // Active highlight: a playlist is "selected" when it's
            // the active filter for the track table. Folders never
            // highlight (they're just navigators).
            let selected = if is_folder {
                false
            } else {
                let mut full = path.to_vec();
                full.push(name.clone());
                self.active_playlist.as_ref() == Some(&full)
            };
            let resp = source_rail_item(ui, icon, &name, selected, collapsed);

            // Right-click context menu — rename / delete. Built-in
            // CloseOnClick is fine since both actions open their own
            // modal dialog (which sets the menu's intent before any
            // disk I/O happens).
            let mut item_path = path.to_vec();
            item_path.push(name.clone());
            let item_label = name.clone();
            let item_is_folder = is_folder;
            resp.context_menu(|ui| {
                ui.set_min_width(140.0);
                if ui.button("Rename…").clicked() {
                    self.pending_playlist_dialog = Some(PlaylistDialog::Rename {
                        at: item_path.clone(),
                        draft: item_label.clone(),
                    });
                    ui.close();
                }
                if ui.button("Delete…").clicked() {
                    self.pending_playlist_dialog = Some(PlaylistDialog::ConfirmDelete {
                        at: item_path.clone(),
                        is_folder: item_is_folder,
                    });
                    ui.close();
                }
            });

            if resp.clicked() {
                if is_folder {
                    // Drill in.
                    let mut next = path.to_vec();
                    next.push(name);
                    self.source_rail_view = SourceRailView::PlaylistsAt { path: next };
                } else {
                    // Pin this playlist as the active filter. Stays
                    // on the same drill-down level so the user can
                    // swap between sibling playlists quickly.
                    let mut leaf = path.to_vec();
                    leaf.push(name);
                    self.active_playlist = Some(leaf);
                    self.browser_tab = BrowserTab::Tracks;
                    self.favourites_only = false;
                    self.genre_filter = None;
                }
            }
        }

        // "+" button at the bottom of the list — opens "New playlist
        // / New folder" sub-menu rooted at the current `path`. Right-
        // click anywhere on a row gives rename/delete; "+ New" is the
        // discoverable create affordance.
        ui.add_space(4.0);
        let plus_label = if collapsed { "+" } else { "+  New…" };
        let plus_resp = source_rail_item(ui, "+", plus_label, false, collapsed);
        plus_resp.clone().context_menu(|ui| {
            ui.set_min_width(160.0);
            if ui.button("New playlist…").clicked() {
                self.pending_playlist_dialog = Some(PlaylistDialog::NewPlaylist {
                    at: path.to_vec(),
                    draft: String::new(),
                });
                ui.close();
            }
            if ui.button("New folder…").clicked() {
                self.pending_playlist_dialog = Some(PlaylistDialog::NewFolder {
                    at: path.to_vec(),
                    draft: String::new(),
                });
                ui.close();
            }
        });
        // Left-click on the + also opens the New popup. (Without
        // this it'd only respond to right-click which is confusing.)
        // Triggering the same context menu programmatically requires
        // egui's Popup API — simpler: a left-click defaults to "new
        // playlist" since that's the 95% case. User can right-click
        // for the folder option.
        if plus_resp.clicked() {
            self.pending_playlist_dialog = Some(PlaylistDialog::NewPlaylist {
                at: path.to_vec(),
                draft: String::new(),
            });
        }
    }

    /// Modal window for playlist edits (new / rename / confirm-
    /// delete). Borrows `self.pending_playlist_dialog.take()` for
    /// the duration of the render so the closure can mutate the
    /// store; on cancel / completion the dialog is dropped. On
    /// error we log to stderr and clear the dialog (the user can
    /// see what went wrong + retry).
    fn render_playlist_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.pending_playlist_dialog.take() else { return };
        let mut new_state: Option<PlaylistDialog> = None;
        let mut close = false;

        match dialog {
            PlaylistDialog::NewPlaylist { at, mut draft } => {
                egui::Window::new("New playlist")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label("Name:");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut draft)
                                .hint_text("e.g. Warmup")
                                .desired_width(200.0),
                        );
                        resp.request_focus();
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                            let create = ui.button("Create").clicked() || enter;
                            if create && !draft.trim().is_empty() {
                                match self.playlists.create_playlist(&at, draft.trim()) {
                                    Ok(()) => { close = true; }
                                    Err(e) => {
                                        eprintln!("playlists: create failed: {e}");
                                        close = true;
                                    }
                                }
                            }
                        });
                    });
                if !close {
                    new_state = Some(PlaylistDialog::NewPlaylist { at, draft });
                }
            }
            PlaylistDialog::NewFolder { at, mut draft } => {
                egui::Window::new("New folder")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label("Name:");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut draft)
                                .hint_text("e.g. House")
                                .desired_width(200.0),
                        );
                        resp.request_focus();
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                            let create = ui.button("Create").clicked() || enter;
                            if create && !draft.trim().is_empty() {
                                match self.playlists.create_folder(&at, draft.trim()) {
                                    Ok(()) => { close = true; }
                                    Err(e) => {
                                        eprintln!("playlists: create folder failed: {e}");
                                        close = true;
                                    }
                                }
                            }
                        });
                    });
                if !close {
                    new_state = Some(PlaylistDialog::NewFolder { at, draft });
                }
            }
            PlaylistDialog::Rename { at, mut draft } => {
                let original = at.last().cloned().unwrap_or_default();
                egui::Window::new("Rename")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(format!("Rename “{original}” to:"));
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut draft)
                                .desired_width(200.0),
                        );
                        resp.request_focus();
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                            let go = ui.button("Rename").clicked() || enter;
                            if go && !draft.trim().is_empty() && draft.trim() != original {
                                match self.playlists.rename(&at, draft.trim()) {
                                    Ok(()) => {
                                        // If the active playlist
                                        // was the renamed leaf,
                                        // update its path so the
                                        // table doesn't go blank.
                                        if self.active_playlist.as_ref() == Some(&at) {
                                            let mut new_path = at.clone();
                                            *new_path.last_mut().unwrap() = draft.trim().to_string();
                                            self.active_playlist = Some(new_path);
                                        }
                                        close = true;
                                    }
                                    Err(e) => {
                                        eprintln!("playlists: rename failed: {e}");
                                        close = true;
                                    }
                                }
                            }
                        });
                    });
                if !close {
                    new_state = Some(PlaylistDialog::Rename { at, draft });
                }
            }
            PlaylistDialog::ConfirmDelete { at, is_folder } => {
                let label = at.last().cloned().unwrap_or_default();
                egui::Window::new(if is_folder { "Delete folder?" } else { "Delete playlist?" })
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        if is_folder {
                            ui.label(format!("Delete folder “{label}” and everything inside it?"));
                            ui.colored_label(
                                palette::for_ui(ui).accent_red,
                                "This can't be undone.",
                            );
                        } else {
                            ui.label(format!("Delete playlist “{label}”?"));
                        }
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                            if ui.button("Delete").clicked() {
                                match self.playlists.delete(&at) {
                                    Ok(()) => {
                                        // Active playlist sat under
                                        // the deleted path → unpin.
                                        if let Some(p) = &self.active_playlist {
                                            if p.starts_with(&at[..]) || p == &at {
                                                self.active_playlist = None;
                                            }
                                        }
                                        // Drill-down sat under the
                                        // deleted folder → pop up.
                                        if let SourceRailView::PlaylistsAt { path } = &self.source_rail_view {
                                            if path.starts_with(&at[..]) {
                                                let mut p = at.clone();
                                                p.pop();
                                                self.source_rail_view = SourceRailView::PlaylistsAt { path: p };
                                            }
                                        }
                                        close = true;
                                    }
                                    Err(e) => {
                                        eprintln!("playlists: delete failed: {e}");
                                        close = true;
                                    }
                                }
                            }
                        });
                    });
                if !close {
                    new_state = Some(PlaylistDialog::ConfirmDelete { at, is_folder });
                }
            }
        }

        self.pending_playlist_dialog = new_state;
    }

    /// Genres drill-down. Derives a sorted unique list of non-empty
    /// genre strings from `TrackMeta` on the fly (cheap — only ~10
    /// distinct genres in a typical library; sort + dedup is
    /// dominated by .to_lowercase()). Click sets `genre_filter`,
    /// which the track-table cache already understands.
    fn render_rail_genres(&mut self, ui: &mut egui::Ui, collapsed: bool) {
        let pal = palette::for_ui(ui);
        if source_rail_item(ui, "‹", "Back", false, collapsed).clicked() {
            self.source_rail_view = SourceRailView::Root;
            return;
        }
        if !collapsed {
            ui.colored_label(pal.muted, egui::RichText::new("Genres").small().strong());
            ui.add_space(2.0);
        }

        // "All" pseudo-row at the top — lets the user clear the
        // filter without leaving the drill-down. Selected when no
        // genre is currently active.
        let all_selected = self.genre_filter.is_none();
        if source_rail_item(ui, "≡", "All", all_selected, collapsed).clicked() {
            self.genre_filter = None;
        }

        // Distinct genres from the live track list. Cheap to recompute
        // every frame at the library sizes we care about (~few hundred
        // tracks → ~few dozen non-empty genres after dedup).
        let mut genres: Vec<&str> = self
            .tracks
            .iter()
            .map(|m| m.genre.trim())
            .filter(|g| !g.is_empty())
            .collect();
        genres.sort_unstable_by_key(|g| g.to_lowercase());
        genres.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

        if genres.is_empty() {
            if !collapsed {
                ui.colored_label(
                    pal.faint,
                    egui::RichText::new("(no genres in library yet)").small(),
                );
            }
            return;
        }
        for g in genres {
            let selected = self.genre_filter.as_deref().map(|s| s.eq_ignore_ascii_case(g)).unwrap_or(false);
            if source_rail_item(ui, "🏷", g, selected, collapsed).clicked() {
                self.genre_filter = Some(g.to_string());
            }
        }
    }

    /// Shared mix bar — beat align / auto-mix / EQ-Stems view /
    /// CUE↔MASTER / 🎧 vol / 🔊 master. Renders inline (called from
    /// the CentralPanel after the decks) so it spans only the
    /// mixer area, not the library / source rail.
    fn render_shared_mix_bar(&mut self, ui: &mut egui::Ui) {
        let pal = palette::for_ui(ui);
        ui.add_space(4.0);
        let frame = egui::Frame::new()
            .fill(pal.panel)
            .stroke(egui::Stroke::new(1.0, pal.line))
            .corner_radius(14.0)
            .inner_margin(egui::Margin {
                left: 16, right: 16, top: 10, bottom: 10,
            });
        frame.show(ui, |ui| ui.horizontal(|ui| {
            let pal = palette::for_ui(ui);

            // Beat align — green pill toggle (matches deck mode pills).
            let beat_align = self.deck_a.telemetry.is_beat_aligned();
            if pill_toggle_dot(ui, "Beat align", beat_align, pal.accent_green).clicked() {
                let next = !beat_align;
                let _ = self.sender.send(DeckCommand::SetBeatAlign { deck: DeckId::A, on: next });
                let _ = self.sender.send(DeckCommand::SetBeatAlign { deck: DeckId::B, on: next });
            }

            // Auto-mix — blue pill toggle. "On" covers both Armed
            // (waiting for the cue point) and Active (mid-blend); the
            // ↻ glyph stays subtle so we keep just the label here.
            let auto_on = !matches!(
                self.auto_mix.lock().unwrap().state,
                AutoMixState::Off,
            );
            if pill_toggle_dot(ui, "Auto-mix", auto_on, pal.accent_blue).clicked() {
                self.toggle_auto_mix();
            }

            ui.add_space(12.0);

            // VIEW: EQ / Stems segmented control. The active segment
            // fills with `pal.ink` (the deep panel tint) and the
            // inactive one stays on chip — matches the design.
            ui.colored_label(pal.faint, egui::RichText::new("VIEW").small());
            let mut stem_on = self.stem_mode.load(std::sync::atomic::Ordering::Relaxed);
            if let Some(next) = segmented_toggle(ui, "EQ", "Stems", stem_on) {
                if next != stem_on {
                    stem_on = next;
                    self.stem_mode.store(stem_on, std::sync::atomic::Ordering::Relaxed);
                }
            }

            ui.add_space(12.0);

            // CUE ↔ MASTER cue-mix slider with centre detent. Labels
            // colour-keyed: pink for CUE, blue for MASTER.
            ui.colored_label(pal.accent_pink, egui::RichText::new("CUE").small().strong());
            let r = h_fader(
                ui, &mut self.cue_mix, 0.0..=1.0, 140.0,
                HFaderOpts { center_detent: true, accent_fill: None },
            );
            if r.changed() {
                let _ = self.sender.send(DeckCommand::SetCueMix { mix: self.cue_mix });
            }
            ui.colored_label(pal.accent_blue, egui::RichText::new("MASTER").small().strong());

            ui.add_space(8.0);

            // Cue headphone gain.
            ui.label(egui::RichText::new("🎧").small());
            let r = h_fader(
                ui, &mut self.cue_gain, 0.0..=1.5, 110.0,
                HFaderOpts { center_detent: false, accent_fill: Some(pal.accent_blue) },
            );
            if r.changed() {
                let _ = self.sender.send(DeckCommand::SetCueGain { gain: self.cue_gain });
            }
            ui.colored_label(pal.muted, mono(format!("{:.2}", self.cue_gain)));

            ui.add_space(8.0);

            // Master out.
            ui.label(egui::RichText::new("🔊").small());
            let r = h_fader(
                ui, &mut self.master_gain, 0.0..=1.5, 140.0,
                HFaderOpts { center_detent: false, accent_fill: Some(pal.accent_blue) },
            );
            if r.changed() {
                let _ = self.sender.send(DeckCommand::SetMasterGain { gain: self.master_gain });
            }
            ui.colored_label(pal.muted, mono(format!("{:.3}", self.master_gain)));
        }));
    }

    fn render_history(&mut self, ui: &mut egui::Ui) {
        // Two-hour gap = session boundary (matches FEATURES.md §11).
        const SESSION_GAP_SECS: u64 = 2 * 3600;
        let sessions = self.history.sessions(SESSION_GAP_SECS);
        if sessions.is_empty() {
            ui.label("No history yet — play a track to start the log.");
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Resolve a path → (title, artist) by walking self.tracks once.
        // Library has hundreds-of-tracks scale; this is cheap. Build a
        // quick lookup map so each session row is O(1).
        let lookup: std::collections::HashMap<&Path, (&str, &str)> = self
            .tracks
            .iter()
            .map(|t| (t.path.as_path(), (t.title.as_str(), t.artist.as_str())))
            .collect();

        let mut export: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (idx, session) in sessions.iter().enumerate() {
                let first = session.first().unwrap();
                let last = session.last().unwrap();
                let header_label = if session.len() > 1 {
                    format!(
                        "Session — {} → {} · {} tracks",
                        fmt_rel(first.timestamp, now),
                        fmt_rel(last.timestamp, now),
                        session.len(),
                    )
                } else {
                    format!("Session — {} · 1 track", fmt_rel(first.timestamp, now))
                };
                egui::CollapsingHeader::new(header_label)
                    // Newest session expanded by default; older ones folded.
                    .default_open(idx == 0)
                    .id_salt(("history-session", first.timestamp))
                    .show(ui, |ui| {
                        if ui.small_button("📋 Copy as setlist").clicked() {
                            export = Some(format_setlist(session, &lookup));
                        }
                        for entry in session.iter() {
                            let (title, artist) = lookup
                                .get(entry.path.as_path())
                                .copied()
                                .unwrap_or_else(|| {
                                    (
                                        entry
                                            .path
                                            .file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("?"),
                                        "",
                                    )
                                });
                            let deck = match entry.deck { DeckId::A => 'A', DeckId::B => 'B' };
                            ui.horizontal(|ui| {
                                ui.monospace(fmt_rel(entry.timestamp, now));
                                ui.label(format!("[{deck}]"));
                                if artist.is_empty() {
                                    ui.label(title);
                                } else {
                                    ui.label(format!("{artist} — {title}"));
                                }
                            });
                        }
                    });
            }
        });
        if let Some(text) = export {
            ui.ctx().copy_text(text);
        }
    }

    /// Manual beat-grid editor (FEATURES.md §2). Replaces the track
    /// table when the user selects "Grid Adjust" in the source rail.
    /// All ops live in `grid_edit::*` as pure functions; this method
    /// is purely layout + dispatching the action that gets applied
    /// via `apply_grid_op`. Lock state defaults to *locked* every
    /// session so a stray tab-click can't change a grid.
    fn render_grid_adjust(&mut self, ui: &mut egui::Ui) {
        let pal = palette::for_ui(ui);
        ui.heading("Grid Adjust");
        ui.label(
            egui::RichText::new(
                "Fine-tune the beat grid when the analyser is off. Edits save automatically to .track-meta and load again next time."
            )
            .small()
            .color(pal.muted),
        );
        ui.add_space(8.0);

        // ---- Deck selector ----------------------------------------
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Editing:").small());
            let mut deck = self.grid_edit_deck;
            if pill_toggle(ui, "Deck A", deck == DeckId::A, pal.accent_blue).clicked() {
                deck = DeckId::A;
            }
            if pill_toggle(ui, "Deck B", deck == DeckId::B, pal.accent_pink).clicked() {
                deck = DeckId::B;
            }
            self.grid_edit_deck = deck;
        });
        ui.add_space(6.0);

        let deck = self.grid_edit_deck;
        let d = match deck { DeckId::A => &self.deck_a, DeckId::B => &self.deck_b };
        if d.loaded_path.is_none() {
            ui.colored_label(
                pal.faint,
                "No track loaded on this deck. Load a track to enable editing.",
            );
            return;
        }
        // Snapshot the readout values; mut self borrows pop in
        // apply_grid_op, so we can't keep `d` across calls.
        let title = d.title.clone().unwrap_or_default();
        let cur_bpm = d.bpm;
        let n_beats = d.beat_grid.len();
        let has_override = d.loaded_path.as_ref()
            .and_then(|p| self.track_meta.get(p))
            .map(|m| m.grid_override.is_some())
            .unwrap_or(false);

        // ---- Track readout ----------------------------------------
        ui.label(egui::RichText::new(format!(
            "🎵  {title}"
        )).strong());
        ui.colored_label(
            pal.muted,
            format!(
                "BPM {:.2}  ·  {} beats{}",
                cur_bpm,
                n_beats,
                if has_override { "  ·  manual override" } else { "" },
            ),
        );
        ui.add_space(8.0);

        // ---- Lock toggle ------------------------------------------
        let lock_label = if self.grid_edit_unlocked {
            "🔓  Editing unlocked"
        } else {
            "🔒  Locked — click to enable edits"
        };
        let lock_accent = if self.grid_edit_unlocked { pal.accent_amber } else { pal.muted };
        if pill_toggle_dot(ui, lock_label, self.grid_edit_unlocked, lock_accent).clicked() {
            self.grid_edit_unlocked = !self.grid_edit_unlocked;
        }
        ui.add_space(10.0);

        let enabled = self.grid_edit_unlocked;

        // ---- Nudge --------------------------------------------------
        ui.label(egui::RichText::new("Nudge grid (fine)").small().strong());
        ui.horizontal(|ui| {
            if grid_btn(ui, "« 10 ms", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::Shift(-0.010));
            }
            if grid_btn(ui, "« 1 ms", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::Shift(-0.001));
            }
            if grid_btn(ui, "1 ms »", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::Shift(0.001));
            }
            if grid_btn(ui, "10 ms »", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::Shift(0.010));
            }
        });
        ui.add_space(8.0);

        // ---- Skip beats --------------------------------------------
        ui.label(egui::RichText::new("Skip whole beats").small().strong());
        ui.horizontal(|ui| {
            for n in [-8, -4, -2, -1, 1, 2, 4, 8] {
                let label = if n < 0 { format!("« {}", -n) } else { format!("{} »", n) };
                if grid_btn(ui, &label, enabled).clicked() {
                    self.apply_grid_op(deck, GridOp::Skip(n));
                }
            }
        });
        ui.add_space(8.0);

        // ---- BPM + downbeat ----------------------------------------
        ui.label(egui::RichText::new("Tempo / downbeat").small().strong());
        ui.horizontal(|ui| {
            if grid_btn(ui, "½× BPM", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::HalveBpm);
            }
            if grid_btn(ui, "2× BPM", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::DoubleBpm);
            }
            if grid_btn(ui, "Set downbeat at ▷", enabled).clicked() {
                self.apply_grid_op(deck, GridOp::DownbeatAtPlayhead);
            }
        });
        ui.add_space(10.0);

        // ---- Reset --------------------------------------------------
        ui.separator();
        ui.add_space(6.0);
        let reset_enabled = enabled && has_override;
        if grid_btn(ui, "↶  Reset to analysis", reset_enabled).clicked() {
            self.apply_grid_op(deck, GridOp::ResetOverride);
        }
        ui.colored_label(
            pal.faint,
            egui::RichText::new(
                "Reset removes the manual override and re-runs the analysis on the next load."
            )
            .small(),
        );
    }

    /// Apply a grid op to the chosen deck. Computes the new analysis
    /// via `grid_edit::*`, sends `UpdateAnalysis` to the engine, mirrors
    /// the result into `DeckUi` so the waveform redraws this frame,
    /// and writes the override into `.track-meta`.
    fn apply_grid_op(&mut self, deck: DeckId, op: GridOp) {
        let (d, accent_path) = match deck {
            DeckId::A => (&self.deck_a, self.deck_a.loaded_path.clone()),
            DeckId::B => (&self.deck_b, self.deck_b.loaded_path.clone()),
        };
        let Some(path) = accent_path else { return };
        if d.sample_rate == 0 || d.beat_grid.is_empty() { return; }

        // Build a TrackAnalysis from the current DeckUi state — that's
        // the canonical mirror of whatever the engine sees today.
        let cur = control::TrackAnalysis {
            analysis_version: 2,
            bpm: d.bpm,
            beat_grid: d.beat_grid.clone(),
            downbeats: d.downbeats.clone(),
            duration_secs: d.total_frames as f64 / d.sample_rate.max(1) as f64,
            sample_rate: d.sample_rate,
            key: d.key,
        };

        // Reset short-circuits: drop the override and reload from
        // cache, no engine command needed (the file reload happens
        // on the next track load).
        if matches!(op, GridOp::ResetOverride) {
            self.track_meta.set_grid_override(&path, None);
            return;
        }

        let new_an = match op {
            GridOp::Shift(delta) => grid_edit::shifted(&cur, delta),
            GridOp::Skip(n) => grid_edit::skip_beats(&cur, n),
            GridOp::HalveBpm => grid_edit::bpm_halved(&cur),
            GridOp::DoubleBpm => grid_edit::bpm_doubled(&cur),
            GridOp::DownbeatAtPlayhead => {
                let t = d.playhead_secs();
                grid_edit::set_downbeat_at(&cur, t)
            }
            GridOp::ResetOverride => unreachable!(),
        };

        // Mirror into DeckUi so the waveform redraws with the new grid
        // *this* frame — the user sees immediate visual feedback.
        // Clone first because `TrackAnalysis` itself isn't `Clone` and
        // we're about to consume it into an Arc.
        let bpm = new_an.bpm;
        let beat_grid = new_an.beat_grid.clone();
        let downbeats = new_an.downbeats.clone();
        {
            let d_mut = self.deck_mut(deck);
            d_mut.bpm = bpm;
            d_mut.beat_grid = beat_grid.clone();
            d_mut.downbeats = downbeats.clone();
        }

        // Engine update — wrap in Arc, send via UpdateAnalysis.
        let arc = std::sync::Arc::new(new_an);
        let _ = self.sender.send(control::DeckCommand::UpdateAnalysis {
            deck,
            analysis: arc,
        });

        // Persist override.
        self.track_meta.set_grid_override(
            &path,
            Some(persistence::GridOverride { bpm, beat_grid, downbeats }),
        );
    }

    fn render_track_table(&mut self, ui: &mut egui::Ui) {
        use egui_extras::{Column, TableBuilder};

        // 1) Build filtered + sorted list of indices into self.tracks.
        //    Cached across frames: the inputs (filter text, sort,
        //    harmonic target, favourites count, analysis cache gen,
        //    etc.) form a fingerprint. While the fingerprint is
        //    unchanged the previous Vec<usize> is reused, sidestepping
        //    the per-frame .to_lowercase() allocations the sort
        //    comparator does on every compare.
        let target_key = match self.harmonic_filter {
            Some(DeckId::A) => self.deck_a.key,
            Some(DeckId::B) => self.deck_b.key,
            None => None,
        };
        let harmonic_target = if self.harmonic_filter.is_some() {
            target_key
        } else {
            None
        };
        let key = FilterSortKey {
            filter_lower: self.filter.to_lowercase(),
            favs_only: self.favourites_only,
            genre_filter: self.genre_filter.clone(),
            harmonic_target,
            sort: self.sort,
            tracks_len: self.tracks.len(),
            favourites_len: self.favourites.paths().len(),
            analysis_gen: self.analysis_cache.generation(),
            history_gen: self.history.generation(),
            active_playlist: self.active_playlist.clone(),
            playlists_gen: self.playlists.generation(),
        };
        if self.filter_sort_cache.as_ref().map(|(k, _)| k) != Some(&key) {
            let filter_lower = &key.filter_lower;
            let favs_only = key.favs_only;
            let genre_filter = key.genre_filter.as_deref();
            let harmonic_active = harmonic_target.is_some();

            // Playlist mode: iterate the playlist's tracks in order
            // and resolve each path against the library. Tracks that
            // aren't in the library (deleted / moved files; rare in
            // practice but possible) silently drop. Sort headers are
            // ignored in playlist mode — the user explicitly asked
            // for *this* order. If they want a sort, they can switch
            // back to All Tracks first.
            if let Some(pl_path) = &key.active_playlist {
                let pl_tracks = self.playlists.playlist_tracks(pl_path);
                let path_to_idx: std::collections::HashMap<&Path, usize> = self
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (m.path.as_path(), i))
                    .collect();
                let indices: Vec<usize> = pl_tracks
                    .map(|paths| {
                        paths.iter()
                            .filter_map(|p| path_to_idx.get(p.as_path()).copied())
                            .collect()
                    })
                    .unwrap_or_default();
                self.filter_sort_cache = Some((key, indices));
            } else {

            // Precompute a (title_lower, artist_lower, genre_lower)
            // tuple per track so the sort comparator stops allocating
            // on every compare — O(N log N × constant) drop in
            // per-frame work even before the cache lands.
            let lower: Vec<(String, String, String)> = self
                .tracks
                .iter()
                .map(|m| (m.title.to_lowercase(), m.artist.to_lowercase(), m.genre.to_lowercase()))
                .collect();

            let mut indices: Vec<usize> = self
                .tracks
                .iter()
                .enumerate()
                .filter(|(i, m)| {
                    if !filter_lower.is_empty() {
                        let (tl, al, _) = &lower[*i];
                        let fn_l = m.filename.to_lowercase();
                        let hit = tl.contains(filter_lower.as_str())
                            || al.contains(filter_lower.as_str())
                            || fn_l.contains(filter_lower.as_str());
                        if !hit {
                            return false;
                        }
                    }
                    if favs_only && !self.favourites.contains(&m.path) {
                        return false;
                    }
                    if let Some(g) = genre_filter {
                        if !m.genre.eq_ignore_ascii_case(g) {
                            return false;
                        }
                    }
                    if harmonic_active {
                        let t = harmonic_target.unwrap();
                        let Some(c) = self.analysis_cache.get(&m.path) else {
                            return false;
                        };
                        let Some(k) = c.key else {
                            return false;
                        };
                        if !persistence::camelot_compatible(t, k) {
                            return false;
                        }
                    }
                    true
                })
                .map(|(i, _)| i)
                .collect();

            let sort = key.sort;
            indices.sort_by(|&a, &b| {
                let (tla, ala, gla) = &lower[a];
                let (tlb, alb, glb) = &lower[b];
                let ord = match sort.column {
                    SortColumn::Title => tla.cmp(tlb),
                    SortColumn::Artist => ala.cmp(alb),
                    SortColumn::Genre => gla.cmp(glb),
                    SortColumn::Key => {
                        let ka = self.analysis_cache.get(&self.tracks[a].path).and_then(|c| c.key);
                        let kb = self.analysis_cache.get(&self.tracks[b].path).and_then(|c| c.key);
                        key_sort_value(ka).cmp(&key_sort_value(kb))
                    }
                    SortColumn::Bpm => {
                        let ba = self
                            .analysis_cache
                            .get(&self.tracks[a].path)
                            .map(|c| c.bpm)
                            .unwrap_or(0.0);
                        let bb = self
                            .analysis_cache
                            .get(&self.tracks[b].path)
                            .map(|c| c.bpm)
                            .unwrap_or(0.0);
                        bpm_sort_value(ba).cmp(&bpm_sort_value(bb))
                    }
                    SortColumn::Length => {
                        let la = track_length_secs(self.analysis_cache.get(&self.tracks[a].path).as_ref());
                        let lb = track_length_secs(self.analysis_cache.get(&self.tracks[b].path).as_ref());
                        length_sort_value(la).cmp(&length_sort_value(lb))
                    }
                    SortColumn::Plays => {
                        // Ascending sort puts unplayed tracks at the top (0
                        // is lowest); descending puts most-played at the top
                        // which is the more useful default click. The user
                        // can flip it.
                        let pa = self.history.counts().get(&self.tracks[a].path).copied().unwrap_or(0);
                        let pb = self.history.counts().get(&self.tracks[b].path).copied().unwrap_or(0);
                        pa.cmp(&pb)
                    }
                };
                if sort.ascending { ord } else { ord.reverse() }
            });
            self.filter_sort_cache = Some((key, indices));
            }
        }
        let filtered_sorted: &[usize] = self
            .filter_sort_cache
            .as_ref()
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[]);

        // 2) Render table. Closures borrow self.* immutably. Click outcomes
        //    are stashed in locals and applied after the table returns, so
        //    we don't have a mutable / immutable borrow conflict.
        let mut new_sort: Option<SortColumn> = None;
        let mut fav_toggle: Option<PathBuf> = None;
        let mut load_action: Option<(PathBuf, DeckId)> = None;
        // Deferred "add this track to this playlist" action — set by
        // the right-click submenu on a track row, applied after the
        // table closure returns so we don't borrow `self.playlists`
        // mutably from inside the per-row closure.
        let mut add_to_playlist: Option<(PathBuf, Vec<String>)> = None;
        // Snapshot the flat playlist list once per frame for the
        // submenu so the closures don't need to call back into
        // `self.playlists`. Cheap — small tree, dropped at the end
        // of the frame.
        let playlists_snapshot: Vec<Vec<String>> = self.playlists.all_playlists();

        let sort = self.sort;
        let tracks = &self.tracks;
        let cache = self.analysis_cache.as_ref();
        let favs = &self.favourites;
        let plays = self.history.counts();
        // Snapshot which deck currently has each path loaded so the
        // A/B load buttons can show the loaded state without a
        // mutable borrow into the closure.
        let loaded_a = self.deck_a.loaded_path.clone();
        let loaded_b = self.deck_b.loaded_path.clone();
        let pal = palette::for_ui(ui);

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            // egui_extras caps the scroll area at ~800px by default — we
            // want it to fill the side panel, so disable the cap.
            .max_scroll_height(f32::INFINITY)
            .column(Column::exact(26.0))                             // ★
            .column(Column::exact(26.0))                             // A
            .column(Column::exact(26.0))                             // B
            .column(Column::initial(220.0).resizable(true))          // title
            .column(Column::initial(140.0).resizable(true))          // artist
            .column(Column::initial(100.0).resizable(true))          // genre
            .column(Column::auto())                                  // key
            .column(Column::auto())                                  // bpm
            .column(Column::auto())                                  // length
            .column(Column::auto())                                  // plays
            .header(22.0, |mut header| {
                header.col(|_| {});
                header.col(|_| {});
                header.col(|_| {});
                header.col(|ui| {
                    if sort_header(ui, "Title", sort, SortColumn::Title) {
                        new_sort = Some(SortColumn::Title);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "Artist", sort, SortColumn::Artist) {
                        new_sort = Some(SortColumn::Artist);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "Genre", sort, SortColumn::Genre) {
                        new_sort = Some(SortColumn::Genre);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "Key", sort, SortColumn::Key) {
                        new_sort = Some(SortColumn::Key);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "BPM", sort, SortColumn::Bpm) {
                        new_sort = Some(SortColumn::Bpm);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "Length", sort, SortColumn::Length) {
                        new_sort = Some(SortColumn::Length);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "Plays", sort, SortColumn::Plays) {
                        new_sort = Some(SortColumn::Plays);
                    }
                });
            })
            .body(|body| {
                body.rows(22.0, filtered_sorted.len(), |mut row| {
                    let row_index = row.index();
                    let idx = filtered_sorted[row_index];
                    let meta = &tracks[idx];
                    let starred = favs.contains(&meta.path);
                    let cached = cache.get(&meta.path);
                    let key = cached.as_ref().and_then(|c| c.key);
                    let bpm = cached.as_ref().map(|c| c.bpm).unwrap_or(0.0);

                    row.col(|ui| {
                        let star = if starred { "★" } else { "☆" };
                        if ui.small_button(star).clicked() {
                            fav_toggle = Some(meta.path.clone());
                        }
                    });
                    let on_a = loaded_a.as_ref() == Some(&meta.path);
                    let on_b = loaded_b.as_ref() == Some(&meta.path);
                    row.col(|ui| {
                        if deck_load_button(ui, "A", on_a, pal.accent_blue, &pal).clicked() {
                            load_action = Some((meta.path.clone(), DeckId::A));
                        }
                    });
                    row.col(|ui| {
                        if deck_load_button(ui, "B", on_b, pal.accent_pink, &pal).clicked() {
                            load_action = Some((meta.path.clone(), DeckId::B));
                        }
                    });
                    row.col(|ui| {
                        // Title cell senses clicks so we can hang a
                        // right-click context menu off it — "Add to
                        // ▸ <playlists>". Left click does nothing
                        // (Title isn't a "load" target — A/B cells
                        // own that). Truncate as before.
                        let title_resp = ui.add(
                            egui::Label::new(&meta.title)
                                .truncate()
                                .sense(egui::Sense::click()),
                        );
                        title_resp.context_menu(|ui| {
                            ui.set_min_width(200.0);
                            if playlists_snapshot.is_empty() {
                                ui.weak("(no playlists yet — create one first)");
                                return;
                            }
                            ui.menu_button("Add to ▸", |ui| {
                                for p in &playlists_snapshot {
                                    let label = p.join(" / ");
                                    if ui.button(label).clicked() {
                                        add_to_playlist = Some((meta.path.clone(), p.clone()));
                                        ui.close();
                                    }
                                }
                            });
                        });
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&meta.artist).truncate());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(&meta.genre).truncate());
                    });
                    row.col(|ui| {
                        ui.label(match key {
                            Some(k) => k.label(),
                            None => "--".to_string(),
                        });
                    });
                    row.col(|ui| {
                        ui.label(mono(if bpm > 0.0 {
                            format!("{:.1}", bpm)
                        } else {
                            "--".to_string()
                        }));
                    });
                    row.col(|ui| {
                        let secs = track_length_secs(cached.as_ref());
                        ui.label(mono(match secs {
                            Some(s) if s > 0.0 => fmt_mmss(s),
                            _ => "--".to_string(),
                        }));
                    });
                    row.col(|ui| {
                        let n = plays.get(&meta.path).copied().unwrap_or(0);
                        // Always render the number (including 0) so the
                        // column is clearly visible even on a fresh
                        // library with no history yet.
                        ui.label(mono(n.to_string()));
                    });
                });
            });

        // 3) Apply deferred actions.
        if let Some(col) = new_sort {
            if self.sort.column == col {
                self.sort.ascending = !self.sort.ascending;
            } else {
                self.sort = SortState {
                    column: col,
                    ascending: true,
                };
            }
        }
        if let Some(p) = fav_toggle {
            self.favourites.toggle(&p);
        }
        if let Some((p, deck)) = load_action {
            self.start_load(p, deck);
        }
        if let Some((track_path, pl_path)) = add_to_playlist {
            if let Err(e) = self.playlists.add_track(&pl_path, &track_path) {
                eprintln!("playlists: add_track failed: {e}");
            }
        }
    }

    fn drain_loads(&mut self) {
        let mut meta_changed = false;
        while let Ok(event) = self.load_rx.try_recv() {
            match event {
                LoadEvent::Initial(res) => {
                    // Hot cue load: look up the .track-meta entry
                    // for this path BEFORE we move res.path into
                    // the deck, convert seconds → frames using the
                    // track's sample_rate, then push the slot array
                    // through to the engine via HotCueLoad. Done
                    // here (not in the load worker) so the engine
                    // gets the data right after LoadTrack lands.
                    let sample_rate = res.sample_rate;
                    let deck = res.deck;
                    let (hot_cues_secs, hot_cue_labels, hot_cue_colours) = self
                        .track_meta
                        .get(&res.path)
                        .map(|m| (
                            m.hot_cues,
                            m.hot_cue_labels.clone(),
                            m.hot_cue_colours,
                        ))
                        .unwrap_or_default();
                    let hot_cue_frames: [Option<u64>; 8] =
                        std::array::from_fn(|i| {
                            hot_cues_secs[i].map(|s| {
                                (s * sample_rate as f64).round() as u64
                            })
                        });
                    let _ = self.sender.send(
                        DeckCommand::HotCueLoad { deck, slots: hot_cue_frames },
                    );

                    let d = self.deck_mut(res.deck);
                    d.title = Some(res.title);
                    d.loaded_path = Some(res.path);
                    d.overview = res.overview;
                    d.hires = res.hires;
                    // Drop any stem peaks from the previous track —
                    // the new stems will arrive when the worker
                    // finishes. Renderer falls back to the single
                    // overview/hires arrays until then.
                    d.stem_overview_drums.clear();
                    d.stem_overview_vocals.clear();
                    d.stem_overview_instr.clear();
                    d.stem_hires_drums.clear();
                    d.stem_hires_vocals.clear();
                    d.stem_hires_instr.clear();
                    d.samples_per_hires = res.samples_per_hires;
                    d.total_frames = res.total_frames;
                    d.sample_rate = res.sample_rate;
                    d.bpm = res.bpm;
                    d.beat_grid = res.beat_grid;
                    d.downbeats = res.downbeats;
                    d.key = res.key;
                    d.loading = false;
                    d.hot_cue_labels = hot_cue_labels;
                    d.hot_cue_colours = hot_cue_colours;
                    meta_changed = true;

                    // Manual beat-grid override (FEATURES.md §2). If
                    // `.track-meta` carries an override for this path,
                    // replace whatever the cache/empty grid set on
                    // both the UI mirror and the engine. Subsequent
                    // Refined / KickAligned events for this load are
                    // dropped — the manual grid is the source of
                    // truth.
                    let override_path = self.deck_mut(deck).loaded_path.clone();
                    let override_data = override_path
                        .as_ref()
                        .and_then(|p| self.track_meta.get(p))
                        .and_then(|m| m.grid_override.clone());
                    if let Some(g) = override_data {
                        let d = self.deck_mut(deck);
                        d.bpm = g.bpm;
                        d.beat_grid = g.beat_grid.clone();
                        d.downbeats = g.downbeats.clone();
                        let analysis = Arc::new(TrackAnalysis {
                            analysis_version: 2,
                            bpm: g.bpm,
                            beat_grid: g.beat_grid,
                            downbeats: g.downbeats,
                            duration_secs: d.total_frames as f64
                                / d.sample_rate.max(1) as f64,
                            sample_rate: d.sample_rate,
                            key: d.key,
                        });
                        let _ = self.sender.send(DeckCommand::UpdateAnalysis {
                            deck,
                            analysis,
                        });
                    }
                }
                LoadEvent::Refined(r) => {
                    // Drop the refined result if the user has already
                    // loaded a different track onto this deck, OR if a
                    // manual grid override is in force for this path.
                    let has_override = self.track_meta
                        .get(&r.path)
                        .map(|m| m.grid_override.is_some())
                        .unwrap_or(false);
                    if has_override {
                        continue;
                    }
                    let d = self.deck_mut(r.deck);
                    if d.loaded_path.as_deref() != Some(r.path.as_path()) {
                        continue;
                    }
                    d.bpm = r.bpm;
                    d.beat_grid = r.beat_grid;
                    d.downbeats = r.downbeats;
                    d.key = r.key;
                    meta_changed = true;
                }
                LoadEvent::Stems(s) => {
                    let d = self.deck_mut(s.deck);
                    if d.loaded_path.as_deref() != Some(s.path.as_path()) {
                        eprintln!(
                            "stems: drop (deck moved on) — got {:?}, deck has {:?}",
                            s.path.file_name(),
                            d.loaded_path.as_ref().and_then(|p| p.file_name()),
                        );
                        continue;
                    }
                    eprintln!(
                        "stems: applied — {} frames @ {} Hz, {} ch",
                        s.stems.frames(),
                        s.stems.sample_rate,
                        s.stems.channels,
                    );
                    d.stem_overview_drums = s.overview_drums;
                    d.stem_overview_vocals = s.overview_vocals;
                    d.stem_overview_instr = s.overview_instr;
                    d.stem_hires_drums = s.hires_drums;
                    d.stem_hires_vocals = s.hires_vocals;
                    d.stem_hires_instr = s.hires_instr;
                    // Kick-trough alignment is O(track-length) DSP —
                    // running it inline would block the UI thread long
                    // enough for Wayland to flag the surface as
                    // unresponsive ("Application Not Responding"
                    // popup). Hand it to a worker; it'll come back as
                    // LoadEvent::KickAligned with the shifted grid.
                    if !d.beat_grid.is_empty() {
                        let stems = Arc::clone(&s.stems);
                        let beat_grid = d.beat_grid.clone();
                        let deck = s.deck;
                        let path = s.path.clone();
                        let tx = self.load_tx.clone();
                        std::thread::spawn(move || {
                            let (shifted, offset_secs) = analysis::align_grid_to_kick_trough(
                                &stems.drums,
                                stems.channels,
                                stems.sample_rate,
                                &beat_grid,
                            );
                            let _ = tx.send(LoadEvent::KickAligned {
                                deck,
                                path,
                                beat_grid: shifted,
                                offset_secs,
                            });
                        });
                    }
                    let _ = self.sender.send(DeckCommand::SetStems {
                        deck: s.deck,
                        stems: s.stems,
                    });
                }
                LoadEvent::KickAligned { deck, path, beat_grid, offset_secs } => {
                    // Manual override wins — drop the kick-trough
                    // phase shift if the user has hand-gridded.
                    let has_override = self.track_meta
                        .get(&path)
                        .map(|m| m.grid_override.is_some())
                        .unwrap_or(false);
                    if has_override {
                        continue;
                    }
                    let d = self.deck_mut(deck);
                    if d.loaded_path.as_deref() != Some(path.as_path()) {
                        // Deck moved on; result no longer relevant.
                        continue;
                    }
                    if offset_secs.abs() <= 1e-6 {
                        eprintln!(
                            "stems: kick-trough — grid unchanged (insufficient kicks detected)"
                        );
                        continue;
                    }
                    eprintln!(
                        "stems: kick-trough phase shift {:+.1} ms applied to {} beats",
                        offset_secs * 1000.0,
                        beat_grid.len()
                    );
                    d.beat_grid = beat_grid.clone();
                    let analysis = Arc::new(TrackAnalysis {
                        analysis_version: 2,
                        bpm: d.bpm,
                        beat_grid,
                        downbeats: d.downbeats.clone(),
                        duration_secs: d.total_frames as f64 / d.sample_rate.max(1) as f64,
                        sample_rate: d.sample_rate,
                        key: d.key,
                    });
                    let _ = self.sender.send(DeckCommand::UpdateAnalysis {
                        deck,
                        analysis,
                    });
                    meta_changed = true;
                }
                LoadEvent::Failed { deck, path } => {
                    let d = self.deck_mut(deck);
                    if d.loading {
                        d.loading = false;
                    }
                    eprintln!(
                        "load: decode failed for {} ({:?})",
                        path.display(),
                        deck,
                    );
                    // Clear the auto-mix pre-load latch if the failed
                    // path is the one auto-mix was waiting on, so the
                    // controller's next tick picks a different track.
                    let mut s = self.auto_mix.lock().unwrap();
                    if let AutoMixState::Armed(ref mut a) = s.state {
                        if a.pre_load_pending.as_deref() == Some(path.as_path()) {
                            a.pre_load_pending = None;
                        }
                    }
                }
            }
        }
        if meta_changed {
            self.sync_auto_mix_meta();
        }
    }

    /// Settings window. Edits land directly on `self.settings`;
    /// changes that affect the live engine (`log_midi`, the per-deck
    /// pitch/beat-align defaults) are reflected immediately, but
    /// device / music_dir / midi_port only take effect on next
    /// launch (annotated in the window itself). Save-on-close lets
    /// us avoid an explicit "Save" button — closing the window or
    /// quitting the app persists.
    fn render_settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = self.settings_open;
        let mut changed = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                egui::Grid::new("settings-grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        let hint_music = self.effective.music_dir.display().to_string();
                        let hint_audio = self.effective.audio_device.clone()
                            .unwrap_or_else(|| "(system default)".into());
                        let hint_cue = self.effective.cue_device.clone()
                            .unwrap_or_else(|| "(none — master only)".into());
                        let hint_midi = self.effective.midi_port.clone();

                        ui.label("Music dir");
                        let mut s = self.settings.music_dir
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        if ui.add(egui::TextEdit::singleline(&mut s).hint_text(&hint_music))
                            .changed()
                        {
                            self.settings.music_dir = if s.is_empty() {
                                None
                            } else {
                                Some(PathBuf::from(&s))
                            };
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("Audio device");
                        if device_combo(
                            ui,
                            "settings-audio-device",
                            &mut self.settings.audio_device,
                            &self.effective.audio_devices,
                            &hint_audio,
                            "(system default)",
                        ) {
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("Cue device");
                        if device_combo(
                            ui,
                            "settings-cue-device",
                            &mut self.settings.cue_device,
                            &self.effective.audio_devices,
                            &hint_cue,
                            "(none — master only)",
                        ) {
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("MIDI port");
                        if midi_combo(
                            ui,
                            "settings-midi-port",
                            &mut self.settings.midi_port,
                            &self.effective.midi_ports,
                            &hint_midi,
                        ) {
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("Log MIDI to stderr");
                        if ui.checkbox(&mut self.settings.log_midi, "").changed() {
                            // log_midi is live — flip the shared
                            // atomic so the MIDI thread sees the
                            // change before the user has to restart.
                            self.log_midi.store(
                                self.settings.log_midi,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            changed = true;
                        }
                        ui.end_row();

                        // Stream-to-room dropdown — populated from
                        // the live SSDP discovery. Persisted as the
                        // device UDN so renaming the speaker / its IP
                        // changing won't break the binding next launch.
                        // Selecting any renderer flips the HTTP
                        // server's gate on so the audio actually
                        // starts flowing. §3 will additionally send
                        // SetAVTransportURI + Play to the renderer
                        // so it pulls the stream.
                        ui.label("Stream to room");
                        let renderers = self.upnp_discovery.renderers();
                        // Cache every discovered URL into settings —
                        // the discovery loop direct-probes the full
                        // set every sweep, so a device only needs to
                        // be SSDP-visible *once* (on any past launch)
                        // to stay findable forever, even when its
                        // advertiser later sleeps.
                        let mut cache_dirty = false;
                        for r in &renderers {
                            if !self.settings.known_renderer_urls.contains(&r.descriptor_url) {
                                self.settings.known_renderer_urls.push(r.descriptor_url.clone());
                                cache_dirty = true;
                            }
                        }
                        if cache_dirty {
                            self.upnp_discovery.set_seed_urls(
                                self.settings.known_renderer_urls.clone(),
                            );
                            changed = true;
                        }

                        if renderer_combo(
                            ui,
                            "settings-renderer",
                            &mut self.settings.network_renderer_udn,
                            &renderers,
                        ) {
                            changed = true;
                            // Save the descriptor URL alongside the
                            // UDN so the discovery thread can direct-
                            // probe the renderer next session — even
                            // if its SSDP advertiser is silent.
                            self.settings.network_renderer_descriptor_url =
                                self.settings.network_renderer_udn.as_ref()
                                    .and_then(|udn| renderers.iter().find(|r| &r.udn == udn))
                                    .map(|r| r.descriptor_url.clone());
                            if let Some(no) = self.network_output.as_ref() {
                                if self.settings.network_renderer_udn.is_some() {
                                    no.enable();
                                } else {
                                    no.disable();
                                }
                            }
                        }
                        // Keep the persisted URL fresh when SSDP
                        // returns a different one for the pinned UDN
                        // (e.g. DHCP gave the speaker a new IP). The
                        // user doesn't see this — it just self-heals.
                        if let Some(udn) = &self.settings.network_renderer_udn {
                            if let Some(live) = renderers.iter().find(|r| &r.udn == udn) {
                                if self.settings.network_renderer_descriptor_url.as_deref()
                                    != Some(live.descriptor_url.as_str())
                                {
                                    self.settings.network_renderer_descriptor_url =
                                        Some(live.descriptor_url.clone());
                                    changed = true;
                                }
                            }
                        }
                        ui.end_row();
                    });

                ui.separator();
                ui.label("Per-deck startup defaults");
                ui.indent("deck-defaults", |ui| {
                    for (label, deck) in [("Deck A", DeckId::A), ("Deck B", DeckId::B)] {
                        ui.horizontal(|ui| {
                            ui.strong(label);
                            let d = self.settings.deck_mut(deck);
                            if ui.checkbox(&mut d.pitch_lock, "pitch-lock").changed() {
                                changed = true;
                            }
                            if ui.checkbox(&mut d.beat_align, "beat-align").changed() {
                                changed = true;
                            }
                        });
                    }
                });

                ui.separator();
                ui.small("Device / music dir / MIDI port take effect on next launch.");
            });

        // Save when the user closes the window OR after any edit
        // (cheap — ~5 KB of TOML written atomically). Tying it to
        // close alone would lose data if the user just exits the app.
        if changed || (!open && self.settings_open) {
            if let Err(e) = self.settings.save() {
                eprintln!("settings: save failed: {e}");
            }
        }
        self.settings_open = open;
    }

    /// Per-frame "deck played" detector for the History tab. Logs
    /// once per LoadTrack — when a deck becomes audible (playing +
    /// gain above threshold) and we haven't already logged its
    /// currently-loaded path. This gives natural hysteresis: a
    /// fader dip below the threshold and back up doesn't re-fire,
    /// and a pause/resume of the same track stays one entry.
    /// Loading a different track resets the latch, so the next
    /// play of that new track logs.
    ///
    /// The threshold is just a "did this track actually get heard"
    /// gate — a deck cued but never faded up shouldn't appear in
    /// history. Same definition is reused by the recording
    /// cue-sheet markers (FEATURES.md §6).
    fn tick_history(&mut self) {
        const AUDIBLE_GAIN: f32 = 0.05; // ≈ -26 dB
        for (i, deck_id) in [DeckId::A, DeckId::B].iter().enumerate() {
            let d = match deck_id {
                DeckId::A => &self.deck_a,
                DeckId::B => &self.deck_b,
            };
            // Clear the latch when the deck swaps to a different
            // track — including unload (loaded_path = None).
            if self.deck_logged_path[i].as_deref() != d.loaded_path.as_deref() {
                if d.loaded_path.is_none() {
                    self.deck_logged_path[i] = None;
                    continue;
                }
                // Different track loaded — pre-emptively clear so the
                // audibility check below decides whether to log.
                self.deck_logged_path[i] = None;
            }
            let Some(path) = d.loaded_path.as_ref() else { continue };
            if self.deck_logged_path[i].is_some() {
                // Already logged this load — done, regardless of fader.
                continue;
            }
            let audible = d.telemetry.is_playing()
                && d.telemetry.current_gain() >= AUDIBLE_GAIN;
            if audible {
                self.history.append(*deck_id, path);
                self.deck_logged_path[i] = Some(path.clone());
            }
        }
    }

    /// Mirror DeckUi.{loaded_path, bpm, beat_grid, downbeats, total_frames,
    /// sample_rate, key} into the auto-mix controller's shared state so
    /// the controller (which doesn't hold a reference to DeckUi) can see
    /// the current track metadata.
    fn sync_auto_mix_meta(&self) {
        let mut s = self.auto_mix.lock().unwrap();
        snapshot_into(&self.deck_a, &mut s.meta_a);
        snapshot_into(&self.deck_b, &mut s.meta_b);
    }

    /// Mirror the track-picker inputs (filter/favs/genre/harmonic key)
    /// into the auto-mix shared state. Called each frame so the
    /// controller picks tracks consistent with what's on screen.
    fn sync_auto_mix_picker(&self) {
        let target_key = match self.harmonic_filter {
            Some(DeckId::A) => self.deck_a.key,
            Some(DeckId::B) => self.deck_b.key,
            None => None,
        };
        let mut s = self.auto_mix.lock().unwrap();
        s.filter_lower = self.filter.to_lowercase();
        s.favourites_only = self.favourites_only;
        s.genre_filter = self.genre_filter.clone();
        s.harmonic_target_key = target_key;
        // tracks/favourites mirroring is best-effort: only re-sync the
        // Arc<Vec> when the underlying length or first/last paths
        // differ from what's already shared (cheap probe; full sync is
        // not free with thousands of tracks).
        let need_tracks_resync = s.tracks.len() != self.tracks.len()
            || s.tracks.first().map(|t| &t.path) != self.tracks.first().map(|t| &t.path)
            || s.tracks.last().map(|t| &t.path) != self.tracks.last().map(|t| &t.path);
        if need_tracks_resync {
            s.tracks = Arc::new(self.tracks.clone());
        }
        if s.favourites.len() != self.favourites.paths().len() {
            s.favourites = Arc::new(self.favourites.paths().clone());
        }
    }

    /// Toggle auto-mix. Off → Armed; Armed/Active → Off (resetting
    /// any in-flight blend's deck volumes). Pre-load on entering
    /// Armed is handled by the controller's next tick (~50 ms later).
    // Hot-cue UI hooks. The pad row that calls these is waiting on
    // the Claude Design mockup; suppress dead-code lints until then.

    /// Hot-cue press from the UI (pad / keyboard / future MIDI
    /// shift-pad). The engine decides set vs jump based on whether
    /// the slot is set already — we just send `HotCueSetOrJump`.
    /// After sending, mirror the new slot map into TrackMeta so the
    /// `.track-meta` file is kept current.
    #[allow(dead_code)]
    fn hot_cue_press(&mut self, deck: DeckId, slot: u8) {
        if slot >= 8 { return; }
        let _ = self.sender.send(DeckCommand::HotCueSetOrJump { deck, slot });
        self.sync_hot_cues_to_meta(deck);
    }

    /// Hot-cue release (key/pad lift). No-op unless that deck's
    /// hot_cue_preview matches the slot (the engine checks).
    #[allow(dead_code)]
    fn hot_cue_release(&mut self, deck: DeckId, slot: u8) {
        if slot >= 8 { return; }
        let _ = self.sender.send(DeckCommand::HotCueRelease { deck, slot });
    }

    /// Shift-click / shift-pad clear.
    #[allow(dead_code)]
    fn hot_cue_clear(&mut self, deck: DeckId, slot: u8) {
        if slot >= 8 { return; }
        let _ = self.sender.send(DeckCommand::HotCueClear { deck, slot });
        self.sync_hot_cues_to_meta(deck);
    }

    /// Mirror the engine's current hot-cue slot positions for a
    /// deck into the `.track-meta` store + persist. Called after
    /// any set/clear so the file always reflects what the engine
    /// knows. The conversion frames → seconds uses the deck's
    /// loaded sample_rate.
    fn sync_hot_cues_to_meta(&mut self, deck: DeckId) {
        let d = match deck { DeckId::A => &self.deck_a, DeckId::B => &self.deck_b };
        let Some(path) = d.loaded_path.clone() else { return };
        let sr = d.sample_rate;
        if sr == 0 { return; }
        let frames = d.telemetry.hot_cue_frames();
        let secs: [Option<f64>; 8] = std::array::from_fn(|i| {
            frames[i].map(|f| f as f64 / sr as f64)
        });
        let labels = d.hot_cue_labels.clone();
        let colours = d.hot_cue_colours;
        // `set_hot_cues` strips labels/colours for any newly-empty slot,
        // so push positions first, then re-apply remaining labels and
        // colours so the menu edits land.
        self.track_meta.set_hot_cues(&path, secs);
        for (i, label) in labels.into_iter().enumerate() {
            if secs[i].is_some() {
                self.track_meta.set_hot_cue_label(&path, i, label);
            }
        }
        for (i, colour) in colours.iter().enumerate() {
            if secs[i].is_some() {
                self.track_meta.set_hot_cue_colour(&path, i, *colour);
            }
        }
    }

    fn toggle_auto_mix(&mut self) {
        // Snapshot of any in-flight mix that we need to clean up
        // commands for before flipping the state to Off.
        let cleanup = {
            let mut s = self.auto_mix.lock().unwrap();
            match &s.state {
                AutoMixState::Off => {
                    eprintln!("auto-mix: armed — controller will pre-load next track");
                    s.state = AutoMixState::Armed(ArmedState::default());
                    None
                }
                AutoMixState::Armed(_) => {
                    eprintln!("auto-mix: disarmed");
                    s.state = AutoMixState::Off;
                    None
                }
                AutoMixState::Active(active) => {
                    eprintln!("auto-mix: cancelled (user toggled mid-blend)");
                    let cleanup = (active.in_deck, active.out_deck);
                    s.state = AutoMixState::Off;
                    Some(cleanup)
                }
            }
        };
        if let Some((in_deck, out_deck)) = cleanup {
            let _ = self.sender.send(DeckCommand::SetGain { deck: in_deck, gain: 0.0 });
            let _ = self.sender.send(DeckCommand::SetStemDrums { deck: in_deck, gain: 1.0 });
            let _ = self.sender.send(DeckCommand::SetGain { deck: out_deck, gain: 1.0 });
            let _ = self.sender.send(DeckCommand::SetStemDrums { deck: out_deck, gain: 1.0 });
        }
    }

    /// Called when the user has touched any deck-affecting control
    /// while a blend was in flight (the orchestrator's own writes go
    /// through `self.sender` directly and don't set `user_touched`).
    /// Tears the blend down and restores both decks.
    fn cancel_auto_mix(&mut self) {
        let cleanup = {
            let mut s = self.auto_mix.lock().unwrap();
            if let AutoMixState::Active(ref active) = s.state {
                let cleanup = (active.in_deck, active.out_deck);
                s.state = AutoMixState::Off;
                Some(cleanup)
            } else {
                None
            }
        };
        if let Some((in_deck, out_deck)) = cleanup {
            eprintln!("auto-mix: cancelled (user touched control mid-blend)");
            let _ = self.sender.send(DeckCommand::SetGain { deck: in_deck, gain: 0.0 });
            let _ = self.sender.send(DeckCommand::SetStemDrums { deck: in_deck, gain: 1.0 });
            let _ = self.sender.send(DeckCommand::SetGain { deck: out_deck, gain: 1.0 });
            let _ = self.sender.send(DeckCommand::SetStemDrums { deck: out_deck, gain: 1.0 });
        }
    }
}

impl eframe::App for DjApp {
    // eframe 0.34 renamed `update` → `ui` and now hands us a
    // `&mut Ui` rooted in the whole window instead of a `&Context`.
    // Panels still take an explicit parent — `show_inside(ui, …)`
    // replaces the old `show(ctx, …)`.
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.drain_loads();
        self.tick_history();
        // Sync picker inputs every frame so the controller (which runs
        // on its own thread) picks tracks consistent with the current
        // filter/favs/genre/harmonic selection. Auto-mix tick itself
        // is driven by the controller thread, not the UI.
        self.sync_auto_mix_picker();
        // Tracks whether any deck-affecting user input fired this
        // frame. Auto-mix aborts at end-of-frame if so. Cell because
        // multiple nested egui closures need to set it.
        let user_touched = std::cell::Cell::new(false);
        if handle_keys(&ctx, &self.sender) {
            user_touched.set(true);
        }

        // Force continuous repaint — the deck waveforms and the
        // running playhead need ~60 Hz anyway, and keeping the event
        // loop hot prevents Hyprland's "Application Not Responding"
        // dialog from popping when the surface goes idle.
        //
        // request_repaint() schedules a paint at the next vsync, but
        // Wayland compositors stop delivering vsync events to
        // unfocused surfaces — so on an idle/unfocused window, the
        // next paint can be delayed indefinitely and tick_auto_mix
        // misses its 22 s trigger window entirely. request_repaint_after
        // uses an OS timer and fires regardless of focus state.
        ctx.request_repaint();
        ctx.request_repaint_after(std::time::Duration::from_millis(50));

        // Top bar: identity + status only. Mix-related controls live
        // in the shared mix bar at the bottom of the window (see the
        // Panel::bottom block further down). Matches the design
        // handoff: "Everything mix-related lives in the shared mix
        // bar, NOT here."
        egui::Panel::top("top")
            .frame(
                egui::Frame::new()
                    .fill(palette::for_ui(root).panel)
                    .inner_margin(egui::Margin {
                        left: 14, right: 14, top: 8, bottom: 8,
                    }),
            )
            .show_inside(root, |ui| {
            let pal = palette::for_ui(ui);
            ui.horizontal(|ui| {
                // "ODJ" rounded-square glyph in accent-pink — the
                // app's actual name.
                let (rect, _resp) = ui.allocate_exact_size(
                    egui::Vec2::new(34.0, 26.0),
                    egui::Sense::hover(),
                );
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 6.0, pal.accent_pink);
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "ODJ",
                    egui::FontId::proportional(12.0),
                    pal.ink,
                );
                ui.add_space(6.0);
                if ui.button("⚙").on_hover_text("Settings").clicked() {
                    self.settings_open = !self.settings_open;
                }
                ui.add_space(6.0);
                // MIDI chip — short summary + hover tooltip with the
                // full status string. Truncate long error messages
                // (e.g. "no MIDI port matched 'ODJ'…") so they don't
                // dominate the top bar.
                let (midi_short, midi_ok) = midi_chip_summary(&self.midi_status);
                let chip_bg = if midi_ok { pal.chip } else { with_opacity(pal.accent_amber, 0.15) };
                let chip_stroke = if midi_ok { pal.line } else { pal.accent_amber };
                let chip_frame = egui::Frame::new()
                    .fill(chip_bg)
                    .stroke(egui::Stroke::new(1.0, chip_stroke))
                    .corner_radius(999.0)
                    .inner_margin(egui::Margin {
                        left: 10, right: 10, top: 2, bottom: 2,
                    });
                chip_frame.show(ui, |ui| {
                    ui.label(mono(midi_short));
                }).response.on_hover_text(&self.midi_status);
                // Right-align the library status.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let analysed = self.analysis_progress.load(Ordering::Relaxed);
                    if analysed < self.analysis_total {
                        ui.label(mono(format!(
                            "analysing {}/{}",
                            analysed, self.analysis_total
                        )));
                    } else {
                        let label = format!(
                            "library: {} tracks · {} analysed",
                            self.tracks.len(),
                            self.analysis_cache.count(),
                        );
                        ui.label(mono(label));
                    }
                });
            });
        });

        // Shared mix bar — spans below the decks. Everything that
        // (Shared mix bar lives INSIDE the central panel below the
        // decks now — see `render_shared_mix_bar` — so it doesn't
        // span under the library / source rail.)

        // Source rail (left of the track list). Six filter sources +
        // a chevron toggle for collapse. Drives `library_source` →
        // which the track list and the History view conditionally
        // respect below.
        let rail_width = if self.source_rail_collapsed { 56.0 } else { 134.0 };
        egui::Panel::left("source-rail")
            .resizable(false)
            .exact_size(rail_width)
            .frame(
                egui::Frame::new()
                    .fill(palette::for_ui(root).inset)
                    .inner_margin(egui::Margin {
                        left: 10, right: 8, top: 10, bottom: 10,
                    }),
            )
            .show_inside(root, |ui| {
                self.render_source_rail(ui);
            });

        egui::Panel::left("tracks")
            .resizable(true)
            .default_size(620.0)
            .show_inside(root, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.browser_tab, BrowserTab::Tracks, "Tracks");
                    ui.selectable_value(&mut self.browser_tab, BrowserTab::History, "History");
                    ui.selectable_value(&mut self.browser_tab, BrowserTab::GridEdit, "Grid Adjust");
                });
                ui.separator();
                if self.browser_tab == BrowserTab::History {
                    self.render_history(ui);
                    return;
                }
                if self.browser_tab == BrowserTab::GridEdit {
                    self.render_grid_adjust(ui);
                    return;
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("🔍  Search library")
                            .desired_width(ui.available_width().min(360.0)),
                    );
                });
                // Favourites / Genre / Harmonic-compat filters used to
                // live in a row above the table; the source rail
                // subsumes "Favourites" and (will subsume) "Genres".
                // The harmonic-compat picker is dev-only — keep it
                // available but tuck it under a "More filters"
                // collapsing section so the default layout matches
                // the design.
                egui::CollapsingHeader::new("More filters")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let label = match self.harmonic_filter {
                                None => "Harmonic: off".to_string(),
                                Some(DeckId::A) => "Harmonic: Deck A".to_string(),
                                Some(DeckId::B) => "Harmonic: Deck B".to_string(),
                            };
                            egui::ComboBox::from_id_salt("compat")
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.harmonic_filter, None, "off");
                                    ui.selectable_value(
                                        &mut self.harmonic_filter,
                                        Some(DeckId::A),
                                        "Deck A",
                                    );
                                    ui.selectable_value(
                                        &mut self.harmonic_filter,
                                        Some(DeckId::B),
                                        "Deck B",
                                    );
                                });
                            ui.separator();
                            let mut genres: Vec<&str> = self
                                .tracks
                                .iter()
                                .map(|m| m.genre.trim())
                                .filter(|g| !g.is_empty())
                                .collect();
                            genres.sort_unstable_by_key(|g| g.to_lowercase());
                            genres.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
                            let genre_label = match &self.genre_filter {
                                None => "Genre: any".to_string(),
                                Some(g) => format!("Genre: {g}"),
                            };
                            egui::ComboBox::from_id_salt("genre")
                                .selected_text(genre_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.genre_filter, None, "any");
                                    for g in genres {
                                        let sel = self.genre_filter.as_deref() == Some(g);
                                        if ui.selectable_label(sel, g).clicked() {
                                            self.genre_filter = Some(g.to_string());
                                        }
                                    }
                                });
                        });
                    });
                ui.separator();

                self.render_track_table(ui);
            });

        egui::CentralPanel::default().show_inside(root, |ui| {
            // Top: deck A info row, the waveform stack, deck B info row.
            // Layout per the design handoff:
            //   [Deck A badge + title · BPM · Key · time]
            //   [Deck A overview ]
            //   [Deck A zoom     ]   ← beat grids adjacent
            //   [Deck B zoom     ]   ←
            //   [Deck B overview ]
            //   [Deck B badge + title · BPM · Key · time]
            deck_info_row(ui, DeckId::A, &self.deck_a);
            overview_waveform(ui, &self.deck_a, DeckId::A, &self.sender);
            zoom_view(ui, &self.deck_a, DeckId::A, &self.sender);
            zoom_view(ui, &self.deck_b, DeckId::B, &self.sender);
            overview_waveform(ui, &self.deck_b, DeckId::B, &self.sender);
            deck_info_row(ui, DeckId::B, &self.deck_b);

            ui.separator();

            // Bottom: controls in mixer-style columns — Deck A on the
            // left, Deck B on the right. The midline is fixed at half
            // the central panel width and the per-deck inner UI is
            // strictly clipped to its half, so a long track title on
            // Deck A can't push Deck B around.
            let col_w = (ui.available_width() - 12.0) * 0.5;
            let col_h = ui.available_height();
            // Wrap each deck column in a framed panel so the deck /
            // FX / shared-mix sections read as distinct boxes (the
            // design's "panel-bg, 14 px radius" treatment).
            let pal = palette::for_ui(ui);
            let deck_frame = egui::Frame::new()
                .fill(pal.panel)
                .stroke(egui::Stroke::new(1.0, pal.line))
                .corner_radius(14.0)
                .inner_margin(egui::Margin {
                    left: 12, right: 12, top: 10, bottom: 10,
                });
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(col_w, col_h),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_min_width(col_w);
                        ui.set_max_width(col_w);
                        deck_frame.show(ui, |ui| {
                            if deck_controls(ui, DeckId::A, &mut self.deck_a, &self.sender) {
                                user_touched.set(true);
                            }
                        });
                    },
                );
                if self.deck_a.hot_cue_meta_dirty {
                    self.deck_a.hot_cue_meta_dirty = false;
                    self.sync_hot_cues_to_meta(DeckId::A);
                }
                ui.add_space(8.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(col_w, col_h),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_min_width(col_w);
                        ui.set_max_width(col_w);
                        deck_frame.show(ui, |ui| {
                            if deck_controls(ui, DeckId::B, &mut self.deck_b, &self.sender) {
                                user_touched.set(true);
                            }
                        });
                    },
                );
                if self.deck_b.hot_cue_meta_dirty {
                    self.deck_b.hot_cue_meta_dirty = false;
                    self.sync_hot_cues_to_meta(DeckId::B);
                }
            });
            // Shared mix bar — spans the mixer area (both deck
            // columns) but stops at the central panel's left edge.
            self.render_shared_mix_bar(ui);
        });
        // End-of-frame abort: any deck-affecting user input cancels
        // an in-flight auto-mix. The orchestrator's own gain/drum
        // writes go directly through `self.sender` and never set
        // `user_touched`, so they don't self-cancel.
        if user_touched.get() && self.auto_mix.lock().unwrap().is_active() {
            self.cancel_auto_mix();
        }
        self.render_settings_window(&ctx);
        // Playlist editing modal (new / rename / confirm-delete).
        // Renders after the main panels so it overlays cleanly.
        self.render_playlist_dialog(&ctx);
        // Reconcile network-output play state at the end of every
        // frame. Cheap when nothing changed (one Vec snapshot from
        // the discovery handle, one Option compare); fires the
        // Play/Stop SOAP calls only on transitions. Sits here at the
        // bottom so any settings-window edit this frame is reflected
        // immediately.
        self.sync_network_play_state();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Best-effort save on a clean exit (window-X, ctrl-C inside
        // egui, etc.). If the disk write fails the user just gets
        // their last-saved values on the next run.
        if let Err(e) = self.settings.save() {
            eprintln!("settings: save on exit failed: {e}");
        }
        // Best-effort UPnP Stop so the Naim doesn't hold a dead
        // session open after we close. Spawned in a thread with a
        // ~2-second SOAP budget — if the renderer is unreachable it
        // just times out and the app exit continues. We don't wait
        // for the thread; the kernel reaps it.
        if let Some((_, ctrl)) = self.network_active.take() {
            upnp::stop(ctrl, "(app exit)".into());
        }
    }
}

/// Renders a deck's header (label + track title + BPM + key) followed by
/// the transport / pitch / vol / EQ rows. Waveforms live above this in the
/// central panel so the two decks' beat grids sit visually adjacent — see
/// the CentralPanel block in `App::update`.
/// Returns `true` if any widget in this column fired a user command —
/// the caller uses that as the "abort auto-mix" signal.
#[must_use]
fn deck_controls(
    ui: &mut egui::Ui,
    deck: DeckId,
    d: &mut DeckUi,
    sender: &Sender,
) -> bool {
    let mut user_touched = false;
    let pal = palette::for_ui(ui);
    // "DECK A" / "DECK B" overline in the deck's accent colour.
    // Title / BPM / Key / time live in the info row that flanks the
    // waveform stack (see `deck_info_row`), so we don't duplicate
    // them here.
    let (label, accent) = match deck {
        DeckId::A => ("DECK A", pal.accent_blue),
        DeckId::B => ("DECK B", pal.accent_pink),
    };
    ui.colored_label(
        accent,
        egui::RichText::new(label).small().strong(),
    );

    // Stem-separation status. Always allocate the row so the layout
    // below doesn't shift when stems finish loading; the label is
    // only visible while we're actually waiting on the worker.
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), 18.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            if d.loaded_path.is_some() && !d.telemetry.are_stems_loaded() {
                ui.colored_label(pal.accent_amber, "🎛 separating stems…");
            }
        },
    );

    // Mode-toggle row: Q / pitch lock / Sync / 🎧 PFL.
    // Play + CUE moved below the channel strip as arcade-style
    // buttons (see the second `horizontal` after the strip).
    ui.horizontal(|ui| {
        if pill_toggle(ui, "Quantize", d.quantize, pal.accent_blue).clicked() {
            user_touched = true;
            d.quantize = !d.quantize;
            let _ = sender.send(DeckCommand::SetQuantize { deck, on: d.quantize });
        }
        let pitch_lock = d.telemetry.is_pitch_locked();
        if pill_toggle(ui, "Keylock", pitch_lock, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::SetPitchLock { deck, on: !pitch_lock });
        }
        // Sync is a one-shot (not a toggle), but renders as an
        // accent-blue pill so it sits next to the others visually.
        if pill_toggle(ui, "Sync", false, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::Sync { deck });
        }
        // Cue toggle — outline-red rather than blue so it visually
        // groups with the bottom CUE arcade button.
        let cue_on = d.telemetry.is_cue_on();
        if pill_toggle(ui, "🎧 Cue", cue_on, pal.accent_red).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::SetCueOn { deck, on: !cue_on });
        }
    });

    ui.add_space(6.0);

    // Loop controls. Sits between the track info / mode toggles and
    // the channel strip below. IN/OUT capture the playhead (beat-
    // quantised in the engine); double / half / clear act on the
    // currently-active loop; EXIT lets the loop finish its current
    // iteration then continues. All commands are no-ops if the
    // relevant loop state isn't set, but we still grey out the
    // buttons to make that obvious.
    ui.horizontal(|ui| {
        let loop_range = d.telemetry.loop_range();
        let has_in = d.telemetry.loop_in_frame().is_some();
        let has_loop = loop_range.is_some();
        if loop_pill(ui, "IN", has_in && !has_loop, true, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopSetIn { deck });
        }
        if loop_pill(ui, "OUT", false, has_in, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopSetOut { deck });
        }
        // One-bar auto-loop pill (active state when a 4-beat loop
        // is currently the active range).
        let is_4 = has_loop && {
            let (i, o) = loop_range.unwrap();
            if d.bpm > 0.0 && d.sample_rate > 0 {
                let beats = (o - i) as f64 / d.sample_rate as f64 * (d.bpm as f64 / 60.0);
                (beats - 4.0).abs() < 0.1
            } else { false }
        };
        if loop_pill(ui, "4", is_4, true, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopAuto { deck, beats: 4 });
        }
        if loop_pill(ui, "½", false, has_loop, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopHalve { deck });
        }
        if loop_pill(ui, "×2", false, has_loop, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopDouble { deck });
        }
        if loop_pill(ui, "Exit", false, has_loop, pal.accent_blue).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopExit { deck });
        }
        if loop_pill(ui, "CLR", false, has_in || has_loop, pal.accent_red).clicked() {
            user_touched = true;
            let _ = sender.send(DeckCommand::LoopClear { deck });
        }
        // Loop length readout in beats when active.
        if let (Some((i, o)), true) = (loop_range, d.bpm > 0.0 && d.sample_rate > 0) {
            let beats = ((o - i) as f64 / d.sample_rate as f64) * (d.bpm as f64 / 60.0);
            ui.colored_label(pal.accent_amber, format!("loop: {:.0} beats", beats));
        }
    });

    ui.add_space(6.0);

    // Hot cue row: 8 slots (2 rows × 4). Tap empty to set, tap set
    // to jump (engine routes via HotCueSetOrJump); shift-tap to
    // clear. Per-slot labels / colours are deferred — the user
    // mentioned that's a feature for the beat-grid tool later.
    if hot_cue_row(ui, deck, d, sender) {
        user_touched = true;
    }

    ui.add_space(6.0);

    // Channel strip: four columns of equal height. PITCH and VOL are
    // identically-shaped fader columns (3 invisible knob-sized slots
    // at the top so their labels + faders align with the *bottom* of
    // the EQ and STEM columns). VOL sits between the two knob
    // columns, not under EQ — matches the layout the user described.
    //
    //   ┌──── LEFT ────┐ ┌──── RIGHT ────┐
    //   │   HIGH knob  │ │   DRUMS knob  │
    //   │   MID  knob  │ │  VOCALS knob  │
    //   │   LOW  knob  │ │   INSTR knob  │
    //   │   PITCH fad  │ │    VOL fad    │
    //   │   value      │ │    value      │
    //   └──────────────┘ └───────────────┘
    //
    // Two columns of equal width. The fader in each column sits
    // directly below the knob stack so its centre aligns with the
    // centre of the knobs above it (PITCH under EQ, VOL under STEMS).
    const KNOB_DIA: f32 = 46.0;
    const FADER_H: f32 = 110.0;
    let track_loaded = d.loaded_path.is_some();
    let stems_ready = d.telemetry.are_stems_loaded();
    let show_separating = track_loaded && !stems_ready;
    let (drums_lbl, vocals_lbl, instr_lbl) = if show_separating {
        ("drums…", "vocals…", "instr…")
    } else {
        ("DRUMS", "VOCALS", "INSTR")
    };

    // Two well frames side by side: [EQ] [STEMS]. Each well is a
    // raised panel with a coloured overline header and three
    // knobs in a row. Matches the design's two-well layout.
    let well = egui::Frame::group(ui.style())
        .fill(pal.raised)
        .corner_radius(10.0)
        .inner_margin(8.0)
        .stroke(egui::Stroke::new(1.0, pal.line));

    // EQ + Stems wells each take half the deck-panel width so that
    // a future FX2 box can drop into the slot below Stems and match
    // exactly. `cell_w` is the shared per-cell width used here AND
    // for the FX module further down.
    let gap = 8.0;
    let cell_w = (ui.available_width() - gap).max(0.0) * 0.5;
    let cell = move |ui: &mut egui::Ui, contents: &mut dyn FnMut(&mut egui::Ui)| {
        ui.allocate_ui_with_layout(
            egui::Vec2::new(cell_w, 0.0),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.set_min_width(cell_w);
                ui.set_max_width(cell_w);
                well.show(ui, |ui| {
                    ui.set_min_width(cell_w - 16.0);
                    // Frame::show creates an inner ui with its own
                    // (default) layout — `Align::Center` from the
                    // outer ui doesn't propagate through. Wrap the
                    // contents explicitly so the knob row and the
                    // overline both centre horizontally.
                    ui.with_layout(
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| contents(ui),
                    );
                });
            },
        );
    };
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        // Centred horizontal knob row.
        //
        // We tried `with_main_align(Center)` first — it didn't work
        // because egui's layout still kept the row left-aligned
        // inside the well's available width. The reliable workaround
        // is to compute the empty horizontal space ourselves and
        // pad equally on the left before the first knob. Each knob
        // widget is `KNOB_DIA + 8` wide (4 px pad on each side),
        // and the default `item_spacing.x` is ~8 px between
        // children — so for N knobs the row is
        //   N * (KNOB_DIA + 8) + (N - 1) * spacing.
        let centred_row = |ui: &mut egui::Ui, n_knobs: usize, contents: &mut dyn FnMut(&mut egui::Ui)| {
            let spacing_x = ui.spacing().item_spacing.x;
            let one_knob = KNOB_DIA + 8.0;
            let row_w = (n_knobs as f32) * one_knob + (n_knobs.saturating_sub(1) as f32) * spacing_x;
            ui.horizontal(|ui| {
                let pad = ((ui.available_width() - row_w) * 0.5).max(0.0);
                if pad > 0.0 { ui.add_space(pad); }
                contents(ui);
            });
        };
        // ---- EQ well ----
        cell(ui, &mut |ui| {
            ui.colored_label(
                pal.accent_blue,
                egui::RichText::new("EQ").small().strong(),
            );
            centred_row(ui, 3, &mut |ui| {
                let cur_high = d.telemetry.current_eq_high_db();
                if let Some(v) = knob(ui, "HIGH", cur_high, -25.0..=6.0, KNOB_DIA) {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::SetEqHigh { deck, db: v });
                }
                let cur_mid = d.telemetry.current_eq_mid_db();
                if let Some(v) = knob(ui, "MID", cur_mid, -25.0..=6.0, KNOB_DIA) {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::SetEqMid { deck, db: v });
                }
                let cur_low = d.telemetry.current_eq_low_db();
                if let Some(v) = knob(ui, "LOW", cur_low, -25.0..=6.0, KNOB_DIA) {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::SetEqLow { deck, db: v });
                }
            });
        });
        // ---- Stems well ----
        cell(ui, &mut |ui| {
            ui.colored_label(
                pal.faint,
                egui::RichText::new("STEMS").small().strong(),
            );
            centred_row(ui, 3, &mut |ui| {
                let cur_drums = d.telemetry.current_stem_drums();
                if let Some(v) = knob_colored(
                    ui, drums_lbl, cur_drums, 0.0..=1.5, KNOB_DIA, pal.stem_drums,
                ) {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::SetStemDrums { deck, gain: v });
                }
                let cur_vocals = d.telemetry.current_stem_vocals();
                if let Some(v) = knob_colored(
                    ui, vocals_lbl, cur_vocals, 0.0..=1.5, KNOB_DIA, pal.stem_vocals,
                ) {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::SetStemVocals { deck, gain: v });
                }
                let cur_instr = d.telemetry.current_stem_instruments();
                if let Some(v) = knob_colored(
                    ui, instr_lbl, cur_instr, 0.0..=1.5, KNOB_DIA, pal.stem_instruments,
                ) {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::SetStemInstruments { deck, gain: v });
                }
            });
        });
    });

    // FX module — same width as a single EQ/Stems cell so a future
    // FX2 box can drop into the right-hand slot without re-layout.
    ui.add_space(8.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.allocate_ui_with_layout(
            egui::Vec2::new(cell_w, 0.0),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.set_min_width(cell_w);
                ui.set_max_width(cell_w);
                if fx_module(ui, deck, d, sender) {
                    user_touched = true;
                }
            },
        );
        // Right slot reserved for a future FX2 module; takes the
        // same `cell_w` so the layout stays a clean 2-column grid.
        ui.allocate_space(egui::Vec2::new(cell_w, 0.0));
    });

    ui.add_space(8.0);
    // Bottom row: PITCH + VOL faders on the left, Play + Cue
    // stacked vertically on the right (saves horizontal space and
    // matches the layout the user asked for).
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 24.0;
        // Faders.
        ui.vertical(|ui| {
            ui.colored_label(pal.faint, egui::RichText::new("PITCH").small());
            let mut speed = d.telemetry.current_speed();
            let r = v_fader(
                ui, &mut speed, PITCH_MIN..=PITCH_MAX, FADER_H,
                VFaderOpts { center_detent: true, accent_fill: None },
            );
            if r.changed() {
                user_touched = true;
                let _ = sender.send(DeckCommand::SetSpeed { deck, ratio: speed });
            }
            ui.label(mono(format!("{:.3}", speed)));
        });
        ui.vertical(|ui| {
            ui.colored_label(pal.faint, egui::RichText::new("VOL").small());
            let mut gain = d.telemetry.current_gain();
            let r = v_fader(
                ui, &mut gain, 0.0..=1.0, FADER_H,
                VFaderOpts { center_detent: false, accent_fill: Some(pal.accent_green) },
            );
            if r.changed() {
                user_touched = true;
                let _ = sender.send(DeckCommand::SetGain { deck, gain });
            }
            ui.label(mono(format!("{:.2}", gain)));
        });
        // Play + Cue stacked vertically. Plain `vertical` (not
        // `vertical_centered`) keeps them snug to the right of VOL
        // instead of grabbing the rest of the row.
        ui.vertical(|ui| {
            let playing = d.telemetry.is_playing();
            if arcade_button(
                ui,
                if playing { "⏸" } else { "▶" },
                52.0,
                pal.accent_green,
            ) {
                user_touched = true;
                let _ = sender.send(DeckCommand::PlayToggle(deck));
            }
            ui.add_space(8.0);
            let (cue_clicked, cue_down) = arcade_button_held(
                ui, "CUE", 52.0, pal.accent_red,
            );
            let _ = cue_clicked;
            match (d.cue_held, cue_down) {
                (false, true) => {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::CuePress(deck));
                }
                (true, false) => {
                    user_touched = true;
                    let _ = sender.send(DeckCommand::CueRelease(deck));
                }
                _ => {}
            }
            d.cue_held = cue_down;
        });
    });

    user_touched
}

/// Squeeze the MIDI status line into a short chip-friendly label.
/// `(short, ok)` — `ok` toggles the chip's neutral vs warning colour.
fn midi_chip_summary(status: &str) -> (String, bool) {
    if let Some(rest) = status.strip_prefix("MIDI: ") {
        if rest.starts_with("no MIDI port matched") {
            (format!("MIDI · none"), false)
        } else if rest == "disabled" {
            (format!("MIDI · off"), false)
        } else {
            (format!("MIDI · {rest}"), true)
        }
    } else {
        (status.to_string(), true)
    }
}

fn fmt_mmss(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Options for `v_fader`.
#[derive(Default, Clone, Copy)]
struct VFaderOpts {
    /// Centre detent + tick (used for Pitch, which sits at 1.0 / 0%
    /// pitch shift by default).
    center_detent: bool,
    /// Optional accent fill drawn from the bottom of the track up
    /// to the handle's centre. Used for Vol (accent-green fill).
    accent_fill: Option<egui::Color32>,
}

/// Vertical fader — same visual language as `h_fader` but oriented
/// for the Pitch + Vol pair. 4 px track, 24×10 rounded handle
/// (rotated). Lower edge = lower value (so Vol pushed UP raises
/// gain, matches every DJ controller ever made).
fn v_fader(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    height: f32,
    opts: VFaderOpts,
) -> egui::Response {
    let pal = palette::for_ui(ui);
    let width = 24.0;
    let (rect, mut resp) = ui.allocate_exact_size(
        egui::Vec2::new(width, height),
        egui::Sense::click_and_drag(),
    );
    let lo = *range.start();
    let hi = *range.end();
    let span = (hi - lo).max(f32::EPSILON);
    let frac = ((*value - lo) / span).clamp(0.0, 1.0);
    let mid_x = rect.center().x;
    // Shrink the usable track range by half-handle-height on each
    // end so the 10 px handle stays fully inside the rect even at
    // value=min or value=max. Without this, the handle pokes out
    // of the rect and overlaps the PITCH/VOL label above and the
    // value readout below.
    let handle_half_h = 5.0;
    let usable_top = rect.top() + handle_half_h;
    let usable_bottom = rect.bottom() - handle_half_h;
    let handle_y = usable_bottom - frac * (usable_bottom - usable_top);

    let painter = ui.painter_at(rect);
    // Track (centred vertical 4 px column).
    let track_rect = egui::Rect::from_min_max(
        egui::Pos2::new(mid_x - 2.0, rect.top()),
        egui::Pos2::new(mid_x + 2.0, rect.bottom()),
    );
    painter.rect_filled(track_rect, 2.0, pal.knob_track);
    // Accent fill (Vol) — from bottom up to the handle.
    if let Some(fill_color) = opts.accent_fill {
        if handle_y < rect.bottom() {
            let fill_rect = egui::Rect::from_min_max(
                egui::Pos2::new(mid_x - 2.0, handle_y),
                egui::Pos2::new(mid_x + 2.0, rect.bottom()),
            );
            painter.rect_filled(fill_rect, 2.0, fill_color);
        }
    }
    // Centre detent tick (Pitch).
    if opts.center_detent {
        let cy = rect.top() + 0.5 * rect.height();
        painter.line_segment(
            [
                egui::Pos2::new(mid_x - 5.0, cy),
                egui::Pos2::new(mid_x + 5.0, cy),
            ],
            egui::Stroke::new(1.0, pal.faint),
        );
    }
    // Handle: 20×10 rounded rectangle horizontal.
    let handle_rect = egui::Rect::from_center_size(
        egui::Pos2::new(mid_x, handle_y),
        egui::Vec2::new(20.0, handle_half_h * 2.0),
    );
    painter.rect_filled(handle_rect, 5.0, pal.ink);

    if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            // Hit-test against the usable range so click positions
            // map 1:1 onto the visible handle travel.
            let usable_h = (usable_bottom - usable_top).max(f32::EPSILON);
            let mut new_frac = ((usable_bottom - p.y) / usable_h).clamp(0.0, 1.0);
            if opts.center_detent && (new_frac - 0.5).abs() < 0.03 {
                new_frac = 0.5;
            }
            let new_value = lo + new_frac * span;
            if (new_value - *value).abs() > f32::EPSILON {
                *value = new_value;
                resp.mark_changed();
            }
        }
    }
    resp
}

/// Options for `h_fader`.
#[derive(Default, Clone, Copy)]
struct HFaderOpts {
    /// Show a centre detent tick and snap towards centre when the
    /// user drags near it. Used for CUE↔MASTER (centred = even
    /// blend).
    center_detent: bool,
    /// Optional accent fill drawn from the left edge of the track
    /// up to the handle's centre. Used for the master volume bar.
    accent_fill: Option<egui::Color32>,
}

/// Custom horizontal fader: 4 px track + 24×10 rounded handle.
/// Matches the design handoff. Drag the handle (or click anywhere
/// on the track) to set a value in `range`. Returns the `Response`
/// so callers can detect `.changed()` / `.dragged()`.
fn h_fader(
    ui: &mut egui::Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
    opts: HFaderOpts,
) -> egui::Response {
    let pal = palette::for_ui(ui);
    let height = 14.0;
    let (rect, mut resp) = ui.allocate_exact_size(
        egui::Vec2::new(width, height),
        egui::Sense::click_and_drag(),
    );
    let lo = *range.start();
    let hi = *range.end();
    let span = (hi - lo).max(f32::EPSILON);

    // Handle position from current value.
    let frac = ((*value - lo) / span).clamp(0.0, 1.0);
    let mid_y = rect.center().y;
    let handle_x = rect.left() + frac * rect.width();

    let painter = ui.painter_at(rect);
    // Track (full-width, 4 px tall, rounded).
    let track_rect = egui::Rect::from_min_max(
        egui::Pos2::new(rect.left(), mid_y - 2.0),
        egui::Pos2::new(rect.right(), mid_y + 2.0),
    );
    painter.rect_filled(track_rect, 2.0, pal.knob_track);
    // Accent fill (master vol) — from left to handle.
    if let Some(fill_color) = opts.accent_fill {
        if handle_x > rect.left() {
            let fill_rect = egui::Rect::from_min_max(
                egui::Pos2::new(rect.left(), mid_y - 2.0),
                egui::Pos2::new(handle_x, mid_y + 2.0),
            );
            painter.rect_filled(fill_rect, 2.0, fill_color);
        }
    }
    // Centre detent tick.
    if opts.center_detent {
        let cx = rect.left() + 0.5 * rect.width();
        painter.line_segment(
            [
                egui::Pos2::new(cx, mid_y - 5.0),
                egui::Pos2::new(cx, mid_y + 5.0),
            ],
            egui::Stroke::new(1.0, pal.faint),
        );
    }
    // Handle: rounded rectangle (24×10) centred at handle_x.
    let handle_rect = egui::Rect::from_center_size(
        egui::Pos2::new(handle_x, mid_y),
        egui::Vec2::new(12.0, 12.0),
    );
    painter.rect_filled(handle_rect, 6.0, pal.ink);

    // Interaction: drag to set value.
    if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let mut new_frac =
                ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            // Snap to centre detent within a small dead-band.
            if opts.center_detent && (new_frac - 0.5).abs() < 0.03 {
                new_frac = 0.5;
            }
            let new_value = lo + new_frac * span;
            if (new_value - *value).abs() > f32::EPSILON {
                *value = new_value;
                resp.mark_changed();
            }
        }
    }
    resp
}

/// One row in the left source rail. Custom-painted instead of using
/// `Button` so the icon column stays a fixed width across different
/// glyphs (otherwise `≡` and `🕓` push the labels around). Returns
/// the underlying `Response` so the caller can `.clicked()` for the
/// left-click action and attach `.context_menu()` for right-click.
///
/// `collapsed = true` strips the label and shrinks the row to icon
/// width only. Caller is responsible for managing selection state
/// (highlighted background) — we just paint based on the `selected`
/// flag.
fn source_rail_item(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    selected: bool,
    collapsed: bool,
) -> egui::Response {
    let pal = palette::for_ui(ui);
    let item_color = if selected { pal.accent_blue } else { pal.muted };
    let h = 24.0;
    let w = if collapsed { 40.0 } else { 110.0 };
    let (rect, r) = ui.allocate_exact_size(
        egui::Vec2::new(w, h),
        egui::Sense::click(),
    );
    let bg = if selected {
        with_opacity(pal.accent_blue, 0.18)
    } else if r.hovered() {
        pal.raised
    } else {
        egui::Color32::TRANSPARENT
    };
    let painter = ui.painter_at(rect);
    if bg != egui::Color32::TRANSPARENT {
        painter.rect_filled(rect, 8.0, bg);
    }
    let icon_x = rect.left() + 14.0;
    painter.text(
        egui::Pos2::new(icon_x, rect.center().y),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(15.0),
        item_color,
    );
    if !collapsed {
        painter.text(
            egui::Pos2::new(icon_x + 16.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.0),
            item_color,
        );
    } else {
        r.clone().on_hover_text(label);
    }
    r
}

/// Rounded-pill toggle. Outlined when off (chip bg + faint stroke),
/// filled in the accent when on. Used for the Quantize / Keylock /
/// Sync / Cue row above each deck's loop strip. Returns the
/// underlying `Response` so the caller can call `.clicked()`.
fn pill_toggle(
    ui: &mut egui::Ui,
    label: &str,
    on: bool,
    accent: egui::Color32,
) -> egui::Response {
    let pal = palette::for_ui(ui);
    let (fill, text_col) = if on {
        (with_opacity(accent, 0.22), accent)
    } else {
        (pal.chip, pal.muted)
    };
    let stroke = if on {
        egui::Stroke::new(1.0, accent)
    } else {
        egui::Stroke::new(1.0, pal.line)
    };
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(text_col).small())
            .fill(fill)
            .stroke(stroke)
            .corner_radius(999.0)
            .min_size(egui::Vec2::new(0.0, 22.0)),
    )
}

/// Grid-edit operations the Adjust panel can fire. One per button.
#[derive(Clone, Copy)]
enum GridOp {
    /// Shift every beat by N seconds. Positive = later.
    Shift(f64),
    /// Re-anchor by N beats at the current BPM.
    Skip(i32),
    HalveBpm,
    DoubleBpm,
    /// Mark the beat nearest the playhead as bar-position-1.
    DownbeatAtPlayhead,
    /// Drop the `.track-meta` override; revert to analyser output.
    ResetOverride,
}

/// Small rectangular button used throughout the grid-adjust panel.
/// Greyed when `enabled` is false; same monospace + chip family as
/// the loop strip so the panel reads as a single grouping.
fn grid_btn(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    let pal = palette::for_ui(ui);
    let (fill, text_col) = if enabled {
        (pal.chip, pal.ink)
    } else {
        (pal.raised, pal.faint)
    };
    let btn = egui::Button::new(
        egui::RichText::new(label).monospace().color(text_col),
    )
    .fill(fill)
    .stroke(egui::Stroke::new(1.0, pal.line))
    .corner_radius(6.0)
    .min_size(egui::Vec2::new(64.0, 26.0));
    ui.add_enabled(enabled, btn)
}

/// Pill toggle with a leading status dot — same shape and accent
/// language as `pill_toggle`, but visually emphasises an on/off
/// *state* (rather than a one-shot like Sync). Used by the shared
/// mix bar (Beat align, Auto-mix) so those toggles read as the same
/// family as the deck-level pills.
fn pill_toggle_dot(
    ui: &mut egui::Ui,
    label: &str,
    on: bool,
    accent: egui::Color32,
) -> egui::Response {
    let pal = palette::for_ui(ui);
    let (fill, text_col) = if on {
        (with_opacity(accent, 0.22), accent)
    } else {
        (pal.chip, pal.muted)
    };
    let stroke = if on {
        egui::Stroke::new(1.0, accent)
    } else {
        egui::Stroke::new(1.0, pal.line)
    };
    // Measure the text so we can lay out [dot · label] inside one
    // rounded pill that we paint ourselves. Sticking with custom
    // paint keeps the dot exactly aligned with the label baseline.
    let font = egui::FontId::proportional(12.0);
    let text_size = ui.ctx().fonts_mut(|f| {
        f.layout_no_wrap(label.to_string(), font.clone(), text_col).size()
    });
    let dot_r = 3.5;
    let pad_x = 10.0;
    let gap = 6.0;
    let pill_w = pad_x * 2.0 + dot_r * 2.0 + gap + text_size.x;
    let pill_h = 22.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::Vec2::new(pill_w, pill_h),
        egui::Sense::click(),
    );
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 999.0, fill);
    p.rect_stroke(
        rect,
        999.0,
        stroke,
        egui::epaint::StrokeKind::Inside,
    );
    let cy = rect.center().y;
    let dot_centre = egui::pos2(rect.left() + pad_x + dot_r, cy);
    let dot_col = if on { accent } else { pal.muted };
    p.circle_filled(dot_centre, dot_r, dot_col);
    let text_pos = egui::pos2(
        dot_centre.x + dot_r + gap,
        cy,
    );
    p.text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        label,
        font,
        text_col,
    );
    resp
}

/// Two-segment pill (EQ / Stems). Returns `Some(new_state)` if a
/// segment was clicked. The active segment is filled with the deep
/// `pal.ink` colour (per the design); the inactive one stays on
/// `pal.chip`. State is `false` for the left option, `true` for the
/// right.
fn segmented_toggle(
    ui: &mut egui::Ui,
    left: &str,
    right: &str,
    state: bool,
) -> Option<bool> {
    let pal = palette::for_ui(ui);
    let font = egui::FontId::proportional(12.0);
    let pad_x = 14.0;
    let h = 22.0;
    let left_w = ui.ctx().fonts_mut(|f| f.layout_no_wrap(left.to_string(), font.clone(), pal.ink).size()).x + pad_x * 2.0;
    let right_w = ui.ctx().fonts_mut(|f| f.layout_no_wrap(right.to_string(), font.clone(), pal.ink).size()).x + pad_x * 2.0;
    let total_w = left_w + right_w;
    let (rect, _) = ui.allocate_exact_size(
        egui::Vec2::new(total_w, h),
        egui::Sense::hover(),
    );
    let p = ui.painter_at(rect);
    // Background pill.
    p.rect_filled(rect, 999.0, pal.chip);
    p.rect_stroke(
        rect,
        999.0,
        egui::Stroke::new(1.0, pal.line),
        egui::epaint::StrokeKind::Inside,
    );
    // Active segment fill.
    let active_left = !state;
    let split_x = rect.left() + left_w;
    let active_rect = if active_left {
        egui::Rect::from_min_max(rect.left_top(), egui::pos2(split_x, rect.bottom()))
    } else {
        egui::Rect::from_min_max(egui::pos2(split_x, rect.top()), rect.right_bottom())
    };
    // Slightly inset the active fill so the outer pill stroke shows.
    let active_rect = active_rect.shrink2(egui::vec2(2.0, 2.0));
    // "Deep" fill — darker than the chip background. `inset` is the
    // same colour the waveform wells use so the segmented control
    // visually nests inside its container.
    p.rect_filled(active_rect, 999.0, pal.inset);

    let mut clicked: Option<bool> = None;
    let left_rect = egui::Rect::from_min_max(rect.left_top(), egui::pos2(split_x, rect.bottom()));
    let right_rect = egui::Rect::from_min_max(egui::pos2(split_x, rect.top()), rect.right_bottom());
    let id_left = ui.id().with(("seg-left", left));
    let id_right = ui.id().with(("seg-right", right));
    let resp_l = ui.interact(left_rect, id_left, egui::Sense::click());
    let resp_r = ui.interact(right_rect, id_right, egui::Sense::click());
    if resp_l.clicked() { clicked = Some(false); }
    if resp_r.clicked() { clicked = Some(true); }

    // Active segment uses primary `ink`; inactive uses muted so the
    // active option pops without being shouty.
    let left_col = if active_left { pal.ink } else { pal.muted };
    let right_col = if active_left { pal.muted } else { pal.ink };
    p.text(
        egui::pos2(rect.left() + left_w / 2.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        left,
        font.clone(),
        left_col,
    );
    p.text(
        egui::pos2(split_x + right_w / 2.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        right,
        font,
        right_col,
    );

    clicked
}

/// Small rectangular pill for the loop strip (IN / OUT / 4 / ½ / ×2
/// / Exit / CLR). Greyed when `enabled` is false; tinted with the
/// accent when `active`. Sharper corners than `pill_toggle` so the
/// row reads as a strip of related controls.
fn loop_pill(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    enabled: bool,
    accent: egui::Color32,
) -> egui::Response {
    let pal = palette::for_ui(ui);
    let (fill, text_col) = if !enabled {
        (pal.raised, pal.faint)
    } else if active {
        (with_opacity(accent, 0.22), accent)
    } else {
        (pal.chip, pal.muted)
    };
    let stroke = if active && enabled {
        egui::Stroke::new(1.0, accent)
    } else {
        egui::Stroke::new(1.0, pal.line)
    };
    let btn = egui::Button::new(
        egui::RichText::new(label).monospace().color(text_col).small(),
    )
        .fill(fill)
        .stroke(stroke)
        .corner_radius(6.0)
        .min_size(egui::Vec2::new(28.0, 20.0));
    ui.add_enabled(enabled, btn)
}

/// Hot-cue button row — 8 slots in 2 rows × 4. Empty slot shows a
/// muted chip with the slot number; a set slot lights up with its
/// per-slot custom colour (or the deck accent as fallback) and shows
/// the slot label (if any) instead of the number. Click an empty
/// slot to set; click a set slot to jump (engine handles set-vs-jump
/// and Q quantisation). Shift-click clears. Right-click pops a
/// context menu (label / colour / delete).
fn hot_cue_row(
    ui: &mut egui::Ui,
    deck: DeckId,
    d: &mut DeckUi,
    sender: &Sender,
) -> bool {
    let mut user_touched = false;
    let pal = palette::for_ui(ui);
    let accent = match deck {
        DeckId::A => pal.accent_blue,
        DeckId::B => pal.accent_pink,
    };
    let frames = d.telemetry.hot_cue_frames();
    let shift = ui.input(|i| i.modifiers.shift);

    // Two rows of 4 buttons. The full row width minus 3 gaps split
    // four ways keeps each button square-ish at our deck width.
    let gap = 6.0;
    let avail = ui.available_width();
    let btn_w = ((avail - gap * 3.0) / 4.0).max(36.0);
    let btn_h = 28.0;

    let paint_slot = |ui: &mut egui::Ui, slot: u8, d: &mut DeckUi, user_touched: &mut bool| {
        let is_set = frames[slot as usize].is_some();
        let slot_colour = d.hot_cue_colours[slot as usize]
            .map(rgb_u32_to_col)
            .unwrap_or(accent);
        let resp = ui.allocate_response(
            egui::Vec2::new(btn_w, btn_h),
            egui::Sense::click(),
        );
        let rect = resp.rect;
        let (fill, text_col, stroke_col) = if is_set {
            (with_opacity(slot_colour, 0.28), slot_colour, slot_colour)
        } else {
            (pal.chip, pal.muted, pal.line)
        };
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 6.0, fill);
        p.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, stroke_col),
            egui::epaint::StrokeKind::Inside,
        );
        let display = if is_set {
            d.hot_cue_labels[slot as usize].clone()
                .unwrap_or_else(|| format!("{}", slot + 1))
        } else {
            format!("{}", slot + 1)
        };
        let font = if is_set {
            egui::FontId::proportional(12.0)
        } else {
            egui::FontId::proportional(12.0)
        };
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            display,
            font,
            text_col,
        );

        if resp.clicked() {
            *user_touched = true;
            if shift && is_set {
                let _ = sender.send(DeckCommand::HotCueClear { deck, slot });
                d.hot_cue_labels[slot as usize] = None;
                d.hot_cue_colours[slot as usize] = None;
                d.hot_cue_meta_dirty = true;
            } else {
                let _ = sender.send(DeckCommand::HotCueSetOrJump { deck, slot });
                if !is_set {
                    // Setting a slot — schedule a meta sync. The
                    // engine snaps to the nearest beat so we read
                    // the resolved frame from telemetry next frame.
                    d.hot_cue_meta_dirty = true;
                }
            }
        }

        // Right-click context menu — label text input, colour
        // swatches, and a delete button. Only meaningful for set
        // slots; empty ones just suppress the menu.
        //
        // Use `Popup::context_menu` directly (rather than
        // `resp.context_menu(…)`) so we can switch the close
        // behaviour to `CloseOnClickOutside`. The default
        // `CloseOnClick` dismisses the menu the instant the user
        // clicks into the TextEdit, which kills the label input.
        if is_set {
            egui::Popup::context_menu(&resp)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui: &mut egui::Ui| {
                ui.set_min_width(220.0);
                ui.label(
                    egui::RichText::new(format!("Hot cue {}", slot + 1))
                        .small()
                        .strong(),
                );
                ui.add_space(2.0);

                // ---- Label input -----------------------------------
                let current = d.hot_cue_labels[slot as usize]
                    .clone()
                    .unwrap_or_default();
                let mut buf = current.clone();
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut buf)
                        .hint_text("Label (e.g. Drop)")
                        .desired_width(f32::INFINITY),
                );
                if resp.changed() && buf != current {
                    d.hot_cue_labels[slot as usize] =
                        if buf.is_empty() { None } else { Some(buf) };
                    d.hot_cue_meta_dirty = true;
                }

                ui.add_space(4.0);
                ui.label(egui::RichText::new("Colour").small());

                // ---- Colour swatches ------------------------------
                // Six presets + a "default" swatch that reverts to
                // the palette colour. Same hex values as the design
                // labelled cues (Intro / Verse / Build / Drop).
                let swatches: [(u32, &str); 7] = [
                    (0xF7E11A, "yellow"),
                    (0xE5484D, "red"),
                    (0x22C55E, "green"),
                    (0x4AC0E7, "blue"),
                    (0xDE6778, "pink"),
                    (0xF5A623, "amber"),
                    (0, "default"),
                ];
                ui.horizontal_wrapped(|ui| {
                    for (rgb, _name) in swatches {
                        let col = if rgb == 0 { accent } else { rgb_u32_to_col(rgb) };
                        let (s_rect, s_resp) = ui.allocate_exact_size(
                            egui::Vec2::new(20.0, 20.0),
                            egui::Sense::click(),
                        );
                        let sp = ui.painter_at(s_rect);
                        sp.rect_filled(s_rect, 4.0, col);
                        // Outline the currently-selected swatch.
                        let selected = match (rgb, d.hot_cue_colours[slot as usize]) {
                            (0, None) => true,
                            (r, Some(v)) if r == v => true,
                            _ => false,
                        };
                        if selected {
                            sp.rect_stroke(
                                s_rect, 4.0,
                                egui::Stroke::new(2.0, pal.ink),
                                egui::epaint::StrokeKind::Outside,
                            );
                        }
                        if s_resp.clicked() {
                            d.hot_cue_colours[slot as usize] =
                                if rgb == 0 { None } else { Some(rgb) };
                            d.hot_cue_meta_dirty = true;
                        }
                    }
                });

                ui.add_space(4.0);
                ui.separator();
                if ui.button("Delete cue").clicked() {
                    let _ = sender.send(DeckCommand::HotCueClear { deck, slot });
                    d.hot_cue_labels[slot as usize] = None;
                    d.hot_cue_colours[slot as usize] = None;
                    d.hot_cue_meta_dirty = true;
                    ui.close();
                }
                });
        }
    };

    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for slot in 0..4u8 { paint_slot(ui, slot, d, &mut user_touched); }
        });
        ui.add_space(gap);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for slot in 4..8u8 { paint_slot(ui, slot, d, &mut user_touched); }
        });
    });

    user_touched
}

#[inline]
fn rgb_u32_to_col(rgb: u32) -> egui::Color32 {
    egui::Color32::from_rgb(
        ((rgb >> 16) & 0xFF) as u8,
        ((rgb >> 8) & 0xFF) as u8,
        (rgb & 0xFF) as u8,
    )
}

/// One-line deck identity bar that sits above (Deck A) or below
/// (Deck B) the waveform stack. Matches the design handoff:
///   [A] Title · Artist (left)        BPM · Key · 02:14 / -01:34 (right)
/// The badge is a rounded square in the deck's accent colour
/// (blue for A, pink for B) with the letter painted in ink.
fn deck_info_row(ui: &mut egui::Ui, deck: DeckId, d: &DeckUi) {
    let pal = palette::for_ui(ui);
    let (badge_letter, badge_bg) = match deck {
        DeckId::A => ("A", pal.accent_blue),
        DeckId::B => ("B", pal.accent_pink),
    };
    ui.horizontal(|ui| {
        // Deck badge: 22 px filled square with the letter in ink.
        let (rect, _resp) = ui.allocate_exact_size(
            egui::Vec2::new(22.0, 22.0),
            egui::Sense::hover(),
        );
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 4.0, badge_bg);
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            badge_letter,
            egui::FontId::proportional(13.0),
            pal.ink,
        );

        // Blank state — show "(no track)" muted and stop. Avoids
        // the cluttered "--:-- / --:-- · -- · --  BPM" row.
        if d.loaded_path.is_none() {
            ui.colored_label(pal.faint, "(no track)");
            return;
        }

        // Title + artist (truncate on overflow).
        let title = d.title.as_deref().unwrap_or("");
        ui.label(egui::RichText::new(title).strong());
        // The TrackMeta artist isn't carried into DeckUi today —
        // best-effort parse from the title via the same helper the
        // library uses; produces empty string for already-clean titles.
        let (artist_parse, _title_parse) = parse_track_name(title);
        if !artist_parse.is_empty() {
            ui.colored_label(pal.muted, "·");
            ui.colored_label(pal.muted, artist_parse);
        }

        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                // Time mono — elapsed / remaining.
                let total = if d.sample_rate > 0 {
                    d.total_frames as f64 / d.sample_rate as f64
                } else { 0.0 };
                let pos = d.playhead_secs().min(total);
                let remaining = (total - pos).max(0.0);
                if total > 0.0 {
                    // played / -remaining · total — gives the DJ all
                    // three at once: where you are, time until end,
                    // and full length of the cue.
                    ui.label(mono(format!(
                        "{} / -{} · {}",
                        fmt_mmss(pos),
                        fmt_mmss(remaining),
                        fmt_mmss(total),
                    )));
                    ui.label("·");
                }
                // Key chip — accent-sky text, per design.
                if let Some(k) = d.key {
                    ui.colored_label(pal.accent_sky, mono(k.label()));
                    ui.label("·");
                }
                // BPM mono — show effective BPM when nudged.
                if d.bpm > 0.0 {
                    let speed = d.telemetry.current_speed();
                    ui.label(mono(format!("{:.1}  BPM", d.bpm * speed)));
                }
            },
        );
    });
}

/// Wrap a string as monospace RichText. Used for every numeric in
/// the UI (BPM, time, key, knob values, beat chips, play count)
/// so numbers line up cleanly column-to-column and don't wobble as
/// digits change. Resolves to JetBrains Mono (bundled via
/// `crate::fonts`); falls back to egui's default monospace if the
/// font registration failed for any reason.
fn mono(s: impl Into<String>) -> egui::RichText {
    egui::RichText::new(s).monospace()
}

/// Per-deck FX module. Layout (per the design handoff):
///   header row: "FX" pink overline · effect dropdown · ON pill
///   body:       Colour rotary | beat picker | Mix rotary
/// The dropdown currently has one entry (Echo) but is shaped so
/// adding Reverb / Filter is a Vec push, not a layout rewrite.
/// Returns true if the user interacted with any control (caller
/// uses this to abort an in-flight auto-mix blend).
#[must_use]
fn fx_module(
    ui: &mut egui::Ui,
    deck: DeckId,
    d: &mut DeckUi,
    sender: &Sender,
) -> bool {
    use egui::{Color32, Stroke};
    let mut touched = false;
    let pal = palette::for_ui(ui);
    // Frame the module ourselves rather than using Frame::group so
    // the pink top stripe can sit ABOVE the frame's stroke instead
    // of underneath it. Approach:
    //   1. Reserve the full rectangle we want to occupy.
    //   2. Paint background + pink top stripe.
    //   3. Run the body via `child_ui` inside the reserved rect.
    //   4. Paint the surrounding border AFTER the body so the
    //      stripe stays visible.
    let frame = egui::Frame::new()
        .fill(pal.raised)
        .inner_margin(egui::Margin {
            left: 10, right: 10, top: 6, bottom: 8,
        })
        .corner_radius(10.0);
    let resp = frame.show(ui, |ui| {
        // Header row.
        ui.horizontal(|ui| {
            ui.colored_label(pal.accent_pink, "FX");
            ui.separator();
            // Single-entry dropdown for now — the architecture is
            // ready for Reverb / Filter; the engine and command
            // surface will just need each new effect added.
            let deck_tag = match deck { DeckId::A => "a", DeckId::B => "b" };
            let selected_text = match d.fx_kind {
                control::FxKindId::Echo => "Echo",
                control::FxKindId::Reverb => "Reverb",
            };
            egui::ComboBox::from_id_salt(format!("fx-effect-{deck_tag}"))
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for (kind, label) in [
                        (control::FxKindId::Echo, "Echo"),
                        (control::FxKindId::Reverb, "Reverb"),
                    ] {
                        let sel = d.fx_kind == kind;
                        if ui.selectable_label(sel, label).clicked() && !sel {
                            d.fx_kind = kind;
                            touched = true;
                            let _ = sender.send(DeckCommand::SetFxKind { deck, kind });
                        }
                    }
                });
            ui.separator();
            // ON pill — green when active, chip-bg when bypassed.
            let label = if d.fx_on { "● ON" } else { "○ OFF" };
            let was_on = d.fx_on;
            if ui.selectable_label(d.fx_on, label).clicked() {
                d.fx_on = !d.fx_on;
                touched = true;
                let _ = sender.send(DeckCommand::SetFxOn { deck, on: d.fx_on });
            }
            // Optional visual nudge so the user knows the click landed
            // even if there's no immediate audible change (e.g. ON
            // with mix=0).
            let _ = (was_on, Color32::WHITE);
        });

        ui.add_space(4.0);

        const KNOB_DIA: f32 = 48.0;
        ui.horizontal(|ui| {
            // Colour rotary — always present (FX-identity pink).
            if let Some(v) =
                knob_colored(ui, "COLOUR", d.fx_colour, 0.0..=1.0, KNOB_DIA, pal.accent_pink)
            {
                d.fx_colour = v;
                touched = true;
                let _ = sender.send(DeckCommand::SetFxColour { deck, value: v });
            }
            ui.add_space(8.0);
            match d.fx_kind {
                // Type B — Echo: beat picker swaps in where the Time
                // knob would otherwise sit.
                control::FxKindId::Echo => {
                    ui.vertical(|ui| {
                        ui.colored_label(pal.faint, "TIME · BEATS");
                        // 2×2 grid: ¼ ½ on top, 1 2 below — same
                        // footprint as a knob column.
                        let beats_pair = |ui: &mut egui::Ui,
                                          d: &mut DeckUi,
                                          touched: &mut bool,
                                          values: [(f32, &str); 2]| {
                            ui.horizontal(|ui| {
                                for (b, label) in values {
                                    let sel = (d.fx_beats - b).abs() < f32::EPSILON;
                                    if ui.selectable_label(sel, label).clicked() && !sel {
                                        d.fx_beats = b;
                                        *touched = true;
                                        let _ = sender.send(DeckCommand::SetFxBeats { deck, beats: b });
                                    }
                                }
                            });
                        };
                        beats_pair(ui, d, &mut touched, [(0.25, "¼"), (0.5, "½")]);
                        beats_pair(ui, d, &mut touched, [(1.0, "1"), (2.0, "2")]);
                    });
                }
                // Type A — Reverb: continuous Time rotary (sky-blue,
                // per the design tokens).
                control::FxKindId::Reverb => {
                    if let Some(v) = knob_colored(
                        ui, "TIME", d.fx_time, 0.0..=1.0, KNOB_DIA, pal.accent_sky,
                    ) {
                        d.fx_time = v;
                        touched = true;
                        let _ = sender.send(DeckCommand::SetFxTime { deck, value: v });
                    }
                }
            }
            ui.add_space(8.0);
            // Mix rotary — always present (blue).
            if let Some(v) =
                knob_colored(ui, "MIX", d.fx_mix, 0.0..=1.0, KNOB_DIA, pal.accent_blue)
            {
                d.fx_mix = v;
                touched = true;
                let _ = sender.send(DeckCommand::SetFxMix { deck, value: v });
            }
        });
    });
    // Just the soft palette.line border — the design's pink top
    // stripe read as a stray line in our render and the user asked
    // to drop it. FX identity comes from the pink "FX" label and
    // the pink "Colour" knob instead.
    let outer = resp.response.rect;
    let painter = ui.painter();
    painter.rect_stroke(
        outer,
        10.0,
        Stroke::new(1.0, pal.line),
        egui::StrokeKind::Inside,
    );
    touched
}

/// Draws a big round arcade-style button. Returns true when clicked
/// (release inside the bounds, like egui's normal Button).
fn arcade_button(ui: &mut egui::Ui, label: &str, diameter: f32, base: Color32) -> bool {
    let (resp, _down) = arcade_button_inner(ui, label, diameter, base, ArcadeStyle::Solid);
    resp.clicked()
}

#[derive(Clone, Copy)]
enum ArcadeStyle {
    Solid,
    Outline,
}

/// Variant that also reports the "pointer is currently down" state,
/// needed for press-and-hold semantics (CUE preview).
fn arcade_button_held(ui: &mut egui::Ui, label: &str, diameter: f32, base: Color32) -> (bool, bool) {
    let (resp, down) = arcade_button_inner(ui, label, diameter, base, ArcadeStyle::Outline);
    (resp.clicked(), down)
}

fn arcade_button_inner(
    ui: &mut egui::Ui,
    label: &str,
    diameter: f32,
    base: Color32,
    style: ArcadeStyle,
) -> (egui::Response, bool) {
    let size = Vec2::splat(diameter);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let down = resp.is_pointer_button_down_on();
    let hovered = resp.hovered();
    let painter = ui.painter();
    let centre = rect.center();
    let r = diameter * 0.5;
    let pal = palette::for_ui(ui);
    match style {
        ArcadeStyle::Solid => {
            // Slight brightness boost on hover, big brightness boost when held.
            let face = if down {
                base
            } else if hovered {
                Color32::from_rgb(
                    (base.r() as u16 * 7 / 8 + 32).min(255) as u8,
                    (base.g() as u16 * 7 / 8 + 32).min(255) as u8,
                    (base.b() as u16 * 7 / 8 + 32).min(255) as u8,
                )
            } else {
                Color32::from_rgb(
                    (base.r() as u16 * 5 / 8) as u8,
                    (base.g() as u16 * 5 / 8) as u8,
                    (base.b() as u16 * 5 / 8) as u8,
                )
            };
            painter.circle_filled(centre, r, face);
            // Subtle inner highlight for a tactile "domed" look.
            painter.circle_stroke(
                centre,
                r - 2.0,
                Stroke::new(1.5, Color32::from_rgba_premultiplied(255, 255, 255, 40)),
            );
            painter.text(
                centre,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(diameter * 0.36),
                Color32::WHITE,
            );
        }
        ArcadeStyle::Outline => {
            // Hollow ring in the accent colour; fills faintly on
            // hover, fills more saturated when held. Label text uses
            // the accent colour so it pops on the panel background.
            let fill = if down {
                with_opacity(base, 0.25)
            } else if hovered {
                with_opacity(base, 0.10)
            } else {
                Color32::TRANSPARENT
            };
            if fill != Color32::TRANSPARENT {
                painter.circle_filled(centre, r - 1.5, fill);
            }
            painter.circle_stroke(centre, r - 1.5, Stroke::new(2.5, base));
            // Slightly smaller text for outline buttons — the 2.5 px
            // ring eats into the disc area, so the same 0.36-of-D
            // size as the solid variant clips on the inside edge.
            painter.text(
                centre,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(diameter * 0.28),
                if down { pal.ink } else { base },
            );
        }
    }
    (resp, down)
}

/// Rotary knob — circular dial with an indicator line at the current
/// angle. Drag up to increase, down to decrease (150 px covers the full
/// range). Double-click resets to 0 dB if the range straddles zero, else
/// to the midpoint. Returns `Some(new)` when the value changed.
fn knob(
    ui: &mut egui::Ui,
    label: &str,
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    diameter: f32,
) -> Option<f32> {
    let accent = palette::for_ui(ui).accent_blue;
    knob_colored(ui, label, value, range, diameter, accent)
}

fn knob_colored(
    ui: &mut egui::Ui,
    label: &str,
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    diameter: f32,
    base_color: Color32,
) -> Option<f32> {
    let label_h = 14.0;
    let value_h = 14.0;
    let pad_x = 4.0;
    // Arc track sits 3 px outside the disc radius with a 2 px stroke,
    // so the visible bounds are radius + 5 in each direction. Pad
    // top + bottom by `arc_pad` so the value/label rows don't
    // collide with the arc.
    let arc_pad = 6.0;
    let total = Vec2::new(
        diameter + pad_x * 2.0,
        label_h + arc_pad + diameter + arc_pad + value_h,
    );
    let (rect, response) = ui.allocate_exact_size(total, Sense::click_and_drag());

    let painter = ui.painter_at(rect);
    let lo = *range.start();
    let hi = *range.end();
    // "Neutral" is the value that should sit at 12 o'clock on the dial.
    //  - lo < 0 < hi (EQ -25..+6 dB):       neutral = 0 (= 0 dB)
    //  - lo == 0 && hi > 1 (gain 0..1.5):   neutral = 1 (= unity)
    //  - everything else (linear, no clear neutral): geometric midpoint.
    let neutral = if lo < 0.0 && hi > 0.0 {
        0.0
    } else if lo == 0.0 && hi > 1.0 {
        1.0
    } else {
        (lo + hi) * 0.5
    };

    painter.text(
        Pos2::new(rect.center().x, rect.top() + label_h * 0.5),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::default(),
        ui.visuals().weak_text_color(),
    );

    let center = Pos2::new(
        rect.center().x,
        rect.top() + label_h + arc_pad + diameter * 0.5,
    );
    let radius = diameter * 0.5 - 1.0;
    let pal = palette::for_ui(ui);
    // Filled face (no stroke ring — the track arc below replaces it).
    painter.circle_filled(center, radius, pal.knob_face);
    // 270° track arc starting at lower-left (-135°), sweeping
    // clockwise to lower-right (+135°). Drawn as a polyline at a
    // radius outside the face so the value-arc on top reads
    // clearly. Matches the design's knob spec.
    let track_r = radius + 3.0;
    let n_seg: usize = 36;
    let mut track_pts = Vec::with_capacity(n_seg + 1);
    for i in 0..=n_seg {
        let t = i as f32 / n_seg as f32; // 0 → 1
        let theta = (-135.0_f32 + t * 270.0).to_radians();
        track_pts.push(center + Vec2::new(theta.sin() * track_r, -theta.cos() * track_r));
    }
    painter.add(egui::Shape::line(track_pts, Stroke::new(2.0, pal.knob_track)));

    // -135° (min) → +135° (max), measured clockwise from 12 o'clock.
    // Piecewise scaling: `lo..neutral` fills the first half of travel,
    // `neutral..hi` fills the second. EQ knobs show 0 dB at 12 o'clock,
    // stem knobs show unity (1.0) at 12 o'clock. When neutral is the
    // geometric midpoint this collapses to a linear mapping.
    let mid = (lo + hi) * 0.5;
    let frac = if (neutral - mid).abs() > f32::EPSILON {
        if value <= neutral {
            let span = (neutral - lo).max(f32::EPSILON);
            0.5 * ((value - lo) / span).clamp(0.0, 1.0)
        } else {
            let span = (hi - neutral).max(f32::EPSILON);
            0.5 + 0.5 * ((value - neutral) / span).clamp(0.0, 1.0)
        }
    } else {
        ((value - lo) / (hi - lo)).clamp(0.0, 1.0)
    };
    let theta = (-135.0_f32 + frac * 270.0).to_radians();
    let tip = center
        + Vec2::new(theta.sin() * radius * 0.85, -theta.cos() * radius * 0.85);
    let highlight = response.hovered() || response.dragged();
    // Brighten the base colour by ~25 % on hover/drag so the user sees
    // the active knob without changing its identity.
    let line_color = if highlight {
        Color32::from_rgb(
            (base_color.r() as u16 + 60).min(255) as u8,
            (base_color.g() as u16 + 60).min(255) as u8,
            (base_color.b() as u16 + 60).min(255) as u8,
        )
    } else {
        base_color
    };
    // Value arc: from start of track (lower-left, -135°) clockwise
    // up to the current value. Drawn AFTER the track in the same
    // radius so it overlays it visually.
    let mut value_pts = Vec::with_capacity(n_seg + 1);
    let value_end_t = frac;
    let n_value = (n_seg as f32 * value_end_t).ceil() as usize;
    for i in 0..=n_value {
        let t = (i as f32 / n_seg as f32).min(value_end_t);
        let theta_p = (-135.0_f32 + t * 270.0).to_radians();
        value_pts.push(center + Vec2::new(theta_p.sin() * track_r, -theta_p.cos() * track_r));
    }
    if value_pts.len() >= 2 {
        painter.add(egui::Shape::line(value_pts, Stroke::new(2.0, line_color)));
    }
    painter.line_segment([center, tip], Stroke::new(2.5, line_color));

    painter.text(
        Pos2::new(rect.center().x, rect.bottom() - value_h * 0.5),
        egui::Align2::CENTER_CENTER,
        format!("{:.1}", value),
        egui::FontId::monospace(11.0),
        ui.visuals().text_color(),
    );

    let mut new = None;
    if response.dragged() {
        let dy = -response.drag_delta().y;
        if dy != 0.0 {
            let v = (value + (dy / 150.0) * (hi - lo)).clamp(lo, hi);
            if (v - value).abs() > f32::EPSILON {
                new = Some(v);
            }
        }
    }
    if response.double_clicked() && (value - neutral).abs() > f32::EPSILON {
        new = Some(neutral);
    }
    new
}

/// Background colour for both waveform views. Pulls from the
/// palette's `inset` token so it tracks dark / light mode without
/// each call site repeating the check.
fn waveform_bg(ui: &egui::Ui) -> Color32 {
    palette::for_ui(ui).inset
}

fn overview_waveform(ui: &mut egui::Ui, d: &DeckUi, deck: DeckId, sender: &Sender) {
    let desired = Vec2::new(ui.available_width(), 60.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, waveform_bg(ui));

    // Click-to-seek: clicking anywhere on the overview jumps the playhead
    // to that fraction of the track.
    if resp.clicked() && d.total_frames > 0 {
        if let Some(pos) = resp.interact_pointer_pos() {
            let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let sample_pos = (frac as f64 * d.total_frames as f64) as u64;
            let _ = sender.send(DeckCommand::Seek { deck, sample_pos });
        }
    }

    if d.overview.is_empty() || d.total_frames == 0 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no track",
            egui::FontId::default(),
            palette::for_ui(ui).faint,
        );
        return;
    }

    let w = rect.width();
    let h = rect.height();
    let mid = rect.center().y;
    let cols = w.ceil() as usize;
    let pal = palette::for_ui(ui);
    // Played-vs-upcoming alpha split — full opacity left of the
    // playhead, dimmed right of it. Per the design handoff: makes
    // the visited region pop, but the whole track stays visible
    // edge-to-edge.
    let head_frac = (d.telemetry.playhead_frames() as f32
        / d.total_frames.max(1) as f32)
        .clamp(0.0, 1.0);
    let head_col = (head_frac * cols as f32) as usize;
    let stems_ready = !d.stem_overview_drums.is_empty()
        && !d.stem_overview_vocals.is_empty()
        && !d.stem_overview_instr.is_empty();
    if stems_ready {
        // Three translucent stem traces overlaid on each other —
        // drums (red), vocals (green), instruments (blue). Where
        // they peak together the colours mix; where one dominates
        // its colour pops. Reverted from the stacked layout; the
        // overlay reads better in motion.
        for (peaks, base_col) in [
            (&d.stem_overview_drums, pal.stem_drums),
            (&d.stem_overview_vocals, pal.stem_vocals),
            (&d.stem_overview_instr, pal.stem_instruments),
        ] {
            let n = peaks.len();
            if n == 0 { continue; }
            for x in 0..cols {
                let t = x as f32 / cols.max(1) as f32;
                let bucket = ((t * n as f32) as usize).min(n - 1);
                let peak = peaks[bucket].min(1.0);
                let half = peak * (h * 0.5);
                let x_px = rect.left() + x as f32;
                // ~55% base alpha so three overlapping stems
                // average to something readable; dimmed further on
                // the upcoming side.
                let layer_alpha = if x < head_col { 0.55 } else { 0.22 };
                let stroke = Stroke::new(1.0, with_opacity(base_col, layer_alpha));
                painter.line_segment(
                    [Pos2::new(x_px, mid - half), Pos2::new(x_px, mid + half)],
                    stroke,
                );
            }
        }
    } else {
        let n = d.overview.len();
        for x in 0..cols {
            let t = x as f32 / cols.max(1) as f32;
            let bucket = (t * n as f32) as usize;
            if bucket >= n {
                break;
            }
            let peak = d.overview[bucket].min(1.0);
            let half = peak * (h * 0.5);
            let x_px = rect.left() + x as f32;
            let alpha = if x < head_col { 1.0 } else { 0.4 };
            let stroke = Stroke::new(1.0, with_opacity(pal.accent_blue, alpha));
            painter.line_segment(
                [Pos2::new(x_px, mid - half), Pos2::new(x_px, mid + half)],
                stroke,
            );
        }
    }

    // Zoom-window box: a translucent strip on the overview marking
    // the region the zoom view is currently displaying. Lets the
    // user see at a glance where in the track they are.
    if d.sample_rate > 0 && d.bpm > 0.0 && d.total_frames > 0 {
        let window_secs = ZOOM_BEATS * 60.0 / d.bpm as f64;
        let track_secs = d.total_frames as f64 / d.sample_rate as f64;
        let view_start = d.playhead_secs() - window_secs * ZOOM_PLAYHEAD_FRAC as f64;
        let view_end = view_start + window_secs;
        let f0 = (view_start / track_secs).clamp(0.0, 1.0) as f32;
        let f1 = (view_end / track_secs).clamp(0.0, 1.0) as f32;
        if f1 > f0 {
            let region = egui::Rect::from_min_max(
                Pos2::new(rect.left() + f0 * w, rect.top()),
                Pos2::new(rect.left() + f1 * w, rect.bottom()),
            );
            painter.rect_filled(
                region, 0.0,
                with_opacity(pal.accent_blue, 0.12),
            );
            painter.rect_stroke(
                region, 0.0,
                Stroke::new(1.0, with_opacity(pal.accent_blue, 0.6)),
                egui::StrokeKind::Inside,
            );
        }
    }

    // Loop region overlay. Three states matching the zoom view:
    //   - IN + OUT  → bright committed-loop fill.
    //   - IN only   → dim "recording" fill from IN to the playhead.
    //   - neither   → nothing.
    let loop_in = d.telemetry.loop_in_frame();
    let loop_range = d.telemetry.loop_range();
    if let Some((in_f, out_f)) = loop_range {
        let in_frac = (in_f as f32 / d.total_frames.max(1) as f32).clamp(0.0, 1.0);
        let out_frac = (out_f as f32 / d.total_frames.max(1) as f32).clamp(0.0, 1.0);
        if out_frac > in_frac {
            let x0 = rect.left() + in_frac * w;
            let x1 = (rect.left() + out_frac * w).max(x0 + 1.0);
            let region = egui::Rect::from_min_max(
                Pos2::new(x0, rect.top()),
                Pos2::new(x1, rect.bottom()),
            );
            painter.rect_filled(
                region, 0.0,
                with_opacity(pal.accent_amber, 0.35),
            );
        }
    } else if let Some(in_f) = loop_in {
        let in_frac = (in_f as f32 / d.total_frames.max(1) as f32).clamp(0.0, 1.0);
        let head_frac = (d.telemetry.playhead_frames() as f32 / d.total_frames.max(1) as f32).clamp(0.0, 1.0);
        if head_frac > in_frac {
            let x0 = rect.left() + in_frac * w;
            let x1 = (rect.left() + head_frac * w).max(x0 + 1.0);
            let region = egui::Rect::from_min_max(
                Pos2::new(x0, rect.top()),
                Pos2::new(x1, rect.bottom()),
            );
            painter.rect_filled(
                region, 0.0,
                with_opacity(pal.accent_amber, 0.22),
            );
        }
    }
    if let Some(in_f) = loop_in {
        let frac = (in_f as f32 / d.total_frames.max(1) as f32).clamp(0.0, 1.0);
        let x = rect.left() + frac * w;
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(1.5, pal.accent_amber),
        );
    }

    // Hot cue ticks — thick verticals at each set slot, painted in
    // the slot's custom colour (falls back to `pal.hot_cue`). Drawn
    // before the playhead so the moving line stays on top, but
    // thicker than the beat grid so they don't get lost in it.
    for (slot, slot_frame) in d.telemetry.hot_cue_frames().iter().enumerate() {
        let Some(slot_frame) = slot_frame else { continue };
        let frac = (*slot_frame as f32 / d.total_frames.max(1) as f32).clamp(0.0, 1.0);
        let x = rect.left() + frac * w;
        let col = d.hot_cue_colours[slot]
            .map(rgb_u32_to_col)
            .unwrap_or(pal.hot_cue);
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(2.5, col),
        );
    }

    let head_frac = d.telemetry.playhead_frames() as f32 / d.total_frames.max(1) as f32;
    let head_x = rect.left() + head_frac.clamp(0.0, 1.0) * w;
    painter.line_segment(
        [
            Pos2::new(head_x, rect.top()),
            Pos2::new(head_x, rect.bottom()),
        ],
        Stroke::new(1.5, pal.accent_amber),
    );
}

/// Scrolling zoom view: ZOOM_BEATS-wide window around the playhead.
/// Beat grid drawn as vertical lines (every 4th brighter as a presumed
/// downbeat — real downbeat detection is v1.5).
fn zoom_view(ui: &mut egui::Ui, d: &DeckUi, deck: DeckId, sender: &Sender) {
    let desired = Vec2::new(ui.available_width(), 90.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, waveform_bg(ui));

    if d.sample_rate == 0 || d.total_frames == 0 {
        return;
    }

    // Window time range = ZOOM_BEATS / (BPM/60) seconds, or fallback to a
    // fixed 8-second window if BPM is unknown.
    let window_secs = if d.bpm > 0.0 {
        ZOOM_BEATS * 60.0 / d.bpm as f64
    } else {
        8.0
    };
    let playhead_t = d.playhead_secs();
    let view_start = playhead_t - window_secs * ZOOM_PLAYHEAD_FRAC as f64;
    let view_end = view_start + window_secs;

    let w = rect.width();
    let h = rect.height();
    let mid = rect.center().y;

    // Map time → x pixel
    let t_to_x = |t: f64| -> f32 {
        let frac = ((t - view_start) / window_secs) as f32;
        rect.left() + frac.clamp(-0.1, 1.1) * w
    };

    // Waveform from hi-res peaks. Map each pixel column to the actual time
    // it represents (view_start + frac * window_secs) and skip columns
    // outside the track. This keeps the waveform aligned with the playhead
    // marker and beat grid even when view_start is negative (i.e. the start
    // of the track is partially in view during the first ~ZOOM_PLAYHEAD_FRAC
    // * window_secs seconds of playback) or view_end exceeds track length.
    //
    // Guard against the "grid moves but waveform disappears" bug: it shows
    // up when d.hires/d.samples_per_hires get out of sync with the rest of
    // the deck state (e.g., a stale event resets one but not the others).
    // Compute peaks_per_sec defensively and skip drawing — with a visible
    // marker + one-shot log — if anything looks wrong, so the user knows
    // it's a bug rather than thinking the track has gone silent.
    if d.sample_rate > 0 && d.total_frames > 0 {
        let track_secs = d.total_frames as f64 / d.sample_rate as f64;
        let cols = w.ceil() as usize;
        if d.hires.is_empty() || d.samples_per_hires == 0 {
            // Track is "loaded" enough to have a beat grid but the
            // peaks data is missing. Render a hairline placeholder so
            // the zoom view doesn't look frozen.
            let pal = palette::for_ui(ui);
            painter.line_segment(
                [Pos2::new(rect.left(), mid), Pos2::new(rect.right(), mid)],
                Stroke::new(1.0, pal.line_strong),
            );
            painter.text(
                Pos2::new(rect.left() + 8.0, rect.top() + 8.0),
                egui::Align2::LEFT_TOP,
                "(no waveform data — track loaded?)",
                egui::FontId::proportional(11.0),
                pal.muted,
            );
            log_waveform_anomaly(deck, d);
        } else {
            let peaks_per_sec = d.sample_rate as f64 / d.samples_per_hires as f64;
            let pal = palette::for_ui(ui);
            let stems_ready = !d.stem_hires_drums.is_empty()
                && !d.stem_hires_vocals.is_empty()
                && !d.stem_hires_instr.is_empty();
            // Playhead pixel — everything left of this is "played"
            // (full opacity), right is "upcoming" (dim).
            let head_x = t_to_x(d.playhead_secs());
            if stems_ready {
                // Three translucent stem traces overlaid — drums
                // (red), vocals (green), instruments (blue).
                // Reverted from stacked. Reading the column-peak
                // for each stem keeps the rendering loop the same
                // as before, just iterated per-stem.
                for (peaks, base_col) in [
                    (&d.stem_hires_drums, pal.stem_drums),
                    (&d.stem_hires_vocals, pal.stem_vocals),
                    (&d.stem_hires_instr, pal.stem_instruments),
                ] {
                    let n = peaks.len();
                    if n == 0 { continue; }
                    for x in 0..cols {
                        let frac0 = x as f64 / cols.max(1) as f64;
                        let frac1 = (x + 1) as f64 / cols.max(1) as f64;
                        let t0 = view_start + frac0 * window_secs;
                        let t1 = view_start + frac1 * window_secs;
                        if t1 <= 0.0 || t0 >= track_secs { continue; }
                        let t0c = t0.max(0.0);
                        let t1c = t1.min(track_secs);
                        let p0 = (t0c * peaks_per_sec) as usize;
                        let p1 = ((t1c * peaks_per_sec) as usize).min(n);
                        if p0 >= p1 { continue; }
                        let mut peak = 0.0_f32;
                        for p in p0..p1 {
                            if peaks[p] > peak { peak = peaks[p]; }
                        }
                        let half = peak.min(1.0) * (h * 0.45);
                        let x_px = rect.left() + x as f32;
                        let layer_alpha = if x_px < head_x { 0.55 } else { 0.22 };
                        let stroke = Stroke::new(1.0, with_opacity(base_col, layer_alpha));
                        painter.line_segment(
                            [Pos2::new(x_px, mid - half), Pos2::new(x_px, mid + half)],
                            stroke,
                        );
                    }
                }
            } else {
                // Single-stream fallback (no stems yet) — keep the
                // simple peak-bar rendering. Alpha-split still applies.
                let n = d.hires.len();
                for x in 0..cols {
                    let frac0 = x as f64 / cols.max(1) as f64;
                    let frac1 = (x + 1) as f64 / cols.max(1) as f64;
                    let t0 = view_start + frac0 * window_secs;
                    let t1 = view_start + frac1 * window_secs;
                    if t1 <= 0.0 || t0 >= track_secs { continue; }
                    let t0c = t0.max(0.0);
                    let t1c = t1.min(track_secs);
                    let p0 = (t0c * peaks_per_sec) as usize;
                    let p1 = ((t1c * peaks_per_sec) as usize).min(n);
                    if p0 >= p1 { continue; }
                    let mut peak = 0.0_f32;
                    for p in p0..p1 {
                        if d.hires[p] > peak { peak = d.hires[p]; }
                    }
                    let half = peak.min(1.0) * (h * 0.45);
                    let x_px = rect.left() + x as f32;
                    let alpha = if x_px < head_x { 1.0 } else { 0.4 };
                    painter.line_segment(
                        [Pos2::new(x_px, mid - half), Pos2::new(x_px, mid + half)],
                        Stroke::new(1.0, with_opacity(pal.accent_blue, alpha)),
                    );
                }
            }
        }
    }

    // Loop region highlight + bound markers. Painted *behind* the
    // beat grid so the grid stays readable. Three states:
    //  - IN only          → dim "recording" fill from IN to the
    //                       current playhead (grows in real time so
    //                       you can see how long the loop will be).
    //  - IN + OUT         → brighter committed-loop fill across the
    //                       full span + bar at OUT.
    //  - neither          → nothing.
    let pal = palette::for_ui(ui);
    if d.sample_rate > 0 {
        let in_t = d.telemetry.loop_in_frame().map(|f| f as f64 / d.sample_rate as f64);
        let range = d.telemetry.loop_range();
        if let Some((in_f, out_f)) = range {
            // Committed loop — both bounds set.
            let in_t = in_f as f64 / d.sample_rate as f64;
            let out_t = out_f as f64 / d.sample_rate as f64;
            if out_t > view_start && in_t < view_end {
                let x0 = t_to_x(in_t.max(view_start));
                let x1 = t_to_x(out_t.min(view_end));
                if x1 > x0 {
                    let region = egui::Rect::from_min_max(
                        Pos2::new(x0, rect.top()),
                        Pos2::new(x1, rect.bottom()),
                    );
                    painter.rect_filled(
                        region, 0.0,
                        with_opacity(pal.accent_amber, 0.27),
                    );
                }
                if out_t >= view_start && out_t <= view_end {
                    let x = t_to_x(out_t);
                    painter.line_segment(
                        [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                        Stroke::new(2.0, pal.accent_amber),
                    );
                }
            }
        } else if let Some(in_t) = in_t {
            // "Recording" state — IN set, OUT pending. Fill from IN to
            // the playhead so the user sees the candidate loop length
            // grow in real time. Dimmer + cooler tint than the
            // committed loop so the two states are distinct.
            let end_t = playhead_t.max(in_t);
            if end_t > view_start && in_t < view_end {
                let x0 = t_to_x(in_t.max(view_start));
                let x1 = t_to_x(end_t.min(view_end));
                if x1 > x0 {
                    let region = egui::Rect::from_min_max(
                        Pos2::new(x0, rect.top()),
                        Pos2::new(x1, rect.bottom()),
                    );
                    painter.rect_filled(
                        region, 0.0,
                        with_opacity(pal.accent_amber, 0.19),
                    );
                }
            }
        }
        // IN marker — drawn in all states so the start of the loop is
        // always clearly visible.
        if let Some(in_t) = in_t {
            if in_t >= view_start && in_t <= view_end {
                let x = t_to_x(in_t);
                painter.line_segment(
                    [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                    Stroke::new(2.0, pal.accent_amber),
                );
            }
        }
    }

    // Beat grid lines.
    if !d.beat_grid.is_empty() {
        // Find first beat at or after view_start.
        let first_idx = match d
            .beat_grid
            .binary_search_by(|b| b.partial_cmp(&view_start).unwrap_or(std::cmp::Ordering::Equal))
        {
            Ok(i) => i,
            Err(i) => i,
        };
        for i in first_idx..d.beat_grid.len() {
            let t = d.beat_grid[i];
            if t > view_end {
                break;
            }
            let x = t_to_x(t);
            // Use model-derived downbeats when present (v2 cache);
            // fall back to "every 4th beat" for pre-v2 entries that
            // haven't been re-analysed yet. Every fourth downbeat
            // (16 beats / 4 bars) is rendered red — that's a natural
            // phrase boundary in dance music, where most DJs aim to
            // start, transition, or drop.
            let (is_downbeat, is_mix_point) = if d.downbeats.is_empty() {
                (i % 4 == 0, i % 16 == 0)
            } else {
                match d.downbeats.binary_search(&(i as u32)) {
                    Ok(j) => (true, j % 4 == 0),
                    Err(_) => (false, false),
                }
            };
            let pal = palette::for_ui(ui);
            let (col, stroke_w) = if is_mix_point {
                (pal.accent_red, 2.0)
            } else if is_downbeat {
                (pal.ink, 1.5)
            } else {
                (pal.line_strong, 0.8)
            };
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(stroke_w, col),
            );
        }
    }

    // Hot cue ticks — thick verticals at each set slot inside the
    // visible window, painted in the slot's custom colour (falls
    // back to `pal.hot_cue`). Thicker than the beat-grid lines so
    // they don't get lost.
    if d.sample_rate > 0 {
        for (slot, slot_frame) in d.telemetry.hot_cue_frames().iter().enumerate() {
            let Some(slot_frame) = slot_frame else { continue };
            let t = *slot_frame as f64 / d.sample_rate as f64;
            if t < view_start || t > view_end { continue; }
            let x = t_to_x(t);
            let col = d.hot_cue_colours[slot]
                .map(rgb_u32_to_col)
                .unwrap_or(pal.hot_cue);
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(3.0, col),
            );
        }
    }

    // Playhead at the fixed fraction position.
    let head_x = rect.left() + ZOOM_PLAYHEAD_FRAC * w;
    painter.line_segment(
        [
            Pos2::new(head_x, rect.top()),
            Pos2::new(head_x, rect.bottom()),
        ],
        Stroke::new(2.0, pal.accent_amber),
    );

    // Click-to-seek: map the clicked x → time → sample index.
    if resp.clicked() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
            let target_secs = view_start + frac * window_secs;
            if target_secs >= 0.0 {
                let sample_pos = (target_secs * d.sample_rate as f64) as u64;
                let _ = sender.send(DeckCommand::Seek { deck, sample_pos });
            }
        }
    }
}

fn sort_header(
    ui: &mut egui::Ui,
    label: &str,
    sort: SortState,
    col: SortColumn,
) -> bool {
    let arrow = if sort.column == col {
        if sort.ascending { " ▲" } else { " ▼" }
    } else {
        ""
    };
    ui.selectable_label(sort.column == col, format!("{label}{arrow}"))
        .clicked()
}

/// Sort key for the "Key" column: Camelot number × 2 + (0 minor / 1 major),
/// so the table groups by wheel position and within that minor before major.
/// `None` keys sort to the end.
fn key_sort_value(k: Option<MusicalKey>) -> u32 {
    const MAJOR: [u8; 12] = [8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6, 1];
    match k {
        None => u32::MAX,
        Some(k) => {
            let major_tonic = if k.is_minor {
                (k.tonic + 3) % 12
            } else {
                k.tonic % 12
            };
            let num = MAJOR[major_tonic as usize] as u32;
            let letter = if k.is_minor { 0 } else { 1 };
            num * 2 + letter
        }
    }
}

/// Sort key for BPM (multiply by 1000 to preserve 3 decimals as integer).
/// Missing / invalid BPMs sort to the end.
fn bpm_sort_value(bpm: f32) -> u32 {
    if bpm <= 0.0 || !bpm.is_finite() {
        u32::MAX
    } else {
        (bpm * 1000.0).round() as u32
    }
}

/// Look up a track's duration. Prefers the cached value (recorded by
/// the v3+ analysis worker); falls back to a beats-derived heuristic
/// `last_beat + one period` for legacy v1/v2 entries that pre-date
/// the column. Returns `None` when there's no analysis at all.
fn track_length_secs(c: Option<&persistence::CachedAnalysis>) -> Option<f64> {
    let c = c?;
    if let Some(d) = c.duration_secs {
        if d > 0.0 { return Some(d); }
    }
    let last_beat = c.beats.last().copied()?;
    if c.bpm > 0.0 {
        Some(last_beat + 60.0 / c.bpm as f64)
    } else {
        Some(last_beat)
    }
}

/// Sort key for the Length column. Unknown / missing → bottom of an
/// ascending sort (same convention as `bpm_sort_value`). Stored as
/// milliseconds so the integer compare matches a float compare.
fn length_sort_value(secs: Option<f64>) -> u64 {
    match secs {
        Some(s) if s.is_finite() && s > 0.0 => (s * 1000.0).round() as u64,
        _ => u64::MAX,
    }
}

/// Short human-readable string for a past Unix-epoch timestamp,
/// relative to `now`. Examples: "just now", "5m ago", "3h ago",
/// "yesterday", "4d ago", "Mar 12". No third-party time crate —
/// the calendar math is Howard Hinnant's civil-from-days algorithm,
/// applied to UTC only (no TZ lookup; we just want a friendly label).
fn fmt_rel(ts: u64, now: u64) -> String {
    if ts > now {
        return "just now".into();
    }
    let dt = now - ts;
    if dt < 60 { return "just now".into(); }
    if dt < 3600 { return format!("{}m ago", dt / 60); }
    if dt < 86_400 { return format!("{}h ago", dt / 3600); }
    if dt < 2 * 86_400 { return "yesterday".into(); }
    if dt < 7 * 86_400 { return format!("{}d ago", dt / 86_400); }
    let (y, m, d, _, _) = civil_from_secs(ts);
    let month = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"];
    let m_idx = (m.max(1) - 1) as usize % 12;
    if dt < 365 * 86_400 {
        format!("{} {}", month[m_idx], d)
    } else {
        format!("{} {} {}", month[m_idx], d, y)
    }
}

/// "YYYY-MM-DD HH:MM UTC" — for the exported setlist (TZ-unambiguous).
fn fmt_utc(ts: u64) -> String {
    let (y, mo, d, h, mi) = civil_from_secs(ts);
    format!("{:04}-{:02}-{:02} {:02}:{:02} UTC", y, mo, d, h, mi)
}

/// Unix epoch seconds → (year, month, day, hour, minute) in UTC.
/// Implementation of Howard Hinnant's "civil from days" algorithm,
/// well-tested for any year in the gregorian calendar.
fn civil_from_secs(ts: u64) -> (i64, u32, u32, u32, u32) {
    let secs_in_day = (ts % 86_400) as u32;
    let h = secs_in_day / 3600;
    let mi = (secs_in_day / 60) % 60;
    let z = (ts / 86_400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y_final = if m <= 2 { y + 1 } else { y };
    (y_final, m, d, h, mi)
}

/// Build a markdown setlist for a single session. Entries appear in
/// chronological order (oldest first within the session). Each line
/// is "N. artist — title  (HH:MM UTC, deck X)" — readable when
/// pasted into Discord, Reddit, a notes app, anywhere.
fn format_setlist(
    session: &[history::HistoryEntry],
    lookup: &std::collections::HashMap<&Path, (&str, &str)>,
) -> String {
    let first = session.first().map(|e| e.timestamp).unwrap_or(0);
    let last = session.last().map(|e| e.timestamp).unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!(
        "# Set — {} → {} ({} tracks)\n\n",
        fmt_utc(first),
        fmt_utc(last),
        session.len(),
    ));
    for (i, entry) in session.iter().enumerate() {
        let (title, artist) = lookup
            .get(entry.path.as_path())
            .copied()
            .unwrap_or_else(|| {
                (
                    entry.path.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
                    "",
                )
            });
        let deck = match entry.deck { DeckId::A => 'A', DeckId::B => 'B' };
        if artist.is_empty() {
            out.push_str(&format!(
                "{}. {}  ({}, deck {})\n",
                i + 1, title, fmt_utc(entry.timestamp), deck,
            ));
        } else {
            out.push_str(&format!(
                "{}. {} — {}  ({}, deck {})\n",
                i + 1, artist, title, fmt_utc(entry.timestamp), deck,
            ));
        }
    }
    out
}

/// Settings ComboBox for cpal device fields. Returns true when the
/// selection changed. `none_label` is the user-facing text for the
/// "no override" choice (e.g. "(system default)" for the master
/// output, "(none — master only)" for the cue bus).
fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    target: &mut Option<String>,
    devices: &[String],
    hint: &str,
    none_label: &str,
) -> bool {
    let current = target.clone().unwrap_or_else(|| none_label.to_string());
    let display = if target.is_some() {
        current.clone()
    } else {
        format!("{none_label} — {hint}")
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(display)
        .width(280.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(target.is_none(), none_label).clicked() {
                if target.is_some() {
                    *target = None;
                    changed = true;
                }
            }
            for d in devices {
                let sel = target.as_deref() == Some(d.as_str());
                if ui.selectable_label(sel, d).clicked() && !sel {
                    *target = Some(d.clone());
                    changed = true;
                }
            }
        });
    changed
}

/// Settings ComboBox for the MIDI port filter. Picking an entry sets
/// the substring filter to the exact port name; picking "(default)"
/// clears the override so the built-in `ODJ,LPD8` fallback applies.
fn midi_combo(
    ui: &mut egui::Ui,
    id: &str,
    target: &mut Option<String>,
    ports: &[String],
    hint: &str,
) -> bool {
    let none_label = "(default)";
    let display = match target {
        Some(s) => s.clone(),
        None => format!("{none_label} — {hint}"),
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(display)
        .width(280.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(target.is_none(), none_label).clicked() {
                if target.is_some() {
                    *target = None;
                    changed = true;
                }
            }
            if ports.is_empty() {
                ui.weak("(no MIDI inputs detected)");
            }
            for p in ports {
                let sel = target.as_deref() == Some(p.as_str());
                if ui.selectable_label(sel, p).clicked() && !sel {
                    *target = Some(p.clone());
                    changed = true;
                }
            }
        });
    changed
}

/// Settings ComboBox for the "Stream to room" UPnP MediaRenderer
/// picker. Persists the device's UDN (so renaming or re-IPing doesn't
/// break the binding); shows the live friendly name + IP. If the
/// persisted selection isn't currently visible on the LAN, we keep
/// the UDN but flag it as offline so the user knows.
fn renderer_combo(
    ui: &mut egui::Ui,
    id: &str,
    target: &mut Option<String>,
    renderers: &[upnp::Renderer],
) -> bool {
    let none_label = "(off — local audio only)";
    let display = match target {
        None => none_label.to_string(),
        Some(udn) => match renderers.iter().find(|r| &r.udn == udn) {
            Some(r) => format!("{} ({})", r.name, r.address),
            None => format!("(offline) {udn}"),
        },
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(display)
        .width(280.0)
        .show_ui(ui, |ui| {
            if ui.selectable_label(target.is_none(), none_label).clicked() {
                if target.is_some() {
                    *target = None;
                    changed = true;
                }
            }
            if renderers.is_empty() {
                ui.weak("(no UPnP renderers on the LAN — scanning…)");
            }
            for r in renderers {
                let sel = target.as_deref() == Some(r.udn.as_str());
                let label = format!("{}  ·  {}", r.name, r.address);
                if ui.selectable_label(sel, label).clicked() && !sel {
                    *target = Some(r.udn.clone());
                    changed = true;
                }
            }
        });
    changed
}

#[must_use]
fn handle_keys(ctx: &egui::Context, sender: &Sender) -> bool {
    // Don't capture transport keys while the user is typing into a
    // text field (search box, etc.) — Space would toggle play instead
    // of inserting a space.
    if ctx.egui_wants_keyboard_input() {
        return false;
    }
    let mut touched = false;
    ctx.input(|i| {
        // Deck A: space play/pause, c hold-cue
        if i.key_pressed(egui::Key::Space) {
            touched = true;
            let _ = sender.send(DeckCommand::PlayToggle(DeckId::A));
        }
        if i.key_pressed(egui::Key::C) {
            touched = true;
            let _ = sender.send(DeckCommand::CuePress(DeckId::A));
        }
        if i.key_released(egui::Key::C) {
            touched = true;
            let _ = sender.send(DeckCommand::CueRelease(DeckId::A));
        }
        // Deck B: b play/pause, n hold-cue
        if i.key_pressed(egui::Key::B) {
            touched = true;
            let _ = sender.send(DeckCommand::PlayToggle(DeckId::B));
        }
        if i.key_pressed(egui::Key::N) {
            touched = true;
            let _ = sender.send(DeckCommand::CuePress(DeckId::B));
        }
        if i.key_released(egui::Key::N) {
            touched = true;
            let _ = sender.send(DeckCommand::CueRelease(DeckId::B));
        }
    });
    touched
}

/// A/B load button for the track table. When the row's track is
/// already loaded on this deck, the button paints in the deck's
/// accent colour (blue for A, pink for B) so the user can scan
/// the library and see what's where. Returns an egui `Button` so
/// the caller can use `.clicked()` like any other Button.
/// Custom-painted 22×22 A/B load button so the letter sits
/// perfectly centred in its square — `egui::Button` left-aligns
/// short text at small sizes regardless of min_size, which the
/// user spotted in the track table.
fn deck_load_button(
    ui: &mut egui::Ui,
    label: &str,
    loaded: bool,
    accent: egui::Color32,
    pal: &palette::Palette,
) -> egui::Response {
    let size = egui::Vec2::new(22.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let (fill, text_col) = if loaded {
        (accent, pal.ink)
    } else if resp.hovered() {
        (pal.raised, accent)
    } else {
        (pal.chip, accent)
    };
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, fill);
    p.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, accent),
        egui::StrokeKind::Inside,
    );
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::monospace(12.0),
        text_col,
    );
    resp
}

/// Apply an opacity factor to a colour for the played/upcoming
/// dim split. egui's premultiplied alpha means we have to scale
/// every channel, not just `a`. Cheap.
fn with_opacity(c: egui::Color32, alpha: f32) -> egui::Color32 {
    let a = alpha.clamp(0.0, 1.0);
    egui::Color32::from_rgba_premultiplied(
        (c.r() as f32 * a) as u8,
        (c.g() as f32 * a) as u8,
        (c.b() as f32 * a) as u8,
        (c.a() as f32 * a) as u8,
    )
}

#[cfg(test)]
mod tests {
    //! Pure-helper tests. These exist to give the eframe upgrade
    //! (and any future refactor) a regression net for the parts of
    //! ui/lib.rs that don't touch egui at all — string parsing, sort
    //! key derivation, waveform downsampling, calendar math.

    use super::*;
    use control::MusicalKey;

    // ----- parse_track_name ---------------------------------------

    #[test]
    fn parse_artist_dash_title() {
        let (a, t) = parse_track_name("Artbat - Afterparty");
        assert_eq!(a, "Artbat");
        assert_eq!(t, "Afterparty");
    }

    #[test]
    fn parse_strips_leading_track_number() {
        // strip_leading_track_number consumes digits + optional "."
        // or "-" + whitespace. Test cases that exercise that path.
        let (a, t) = parse_track_name("12. Artist - Title");
        assert_eq!(a, "Artist");
        assert_eq!(t, "Title");
        let (a, t) = parse_track_name("01 Some Track");
        assert!(a.is_empty());
        assert_eq!(t, "Some Track");
    }

    #[test]
    fn parse_no_separator_treats_as_title() {
        let (a, t) = parse_track_name("Just A Title");
        assert!(a.is_empty());
        assert_eq!(t, "Just A Title");
    }

    #[test]
    fn parse_multiple_dashes_keeps_rest_as_title() {
        let (a, t) = parse_track_name("Artist - Title - Remix");
        assert_eq!(a, "Artist");
        assert_eq!(t, "Title - Remix");
    }

    // ----- sort helpers -------------------------------------------

    #[test]
    fn key_sort_none_last() {
        let some_key = MusicalKey { tonic: 0, is_minor: false };
        assert!(key_sort_value(Some(some_key)) < key_sort_value(None));
    }

    #[test]
    fn key_sort_orders_minor_before_major_at_same_wheel_position() {
        // Same camelot number → minor (A) sorts before major (B).
        let cm = MusicalKey { tonic: 0, is_minor: true };  // C minor → 5A
        let eb_maj = MusicalKey { tonic: 3, is_minor: false }; // Eb major → 5B
        assert!(key_sort_value(Some(cm)) < key_sort_value(Some(eb_maj)));
    }

    #[test]
    fn bpm_sort_zero_and_nan_sort_last() {
        let v_real = bpm_sort_value(126.5);
        assert!(v_real < bpm_sort_value(0.0));
        assert!(v_real < bpm_sort_value(f32::NAN));
        assert!(v_real < bpm_sort_value(-1.0));
    }

    #[test]
    fn bpm_sort_preserves_order_within_valid_range() {
        assert!(bpm_sort_value(120.0) < bpm_sort_value(128.0));
        assert!(bpm_sort_value(127.9) < bpm_sort_value(128.0));
    }

    // ----- timestamp / calendar helpers ---------------------------

    #[test]
    fn civil_from_secs_known_epoch() {
        // 1970-01-01 00:00 UTC
        assert_eq!(civil_from_secs(0), (1970, 1, 1, 0, 0));
        // 2025-01-01 00:00 UTC = 1735689600
        assert_eq!(civil_from_secs(1735689600), (2025, 1, 1, 0, 0));
        // Plus 1h 30m
        assert_eq!(civil_from_secs(1735689600 + 5400), (2025, 1, 1, 1, 30));
    }

    #[test]
    fn civil_handles_year_boundaries() {
        // 2024-12-31 23:59 UTC = 1735689540
        let (y, m, d, h, mi) = civil_from_secs(1735689540);
        assert_eq!((y, m, d), (2024, 12, 31));
        assert_eq!((h, mi), (23, 59));
        // +60s → 2025-01-01 00:00
        let (y, m, d, h, mi) = civil_from_secs(1735689600);
        assert_eq!((y, m, d), (2025, 1, 1));
        assert_eq!((h, mi), (0, 0));
    }

    #[test]
    fn fmt_utc_format() {
        assert_eq!(fmt_utc(1735689600), "2025-01-01 00:00 UTC");
    }

    #[test]
    fn fmt_rel_buckets() {
        let now = 1_000_000;
        assert_eq!(fmt_rel(now, now), "just now");
        assert_eq!(fmt_rel(now - 30, now), "just now"); // <60 s
        assert_eq!(fmt_rel(now - 300, now), "5m ago");
        assert_eq!(fmt_rel(now - 7200, now), "2h ago");
        assert_eq!(fmt_rel(now - 90_000, now), "yesterday"); // 25h
        assert_eq!(fmt_rel(now - 4 * 86_400, now), "4d ago");
        // Future timestamps (clock skew etc.) shouldn't panic.
        assert_eq!(fmt_rel(now + 5, now), "just now");
    }

    // ----- waveform downsamplers ----------------------------------

    #[test]
    fn overview_returns_requested_bucket_count() {
        // 1 s of mono sine at 44.1 kHz → 4 buckets.
        let n = 44_100;
        let samples: Vec<f32> = (0..n)
            .map(|i| (i as f32 * 0.01).sin())
            .collect();
        let buckets = compute_overview_from(&samples, 1, 4);
        assert_eq!(buckets.len(), 4);
        for b in &buckets {
            assert!(*b > 0.0 && *b <= 1.0, "bucket out of range: {b}");
        }
    }

    #[test]
    fn overview_handles_silent_input() {
        // Silent input → all zeros.
        let silent = vec![0.0_f32; 1000];
        let v = compute_overview_from(&silent, 1, 8);
        assert_eq!(v.len(), 8);
        assert!(v.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn hires_handles_stereo_interleaved() {
        let samples: Vec<f32> = (0..400)
            .map(|i| if i % 4 == 0 { 0.5 } else { -0.5 })
            .collect();
        let v = compute_hires_peaks_from(&samples, 2, 50);
        assert!(!v.is_empty());
        assert!(v.iter().all(|x| *x >= 0.0 && *x <= 1.0));
    }
}

