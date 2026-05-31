//! egui/eframe GUI: track picker on the left, two deck panels stacked on
//! the right. Each deck has: overview waveform, scrolling 16-beat zoom view
//! with beat grid, transport (Play/Pause + CUE), pitch slider, quantize.

mod persistence;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender as StdSender, channel};
use std::time::Duration;

use audio::{DeckTelemetry, Engine, Sender};
use control::{DeckCommand, DeckId, MusicalKey, TrackAnalysis, TrackBuffer};
use eframe::egui;
use egui::{Color32, Pos2, Sense, Stroke, Vec2};
use persistence::{AnalysisCache, CachedAnalysis, Favourites};

const SUPPORTED_EXTS: &[&str] = &["mp3", "flac", "wav", "m4a", "aac", "ogg"];
const OVERVIEW_BUCKETS: usize = 2000;
/// Hi-res peaks density (peaks per source-sample). Smaller = more memory but
/// finer resolution in the scrolling view. 64 ≈ 690 peaks/sec at 44.1k.
const HIRES_SAMPLES_PER_PEAK: usize = 64;
const PITCH_MIN: f32 = 0.92;
const PITCH_MAX: f32 = 1.08;
const ZOOM_BEATS: f64 = 16.0;
const ZOOM_PLAYHEAD_FRAC: f32 = 0.33;

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
        for path in tracks {
            if cache.contains(&path) {
                continue;
            }
            let Ok(buffer) = decode::load_to_buffer(&path) else {
                progress.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let r = analysis::analyse(&buffer);
            cache.insert(
                path.clone(),
                CachedAnalysis {
                    bpm: r.bpm,
                    key: r.key,
                    beats: r.beat_grid,
                },
            );
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
            let (artist, title) = parse_track_name(stem);
            out.push(TrackMeta {
                path: p,
                filename,
                title,
                artist,
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
    /// Parsed display title (best-effort).
    pub title: String,
    /// Parsed artist (empty if not present in the filename).
    pub artist: String,
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
    Key,
    Bpm,
}

#[derive(Debug, Clone, Copy)]
struct SortState {
    column: SortColumn,
    ascending: bool,
}

fn compute_overview(buf: &TrackBuffer, num_buckets: usize) -> Vec<f32> {
    let total = buf.frames();
    let ch = buf.channels.max(1) as usize;
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
                    let v = buf.samples[i0 + c].abs();
                    if v > peak {
                        peak = v;
                    }
                }
            }
            peak
        })
        .collect()
}

fn compute_hires_peaks(buf: &TrackBuffer, samples_per_peak: usize) -> Vec<f32> {
    let total = buf.frames();
    let ch = buf.channels.max(1) as usize;
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
                let v = buf.samples[i0 + c].abs();
                if v > peak {
                    peak = v;
                }
            }
        }
        out.push(peak);
    }
    out
}

struct LoadResult {
    deck: DeckId,
    title: String,
    overview: Vec<f32>,
    hires: Vec<f32>,
    samples_per_hires: usize,
    total_frames: u64,
    sample_rate: u32,
    bpm: f32,
    beat_grid: Vec<f64>,
    key: Option<MusicalKey>,
}

struct DeckUi {
    title: Option<String>,
    overview: Vec<f32>,
    hires: Vec<f32>,
    samples_per_hires: usize,
    total_frames: u64,
    sample_rate: u32,
    bpm: f32,
    beat_grid: Vec<f64>,
    key: Option<MusicalKey>,
    telemetry: DeckTelemetry,
    quantize: bool,
    cue_held: bool,
    loading: bool,
}

