//! **Spike** — data model + evaluator for smart playlists.
//!
//! Status: not wired into the UI. This file proves the predicate
//! / combine / evaluate shape works against the data the rest of
//! the app already has (TrackMeta, CachedAnalysis, Favourites,
//! HistoryStore). The wizard UI + .smart.toml persistence + the
//! right-click menu integration are deliberately deferred to a
//! follow-on feature commit.
//!
//! ## Concept
//!
//! A `SmartRule` is a set of `Predicate`s combined with AND or OR.
//! Evaluating it against the library returns the set of matching
//! tracks, *in the order the user asked for* (BPM ascending, key
//! ordered by Camelot wheel, played count, etc.). The same .m3u
//! file format we already use becomes the cache of resolved
//! matches: re-evaluating on app start (or after the library has
//! changed) rewrites the .m3u so every other audio tool still sees
//! a normal static playlist. The sidecar `.smart.toml` carries the
//! rules.
//!
//! ## On-disk layout (planned)
//!
//! Two files per smart playlist, same stem, alongside the static
//! playlists in `<music-dir>/.playlists/`:
//!
//!     Late Night.m3u          (live cache of resolved matches)
//!     Late Night.smart.toml   (the rule)
//!
//! `PlaylistStore::load()` learns to look for a sibling `.smart.toml`
//! when scanning .m3u files; presence flips the node type from
//! "static playlist" to "smart playlist" in the UI. Right-click
//! gets "Edit rule…" alongside "Rename / Delete" for smart entries.
//!
//! ## Wizard UX (planned)
//!
//! Modal pops on "New smart playlist…":
//!   - Name field at top
//!   - Combine: [All] [Any] segmented control
//!   - Stack of predicate rows; each row picks a Predicate kind
//!     from a dropdown and shows the kind-specific inputs
//!     ([+] adds a row, [×] removes)
//!   - Sort: dropdown over the existing SortColumn enum
//!   - Live "matches N tracks" preview at the bottom
//!   - [Cancel] [Create]
//!
//! ## Re-evaluation
//!
//! Rules are re-evaluated when:
//!   - App starts (cheap — N tracks, simple predicate eval)
//!   - Analysis cache generation bumps (new BPM/key data lands)
//!   - History generation bumps (play counts changed)
//!   - User edits the rule
//!   - User toggles favourite on a track (affects IsFavourite preds)
//!
//! The resolved .m3u is rewritten any time the match set changes
//! so external tools always see fresh contents.

use std::path::{Path, PathBuf};

use control::MusicalKey;
use serde::{Deserialize, Serialize};

use crate::persistence::{camelot_compatible, CachedAnalysis, TrackMeta};

/// One predicate in a smart playlist rule. Evaluating one against
/// a track is cheap — string compares + a few numeric checks; no
/// allocations beyond the rule already holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Predicate {
    /// `min ≤ bpm ≤ max`. Excludes tracks without an analysed BPM.
    BpmRange { min: f32, max: f32 },
    /// `|bpm − target| ≤ tolerance`. Excludes tracks without an
    /// analysed BPM. Convenient shortcut for "near 128 BPM".
    BpmNear { target: f32, tolerance: f32 },
    /// Track's key is harmonically compatible with `target` per
    /// the Camelot wheel (same letter ±1, or relative major↔minor).
    /// Excludes tracks without a detected key.
    KeyCamelotCompatibleWith { tonic: u8, is_minor: bool },
    /// Genre matches any of these (case-insensitive).
    GenreIn { genres: Vec<String> },
    /// Title contains substring (case-insensitive).
    TitleContains { needle: String },
    /// Artist contains substring (case-insensitive).
    ArtistContains { needle: String },
    /// Track is in the favourites store.
    IsFavourite,
    /// Played count >= n (history).
    PlayedAtLeastNTimes { n: u32 },
    /// Never been played (no history entry).
    NeverPlayed,
    /// Track has any hot cue set (in .track-meta).
    HasHotCues,
    /// Track has a manual grid override (in .track-meta).
    HasManualGrid,
    /// Logical negation of a single predicate.
    Not { inner: Box<Predicate> },
}

/// How multiple predicates compose. `All` is AND; `Any` is OR. (For
/// nested groupings the user adds a Not(Or(…)) etc. — single-level
/// combine is enough for the wizard's flat-row UX, more complex
/// boolean trees can come later if anyone actually wants them.)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Combine {
    All,
    Any,
}

