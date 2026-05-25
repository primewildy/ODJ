# DJ Controller — Design & Architecture

Status: living document, reflects what's in the code as of the latest
commit. The earlier "v1 plan" version of this file lives in git history.

## 1. Goals

- Two-deck DJ controller for Linux, MIDI-driven, low-latency.
- "Pro" enough to actually mix on: BPM/beat detection, key detection,
  auto-sync, phase-aligned cueing, EQ, key-lock.
- Designed so a custom microcontroller (phase-2 RP2040 / ESP32-S3) with
  USB MIDI drops straight in as another producer.

Latency baseline (measured): **2.90 ms** through PipeWire on this machine
at 128 frames × 44.1 kHz F32 stereo.

## 2. Non-goals (still)

- Effects beyond EQ.
- Stem separation (would need an ML model + GPU).
- macOS / Windows.
- Library management beyond a single non-recursive music directory.
- Recording the mix to file.
- Sample-accurate sync between master and cue output streams (separate
  clocks; drift is irrelevant for headphone cueing).

## 3. Threading model

```
┌────────────┐   ┌──────────┐   ┌──────────┐   ┌───────────────────┐
│  Keyboard  │   │   MIDI   │   │   GUI    │   │ Analysis worker    │
│  (egui)    │   │ (midir)  │   │  (egui)  │   │ (background thread)│
└─────┬──────┘   └────┬─────┘   └────┬─────┘   └─────────┬─────────┘
      │               │              │                   │
      │               │              │                   ▼
      │               │              │           AnalysisCache (Mutex)
      │               │              │           + .analysis-cache file
      │               │              │
      └───────────────┴──────────────┘
                      ▼
              Sender (Arc<Mutex<rtrb::Producer<DeckCommand>>>)
                      │
                      ▼
              ┌───────────────┐
              │ DeckCommand   │  SPSC, lock-free, no alloc on drain
              │  ring (rtrb)  │
              └───────┬───────┘
                      ▼
         ┌─────────────────────────────────────┐
         │  Audio thread (cpal callback)       │
         │  ┌─────────┐   ┌─────────┐          │
         │  │ Deck A  │   │ Deck B  │          │
         │  │ State + │   │ State + │          │
         │  │ Pvoc    │   │ Pvoc    │          │
         │  │ EQ      │   │ EQ      │          │
         │  └────┬────┘   └────┬────┘          │
         │       ▼              ▼               │
         │     scratch       scratch  (per deck)│
         │       │              │               │
         │       └──── mix ─────┘               │
         └─────────────────┬───────────────────┘
                           │ per-deck atomics (telemetry)
                           ▼
                ┌──────────────────────────┐
                │ DeckTelemetry (Arc atomics)│
                │ playhead, playing, speed,  │
                │ gain, pitch_lock,          │
                │ beat_align, eq_low, eq_high│
                └──────────┬────────────────┘
                           ▼
                         GUI/UI
```

- **One command ring** (rtrb SPSC). The `Sender` type wraps the producer
  side in `Arc<Mutex<>>` so multiple producers (keyboard, MIDI, GUI) can
  share it. The mutex is producer-side only — the audio thread keeps an
  un-shared `Consumer` and drains lock-free.
- **Per-deck atomics for state telemetry.** No telemetry ring (yet); the
  UI polls atomics each frame. Fine for the data we expose. A ring will
  be needed if we add peak meters or beat-event notifications.
- **Decoder + analysis run on background threads.** Decode goes into
  `Arc<TrackBuffer>` (interleaved f32). Loading a track sends one
  `DeckCommand::LoadTrack { buffer, analysis }` carrying both Arcs — the
  swap is allocation-free on the audio side.
- **Background analysis worker** is spawned once at startup. It iterates
  the music directory, decodes + analyses anything not already in the
  cache, appends each result to disk. UI shows progress in the top bar.

## 4. Crate layout

```
crates/
  control/    # DeckCommand, DeckId, TrackAnalysis, MusicalKey, ring types
  decode/     # symphonia integration → TrackBuffer
  analysis/   # spectral-flux BPM + beat grid, Krumhansl key detection
  audio/      # cpal stream, mixer, EQ biquads, phase vocoder, beat align
  ui/         # eframe app, sortable track table, persistence (favourites
              # + analysis cache), background worker, GUI rendering
src/
  main.rs     # CLI, eframe entry, MIDI thread spawn
  midi.rs     # LPD8 mapping
```

Reasoning: `audio/` is kept dep-light (cpal + rustfft, no UI). The hot
path is auditable in isolation.

## 5. `DeckCommand` (current)

The one command surface. All producers (keyboard, MIDI, GUI, background
loaders) emit these.

```rust
enum DeckCommand {
    LoadTrack { deck, buffer: Arc<TrackBuffer>, analysis: Arc<TrackAnalysis> },
    Play(DeckId),
    Pause(DeckId),
    PlayToggle(DeckId),
    Stop(DeckId),
    SetCue { deck, sample_pos: u64 },
    JumpToCue(DeckId),
    CuePress(DeckId),                    // Pioneer state machine
    CueRelease(DeckId),                  // Pioneer state machine
    Seek { deck, sample_pos: u64 },
    SetSpeed { deck, ratio: f32 },       // persistent
    NudgeSpeed { deck, delta: f32 },     // persistent fine-tune (unused on MIDI)
    SetNudge { deck, offset: f32 },      // temporary while-held
    SetQuantize { deck, on: bool },
    SetGain { deck, gain: f32 },
    SetPitchLock { deck, on: bool },
    SetEqLow { deck, db: f32 },
    SetEqHigh { deck, db: f32 },
    SetBeatAlign { deck, on: bool },
    SetCueOn { deck, on: bool },         // PFL toggle: this deck → cue mix
    Sync { deck },                       // match this deck to the other
}
```

