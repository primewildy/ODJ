//! Auto-mix orchestrator. Runs on its own background thread so the
//! state machine continues to tick even when the egui window is
//! occluded or on another Wayland workspace (Hyprland stops sending
//! frame events to suspended surfaces, which would otherwise pause
//! the UI thread for tens of seconds and cause us to miss the trigger
//! window).
//!
//! Lifecycle:
//!   UI thread       arms via AutoMixShared.state
//!   Controller      polls shared state at 20 Hz, decides actions
//!   Controller      sends DeckCommand directly via Sender
//!   Controller      spawns load workers directly via spawn_load_worker
//!   UI thread       updates shared meta in drain_loads when LoadEvent lands
//!   UI thread       syncs picker inputs (filter/favs/genre) each frame

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Sender as StdSender;

use audio::{DeckTelemetry, Sender};
use control::{DeckCommand, DeckId, MusicalKey, TrackAnalysis};

use crate::persistence::{self, AnalysisCache, CachedAnalysis};
use crate::{
    AnalysisRefined, HIRES_SAMPLES_PER_PEAK, LoadEvent, LoadInitial, OVERVIEW_BUCKETS, StemPeaks,
    TrackMeta, compute_hires_peaks, compute_hires_peaks_from, compute_overview,
    compute_overview_from,
};

// --- constants ---------------------------------------------------------

/// 16 PhaseA + 16 PhaseC.
const BLEND_DURATION_BEATS: f64 = 32.0;
/// Lookahead between trigger and the next 16-beat red line on the
/// OUT deck. The blend snaps to the next red line ≥ out_t + 1 s.
const RED_LINE_LOOKAHEAD_BEATS: f64 = 16.0;
/// Tracks usually have a fade or sparse outro in the last few bars.
/// We want the blend to *complete* this many beats before the OUT
/// track's natural end, so the fade-out isn't audible.
const END_MARGIN_BEATS: f64 = 64.0;
/// Total remaining time at the trigger moment.
const TRIGGER_BEATS: f64 = BLEND_DURATION_BEATS + RED_LINE_LOOKAHEAD_BEATS + END_MARGIN_BEATS;

/// Tick cadence on the controller thread.
const TICK_INTERVAL_MS: u64 = 50;

// --- types -------------------------------------------------------------

pub enum AutoMixState {
    Off,
    Armed(ArmedState),
    Active(AutoMixActive),
}

#[derive(Default)]
pub struct ArmedState {
    /// Path of the track we asked to be pre-loaded onto the idle deck.
    /// Suppresses duplicate loads while the load is in flight; cleared
    /// once the deck reports the matching `loaded_path`.
    pub pre_load_pending: Option<PathBuf>,
    /// OUT-deck playhead at the last status heartbeat. Used to throttle
    /// the "watching, X beats remaining" log.
    pub last_status_log: Option<f64>,
    /// Set when `maybe_start_idle` has already fired a Play command for
    /// this "no playing deck" event. Cleared as soon as a deck reports
    /// playing again.
    pub recovery_attempted: bool,
}

pub struct AutoMixActive {
    pub out_deck: DeckId,
    pub in_deck: DeckId,
    pub in_path: PathBuf,
    pub sync_sent: bool,
    pub phase: MixPhase,
}

pub enum MixPhase {
    Cueing,
    PhaseA { start_t: f64, end_t: f64 },
    PhaseC { start_t: f64, end_t: f64 },
}

/// Per-deck snapshot of the analysis/metadata that auto-mix needs to
/// reason about. Mirrored from `DeckUi` by the UI thread in
/// `drain_loads`. The controller thread reads it under the shared
/// mutex.
#[derive(Default, Clone)]
pub struct DeckMeta {
    pub loaded_path: Option<PathBuf>,
    pub bpm: f32,
    pub beat_grid: Vec<f64>,
    pub downbeats: Vec<u32>,
    pub total_frames: u64,
    pub sample_rate: u32,
    pub key: Option<MusicalKey>,
}