/// Sort order for the resolved match set. Maps onto SortColumn but
/// kept separate so the smart-playlist file format isn't pinned to
/// the UI module's enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    Title,
    Artist,
    Bpm,
    KeyCamelot,
    /// Newest in history first (most recently played).
    PlayedRecency,
    /// Random — different every evaluation. Useful for "shuffle"-
    /// style smart playlists.
    Random,
}

/// Full smart-playlist definition. Serialised to `.smart.toml` in
/// the same directory as the matching `.m3u` cache file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SmartRule {
    pub combine: Combine,
    pub predicates: Vec<Predicate>,
    pub sort: SortBy,
    /// Cap the match set. `None` = no cap. Useful for "top 100
    /// most-played" style playlists.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Read-only view of everything the evaluator needs. Cheaper than
/// requiring the caller to pass &Application; lets tests construct
/// a synthetic snapshot.
pub struct LibrarySnapshot<'a> {
    pub tracks: &'a [super::TrackMeta],
    pub analysis_cache: &'a dyn AnalysisLookup,
    pub favourites: &'a dyn FavouritesLookup,
    pub history: &'a dyn HistoryLookup,
    pub track_meta: &'a dyn TrackMetaLookup,
}

pub trait AnalysisLookup {
    fn get(&self, path: &Path) -> Option<CachedAnalysis>;
}

pub trait FavouritesLookup {
    fn contains(&self, path: &Path) -> bool;
}

pub trait HistoryLookup {
    /// Total play count.
    fn count(&self, path: &Path) -> u32;
    /// UNIX timestamp of the most-recent play (None = never).
    fn last_played(&self, path: &Path) -> Option<u64>;
}

pub trait TrackMetaLookup {
    fn get(&self, path: &Path) -> Option<TrackMeta>;
}

impl SmartRule {
    /// Evaluate the rule against `lib` and return matching track
    /// paths, sorted + limited per the rule. Order is stable
    /// modulo `SortBy::Random` (which is intentionally not).
    pub fn evaluate(&self, lib: &LibrarySnapshot) -> Vec<PathBuf> {
        let mut matched: Vec<&super::TrackMeta> = lib
            .tracks
            .iter()
            .filter(|t| self.matches(t, lib))
            .collect();
        sort_matched(&mut matched, self.sort, lib);
        let n = self.limit.unwrap_or(usize::MAX);
        matched.into_iter().take(n).map(|t| t.path.clone()).collect()
    }

    /// Does a single track satisfy this rule?
    fn matches(&self, t: &super::TrackMeta, lib: &LibrarySnapshot) -> bool {
        match self.combine {
            Combine::All => self.predicates.iter().all(|p| eval_predicate(p, t, lib)),
            Combine::Any => self.predicates.iter().any(|p| eval_predicate(p, t, lib)),
        }
    }
}

fn eval_predicate(p: &Predicate, t: &super::TrackMeta, lib: &LibrarySnapshot) -> bool {
    match p {
        Predicate::BpmRange { min, max } => {
            lib.analysis_cache.get(&t.path)
                .map(|c| c.bpm >= *min && c.bpm <= *max)
                .unwrap_or(false)
        }
        Predicate::BpmNear { target, tolerance } => {
            lib.analysis_cache.get(&t.path)
                .map(|c| (c.bpm - *target).abs() <= *tolerance)
                .unwrap_or(false)
        }
        Predicate::KeyCamelotCompatibleWith { tonic, is_minor } => {
            let target = MusicalKey { tonic: *tonic, is_minor: *is_minor };
            lib.analysis_cache.get(&t.path)
                .and_then(|c| c.key)
                .map(|k| camelot_compatible(target, k))
                .unwrap_or(false)
        }
        Predicate::GenreIn { genres } => {
            let g = t.genre.trim();
            !g.is_empty() && genres.iter().any(|x| x.eq_ignore_ascii_case(g))
        }
        Predicate::TitleContains { needle } => {
            t.title.to_lowercase().contains(&needle.to_lowercase())
        }
        Predicate::ArtistContains { needle } => {
            t.artist.to_lowercase().contains(&needle.to_lowercase())
        }
        Predicate::IsFavourite => lib.favourites.contains(&t.path),
        Predicate::PlayedAtLeastNTimes { n } => lib.history.count(&t.path) >= *n,
        Predicate::NeverPlayed => lib.history.count(&t.path) == 0,
        Predicate::HasHotCues => {
            lib.track_meta.get(&t.path)
                .map(|m| m.hot_cues.iter().any(Option::is_some))
                .unwrap_or(false)
        }
        Predicate::HasManualGrid => {
            lib.track_meta.get(&t.path)
                .map(|m| m.grid_override.is_some())
                .unwrap_or(false)
        }
        Predicate::Not { inner } => !eval_predicate(inner, t, lib),
    }
}

