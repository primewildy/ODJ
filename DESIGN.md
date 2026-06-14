# DJ Controller — Design & Architecture

Status: living document, reflects what's in the code as of the latest
commit. The earlier "v1 plan" version of this file lives in git history.

## 1. Goals

- Two-deck DJ controller for Linux, MIDI-driven, low-latency.
- "Pro" enough to actually mix on: BPM/beat detection, key detection,
  auto-sync, phase-aligned cueing, EQ, key-lock.
- Designed so a custom microcontroller (phase-2 RP2040 / ESP32-S3) with
  USB MIDI drops straight in as another producer.

Latency baseline: **~5.8 ms** through PipeWire at 256 frames × 44.1 kHz
F32 stereo (the default — gives enough headroom for both the master and
cue ALSA streams to schedule without xruns). **2.9 ms** at 128 frames is
fine for single-stream master-only setups; flip `TARGET_BUFFER_FRAMES`
in `crates/audio/src/lib.rs` if you want the lower latency.

## 2. Non-goals (still)

- macOS / Windows.
- Library management beyond a single non-recursive music directory.
- Recording the mix to file (deferred — see FEATURES.md §6).
- Sample-accurate sync between master and cue output streams (separate
  clocks; drift is irrelevant for headphone cueing).

(Stems and beat-synced FX *were* in this list — both are now in the
engine. See §8.)

## 3. Threading model

```
┌──────────┐  ┌────────┐  ┌──────────┐  ┌───────────────┐  ┌──────────────┐
│  MIDI    │  │  GUI   │  │ Analysis │  │ Per-track     │  │ Auto-mix     │
│ (midir)  │  │ (egui) │  │  worker  │  │ load worker   │  │ controller   │
└────┬─────┘  └───┬────┘  └────┬─────┘  │  (decode +    │  │   (poll)     │
     │            │            │        │  refine +     │  └──────┬───────┘
     │            │            │        │  stems +      │         │
     │            │            ▼        │  kick-align)  │         │
     │            │      .analysis-cache└──────┬────────┘         │
     │            │      (HashMap+Mutex)       │                  │
     │            │                            ▼                  │
     │            │                       load_rx channel ───────►│
     │            │                       (LoadEvent::Initial /   │
     │            │                        Refined / Stems /      │
     │            │                        KickAligned / Failed)  │
     └────────────┴────────────┬───────────────────────────────────┘
                               ▼
              Sender (Arc<Mutex<rtrb::Producer<DeckCommand>>>)
                               │
                               ▼
              ┌──────────────────────────┐
              │ DeckCommand SPSC (rtrb)  │  lock-free, alloc-free drain
              └────────────┬─────────────┘
                           ▼
         ┌──────────────────────────────────────────┐
         │  Audio thread (cpal master callback)     │
         │  ┌──────────────┐   ┌──────────────┐     │
         │  │   Deck A     │   │   Deck B     │     │
         │  │ render → EQ  │   │ render → EQ  │     │
         │  │   → FX       │   │   → FX       │     │
         │  │   → envelope │   │   → envelope │     │
         │  └──────┬───────┘   └──────┬───────┘     │
         │         └─────── master ───┘             │
         │         └──── cue mix (PFL) ────► SPSC ──┼─► Cue stream
         │                                          │      (cpal #2,
         │  Retire ring (Arc drops, lock-free) ─────┼──►   own callback)
         └────────────────┬─────────────────────────┘
                          ▼
            DeckTelemetry (per-deck Arc atomics) ◄── GUI polls @ 60 Hz
```

**Threads at runtime:**

| Thread                                  | Spawned in                          | Job |
|-----------------------------------------|-------------------------------------|-----|
| Audio master                            | `cpal::build_output_stream` (RT prio) | Drains command ring, mixes both decks |
| Audio cue                               | `cpal::build_output_stream` (RT prio) | Consumes cue SPSC, pushes to headphones |
| GUI / main                              | eframe                              | UI render, command emission |
| Wayland heartbeat                       | `main.rs`                           | `ctx.request_repaint()` @ 10 Hz so the xdg_wm_base ping doesn't time out off-focus |
| System theme watcher                    | `ui::theme::spawn_watcher`          | Polls `org.freedesktop.appearance` via gdbus; re-applies palette on change |
| MIDI                                    | `midi::start`                       | Reads from the MIDI port, emits `DeckCommand`s |
| Background analysis worker              | `ui::spawn_analysis_worker`         | Iterates music dir, decodes + analyses uncached tracks, appends to `.analysis-cache`. Decode errors logged; `catch_unwind` around the DSP so one pathological file doesn't kill the batch |
| Per-track load worker                   | `ui::auto_mix::spawn_load_worker`   | One thread per `LoadTrack` request: decode, peaks, `LoadEvent::Initial`. If uncached, spawns the *refined analyser* child thread; always spawns the *stem* child thread |
| Refined analyser child                  | nested inside load worker            | Fresh full-quality `analyse()` for tracks not yet in cache; emits `LoadEvent::Refined` + `UpdateAnalysis` |
| Stem worker child                       | nested inside load worker            | HTDemucs ONNX separation (CPU or GPU via `ort`); emits `LoadEvent::Stems` + `SetStems` |
| Kick-align child                        | spawned on `Stems` event             | Cross-correlates the drums stem's kick-trough with the analysis beat grid, emits `LoadEvent::KickAligned` + `UpdateAnalysis` with a phase-shifted grid |
| Retire drain                            | `audio` engine init                 | Background `Consumer<RetiredArc>` so the audio callback never drops an Arc inline |
| Auto-mix controller                     | `ui::auto_mix::AutoMixController::spawn` | Polls deck telemetry, drives the armed → active blend (pre-load next track, ramp gains + drum stems) |