/// Everything the controller thread needs to read or mutate.
pub struct AutoMixShared {
    pub state: AutoMixState,
    pub meta_a: DeckMeta,
    pub meta_b: DeckMeta,
    // Picker inputs, mirrored from the UI thread each frame.
    pub tracks: Arc<Vec<TrackMeta>>,
    pub filter_lower: String,
    pub favourites_only: bool,
    pub favourites: Arc<std::collections::HashSet<PathBuf>>,
    /// Resolved target key for harmonic filtering (Some when the user
    /// has the H-A or H-B filter on AND that deck has a detected key).
    pub harmonic_target_key: Option<MusicalKey>,
    pub genre_filter: Option<String>,
}

impl AutoMixShared {
    pub fn new() -> Self {
        Self {
            state: AutoMixState::Off,
            meta_a: DeckMeta::default(),
            meta_b: DeckMeta::default(),
            tracks: Arc::new(Vec::new()),
            filter_lower: String::new(),
            favourites_only: false,
            favourites: Arc::new(std::collections::HashSet::new()),
            harmonic_target_key: None,
            genre_filter: None,
        }
    }

    pub fn is_active(&self) -> bool { matches!(self.state, AutoMixState::Active(_)) }
}

// --- controller --------------------------------------------------------

#[derive(Clone)]
pub struct AutoMixController {
    pub shared: Arc<Mutex<AutoMixShared>>,
    pub sender: Sender,
    pub telemetry_a: DeckTelemetry,
    pub telemetry_b: DeckTelemetry,
    pub load_tx: StdSender<LoadEvent>,
    pub analysis_cache: Arc<AnalysisCache>,
    pub stem_cache: Option<Arc<stems::SessionCache>>,
}

impl AutoMixController {
    /// Spawn the controller's background tick loop. Returns immediately;
    /// the thread runs for the lifetime of the process.
    pub fn spawn(self) {
        std::thread::spawn(move || self.run());
    }