fn sort_matched(
    matched: &mut Vec<&super::TrackMeta>,
    sort: SortBy,
    lib: &LibrarySnapshot,
) {
    match sort {
        SortBy::Title => matched.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase())),
        SortBy::Artist => matched.sort_by(|a, b| a.artist.to_lowercase().cmp(&b.artist.to_lowercase())),
        SortBy::Bpm => matched.sort_by(|a, b| {
            let ba = lib.analysis_cache.get(&a.path).map(|c| c.bpm).unwrap_or(0.0);
            let bb = lib.analysis_cache.get(&b.path).map(|c| c.bpm).unwrap_or(0.0);
            ba.partial_cmp(&bb).unwrap_or(std::cmp::Ordering::Equal)
        }),
        SortBy::KeyCamelot => matched.sort_by(|a, b| {
            let ka = lib.analysis_cache.get(&a.path).and_then(|c| c.key);
            let kb = lib.analysis_cache.get(&b.path).and_then(|c| c.key);
            camelot_sort_value(ka).cmp(&camelot_sort_value(kb))
        }),
        SortBy::PlayedRecency => matched.sort_by(|a, b| {
            let la = lib.history.last_played(&a.path).unwrap_or(0);
            let lb = lib.history.last_played(&b.path).unwrap_or(0);
            lb.cmp(&la) // descending — newest first
        }),
        SortBy::Random => {
            // Pseudo-random via a stable per-path hash * frame seed.
            // For real use, plug a real PRNG; spike test uses path
            // length as a stand-in (deterministic per fixture).
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            matched.sort_by_key(|t| {
                let mut h = DefaultHasher::new();
                t.path.hash(&mut h);
                h.finish()
            });
        }
    }
}

fn camelot_sort_value(k: Option<MusicalKey>) -> u32 {
    // 1..12 wheel position × 10 + 0 (A) or 1 (B); unknown → max.
    match k {
        None => u32::MAX,
        Some(key) => {
            // Reuse the camelot_number helper if it's pub; here we
            // inline the major-tonic rotation to avoid coupling to
            // persistence::camelot_number being a private fn.
            const MAJOR: [u8; 12] = [8, 3, 10, 5, 12, 7, 2, 9, 4, 11, 6, 1];
            let major_tonic = if key.is_minor { (key.tonic + 3) % 12 } else { key.tonic % 12 };
            let n = MAJOR[major_tonic as usize] as u32;
            let letter = if key.is_minor { 0 } else { 1 };
            n * 10 + letter
        }
    }
}