**Why so many child threads.** Decode, refined analysis, stem separation
and kick alignment can each take seconds. Stacking them inline on the
GUI thread would freeze the window (and trip Hyprland's xdg_wm_base
ping); spawning per-task threads keeps the UI responsive and lets the
cheaper tasks land first (peaks → analyser → stems).

**Shared invariants:**

- **One command ring** (rtrb SPSC). The `Sender` type wraps the producer
  side in `Arc<Mutex<>>` so multiple producers can share it. The mutex
  is producer-side only — the audio thread keeps an un-shared `Consumer`
  and drains lock-free.
- **Per-deck atomics for state telemetry.** No telemetry ring; the UI
  polls atomics each frame. A ring will be needed if we add peak meters
  or beat-event notifications.
- **Heavy Arcs drop off-thread.** `LoadTrack` and `UpdateAnalysis`
  *replace* a deck's existing `Arc<TrackBuffer>` / `Arc<TrackAnalysis>` /
  `Arc<TrackStems>` on the audio thread, which pushes the old Arc into
  the retire ring. The retire-drain thread is the only place those Arcs
  actually `Drop`.

## 4. Crate layout

```
crates/
  control/      # DeckCommand, DeckId, TrackAnalysis, TrackBuffer, MusicalKey
  decode/       # symphonia integration → TrackBuffer (lenient m4a/ALAC,
                #   defers channel-count to first decoded packet)
  analysis/     # spectral-flux BPM + beat grid + brute-force phase
                #   refinement, ONNX downbeat model, Krumhansl key,
                #   kick-trough alignment
  stems/        # HTDemucs ONNX wrapper (ort + CUDA/CPU EP); writes
                #   per-session stem cache so reloads are instant
  audio/
    lib.rs      # cpal streams, Mixer, DeckState, retire ring
    eq.rs       # RBJ biquads (low-shelf 250 Hz, peak 1 kHz Q≈0.7,
                #   high-shelf 4 kHz)
    pvoc.rs     # streaming phase vocoder (pitch lock)
    fx.rs       # post-EQ FX chain — beat-synced Echo (Type B) and
                #   Schroeder Reverb (Type A); pre-allocated, no
                #   allocs in apply()
  ui/
    lib.rs            # DjApp, panels, custom widgets, command emission
    auto_mix.rs       # load worker, refined-analyse child, stems
                      #   child, kick-align child, AutoMixController
    grid_edit.rs      # pure beat-grid ops (+ unit tests): shifted,
                      #   skip_beats, bpm_halved, bpm_doubled,
                      #   set_downbeat_at
    history.rs        # session log + setlist export (.history file)
    palette.rs        # design tokens — accents, neutrals, stems,
                      #   hot-cue default. Dark + light variants;
                      #   visuals_from() builds egui::Visuals
    settings.rs       # XDG settings.toml — audio/cue device, MIDI
                      #   port, music dir, per-deck startup defaults
    theme.rs          # gdbus + gsettings system-theme detection
                      #   (Wayland's eframe follow_system_theme is
                      #   macOS/Windows-only)
    persistence.rs    # AnalysisCache (.analysis-cache, v3),
                      #   Favourites (.favourites),
                      #   TrackMetaStore (.track-meta — hot cues,
                      #   labels, colours, manual grid overrides)
    fonts.rs          # bundled Roboto + JetBrains Mono via include_bytes!

src/
  main.rs       # CLI parse, settings/CLI/default resolution, MIDI
                #   thread spawn, eframe entry, Wayland heartbeat,
                #   theme watcher
  midi.rs       # ODJ-controller + LPD8 hardcoded mapping
```

Reasoning: `audio/` is kept dep-light (cpal + rustfft, no UI). The hot
path is auditable in isolation. `stems/` is isolated for the same
reason — ort/CUDA pulls a lot in.

## 5. `DeckCommand` (current)

The one command surface. All producers (keyboard, MIDI, GUI, background
loaders, auto-mix controller) emit these.