impl DeckUi {
    fn new(telemetry: DeckTelemetry) -> Self {
        Self {
            title: None,
            overview: Vec::new(),
            hires: Vec::new(),
            samples_per_hires: HIRES_SAMPLES_PER_PEAK,
            total_frames: 0,
            sample_rate: 0,
            bpm: 0.0,
            beat_grid: Vec::new(),
            key: None,
            telemetry,
            quantize: true,
            cue_held: false,
            loading: false,
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
    deck_a: DeckUi,
    deck_b: DeckUi,
    load_rx: Receiver<LoadResult>,
    load_tx: StdSender<LoadResult>,
    midi_status: String,
    analysis_cache: Arc<AnalysisCache>,
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
}

impl DjApp {
    pub fn new(engine: Engine, music_dir: PathBuf, midi_status: String) -> Self {
        let tracks = scan_music_dir(&music_dir);
        let (load_tx, load_rx) = channel();
        let deck_a = DeckUi::new(engine.telemetry(DeckId::A));
        let deck_b = DeckUi::new(engine.telemetry(DeckId::B));
        let sender = engine.sender();

        let analysis_cache = Arc::new(AnalysisCache::load(&music_dir));
        let favourites = Favourites::load(&music_dir);
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

        Self {
            engine,
            sender,
            music_dir,
            tracks,
            filter: String::new(),
            deck_a,
            deck_b,
            load_rx,
            load_tx,
            midi_status,
            analysis_cache,
            favourites,
            favourites_only: false,
            harmonic_filter: None,
            analysis_progress,
            analysis_total,
            sort: SortState {
                column: SortColumn::Title,
                ascending: true,
            },
            cue_gain: 1.0,
            cue_mix: 1.0,
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
        let tx = self.load_tx.clone();
        let sender = self.sender.clone();
        let cache = Arc::clone(&self.analysis_cache);
        std::thread::spawn(move || {
            let buffer = match decode::load_to_buffer(&path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("decode {} failed: {e}", path.display());
                    return;
                }
            };
            let overview = compute_overview(&buffer, OVERVIEW_BUCKETS);
            let hires = compute_hires_peaks(&buffer, HIRES_SAMPLES_PER_PEAK);
            let total_frames = buffer.frames() as u64;
            let sample_rate = buffer.sample_rate;
            let duration_secs = buffer.duration_secs();

            // Use cached analysis if available, else compute now (which
            // also writes back to the cache).
            let (bpm, beat_grid, key) = if let Some(c) = cache.get(&path) {
                (c.bpm, c.beats, c.key)
            } else {
                let r = analysis::analyse(&buffer);
                let entry = CachedAnalysis {
                    bpm: r.bpm,
                    key: r.key,
                    beats: r.beat_grid.clone(),
                };
                cache.insert(path.clone(), entry);
                (r.bpm, r.beat_grid, r.key)
            };
            let analysis = Arc::new(TrackAnalysis {
                analysis_version: 1,
                bpm,
                beat_grid: beat_grid.clone(),
                duration_secs,
                sample_rate,
                key,
            });

            // Engine receives both buffer and analysis; UI receives overview
            // + hires + beats for drawing.
            let _ = sender.send(DeckCommand::LoadTrack {
                deck,
                buffer,
                analysis,
            });
            let _ = tx.send(LoadResult {
                deck,
                title: name,
                overview,
                hires,
                samples_per_hires: HIRES_SAMPLES_PER_PEAK,
                total_frames,
                sample_rate,
                bpm,
                beat_grid,
                key,
            });
        });
    }

    fn render_track_table(&mut self, ui: &mut egui::Ui) {
        use egui_extras::{Column, TableBuilder};

        // 1) Build filtered + sorted list of indices into self.tracks.
        let filtered_sorted: Vec<usize> = {
            let filter_lower = self.filter.to_lowercase();
            let favs_only = self.favourites_only;
            let target_key = match self.harmonic_filter {
                Some(DeckId::A) => self.deck_a.key,
                Some(DeckId::B) => self.deck_b.key,
                None => None,
            };
            let harmonic_active =
                self.harmonic_filter.is_some() && target_key.is_some();

            let mut indices: Vec<usize> = self
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    if !filter_lower.is_empty() {
                        let hit = m.title.to_lowercase().contains(&filter_lower)
                            || m.artist.to_lowercase().contains(&filter_lower)
                            || m.filename.to_lowercase().contains(&filter_lower);
                        if !hit {
                            return false;
                        }
                    }
                    if favs_only && !self.favourites.contains(&m.path) {
                        return false;
                    }
                    if harmonic_active {
                        let t = target_key.unwrap();
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

            let sort = self.sort;
            indices.sort_by(|&a, &b| {
                let ma = &self.tracks[a];
                let mb = &self.tracks[b];
                let ord = match sort.column {
                    SortColumn::Title => {
                        ma.title.to_lowercase().cmp(&mb.title.to_lowercase())
                    }
                    SortColumn::Artist => {
                        ma.artist.to_lowercase().cmp(&mb.artist.to_lowercase())
                    }
                    SortColumn::Key => {
                        let ka = self.analysis_cache.get(&ma.path).and_then(|c| c.key);
                        let kb = self.analysis_cache.get(&mb.path).and_then(|c| c.key);
                        key_sort_value(ka).cmp(&key_sort_value(kb))
                    }
                    SortColumn::Bpm => {
                        let ba = self
                            .analysis_cache
                            .get(&ma.path)
                            .map(|c| c.bpm)
                            .unwrap_or(0.0);
                        let bb = self
                            .analysis_cache
                            .get(&mb.path)
                            .map(|c| c.bpm)
                            .unwrap_or(0.0);
                        bpm_sort_value(ba).cmp(&bpm_sort_value(bb))
                    }
                };
                if sort.ascending {
                    ord
                } else {
                    ord.reverse()
                }
            });
            indices
        };

        // 2) Render table. Closures borrow self.* immutably. Click outcomes
        //    are stashed in locals and applied after the table returns, so
        //    we don't have a mutable / immutable borrow conflict.
        let mut new_sort: Option<SortColumn> = None;
        let mut fav_toggle: Option<PathBuf> = None;
        let mut load_action: Option<(PathBuf, DeckId)> = None;

        let sort = self.sort;
        let tracks = &self.tracks;
        let cache = self.analysis_cache.as_ref();
        let favs = &self.favourites;

        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::auto())                                  // ★
            .column(Column::auto())                                  // A
            .column(Column::auto())                                  // B
            .column(Column::initial(220.0).resizable(true))          // title
            .column(Column::initial(140.0).resizable(true))          // artist
            .column(Column::auto())                                  // key
            .column(Column::auto())                                  // bpm
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
                    if sort_header(ui, "Key", sort, SortColumn::Key) {
                        new_sort = Some(SortColumn::Key);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, "BPM", sort, SortColumn::Bpm) {
                        new_sort = Some(SortColumn::Bpm);
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
                    row.col(|ui| {
                        if ui.small_button("A").clicked() {
                            load_action = Some((meta.path.clone(), DeckId::A));
                        }
                    });
                    row.col(|ui| {
                        if ui.small_button("B").clicked() {
                            load_action = Some((meta.path.clone(), DeckId::B));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&meta.title);
                    });
                    row.col(|ui| {
                        ui.label(&meta.artist);
                    });
                    row.col(|ui| {
                        ui.label(match key {
                            Some(k) => k.label(),
                            None => "--".to_string(),
                        });
                    });
                    row.col(|ui| {
                        ui.label(if bpm > 0.0 {
                            format!("{:.1}", bpm)
                        } else {
                            "--".to_string()
                        });
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
    }

    fn drain_loads(&mut self) {
        while let Ok(res) = self.load_rx.try_recv() {
            let d = self.deck_mut(res.deck);
            d.title = Some(res.title);
            d.overview = res.overview;
            d.hires = res.hires;
            d.samples_per_hires = res.samples_per_hires;
            d.total_frames = res.total_frames;
            d.sample_rate = res.sample_rate;
            d.bpm = res.bpm;
            d.beat_grid = res.beat_grid;
            d.key = res.key;
            d.loading = false;
        }
    }
}

impl eframe::App for DjApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_loads();
        handle_keys(ctx, &self.sender);

        ctx.request_repaint_after(Duration::from_millis(16));

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("DJ");
                ui.separator();
                ui.label(&self.midi_status);
                ui.separator();
                let mut beat_align = self.deck_a.telemetry.is_beat_aligned();
                if ui.checkbox(&mut beat_align, "Beat Align").changed() {
                    let _ = self.sender.send(DeckCommand::SetBeatAlign {
                        deck: DeckId::A,
                        on: beat_align,
                    });
                    let _ = self.sender.send(DeckCommand::SetBeatAlign {
                        deck: DeckId::B,
                        on: beat_align,
                    });
                }
                ui.separator();
                if ui.add(egui::Slider::new(&mut self.cue_gain, 0.0..=1.5)
                        .text("🎧 vol"))
                    .changed()
                {
                    let _ = self.sender.send(DeckCommand::SetCueGain { gain: self.cue_gain });
                }
                if ui.add(egui::Slider::new(&mut self.cue_mix, 0.0..=1.0)
                        .text("CUE↔MASTER"))
                    .changed()
                {
                    let _ = self.sender.send(DeckCommand::SetCueMix { mix: self.cue_mix });
                }
                ui.separator();
                let analysed = self.analysis_progress.load(Ordering::Relaxed);
                if analysed < self.analysis_total {
                    ui.label(format!(
                        "analysing {}/{}",
                        analysed, self.analysis_total
                    ));
                } else {
                    ui.label(format!(
                        "library: {} tracks ({} analysed)",
                        self.tracks.len(),
                        self.analysis_cache.count(),
                    ));
                }
            });
        });