// ---- tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::TrackMeta as UiTrackMeta;
    use crate::persistence::{self, CachedAnalysis, TrackMeta as MetaSidecar};

    /// Minimal in-memory mock library for the evaluator tests. Keeps
    /// the spike standalone — no .analysis-cache / .favourites
    /// / .history files involved.
    #[derive(Default)]
    struct Mock {
        analyses: HashMap<PathBuf, CachedAnalysis>,
        favourites: std::collections::HashSet<PathBuf>,
        plays: HashMap<PathBuf, (u32, u64)>, // count, last
        meta: HashMap<PathBuf, MetaSidecar>,
    }

    impl AnalysisLookup for Mock {
        fn get(&self, p: &Path) -> Option<CachedAnalysis> {
            self.analyses.get(p).cloned()
        }
    }
    impl FavouritesLookup for Mock {
        fn contains(&self, p: &Path) -> bool { self.favourites.contains(p) }
    }
    impl HistoryLookup for Mock {
        fn count(&self, p: &Path) -> u32 { self.plays.get(p).map(|x| x.0).unwrap_or(0) }
        fn last_played(&self, p: &Path) -> Option<u64> { self.plays.get(p).map(|x| x.1) }
    }
    impl TrackMetaLookup for Mock {
        fn get(&self, p: &Path) -> Option<MetaSidecar> {
            self.meta.get(p).cloned()
        }
    }

    fn ui_track(path: &str, title: &str, artist: &str, genre: &str) -> UiTrackMeta {
        UiTrackMeta {
            path: PathBuf::from(path),
            filename: path.rsplit('/').next().unwrap_or(path).to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            genre: genre.to_string(),
        }
    }

    fn analysis(bpm: f32, key: Option<MusicalKey>) -> CachedAnalysis {
        CachedAnalysis {
            bpm, key, beats: Vec::new(), downbeats: Vec::new(),
            version: 3, duration_secs: Some(180.0),
        }
    }

    #[test]
    fn bpm_range_matches_in_range_only() {
        let tracks = vec![
            ui_track("/a.mp3", "A", "X", "House"),
            ui_track("/b.mp3", "B", "Y", "House"),
            ui_track("/c.mp3", "C", "Z", "House"),
        ];
        let mut mock = Mock::default();
        mock.analyses.insert("/a.mp3".into(), analysis(118.0, None));
        mock.analyses.insert("/b.mp3".into(), analysis(126.0, None));
        mock.analyses.insert("/c.mp3".into(), analysis(140.0, None));
        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &mock, favourites: &mock,
            history: &mock, track_meta: &mock,
        };
        let rule = SmartRule {
            combine: Combine::All,
            predicates: vec![Predicate::BpmRange { min: 120.0, max: 130.0 }],
            sort: SortBy::Bpm,
            limit: None,
        };
        let out = rule.evaluate(&lib);
        assert_eq!(out, vec![PathBuf::from("/b.mp3")]);
    }

    #[test]
    fn key_camelot_compatible_matches_related_keys() {
        // C minor (tonic=0, minor=true) is at 5A on the wheel.
        // Compatible: 5A itself, 5B (Eb major), 4A, 6A.
        let tracks = vec![
            ui_track("/cmin.mp3", "C min", "X", "House"),
            ui_track("/ebmaj.mp3", "Eb maj", "X", "House"),
            ui_track("/fsharp.mp3", "F#", "X", "House"),
        ];
        let mut mock = Mock::default();
        mock.analyses.insert(
            "/cmin.mp3".into(),
            analysis(120.0, Some(MusicalKey { tonic: 0, is_minor: true })),
        );
        mock.analyses.insert(
            "/ebmaj.mp3".into(),
            analysis(120.0, Some(MusicalKey { tonic: 3, is_minor: false })),
        );
        mock.analyses.insert(
            "/fsharp.mp3".into(),
            analysis(120.0, Some(MusicalKey { tonic: 6, is_minor: false })),
        );
        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &mock, favourites: &mock,
            history: &mock, track_meta: &mock,
        };
        let rule = SmartRule {
            combine: Combine::All,
            predicates: vec![Predicate::KeyCamelotCompatibleWith { tonic: 0, is_minor: true }],
            sort: SortBy::Title,
            limit: None,
        };
        let out = rule.evaluate(&lib);
        // C minor + Eb major match (5A and 5B); F# major is distant.
        assert!(out.contains(&PathBuf::from("/cmin.mp3")));
        assert!(out.contains(&PathBuf::from("/ebmaj.mp3")));
        assert!(!out.contains(&PathBuf::from("/fsharp.mp3")));
    }

    #[test]
    fn combine_any_acts_as_or() {
        let tracks = vec![
            ui_track("/a.mp3", "A", "X", "House"),
            ui_track("/b.mp3", "B", "X", "Techno"),
            ui_track("/c.mp3", "C", "X", "Disco"),
        ];
        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &Mock::default(),
            favourites: &Mock::default(), history: &Mock::default(),
            track_meta: &Mock::default(),
        };
        let rule = SmartRule {
            combine: Combine::Any,
            predicates: vec![
                Predicate::GenreIn { genres: vec!["House".into()] },
                Predicate::GenreIn { genres: vec!["Disco".into()] },
            ],
            sort: SortBy::Title,
            limit: None,
        };
        let out = rule.evaluate(&lib);
        assert_eq!(out, vec![PathBuf::from("/a.mp3"), PathBuf::from("/c.mp3")]);
    }

    #[test]
    fn combine_all_with_not() {
        let tracks = vec![
            ui_track("/a.mp3", "Drop the bass", "X", "House"),
            ui_track("/b.mp3", "Cooldown", "X", "House"),
            ui_track("/c.mp3", "Drop it", "X", "Techno"),
        ];
        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &Mock::default(),
            favourites: &Mock::default(), history: &Mock::default(),
            track_meta: &Mock::default(),
        };
        // House AND title contains "drop" — should match "Drop the
        // bass" only (the Techno track is excluded by genre).
        let rule = SmartRule {
            combine: Combine::All,
            predicates: vec![
                Predicate::GenreIn { genres: vec!["House".into()] },
                Predicate::TitleContains { needle: "drop".into() },
            ],
            sort: SortBy::Title,
            limit: None,
        };
        let out = rule.evaluate(&lib);
        assert_eq!(out, vec![PathBuf::from("/a.mp3")]);
    }

    #[test]
    fn never_played_filter() {
        let tracks = vec![
            ui_track("/a.mp3", "A", "X", "House"),
            ui_track("/b.mp3", "B", "X", "House"),
        ];
        let mut mock = Mock::default();
        mock.plays.insert("/a.mp3".into(), (3, 1000));
        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &mock, favourites: &mock,
            history: &mock, track_meta: &mock,
        };
        let rule = SmartRule {
            combine: Combine::All,
            predicates: vec![Predicate::NeverPlayed],
            sort: SortBy::Title,
            limit: None,
        };
        let out = rule.evaluate(&lib);
        assert_eq!(out, vec![PathBuf::from("/b.mp3")]);
    }

    #[test]
    fn limit_truncates_after_sort() {
        let tracks: Vec<UiTrackMeta> = (1..=5)
            .map(|i| ui_track(
                &format!("/{i}.mp3"),
                &format!("Track {i}"),
                "X",
                "House",
            ))
            .collect();
        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &Mock::default(),
            favourites: &Mock::default(), history: &Mock::default(),
            track_meta: &Mock::default(),
        };
        let rule = SmartRule {
            combine: Combine::All,
            predicates: vec![Predicate::GenreIn { genres: vec!["House".into()] }],
            sort: SortBy::Title,
            limit: Some(2),
        };
        let out = rule.evaluate(&lib);
        assert_eq!(out.len(), 2);
        // Title-sorted, so Track 1 + Track 2 come first.
        assert_eq!(out[0], PathBuf::from("/1.mp3"));
        assert_eq!(out[1], PathBuf::from("/2.mp3"));
    }

    #[test]
    fn toml_roundtrip_full_rule() {
        // Real-world wizard output — multiple predicate kinds, AND
        // combine, BPM sort, limit. Must serialise to TOML cleanly
        // (this is the on-disk format) and deserialise back to an
        // equal value.
        let rule = SmartRule {
            combine: Combine::All,
            predicates: vec![
                Predicate::BpmRange { min: 120.0, max: 130.0 },
                Predicate::KeyCamelotCompatibleWith { tonic: 0, is_minor: true },
                Predicate::GenreIn { genres: vec!["House".into(), "Deep House".into()] },
                Predicate::IsFavourite,
                Predicate::Not { inner: Box::new(Predicate::NeverPlayed) },
            ],
            sort: SortBy::Bpm,
            limit: Some(50),
        };
        let s = toml::to_string(&rule).expect("encode");
        let back: SmartRule = toml::from_str(&s).expect("decode");
        assert_eq!(back, rule);
    }

    #[test]
    fn has_hot_cues_and_manual_grid() {
        let tracks = vec![
            ui_track("/a.mp3", "A", "X", "House"),
            ui_track("/b.mp3", "B", "X", "House"),
        ];
        let mut mock = Mock::default();
        let mut a_meta = MetaSidecar::default();
        a_meta.hot_cues[2] = Some(64.0);
        mock.meta.insert("/a.mp3".into(), a_meta);
        let mut b_meta = MetaSidecar::default();
        b_meta.grid_override = Some(persistence::GridOverride {
            bpm: 128.0, beat_grid: vec![0.0, 0.5], downbeats: vec![0],
        });
        mock.meta.insert("/b.mp3".into(), b_meta);

        let lib = LibrarySnapshot {
            tracks: &tracks, analysis_cache: &mock, favourites: &mock,
            history: &mock, track_meta: &mock,
        };

        let only_cues = SmartRule {
            combine: Combine::All,
            predicates: vec![Predicate::HasHotCues],
            sort: SortBy::Title, limit: None,
        };
        assert_eq!(only_cues.evaluate(&lib), vec![PathBuf::from("/a.mp3")]);

        let only_grid = SmartRule {
            combine: Combine::All,
            predicates: vec![Predicate::HasManualGrid],
            sort: SortBy::Title, limit: None,
        };
        assert_eq!(only_grid.evaluate(&lib), vec![PathBuf::from("/b.mp3")]);
    }
}