```rust
enum DeckCommand {
    // ---- Loading ------------------------------------------------------
    LoadTrack { deck, buffer: Arc<TrackBuffer>, analysis: Arc<TrackAnalysis> },
    UpdateAnalysis { deck, analysis: Arc<TrackAnalysis> },  // swap grid live
    SetStems { deck, stems: Arc<TrackStems> },              // async; renderer
                                                            // routes to stem
                                                            // gains when set
    // ---- Transport ----------------------------------------------------
    Play(DeckId), Pause(DeckId), PlayToggle(DeckId), Stop(DeckId),
    SetCue { deck, sample_pos: u64 },
    JumpToCue(DeckId),
    CuePress(DeckId), CueRelease(DeckId),  // Pioneer state machine
    Seek { deck, sample_pos: u64 },
    Sync { deck },                          // BPM-match the other deck

    // ---- Tempo / pitch ------------------------------------------------
    SetSpeed { deck, ratio: f32 },          // persistent (pitch fader)
    NudgeSpeed { deck, delta: f32 },        // persistent fine-tune
    SetNudge { deck, offset: f32 },         // temporary while-held
    SetQuantize { deck, on: bool },
    SetPitchLock { deck, on: bool },
    SetBeatAlign { deck, on: bool },        // phase-align play-start AND
                                            // re-align after grid edits

    // ---- Mixer --------------------------------------------------------
    SetGain { deck, gain: f32 },            // channel fader, 0..1.5
    SetEqLow { deck, db: f32 },             // shelf 250 Hz, -25..+6 dB
    SetEqMid { deck, db: f32 },             // peaking 1 kHz Q 0.7
    SetEqHigh { deck, db: f32 },            // shelf 4 kHz
    SetStemDrums { deck, gain: f32 },       // 0..1.5
    SetStemVocals { deck, gain: f32 },
    SetStemInstruments { deck, gain: f32 }, // = bass + other summed

    // ---- Cue / PFL / master ------------------------------------------
    SetCueOn { deck, on: bool },            // route this deck to cue bus
    SetCueGain { gain: f32 },               // headphone volume
    SetCueMix { mix: f32 },                 // CUE ↔ MASTER blend in 'phones
    SetMasterGain { gain: f32 },            // global out, post-mix

    // ---- FX (per deck, post-EQ, pre-fader) ---------------------------
    SetFxKind { deck, kind: FxKindId },     // Echo | Reverb
    SetFxOn { deck, on: bool },
    SetFxColour { deck, value: f32 },       // 0..1; Echo: feedback, Reverb: damping
    SetFxTime { deck, value: f32 },         // 0..1; Reverb: room size (Echo uses Beats)
    SetFxMix { deck, value: f32 },          // wet / dry
    SetFxBeats { deck, beats: f32 },        // Echo only; 1/8..4 beats

    // ---- Hot cues (8 slots/deck) -------------------------------------
    HotCueSetOrJump { deck, slot: u8 },     // engine decides set vs jump
    HotCueRelease    { deck, slot: u8 },    // ends preview-while-held
    HotCueClear      { deck, slot: u8 },
    HotCueLoad       { deck, slots: [Option<u64>; 8] },  // bulk load on track open

    // ---- Loops (beat-quantised in the engine) ------------------------
    LoopSetIn  { deck }, LoopSetOut { deck },
    LoopAuto   { deck, beats: u32 },        // one-shot N-beat loop
    LoopHalve  { deck }, LoopDouble { deck },
    LoopExit   { deck },                    // play out this iteration
    LoopClear  { deck },
}
```

The `UpdateAnalysis` handler also re-runs `beat_align_to` when this deck
and the other are both playing — so a grid nudge on a wonky deck
audibly re-phases to a known-good reference deck on every click.

## 6. `TrackAnalysis` (current)

```rust
struct TrackAnalysis {
    analysis_version: u32,
    bpm: f32,
    beat_grid: Vec<f64>,           // beat times in seconds
    downbeats: Vec<u32>,           // indices into beat_grid of bar-1
                                   //   beats (populated by the ONNX
                                   //   model from v2+; empty otherwise
                                   //   and the UI falls back to i % 4)
    duration_secs: f64,
    sample_rate: u32,
    key: Option<MusicalKey>,       // Krumhansl-Schmuckler, Camelot label
}

struct MusicalKey { tonic: u8, is_minor: bool }
```

Schema items from the original plan still **deferred**:
`phrase_boundaries`, `auto_cue`, `confidence`. See TODO.md.

A *manual* grid override per track lives in `.track-meta` (see §13).
When present it replaces the analyser's grid on load AND suppresses the
kick-trough alignment + refined `UpdateAnalysis` sends so the user's
hand-gridded version is the source of truth until they hit "Reset to
analysis" in the Grid Adjust panel.

## 7. Analysis pipeline

In `crates/analysis/src/lib.rs`:

1. Block-average down to ~11 025 Hz mono.
2. STFT (N=1024, hop=512, Hann window) over the whole track.
3. Spectral-flux onset envelope (positive bin differences only).
4. Subtract a local mean over a ~1-second window to detrend.
5. Autocorrelate the envelope; peak in lag range [60..200 BPM].
6. **Parabolic interpolation** around the peak for sub-frame precision.
7. **Half/double bias**: rotate to [80..180] BPM if autocorr landed at
   an octave.
8. **Brute-force refinement**: search candidate BPMs in ±5 BPM at
   0.05 BPM resolution. For each candidate, try 32 sub-frame phase
   offsets and score by mean envelope value at predicted beat positions.
   Pick the global best — this is what fixes drifting beat grids.