## 6. `TrackAnalysis` (current)

```rust
struct TrackAnalysis {
    analysis_version: u32,
    bpm: f32,
    beat_grid: Vec<f64>,           // beat times in seconds
    duration_secs: f64,
    sample_rate: u32,
    key: Option<MusicalKey>,       // Krumhansl-Schmuckler, Camelot label
}

struct MusicalKey { tonic: u8, is_minor: bool }
```

Schema items from the original plan that are **deferred**: `downbeats`,
`phrase_boundaries`, `auto_cue`, `confidence`. See TODO.md.

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
out := 0
for each deck:
    scratch := 0
    if deck.pitch_lock:
        render_deck_pv(deck, scratch, ...)   // phase vocoder
    else:
        render_deck(deck, scratch, ...)       // vinyl-coupled resampling
    apply_eq(deck, scratch, ...)              // low-shelf + high-shelf biquads
    out += scratch * deck.gain_linear
publish_telemetry(deck_a, deck_b)
```

**Vinyl path** (`render_deck`): linear interpolation from the source
buffer at `playhead`. Per-frame step is
`(src_sr / engine_sr) * (speed_ratio + nudge_offset)`. Mono-source
broadcasts to all out channels; stereo passes through.

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

**EQ** (`crates/audio/src/eq.rs`): RBJ biquad shelving filters, Direct
Form II Transposed, per-stereo-channel state. Set-coefficient methods
preserve the running state so dragging an EQ knob doesn't click.

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

**Beat Align** (global, default ON): when a deck transitions
paused→playing, if the *other* deck is playing and both have analyses,
shift this deck's playhead so its nearest beat aligns *in real time*
with the other deck's nearest beat. Crucially, only the playhead moves
— `cue_frame` stays anchored to the beat marker where Q put it. A
subsequent `CueRelease` returns there cleanly. Commit (Cue Play) keeps
the phase-shifted playhead, so the track continues in beat lock.

**Sync** (UI button): set this deck's `speed_ratio` so its effective
BPM matches the other deck's effective BPM, clamped to ±8%.

**Nudge** (pads, while held): adds ±0.04 (4%) to the effective playback
rate for the duration of the press, returning to base on release.
Doesn't change `speed_ratio` and isn't reflected in BPM telemetry — so
the displayed BPM stays steady while you push or pull.

## 10. Telemetry (per-deck atomics)

```rust
struct DeckTelemetry {
    playhead: Arc<AtomicU64>,         // source-frames
    playing: Arc<AtomicBool>,
    speed: Arc<AtomicU32>,            // f32 bits, base speed only
    gain: Arc<AtomicU32>,             // f32 bits
    pitch_lock: Arc<AtomicBool>,
    beat_align: Arc<AtomicBool>,
    eq_low_db: Arc<AtomicU32>,
    eq_high_db: Arc<AtomicU32>,
}
```

Stored once per audio callback. UI polls in `update()` (60 Hz).
`nudge_offset` is intentionally NOT in telemetry — it's transient and
the published "speed" should reflect the DJ's set tempo, not their
finger pressure.

## 11. UI (eframe / egui)

Single window:

- **Top bar**: app name, MIDI status, "Beat Align" global checkbox,
  library counter / analysis progress.
- **Left side panel** (track picker):
  - Filter textbox.
  - "★ only" checkbox + harmonic-compatibility dropdown (off / Deck A /
    Deck B).
  - Sortable table built with `egui_extras::TableBuilder`: ★ / A / B /
    Title / Artist / Key / BPM. Click a column header to sort, click
    again to flip direction. Title and Artist columns are resizable.
- **Central panel**: Deck A (top), Deck B (bottom), each with:
  - Header: deck label · track title · BPM (effective + base) · Key.
  - Overview waveform (downsampled across the whole track).
  - Scrolling 16-beat zoom view with beat grid overlay (presumed
    downbeats every 4th beat — real downbeat detection is in TODO).
  - Transport row: Play/Pause, CUE (hold for preview), Q, 🔒 key (pitch
    lock), Sync, pitch slider, volume slider, position readout.
  - EQ row: low + high shelf sliders mirroring K3/K4 (or K7/K8).

## 12. MIDI mapping (current)

Hardcoded LPD8 PROG-1 layout in `src/midi.rs`. Schema-driven TOML
mappings (from the original plan) are deferred — see TODO.

Note range covers pads (notes 36–43) and CCs (CC 1–8). See
[README.md](./README.md) for the user-facing controls table.

## 13. Persistence

Two line-based files in the music directory:

- `.favourites` — one absolute path per line. Rewritten on each toggle.
- `.analysis-cache` — `path|bpm|tonic|is_minor|beat0,beat1,...` per line.
  Appended on each successful analysis. `tonic = -1` means key detection
  failed.

Plain text, no dependencies, hand-diffable. Pipe is uncommon in audio
filenames; entries with pipes in their path are skipped rather than
corrupted.

On startup the cache file is loaded into a `HashMap<PathBuf,
CachedAnalysis>` behind a `Mutex`. The background worker reads/writes it
serially; the UI reads it during table render.

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
3. Multi-format decode via symphonia.
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
13. Beat Align (phase-snap on play start).
14. Vinyl-style temporary nudge on pads.
15. Krumhansl-Schmuckler key detection + Camelot labels.
16. Favourites + harmonic-compat filter + background analysis cache.
17. Sortable track table.

## Notable design choices

Per-decision design notes (the *why* behind specific choices — the
brute-force BPM refinement, the cue-frame anchor decision, the LPD8
pad layout, etc.) live in [`docs/notes/`](./docs/notes/). Not required
reading to use or extend the project; useful when you want the
rationale behind a specific decision.