        egui::SidePanel::left("tracks")
            .resizable(true)
            .default_width(620.0)
            .show(ctx, |ui| {
                ui.heading("Tracks");
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.text_edit_singleline(&mut self.filter);
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.favourites_only, "★ only");
                    ui.separator();
                    let label = match self.harmonic_filter {
                        None => "Compat: off".to_string(),
                        Some(DeckId::A) => "Compat: Deck A".to_string(),
                        Some(DeckId::B) => "Compat: Deck B".to_string(),
                    };
                    egui::ComboBox::from_id_source("compat")
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
                });
                ui.separator();

                self.render_track_table(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            // Top: waveforms stacked so the two zoom (beat-grid) views sit
            // back-to-back — eyeballing two decks' beat positions next to
            // each other is the whole point of this layout.
            //
            //   [Deck A overview ]
            //   [Deck A zoom     ]   ← beat grids adjacent
            //   [Deck B zoom     ]   ←
            //   [Deck B overview ]
            overview_waveform(ui, &self.deck_a, DeckId::A, &self.sender);
            zoom_view(ui, &self.deck_a, DeckId::A, &self.sender);
            zoom_view(ui, &self.deck_b, DeckId::B, &self.sender);
            overview_waveform(ui, &self.deck_b, DeckId::B, &self.sender);

            ui.separator();

            // Bottom: controls in mixer-style columns — Deck A on the
            // left, Deck B on the right. Each column gets its own deck
            // header + transport + pitch/vol + EQ.
            let col_w = (ui.available_width() - 12.0) * 0.5;
            let col_h = ui.available_height();
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(col_w, col_h),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| deck_controls(ui, DeckId::A, &mut self.deck_a, &self.sender),
                );
                ui.separator();
                ui.allocate_ui_with_layout(
                    Vec2::new(col_w, col_h),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| deck_controls(ui, DeckId::B, &mut self.deck_b, &self.sender),
                );
            });
        });
    }
}