9. Generate beat times from the chosen (period, phase).
10. **Chroma** is accumulated alongside the STFT (bin → pitch class
    via MIDI = 69 + 12 log2(freq/440), restricted to 60–4000 Hz). After
    all frames: normalise, Pearson-correlate against 24 Krumhansl-Kessler
    key profiles (12 major + 12 minor) rotated to each tonic. The highest
    correlation wins.

Result: `AnalysisResult { bpm, beat_grid, key }`. The UI wraps it in a
`TrackAnalysis` Arc and ships it via `LoadTrack`.

## 8. Audio rendering

Per audio callback in `crates/audio/src/lib.rs`:

```
out := 0; cue_mix := 0
for each deck:
    scratch := 0
    if pending_hot_cue and playhead has crossed fire-at frame:
        playhead := target_frame   // beat-quantised hot-cue jump fires
    if deck.pitch_lock and effective_speed > 0:
        render_deck_pv(deck, scratch, ...)    // phase vocoder, stem-aware
    else:
        render_deck(deck, scratch, ...)        // vinyl resampler, stem-aware
    apply_eq(deck, scratch, ...)               // low-shelf / mid-peaking / high-shelf
    apply_fx(deck, scratch, ...)               // Echo or Reverb, in-place
    apply_play_envelope(deck, scratch, ...)    // 5 ms click-free ramp
    if deck.cue_on:
        cue_mix += scratch                     // pre-fader, post-FX
    out += scratch * deck.gain_linear
out *= master_gain
publish_telemetry(deck_a, deck_b)
push cue_mix into the cue SPSC ring (if a cue device is open)
```

**Vinyl path** (`render_deck`): linear interpolation from either the
source buffer or the loaded stem buffers (drums / vocals / bass+other
weighted by their gain knobs). Per-frame step is
`(src_sr / engine_sr) * (speed_ratio + nudge_offset)`. Stems must match
the source's sample rate and channel layout — checked at load time;
mismatches fall back to the single-buffer path. Loop wrap fires
at the end of every frame.

**Key-lock path** (`render_deck_pv`): standard streaming phase vocoder.

- N_FFT = 1024, HOP_S = 256 (synthesis hop), HOP_A = HOP_S × speed (with
  fractional accumulator to avoid drift over long playback).
- Hann window both analysis and synthesis (Hann² OLA gain = 1.5 at hop =
  N/4, output scaled by 1/(N·1.5)).
- Phase update: `dphase = wrap(observed_phase - last_phase - expected)`;
  `true_freq = (expected + dphase) / hop_a`; `synth_phase += true_freq ·
  HOP_S`.
- Allocation-free at runtime — all buffers pre-allocated to MAX_HOP_A=512.
- Switched in/out by a per-deck `pitch_lock: bool`; `reset()` clears
  state on toggle to avoid stale OLA replays.

**EQ** (`crates/audio/src/eq.rs`): RBJ biquad filters, Direct Form II
Transposed, per-stereo-channel state. Three bands: low shelf @ 250 Hz,
peaking @ 1 kHz (Q ≈ 0.7), high shelf @ 4 kHz. Set-coefficient methods
preserve the running state so dragging an EQ knob doesn't click.

**FX** (`crates/audio/src/fx.rs`): per-deck post-EQ, pre-fader chain.

- **Echo (Type B)** — tempo-synced delay. The UI's beat picker (1/8,
  1/4, 1/2, 1, 2, 4 beats) maps to `beats` and the chain reads it
  alongside the effective BPM (`analysis_bpm × speed × nudge`) to
  produce a sample-accurate delay length. Changing `beats` crossfades
  the read tap over ~10 ms so the beat picker doesn't click. Feedback
  is the Colour knob; tail decays to silence after `on=false` rather
  than getting hard-zeroed.
- **Reverb (Type A)** — Schroeder: 4 parallel combs + 2 series
  allpasses. Comb tunings come from the Freeverb defaults. Time = room
  size scaling on the combs; Colour = high-frequency damping inside
  the comb feedback loops; Mix = wet/dry.
- All buffers pre-allocated to the worst case at deck construction.
  No allocations, no panics, no I/O in `apply()`.

## 9. Cue & playback semantics

**Pioneer CUE state machine** (in the engine, driven by
`CuePress`/`CueRelease`):

| State on press         | Press action                          | Release action                    |
|------------------------|---------------------------------------|-----------------------------------|
| Playing                | playhead := cue, pause                | no-op                             |
| Paused (anywhere)      | cue := playhead, start playing preview| playhead := cue, pause            |

Tap and hold follow the same code path — a tap is a hold of zero
duration. Mid-preview, pressing PLAY commits ("Cue Play"): `playing`
stays true, `in_preview` clears, and `CueRelease` is then a no-op so
the track keeps playing.

**Quantise** (`Q`, per deck): when ON, the CuePress-paused branch snaps
the new cue to the nearest beat in this deck's grid before starting
preview.