    fn run(self) {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(TICK_INTERVAL_MS));
            self.tick();
        }
    }

    fn tick(&self) {
        // Determine state without holding the lock for long.
        let state_kind = {
            let s = self.shared.lock().unwrap();
            match &s.state {
                AutoMixState::Off => return,
                AutoMixState::Armed(_) => StateKind::Armed,
                AutoMixState::Active(_) => StateKind::Active,
            }
        };
        match state_kind {
            StateKind::Armed => self.tick_armed(),
            StateKind::Active => self.tick_active(),
        }
    }

    fn playhead_secs(&self, deck: DeckId, meta: &DeckMeta) -> f64 {
        if meta.sample_rate == 0 { return 0.0; }
        let tel = match deck { DeckId::A => &self.telemetry_a, DeckId::B => &self.telemetry_b };
        tel.playhead.load(std::sync::atomic::Ordering::Relaxed) as f64 / meta.sample_rate as f64
    }

    fn is_playing(&self, deck: DeckId) -> bool {
        let tel = match deck { DeckId::A => &self.telemetry_a, DeckId::B => &self.telemetry_b };
        tel.playing.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn playhead_frames(&self, deck: DeckId) -> u64 {
        let tel = match deck { DeckId::A => &self.telemetry_a, DeckId::B => &self.telemetry_b };
        tel.playhead.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn tick_armed(&self) {
        let playing_a = self.is_playing(DeckId::A);
        let playing_b = self.is_playing(DeckId::B);
        // Clear recovery flag if anything is playing now.
        if playing_a || playing_b {
            let mut s = self.shared.lock().unwrap();
            if let AutoMixState::Armed(ref mut a) = s.state {
                if a.recovery_attempted { a.recovery_attempted = false; }
            }
        }
        // Identify OUT deck. If both/neither playing, we can't blend.
        let out_deck = if playing_a && !playing_b {
            DeckId::A
        } else if playing_b && !playing_a {
            DeckId::B
        } else if !playing_a && !playing_b {
            self.maybe_start_idle();
            return;
        } else {
            return;
        };
        // Snapshot what we need.
        let (out_meta, in_meta) = {
            let s = self.shared.lock().unwrap();
            let (a, b) = (s.meta_a.clone(), s.meta_b.clone());
            match out_deck { DeckId::A => (a, b), DeckId::B => (b, a) }
        };
        let bpm = out_meta.bpm;
        if bpm <= 0.0 || out_meta.beat_grid.is_empty() || out_meta.total_frames == 0 || out_meta.sample_rate == 0 {
            // Pre-load anyway while we wait for the playing deck's analysis to land.
            self.maybe_preload_next(out_deck, &in_meta);
            return;
        }
        let total_secs = out_meta.total_frames as f64 / out_meta.sample_rate as f64;
        let out_t = self.playhead_secs(out_deck, &out_meta);
        let remaining = total_secs - out_t;
        // Pre-load + heartbeat (always cheap when nothing changes).
        self.maybe_preload_next(out_deck, &in_meta);
        let bar_secs = 60.0 / bpm as f64;
        let trigger_at_secs = TRIGGER_BEATS * bar_secs;
        self.maybe_heartbeat(out_deck, &in_meta, out_t, remaining, bar_secs);
        if remaining > trigger_at_secs {
            return;
        }
        // Trigger window — try to begin a blend.
        if in_meta.loaded_path.is_none() {
            eprintln!("auto-mix: trigger fired but idle deck has no track — skipping blend");
            return;
        }
        if in_meta.beat_grid.is_empty() {
            eprintln!("auto-mix: trigger fired but idle deck still analysing — waiting");
            return;
        }
        eprintln!(
            "auto-mix: trigger fired — {:.0}s ({} beats) remaining on {:?}",
            remaining,
            (remaining / bar_secs).round() as i64,
            out_deck
        );
        self.begin_active_mix(out_deck, &in_meta);
    }

    fn maybe_heartbeat(
        &self,
        out_deck: DeckId,
        in_meta: &DeckMeta,
        out_t: f64,
        remaining: f64,
        bar_secs: f64,
    ) {
        let should_log = {
            let s = self.shared.lock().unwrap();
            if let AutoMixState::Armed(ref a) = s.state {
                a.last_status_log
                    .map(|prev| (out_t - prev).abs() > 30.0)
                    .unwrap_or(true)
            } else {
                false
            }
        };
        if !should_log { return; }
        let beats_remaining = (remaining / bar_secs).round() as i64;
        let in_state = match (&in_meta.loaded_path, in_meta.beat_grid.is_empty()) {
            (None, _) => "idle deck empty".to_string(),
            (Some(p), true) => format!(
                "idle deck loading {}",
                p.file_name().and_then(|s| s.to_str()).unwrap_or("?")
            ),
            (Some(p), false) => format!(
                "idle deck ready: {}",
                p.file_name().and_then(|s| s.to_str()).unwrap_or("?")
            ),
        };
        eprintln!(
            "auto-mix: watching {:?} — {:.0}s ({} beats) until blend, {}",
            out_deck, remaining, beats_remaining, in_state
        );
        let mut s = self.shared.lock().unwrap();
        if let AutoMixState::Armed(ref mut a) = s.state {
            a.last_status_log = Some(out_t);
        }
    }

    fn maybe_preload_next(&self, out_deck: DeckId, in_meta: &DeckMeta) {
        let idle_deck = match out_deck { DeckId::A => DeckId::B, DeckId::B => DeckId::A };
        // If idle deck has a track, just sync the pending flag and return.
        if in_meta.loaded_path.is_some() {
            let mut s = self.shared.lock().unwrap();
            if let AutoMixState::Armed(ref mut a) = s.state {
                if a.pre_load_pending == in_meta.loaded_path {
                    a.pre_load_pending = None;
                }
            }
            return;
        }
        // Already a load in flight?
        {
            let s = self.shared.lock().unwrap();
            if let AutoMixState::Armed(ref a) = s.state {
                if a.pre_load_pending.is_some() {
                    return;
                }
            }
        }
        self.force_preload_on(idle_deck);
    }

    fn force_preload_on(&self, deck: DeckId) {
        let Some(path) = self.pick_random_next_track() else {
            eprintln!("auto-mix: no candidate to pre-load");
            return;
        };
        eprintln!(
            "auto-mix: pre-loading {} on {:?}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
            deck
        );
        let _ = self.sender.send(DeckCommand::SetGain { deck, gain: 0.0 });
        let _ = self.sender.send(DeckCommand::SetStemDrums { deck, gain: 0.0 });
        spawn_load_worker(
            path.clone(),
            deck,
            self.sender.clone(),
            self.load_tx.clone(),
            Arc::clone(&self.analysis_cache),
            self.stem_cache.as_ref().map(Arc::clone),
        );
        let mut s = self.shared.lock().unwrap();
        if let AutoMixState::Armed(ref mut a) = s.state {
            a.pre_load_pending = Some(path);
        }
    }

    fn pick_random_next_track(&self) -> Option<PathBuf> {
        let s = self.shared.lock().unwrap();
        let filter_lower = s.filter_lower.clone();
        let target_key = s.harmonic_target_key;
        let harmonic_active = target_key.is_some();
        let now_a = s.meta_a.loaded_path.clone();
        let now_b = s.meta_b.loaded_path.clone();
        let favourites_only = s.favourites_only;
        let favourites = Arc::clone(&s.favourites);
        let genre_filter = s.genre_filter.clone();
        let tracks = Arc::clone(&s.tracks);
        drop(s);

        let candidates: Vec<&PathBuf> = tracks
            .iter()
            .filter(|m| {
                if Some(&m.path) == now_a.as_ref() || Some(&m.path) == now_b.as_ref() {
                    return false;
                }
                if favourites_only && !favourites.contains(&m.path) {
                    return false;
                }
                if !filter_lower.is_empty() {
                    let hit = m.title.to_lowercase().contains(&filter_lower)
                        || m.artist.to_lowercase().contains(&filter_lower)
                        || m.filename.to_lowercase().contains(&filter_lower);
                    if !hit { return false; }
                }
                if let Some(g) = genre_filter.as_deref() {
                    if !m.genre.eq_ignore_ascii_case(g) {
                        return false;
                    }
                }
                if harmonic_active {
                    let t = target_key.unwrap();
                    let Some(c) = self.analysis_cache.get(&m.path) else { return false; };
                    let Some(k) = c.key else { return false; };
                    if !persistence::camelot_compatible(t, k) {
                        return false;
                    }
                }
                true
            })
            .map(|m| &m.path)
            .collect();
        if candidates.is_empty() { return None; }
        // Cheap PRNG seeded from playhead frames.
        let seed = self.playhead_frames(DeckId::A)
            ^ self.playhead_frames(DeckId::B)
            ^ (candidates.len() as u64).wrapping_mul(0x9e3779b97f4a7c15);
        let idx = (seed.wrapping_mul(0xbf58476d1ce4e5b9) >> 32) as usize % candidates.len();
        Some(candidates[idx].clone())
    }

    fn begin_active_mix(&self, out_deck: DeckId, in_meta: &DeckMeta) -> bool {
        let in_deck = match out_deck { DeckId::A => DeckId::B, DeckId::B => DeckId::A };
        let Some(in_path) = in_meta.loaded_path.clone() else { return false; };
        if in_meta.beat_grid.is_empty() { return false; }
        eprintln!(
            "auto-mix: triggering blend out={:?} in={:?} → {}",
            out_deck,
            in_deck,
            in_path.file_name().and_then(|s| s.to_str()).unwrap_or("?")
        );
        let _ = self.sender.send(DeckCommand::SetGain { deck: in_deck, gain: 0.0 });
        let _ = self.sender.send(DeckCommand::SetStemDrums { deck: in_deck, gain: 0.0 });
        let mut s = self.shared.lock().unwrap();
        s.state = AutoMixState::Active(AutoMixActive {
            out_deck,
            in_deck,
            in_path,
            sync_sent: false,
            phase: MixPhase::Cueing,
        });
        true
    }

    fn maybe_start_idle(&self) {
        {
            let s = self.shared.lock().unwrap();
            if !matches!(s.state, AutoMixState::Armed(_)) {
                return;
            }
        }
        // Find a fresh deck — playhead not at end of buffer.
        let is_fresh = |meta: &DeckMeta, deck: DeckId| -> bool {
            if meta.loaded_path.is_none() || meta.beat_grid.is_empty() {
                return false;
            }
            if meta.total_frames == 0 || meta.sample_rate == 0 {
                return false;
            }
            let pos = self.playhead_frames(deck);
            let margin = meta.sample_rate as u64 * 2;
            pos + margin < meta.total_frames
        };
        let (meta_a, meta_b) = {
            let s = self.shared.lock().unwrap();
            (s.meta_a.clone(), s.meta_b.clone())
        };
        let candidate = if is_fresh(&meta_a, DeckId::A) {
            Some((DeckId::A, meta_a.loaded_path.clone()))
        } else if is_fresh(&meta_b, DeckId::B) {
            Some((DeckId::B, meta_b.loaded_path.clone()))
        } else {
            None
        };
        let Some((d, path)) = candidate else {
            // No fresh deck. Don't latch — recovery_attempted is the
            // post-START guard, not "did we look", so leaving it false
            // means we'll re-evaluate every tick (20 Hz) and pounce
            // the moment a pre-load lands. Kick a pre-load on any
            // truly empty deck so we're not stuck waiting forever on
            // a deck the user hasn't loaded anything onto.
            for (m, dk) in [(&meta_a, DeckId::A), (&meta_b, DeckId::B)] {
                if m.loaded_path.is_none() {
                    let pending_match = {
                        let s = self.shared.lock().unwrap();
                        matches!(&s.state, AutoMixState::Armed(a)
                            if a.pre_load_pending.is_some())
                    };
                    if !pending_match {
                        self.force_preload_on(dk);
                    }
                    break;
                }
            }
            return;
        };
        eprintln!(
            "auto-mix: no playing deck — starting {:?} ({}) fresh",
            d,
            path.as_ref()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("?")
        );
        let _ = self.sender.send(DeckCommand::SetGain { deck: d, gain: 1.0 });
        let _ = self.sender.send(DeckCommand::SetStemDrums { deck: d, gain: 1.0 });
        let _ = self.sender.send(DeckCommand::Play(d));
        {
            let mut s = self.shared.lock().unwrap();
            if let AutoMixState::Armed(ref mut a) = s.state {
                a.last_status_log = None;
                a.recovery_attempted = true;
            }
        }
        let other = match d { DeckId::A => DeckId::B, DeckId::B => DeckId::A };
        self.force_preload_on(other);
    }

    fn tick_active(&self) {
        // Snapshot what we need without holding the lock during sends.
        let (out_deck, in_deck, in_path, sync_sent, phase_snap) = {
            let s = self.shared.lock().unwrap();
            let AutoMixState::Active(ref active) = s.state else { return };
            let phase = match active.phase {
                MixPhase::Cueing => PhaseSnap::Cueing,
                MixPhase::PhaseA { start_t, end_t } => PhaseSnap::PhaseA { start_t, end_t },
                MixPhase::PhaseC { start_t, end_t } => PhaseSnap::PhaseC { start_t, end_t },
            };
            (active.out_deck, active.in_deck, active.in_path.clone(), active.sync_sent, phase)
        };
        let (out_meta, in_meta) = {
            let s = self.shared.lock().unwrap();
            let (a, b) = (s.meta_a.clone(), s.meta_b.clone());
            match out_deck { DeckId::A => (a, b), DeckId::B => (b, a) }
        };
        let bpm = out_meta.bpm.clamp(60.0, 220.0);
        let out_total_secs = if out_meta.sample_rate > 0 {
            out_meta.total_frames as f64 / out_meta.sample_rate as f64
        } else { f64::INFINITY };
        let out_t = self.playhead_secs(out_deck, &out_meta);
        let bar16_secs = RED_LINE_LOOKAHEAD_BEATS * 60.0 / bpm as f64;
        let in_playing = self.is_playing(in_deck);
        match phase_snap {
            PhaseSnap::Cueing => {
                if in_meta.loaded_path.as_deref() != Some(in_path.as_path())
                    || in_meta.beat_grid.is_empty()
                {
                    return;
                }
                if !sync_sent {
                    let _ = self.sender.send(DeckCommand::Sync { deck: in_deck });
                    let mut s = self.shared.lock().unwrap();
                    if let AutoMixState::Active(ref mut a) = s.state {
                        a.sync_sent = true;
                    }
                }
                // Preferred: align to every 4th detected downbeat
                // (16-bar red line). Fall back to every 16th beat in
                // the raw beat grid when downbeats are empty — better
                // a slightly-off bar boundary than disarming entirely
                // on a track the model didn't tag.
                let next_mix = out_meta.downbeats.iter().enumerate()
                    .filter(|(j, _)| j % 4 == 0)
                    .filter_map(|(_, &i)| out_meta.beat_grid.get(i as usize).copied())
                    .find(|&t| t > out_t + 1.0)
                    .or_else(|| {
                        out_meta.beat_grid.iter().enumerate()
                            .filter(|(j, _)| j % 16 == 0)
                            .map(|(_, &t)| t)
                            .find(|&t| t > out_t + 1.0)
                    });
                let Some(mix_t) = next_mix else {
                    // Beat grid itself is empty / track is fully past
                    // playhead. Re-arm rather than turning auto-mix
                    // off — the user wanted it on; respect that.
                    eprintln!("auto-mix: no upcoming beat on out deck — re-arming");
                    self.rearm();
                    return;
                };
                // Blend must end at least END_MARGIN beats before track end.
                let blend_end = mix_t + 2.0 * bar16_secs;
                let end_margin_secs = END_MARGIN_BEATS * 60.0 / bpm as f64;
                if blend_end > out_total_secs - end_margin_secs {
                    eprintln!(
                        "auto-mix: not enough track left for 32-beat blend ({:.1}-beat margin) — re-arming for next",
                        END_MARGIN_BEATS,
                    );
                    self.rearm();
                    return;
                }
                eprintln!(
                    "auto-mix: PhaseA armed — start {:.1}s, drum swap {:.1}s, end {:.1}s (track ends {:.1}s)",
                    mix_t,
                    mix_t + bar16_secs,
                    blend_end,
                    out_total_secs
                );
                let mut s = self.shared.lock().unwrap();
                if let AutoMixState::Active(ref mut a) = s.state {
                    a.phase = MixPhase::PhaseA { start_t: mix_t, end_t: mix_t + bar16_secs };
                }
            }
            PhaseSnap::PhaseA { start_t, end_t } => {
                if !in_playing && out_t >= start_t {
                    let _ = self.sender.send(DeckCommand::Play(in_deck));
                }
                if out_t < start_t { return; }
                let frac = ((out_t - start_t) / (end_t - start_t)).clamp(0.0, 1.0) as f32;
                let _ = self.sender.send(DeckCommand::SetGain { deck: in_deck, gain: frac });
                if out_t >= end_t {
                    let _ = self.sender.send(DeckCommand::SetStemDrums { deck: in_deck, gain: 1.0 });
                    let _ = self.sender.send(DeckCommand::SetStemDrums { deck: out_deck, gain: 0.0 });
                    let mut s = self.shared.lock().unwrap();
                    if let AutoMixState::Active(ref mut a) = s.state {
                        a.phase = MixPhase::PhaseC { start_t: end_t, end_t: end_t + bar16_secs };
                    }
                }
            }
            PhaseSnap::PhaseC { start_t, end_t } => {
                let frac = ((out_t - start_t) / (end_t - start_t)).clamp(0.0, 1.0) as f32;
                let _ = self.sender.send(DeckCommand::SetGain {
                    deck: out_deck,
                    gain: 1.0 - frac,
                });
                if out_t >= end_t {
                    let _ = self.sender.send(DeckCommand::Pause(out_deck));
                    let _ = self.sender.send(DeckCommand::SetGain { deck: out_deck, gain: 1.0 });
                    let _ = self.sender.send(DeckCommand::SetStemDrums { deck: out_deck, gain: 1.0 });
                    eprintln!("auto-mix: blend complete — re-armed for next");
                    {
                        let mut s = self.shared.lock().unwrap();
                        s.state = AutoMixState::Armed(ArmedState::default());
                    }
                    // Just-paused deck still has the old track loaded; force a replacement.
                    self.force_preload_on(out_deck);
                }
            }
        }
    }

    /// Drop the current Active attempt and return to Armed. Used when
    /// a particular blend can't proceed (e.g. red-line search failed,
    /// blend wouldn't fit before track end) but the user still wants
    /// auto-mix engaged. The controller will reconsider on the next
    /// tick — useful e.g. when a new track is mid-load.
    fn rearm(&self) {
        let mut s = self.shared.lock().unwrap();
        if matches!(s.state, AutoMixState::Active(_)) {
            s.state = AutoMixState::Armed(ArmedState::default());
        }
    }

}

enum StateKind { Armed, Active }

enum PhaseSnap {
    Cueing,
    PhaseA { start_t: f64, end_t: f64 },
    PhaseC { start_t: f64, end_t: f64 },
}

// --- load worker -------------------------------------------------------

/// Spawn the per-track decoder/analysis/stem worker. Sends a
/// `DeckCommand::LoadTrack` to the audio engine when the buffer is
/// ready and a `LoadEvent::Initial` back to the UI thread for state
/// update. The slow analysis path + stem worker run in their own
/// child threads. Callable from any thread (used by both UI's
/// `start_load` and the auto-mix controller's pre-load logic).
pub fn spawn_load_worker(
    path: PathBuf,
    deck: DeckId,
    sender: Sender,
    tx: StdSender<LoadEvent>,
    cache: Arc<AnalysisCache>,
    stem_cache: Option<Arc<stems::SessionCache>>,
) {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::thread::spawn(move || {
        let buffer = match decode::load_to_buffer(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("decode {} failed: {e}", path.display());
                let _ = tx.send(LoadEvent::Failed { deck, path: path.clone() });
                return;
            }
        };
        let overview = compute_overview(&buffer, OVERVIEW_BUCKETS);
        let hires = compute_hires_peaks(&buffer, HIRES_SAMPLES_PER_PEAK);
        let total_frames = buffer.frames() as u64;
        let sample_rate = buffer.sample_rate;
        let duration_secs = buffer.duration_secs();

        let cached = cache.get(&path);
        let (initial_bpm, initial_beats, initial_downbeats, initial_key) =
            if let Some(c) = cached.as_ref() {
                (c.bpm, c.beats.clone(), c.downbeats.clone(), c.key)
            } else {
                (0.0, Vec::new(), Vec::new(), None)
            };
        let initial_analysis = Arc::new(TrackAnalysis {
            analysis_version: cached.as_ref().map(|c| c.version).unwrap_or(0),
            bpm: initial_bpm,
            beat_grid: initial_beats.clone(),
            downbeats: initial_downbeats.clone(),
            duration_secs,
            sample_rate,
            key: initial_key,
        });

        let _ = sender.send(DeckCommand::LoadTrack {
            deck,
            buffer: Arc::clone(&buffer),
            analysis: initial_analysis,
        });
        let _ = tx.send(LoadEvent::Initial(LoadInitial {
            deck,
            path: path.clone(),
            title: name,
            overview,
            hires,
            samples_per_hires: HIRES_SAMPLES_PER_PEAK,
            total_frames,
            sample_rate,
            bpm: initial_bpm,
            beat_grid: initial_beats,
            downbeats: initial_downbeats,
            key: initial_key,
        }));

        if let Some(sc) = stem_cache {
            let path_s = path.clone();
            let sender_s = sender.clone();
            let tx_s = tx.clone();
            std::thread::spawn(move || {
                unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10); }
                match sc.separate(&path_s) {
                    Ok(stems) => {
                        // Match the audio engine's stem routing (see
                        // `sample_at` in audio/lib.rs:1145):
                        //   DRUMS knob   → stems.drums
                        //   VOCALS knob  → stems.vocals
                        //   INSTR knob   → stems.bass + stems.other
                        // The waveform overlays should mirror this so
                        // the colours match what the user actually
                        // hears when they push a stem knob.
                        let instr_buf: Vec<f32> = stems
                            .bass
                            .iter()
                            .zip(stems.other.iter())
                            .map(|(b, o)| b + o)
                            .collect();
                        let ch = stems.channels;
                        let stems_peaks = StemPeaks {
                            deck,
                            path: path_s.clone(),
                            stems: Arc::clone(&stems),
                            overview_drums: compute_overview_from(&stems.drums, ch, OVERVIEW_BUCKETS),
                            overview_vocals: compute_overview_from(&stems.vocals, ch, OVERVIEW_BUCKETS),
                            overview_instr: compute_overview_from(&instr_buf, ch, OVERVIEW_BUCKETS),
                            hires_drums: compute_hires_peaks_from(&stems.drums, ch, HIRES_SAMPLES_PER_PEAK),
                            hires_vocals: compute_hires_peaks_from(&stems.vocals, ch, HIRES_SAMPLES_PER_PEAK),
                            hires_instr: compute_hires_peaks_from(&instr_buf, ch, HIRES_SAMPLES_PER_PEAK),
                        };
                        drop(stems);
                        let _ = sender_s;
                        let _ = tx_s.send(LoadEvent::Stems(stems_peaks));
                    }
                    Err(e) => eprintln!("stems: {} failed: {e:#}", path_s.display()),
                }
            });
        }

        if cached.is_none() {
            let cache_slow = Arc::clone(&cache);
            let sender_slow = sender.clone();
            let tx_slow = tx.clone();
            std::thread::spawn(move || {
                unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10); }
                let r = analysis::analyse(&buffer);
                let entry = CachedAnalysis {
                    bpm: r.bpm,
                    key: r.key,
                    beats: r.beat_grid.clone(),
                    downbeats: r.downbeats.clone(),
                    version: r.analysis_version,
                    duration_secs: Some(duration_secs),
                };
                cache_slow.insert(path.clone(), entry);
                let refined = Arc::new(TrackAnalysis {
                    analysis_version: r.analysis_version,
                    bpm: r.bpm,
                    beat_grid: r.beat_grid.clone(),
                    downbeats: r.downbeats.clone(),
                    duration_secs,
                    sample_rate,
                    key: r.key,
                });
                let _ = sender_slow.send(DeckCommand::UpdateAnalysis {
                    deck,
                    analysis: refined,
                });
                let _ = tx_slow.send(LoadEvent::Refined(AnalysisRefined {
                    deck,
                    path,
                    bpm: r.bpm,
                    beat_grid: r.beat_grid,
                    downbeats: r.downbeats,
                    key: r.key,
                }));
            });
        }
    });
}