/// Renders a deck's header (label + track title + BPM + key) followed by
/// the transport / pitch / vol / EQ rows. Waveforms live above this in the
/// central panel so the two decks' beat grids sit visually adjacent — see
/// the CentralPanel block in `App::update`.
fn deck_controls(ui: &mut egui::Ui, deck: DeckId, d: &mut DeckUi, sender: &Sender) {
    let label = match deck {
        DeckId::A => "Deck A",
        DeckId::B => "Deck B",
    };
    ui.horizontal(|ui| {
        ui.heading(label);
        ui.separator();
        let title = d.title.as_deref().unwrap_or("(no track)");
        ui.label(title);
        ui.separator();
        // Effective BPM = analysis BPM × current speed ratio. Shows what the
        // deck is actually playing at, for visual beat-matching.
        let speed = d.telemetry.current_speed();
        let bpm_str = if d.bpm > 0.0 {
            if (speed - 1.0).abs() < 0.0005 {
                format!("BPM: {:>5.2}", d.bpm)
            } else {
                format!("BPM: {:>5.2} ({:>5.2})", d.bpm * speed, d.bpm)
            }
        } else {
            "BPM: --".to_string()
        };
        ui.label(bpm_str);
        ui.separator();
        let key_str = match d.key {
            Some(k) => format!("Key: {}", k.label()),
            None => "Key: --".to_string(),
        };
        ui.label(key_str);
    });

    ui.horizontal(|ui| {
        let playing = d.telemetry.is_playing();
        let play_label = if playing { "⏸ Pause" } else { "▶ Play" };
        if ui.button(play_label).clicked() {
            let _ = sender.send(DeckCommand::PlayToggle(deck));
        }

        let cue_resp = ui.button("⏮ CUE");
        let now_down = cue_resp.is_pointer_button_down_on();
        match (d.cue_held, now_down) {
            (false, true) => {
                let _ = sender.send(DeckCommand::CuePress(deck));
            }
            (true, false) => {
                let _ = sender.send(DeckCommand::CueRelease(deck));
            }
            _ => {}
        }
        d.cue_held = now_down;

        if ui.checkbox(&mut d.quantize, "Q").changed() {
            let _ = sender.send(DeckCommand::SetQuantize {
                deck,
                on: d.quantize,
            });
        }
        // Pitch lock toggle: when on, tempo changes without pitch change
        // (phase vocoder). When off, vinyl-style coupled pitch+tempo.
        let mut pitch_lock = d.telemetry.is_pitch_locked();
        if ui.checkbox(&mut pitch_lock, "🔒 key").changed() {
            let _ = sender.send(DeckCommand::SetPitchLock {
                deck,
                on: pitch_lock,
            });
        }
        if ui.button("⟲ Sync").clicked() {
            let _ = sender.send(DeckCommand::Sync { deck });
        }
        // PFL / cue monitor toggle. Mirrors deck.cue_on telemetry.
        let mut cue_on = d.telemetry.is_cue_on();
        if ui.toggle_value(&mut cue_on, "🎧 CUE").changed() {
            let _ = sender.send(DeckCommand::SetCueOn { deck, on: cue_on });
        }

        ui.separator();
        let mut speed = d.telemetry.current_speed();
        let r = ui.add(
            egui::Slider::new(&mut speed, PITCH_MIN..=PITCH_MAX)
                .text("pitch")
                .fixed_decimals(3),
        );
        if r.changed() {
            let _ = sender.send(DeckCommand::SetSpeed { deck, ratio: speed });
        }

        let mut gain = d.telemetry.current_gain();
        let r = ui.add(
            egui::Slider::new(&mut gain, 0.0..=1.0)
                .text("vol")
                .fixed_decimals(2),
        );
        if r.changed() {
            let _ = sender.send(DeckCommand::SetGain { deck, gain });
        }

        ui.separator();
        ui.label(format!(
            "{:>+5.2}% | {:.2}s",
            (d.telemetry.current_speed() - 1.0) * 100.0,
            d.playhead_secs(),
        ));
    });

    ui.horizontal(|ui| {
        let mut low = d.telemetry.current_eq_low_db();
        let r = ui.add(
            egui::Slider::new(&mut low, -25.0..=6.0)
                .text("low")
                .fixed_decimals(1),
        );
        if r.changed() {
            let _ = sender.send(DeckCommand::SetEqLow { deck, db: low });
        }
        let mut mid = d.telemetry.current_eq_mid_db();
        let r = ui.add(
            egui::Slider::new(&mut mid, -25.0..=6.0)
                .text("mid")
                .fixed_decimals(1),
        );
        if r.changed() {
            let _ = sender.send(DeckCommand::SetEqMid { deck, db: mid });
        }
        let mut high = d.telemetry.current_eq_high_db();
        let r = ui.add(
            egui::Slider::new(&mut high, -25.0..=6.0)
                .text("high")
                .fixed_decimals(1),
        );
        if r.changed() {
            let _ = sender.send(DeckCommand::SetEqHigh { deck, db: high });
        }
    });
}