**Beat Align** (per deck, default ON): when this deck transitions
paused→playing, if the *other* deck is playing and both have analyses,
shift this deck's playhead so its nearest beat aligns in real time with
the other deck's nearest beat. Only the playhead moves — `cue_frame`
stays anchored to the beat marker where Q put it. A subsequent
`CueRelease` returns there cleanly. Commit (Cue Play) keeps the
phase-shifted playhead, so the track continues in beat lock.
**Also re-runs on every `UpdateAnalysis`** — so manual grid edits on a
wonky deck audibly snap to a known-good reference deck on each click.

**Hot cues** (8 slots per deck, persisted in `.track-meta`):

| Slot state          | Deck state | Action on `HotCueSetOrJump`                          |
|---------------------|------------|------------------------------------------------------|
| Empty               | any        | Set slot at current playhead, beat-snapped if Q is on |
| Set                 | Playing, Q | Schedule a **pending jump** that fires at the next beat in the *current* playhead's grid; the actual teleport happens in the render loop |
| Set                 | Playing, no Q | Immediate teleport                                |
| Set                 | Paused     | Start a **hot-cue preview**: jump to slot frame, play; `HotCueRelease` returns to slot frame and pauses |

The pending-jump structure (`PendingHotCueJump { target_frame,
fire_at_frame }`) is the audible analog of Q: sloppy press timing still
lands on a beat boundary. Any input that mutates the playhead (CuePress,
Seek, Stop, LoadTrack, another HotCue press, HotCueClear of the
target slot) cancels the pending jump.

`hot_cue_preview: Option<u8>` is separate from `in_preview` (CUE's
preview state) so the two state machines can't crosswire — a CUE press
cancels any active hot-cue preview and vice versa.

**Sync** (UI button): set this deck's `speed_ratio` so its effective
BPM matches the other deck's effective BPM, clamped to ±8%.

**Nudge** (pads / jog scrub, while held): adds to the effective
playback rate for the duration of the press, returning to base on
release. Doesn't change `speed_ratio` and isn't reflected in BPM
telemetry — so the displayed BPM stays steady while you push or pull.

**Loops** (beat-quantised, per deck): `LoopSetIn` captures the playhead
as IN (always snaps to nearest beat); `LoopSetOut` sets OUT and the
engine starts wrapping playhead from OUT back to IN at the end of each
iteration. `LoopHalve` / `LoopDouble` adjust OUT by ±N beats keeping IN
fixed. `LoopAuto { beats }` is a one-shot N-beat loop from the nearest
beat. `LoopExit` lets the current iteration finish and then plays
through.

## 10. Telemetry (per-deck atomics)

```rust
struct DeckTelemetry {
    playhead: Arc<AtomicU64>,                  // source-frames
    playing: Arc<AtomicBool>,
    speed: Arc<AtomicU32>,                     // f32 bits, base only
    gain: Arc<AtomicU32>,
    pitch_lock: Arc<AtomicBool>,
    beat_align: Arc<AtomicBool>,
    eq_low_db: Arc<AtomicU32>,
    eq_mid_db: Arc<AtomicU32>,
    eq_high_db: Arc<AtomicU32>,
    stem_drums: Arc<AtomicU32>,
    stem_vocals: Arc<AtomicU32>,
    stem_instruments: Arc<AtomicU32>,
    stems_loaded: Arc<AtomicBool>,
    cue_on: Arc<AtomicBool>,
    loop_in: Arc<AtomicU64>,                   // u64::MAX = unset sentinel
    loop_out: Arc<AtomicU64>,
    hot_cues: [Arc<AtomicU64>; 8],             // u64::MAX = empty
}
```

Stored once per audio callback. UI polls in `update()` (60 Hz).
`nudge_offset` is intentionally NOT in telemetry — it's transient and
the published "speed" should reflect the DJ's set tempo, not their
finger pressure. `hot_cue_preview` / `pending_hot_cue` aren't exposed
either; they're engine-internal state machines and the UI doesn't need
to see the in-flight phase.

## 11. UI (eframe / egui)

eframe 0.34. Single window. Custom palette + bundled fonts (Roboto and
JetBrains Mono) — see `crates/ui/src/palette.rs` and `fonts.rs`. Theme
follows the OS (`ui::theme::spawn_watcher` polls the desktop portal).

Panel layout:

```
┌──────────────────────────────────────────────────────────────────────┐
│ Top bar: ODJ glyph │ ⚙ │ MIDI chip │ library: N tracks · M analysed │
├──────┬───────────────────────────────────────────────────────────────┤
│ Src  │ Tabs: Tracks · History · Grid Adjust                          │
│ rail │ ────                                                          │
│ ◉ All│ [ Tracks-tab content varies by tab — see below ]              │
│ ♫ Pl │                                                               │
│ 🏷 Gn│      ┌────────── Deck A info row ─────────┐                   │
│ ★ Fv │      │  [A] title · artist · t/-r · k · BPM│                   │
│ ⇄ Sm │      ├────── overview wave (full) ────────┤                   │
│ 🕓 Hs│      ├────── zoom wave (16 beat) ─────────┤                   │
│ ⌗ Gr │      ├────── zoom wave (16 beat) ─────────┤                   │
│      │      ├────── overview wave (full) ────────┤                   │
│      │      │  [B] title · artist · t/-r · k · BPM│                   │
│      │      └────────── Deck B info row ─────────┘                   │
│      │                                                               │
│      │      ┌── Deck A panel ──┐  ┌── Deck B panel ──┐               │
│      │      │  Q  KL  Sync 🎧  │  │   …               │               │
│      │      │  loop strip      │  │                  │               │
│      │      │  hot-cue grid    │  │                  │               │
│      │      │  [EQ ]  [STEMS]  │  │                  │               │
│      │      │  [FX]            │  │                  │               │
│      │      │  pitch │ vol │   │  │                  │               │
│      │      │  PLAY     CUE    │  │                  │               │
│      │      └──────────────────┘  └──────────────────┘               │
│      │      ┌── Shared mix bar ────────────────────┐                  │
│      │      │ Beat-align · Auto-mix · VIEW EQ/Stems│                  │
│      │      │  CUE↔MASTER fader · 🎧 gain · 🔊     │                  │
│      │      └──────────────────────────────────────┘                  │
└──────┴───────────────────────────────────────────────────────────────┘
```

**Source rail** (left, collapsible to icons): All tracks, Playlists,
Genres, Favourites, Similar, History, Grid Adjust. Clicking History or
Grid Adjust also flips the browser tab; clicking Favourites flips the
table filter; Playlists / Genres / Similar are visual placeholders for
now.

**Tracks tab** (browser):
- Search field, then a `TableBuilder` with columns:
  ★ · A · B · Title · Artist · Genre · Key · BPM · Length · Plays.
- Click a header to sort; click again to flip. Title/Artist resizable.
- A / B are 22 px squares that load the track on that deck; they light
  up in the deck's accent when that path is the current load.
- Right-clicking a track row is reserved for future (rate / re-analyse
  trigger).

**History tab**: collapsed-by-default session groupings from
`.history`. Newest session expanded. Each row: time · deck · title.
"Copy as setlist" button puts a plain-text markdown setlist on the
clipboard.