fn overview_waveform(ui: &mut egui::Ui, d: &DeckUi, deck: DeckId, sender: &Sender) {
    let desired = Vec2::new(ui.available_width(), 60.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_gray(20));

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
            Color32::DARK_GRAY,
        );
        return;
    }

    let w = rect.width();
    let h = rect.height();
    let mid = rect.center().y;
    let stroke = Stroke::new(1.0, Color32::from_rgb(120, 200, 255));
    let n = d.overview.len();

    let cols = w.ceil() as usize;
    for x in 0..cols {
        let t = x as f32 / cols.max(1) as f32;
        let bucket = (t * n as f32) as usize;
        if bucket >= n {
            break;
        }
        let peak = d.overview[bucket].min(1.0);
        let half = peak * (h * 0.5);
        let x_px = rect.left() + x as f32;
        painter.line_segment(
            [Pos2::new(x_px, mid - half), Pos2::new(x_px, mid + half)],
            stroke,
        );
    }

    let head_frac = d.telemetry.playhead_frames() as f32 / d.total_frames.max(1) as f32;
    let head_x = rect.left() + head_frac.clamp(0.0, 1.0) * w;
    painter.line_segment(
        [
            Pos2::new(head_x, rect.top()),
            Pos2::new(head_x, rect.bottom()),
        ],
        Stroke::new(1.5, Color32::from_rgb(255, 200, 80)),
    );
}