**Grid Adjust tab** (FEATURES.md §2): manual beat-grid editor.
- Deck selector (A/B pill toggle in the deck's accent).
- 🔒 Lock toggle — default *locked every session*. Edits greyed
  out until unlocked.
- Nudge buttons: `« 10 ms`, `« 1 ms`, `1 ms »`, `10 ms »`.
- Skip beats: `« 8 « 4 « 2 « 1  1 » 2 » 4 » 8 »`.
- Tempo / downbeat: `½× BPM`, `2× BPM`, `Set downbeat at ▷`.
- Reset to analysis (drops the override; available only when one is
  set).
- Status: title, BPM, beat count, "manual override" tag when set.
- Each op rebuilds the analysis via pure functions in
  `crates/ui/src/grid_edit.rs` (unit-tested), sends `UpdateAnalysis`
  to the engine, mirrors into `DeckUi.bpm/beat_grid/downbeats` the
  *same frame* (so the waveform redraws live), and writes to
  `.track-meta`.

**Deck panels** (one per deck, mirror-imaged in layout):
- Overline: "DECK A" / "DECK B" in the deck's accent (blue / pink).
- Mode pills: Q, Keylock, Sync, 🎧 Cue.
- Loop strip: IN · OUT · 4 · ½ · ×2 · Exit · CLR · beat-count readout.
- Hot-cue grid: 2 × 4 buttons; click empty to set, click set to jump
  (Q-quantised when on), shift-click clears; right-click opens
  context menu (label / 7 colour swatches / delete).
- EQ well: HIGH · MID · LOW knobs.
- Stems well: DRUMS · VOCALS · INSTR knobs (greyed "drums…" /
  "vocals…" / "instr…" while the stem worker is running; lit when
  `stems_loaded`).
- FX module: kind dropdown (Echo / Reverb) · ON pill · Colour knob ·
  beat picker (Echo) or Time knob (Reverb) · Mix knob.
- Channel strip: PITCH fader · VOL fader · readouts.
- Arcade buttons: PLAY (green fill) · CUE (red outline).

**Shared mix bar** (under both decks; spans only the mixer columns):
- Beat-align pill toggle (green dot) — applies to both decks.
- Auto-mix pill toggle (blue dot) — armed / active.
- VIEW EQ ↔ Stems segmented (deep `inset` fill on the active half).
- CUE ↔ MASTER horizontal fader with centre detent.
- 🎧 cue-gain fader + readout.
- 🔊 master fader + readout.

**Waveforms**: overview is a downsampled peak chart spanning the whole
track; zoom is a 16-beat window centred ~30 % into view. Both render
three translucent stem traces (drums / vocals / instr) when stems are
loaded, single-stream blue otherwise. Beat-grid ticks (downbeats
brighter, regular dimmer); loop region highlight (amber); hot-cue
markers (each in its slot's custom colour, fallback `pal.hot_cue`,
thicker than the beat lines so they don't get lost); playhead in
amber.

**Settings window** (modal, behind the ⚙ in the top bar): audio device
combo (master + cue from `audio::list_output_devices`), MIDI port
combo, music dir path, per-deck pitch-lock + beat-align defaults, MIDI
log toggle. Persists to `settings.toml` (XDG `~/.config/odj/`).

## 12. MIDI mapping (current)

Hardcoded LPD8 PROG-1 layout in `src/midi.rs`. Schema-driven TOML
mappings (from the original plan) are deferred — see TODO.

Note range covers pads (notes 36–43) and CCs (CC 1–8). See
[README.md](./README.md) for the user-facing controls table.

## 13. Persistence

Four line-based files in the music directory + one config file under
XDG. All plain text, no dependencies, hand-diffable. Pipe (`|`) is the
field separator everywhere; entries with `|` in the path are skipped
rather than corrupted.

`.favourites` — one absolute path per line. Rewritten atomically
(write-temp + rename) on each toggle.

`.analysis-cache` — append-only, one line per analysed track. Versioned:

```
v1|path|bpm|tonic|is_minor|beat0,beat1,...
v2|path|bpm|tonic|is_minor|beat0,beat1,...|db0,db1,...
v3|path|bpm|tonic|is_minor|beat0,beat1,...|db0,db1,...|duration_secs
```

- `tonic = -1` means key detection failed.
- `downbeats` are indices into `beat_grid`; empty for v1 (UI falls
  back to `i % 4 == 0`).
- `duration_secs` was added in v3 to drive the track-table Length
  column and the deck info row's total readout; v1/v2 entries on disk
  parse with `duration_secs = None` and the UI falls back to
  `last_beat + 60/bpm`.
- New analyses always write at the newest version. The current
  `CACHE_VERSION` constant gates which on-disk entries the worker
  treats as "needs re-analysis" — bumping it forces re-analysis of
  everything below it. Additive-only schema changes (like v3 duration)
  *don't* bump the gate so the user doesn't lose their analysis cache.

`.history` — session play log. Each play of a track for ≥30 seconds
appends a `timestamp|deck|path` line. The history view groups them
into sessions by 2-hour gap.

`.track-meta` — user-authored per-track metadata (hot cues + manual
grid overrides). Pipe-separated header + `key=value;key=value` payload:

```
v1|<path>|<field>;<field>;...
```

Known fields:

| Field             | Value                                                          |
|-------------------|----------------------------------------------------------------|
| `hot_cues`        | 8 comma-separated f64 seconds (empty = unset)                  |
| `hot_cue_labels`  | 8 `:`-separated labels (sanitised — `:;`,\n` → `_`)          |
| `hot_cue_colours` | 8 comma-separated `RRGGBB` hex (empty = palette default)       |
| `grid_bpm`        | f32 — manual grid's BPM                                        |
| `grid_beats`      | comma-separated f64 beat times in seconds                      |
| `grid_downbeats`  | comma-separated u32 indices into `grid_beats`                  |

The presence of `grid_beats` marks the track as manually-gridded.
When loaded, the manual grid:

1. Replaces the cache/empty grid on `LoadEvent::Initial` (engine via
   `UpdateAnalysis`, UI mirror inline).
2. Suppresses the `LoadEvent::Refined` analyser swap for that path.
3. Suppresses the `LoadEvent::KickAligned` phase shift for that path.

The grid override stays in force until the user hits "Reset to
analysis" in the Grid Adjust panel, which calls
`set_grid_override(path, None)` and lets the regular flow take over
again on the next load.

The parser silently skips unknown fields → forward-compat: adding a
new TrackMeta key (saved loops, etc.) won't break older builds.

Settings (audio device, cue device, MIDI port, music dir, per-deck
defaults, MIDI log toggle) live in **`settings.toml`** under XDG
`~/.config/odj/` — not in the music directory. CLI flags > settings >
built-in defaults, resolved once in `main.rs`.

All persistence stores load into in-memory maps at startup. Writes go
through atomic write-temp + rename for the rewrite-on-change files
(`.favourites`, `.track-meta`, `settings.toml`); `.analysis-cache` is
append-only.

## 14. Cue / PFL routing

When a second cpal device is configured (`--cue-device <name>`), the
engine opens a separate output stream alongside master and accumulates
a parallel "cue mix" each callback:

```
For each deck (post-EQ, post-fade-envelope scratch):
    master_out  += scratch * deck.gain_linear     (channel fader)
    if deck.cue_on:
        cue_mix += scratch                         (pre-fader, no gain)

After both decks:
    push cue_mix into a lock-free SPSC ring (`rtrb::Producer<f32>`)
```

The cue stream's cpal callback is a trivial consumer:

```
for each sample in out:
    *sample = cue_consumer.pop().unwrap_or(0.0)
```

**Ring size:** 4096 stereo samples ≈ 23 ms at 44.1 k. Plenty for jitter
between the two stream callbacks.

**Drift:** the two devices have independent clocks. Over a long
session the cue stream's position drifts relative to master by up to
tens of milliseconds — pop-ups (consumer ahead) appear as silence in
headphones, full ring (consumer behind) drops samples from the
producer side. **Both modes are inaudible during normal cue use**:
master and cue are heard on separate transducers (speakers vs
headphones); never directly compared.

**Why pre-fader:** standard DJ PFL semantics. The channel fader stays
down for the deck you're cueing (master doesn't hear it), but the
cue bus picks up the post-EQ signal so you can tweak EQ during prep
with audible feedback in headphones.

**Multiple decks cued at once** sum into the cue bus — useful for
checking a phrase transition between two playing tracks.

If no `--cue-device` is given, the entire cue path is bypassed
(`Mixer::cue_producer` is `None`); zero overhead.

## 15. Per-process device routing

cpal's `default_output_device()` returns a broken "default" ALSA PCM on
PipeWire. We explicitly find the cpal device named `"pipewire"` and use
it. For routing to a specific sink (e.g. bypassing a default mono-summing
loopback) the `--device <pipewire-node>` CLI flag sets `PIPEWIRE_NODE`
before audio init. This is **per-process** — does not change the user's
global default sink. See `src/main.rs` and `crates/audio/src/lib.rs::pick_device`.

## 16. Phase-2 hardware (still in the plan)

Custom controller built around RP2040 or ESP32-S3 + TinyUSB MIDI. Plan:

- 14-bit MIDI CC (MSB + LSB pair) for the pitch fader. The current
  engine accepts `SetSpeed { ratio: f32 }` directly; only the MIDI parse
  side needs the 14-bit reassembly.
- Rotary encoders for jog wheels: emit relative CC (e.g. CC value 0x40 ±
  delta). Map to a momentary `SetNudge` while spinning, with magnitude
  proportional to rotation speed.
- Class-compliant USB MIDI → no host driver work; midir picks it up the
  same as the LPD8.

## 17. Build sequence (historical)

The project was built in roughly this order, captured here for context:

1. Spike: prove cpal/PipeWire latency. Got 2.9 ms.
2. Scaffold workspace + first single-deck WAV playback from keyboard.
3. Multi-format decode via symphonia (mp3, flac, wav; aac/alac added
   later — see `crates/decode/src/lib.rs` for the lenient channel-
   layout fallback that handles m4a quirks).
4. MIDI integration (LPD8) + cloneable `Sender`.
5. Pioneer CUE state machine + "Cue Play" commit.
6. Two-deck engine + per-deck pitch via resampling.
7. egui GUI with overview waveform + transport.
8. BPM + beat grid analysis (incrementally refined: integer-lag autocorr
   → parabolic refinement → half/double bias → brute-force phase-aligned
   refinement at 0.05 BPM steps).
9. Scrolling 16-beat zoom view.
10. Quantise cue to nearest beat.
11. Volume + EQ + Sync.
12. Phase vocoder for key-lock.
13. Beat Align (phase-snap on play start, later also on `UpdateAnalysis`).
14. Vinyl-style temporary nudge on pads.
15. Krumhansl-Schmuckler key detection + Camelot labels.
16. Favourites + harmonic-compat filter + background analysis cache.
17. Sortable track table.
18. ONNX downbeat model (beat_this) + cache schema v2 with downbeats.
19. HTDemucs stem separation crate; three-stem mix (drums / vocals /
    bass+other) with translucent overlay waveforms.
20. Kick-trough phase alignment (refines the grid after stems land).
21. Auto-mix: armed → active controller, gain + drum-stem blend.
22. Beat-synced Echo + Schroeder Reverb (per-deck post-EQ chain).
23. Master crossfader removed; replaced with CUE↔MASTER fader, master
    output gain, and dedicated cue-gain control.
24. Eight-slot loop-strip + auto-loop pill (`LoopAuto { beats }`).
25. Settings UI + XDG `settings.toml`; CLI > settings > defaults.
26. eframe 0.28 → 0.34 (multi-stage; mostly mechanical API
    deprecations + the `App::update → App::ui` migration).
27. UI refresh against the Claude Design handoff: source rail, info
    rows flanking the waveforms, EQ + Stems as raised wells, custom
    h/v faders, knob arc track + value arc, palette tokens, bundled
    fonts, theme-from-OS watcher.
28. Session history + setlist export.
29. Hot cues (FEATURES.md §3): 8 slots, persisted in `.track-meta`,
    quantised jump (defers to next beat when Q is on), waveform
    ticks coloured per slot, right-click context menu for label /
    colour / delete.
30. Beat-grid editing (FEATURES.md §2): pure ops in
    `crates/ui/src/grid_edit.rs`, Grid Adjust source-rail tab, manual
    override persisted in `.track-meta`, three-writer race in the
    load worker resolved (manual grid wins over both kick-align and
    refined analyser).
31. Track length restored: cache schema v3, Length column in the
    track table, total in the deck info row alongside played/-remaining.

## Notable design choices

Per-decision design notes (the *why* behind specific choices — the
brute-force BPM refinement, the cue-frame anchor decision, the LPD8
pad layout, etc.) live in [`docs/notes/`](./docs/notes/). Not required
reading to use or extend the project; useful when you want the
rationale behind a specific decision.