/// Scrolling zoom view: ZOOM_BEATS-wide window around the playhead.
/// Beat grid drawn as vertical lines (every 4th brighter as a presumed
/// downbeat — real downbeat detection is v1.5).
fn zoom_view(ui: &mut egui::Ui, d: &DeckUi, deck: DeckId, sender: &Sender) {
    let desired = Vec2::new(ui.available_width(), 90.0);
    let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::from_gray(15));

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
    if !d.hires.is_empty() && d.sample_rate > 0 {
        let peaks_per_sec = d.sample_rate as f64 / d.samples_per_hires as f64;
        let track_secs = d.total_frames as f64 / d.sample_rate as f64;
        let stroke = Stroke::new(1.0, Color32::from_rgb(120, 200, 255));
        let cols = w.ceil() as usize;
        for x in 0..cols {
            let frac0 = x as f64 / cols.max(1) as f64;
            let frac1 = (x + 1) as f64 / cols.max(1) as f64;
            let t0 = view_start + frac0 * window_secs;
            let t1 = view_start + frac1 * window_secs;
            if t1 <= 0.0 || t0 >= track_secs {
                continue;
            }
            let t0c = t0.max(0.0);
            let t1c = t1.min(track_secs);
            let p0 = (t0c * peaks_per_sec) as usize;
            let p1 = ((t1c * peaks_per_sec) as usize).min(d.hires.len());
            if p0 >= p1 {
                continue;
            }
            let mut peak = 0.0f32;
            for p in p0..p1 {
                if d.hires[p] > peak {
                    peak = d.hires[p];
                }
            }
            let half = peak.min(1.0) * (h * 0.45);
            let x_px = rect.left() + x as f32;
            painter.line_segment(
                [Pos2::new(x_px, mid - half), Pos2::new(x_px, mid + half)],
                stroke,
            );
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
            // Every 4th beat presumed downbeat (4/4 assumption; real
            // downbeat detection lands in v1.5).
            let is_downbeat = i % 4 == 0;
            let (col, stroke_w) = if is_downbeat {
                (Color32::from_rgb(220, 220, 220), 1.5)
            } else {
                (Color32::from_gray(90), 0.8)
            };
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(stroke_w, col),
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
        Stroke::new(2.0, Color32::from_rgb(255, 200, 80)),
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

fn handle_keys(ctx: &egui::Context, sender: &Sender) {
    ctx.input(|i| {
        // Deck A: space play/pause, c hold-cue
        if i.key_pressed(egui::Key::Space) {
            let _ = sender.send(DeckCommand::PlayToggle(DeckId::A));
        }
        if i.key_pressed(egui::Key::C) {
            let _ = sender.send(DeckCommand::CuePress(DeckId::A));
        }
        if i.key_released(egui::Key::C) {
            let _ = sender.send(DeckCommand::CueRelease(DeckId::A));
        }
        // Deck B: b play/pause, n hold-cue
        if i.key_pressed(egui::Key::B) {
            let _ = sender.send(DeckCommand::PlayToggle(DeckId::B));
        }
        if i.key_pressed(egui::Key::N) {
            let _ = sender.send(DeckCommand::CuePress(DeckId::B));
        }
        if i.key_released(egui::Key::N) {
            let _ = sender.send(DeckCommand::CueRelease(DeckId::B));
        }
    });
}
