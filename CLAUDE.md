# CLAUDE.md

Project-specific notes for working on this codebase with Claude Code.

## What this is

A two-deck DJ controller for Linux written in Rust. Built incrementally
over several sessions as a personal project, on the road to driving a
custom MIDI hardware controller. See [README.md](./README.md) for the
user-facing summary and [DESIGN.md](./DESIGN.md) for architecture.

## Build & run

```
cargo build --release        # all crates + binary
cargo run --release          # launch the GUI
```

The first launch with a populated music directory spends a few minutes
in the background pre-analysing tracks. Subsequent launches use the
cache (`<music-dir>/.analysis-cache`).

No test suite yet (see TODO.md).

## Code style observed in this codebase

- **No drive-by comments.** Comments only explain non-obvious *why*
  (e.g. the Pioneer CUE state-machine arm, the half/double BPM bias).
- **Match the existing terseness.** Existing code rarely has docstrings
  on private functions; keep that. Add a comment when the next reader
  would otherwise need to guess.
- **Errors at boundaries only.** Internal code trusts its callers —
  e.g. `render_deck_pv` doesn't re-validate inputs that `Mixer::apply`
  already vetted. User-facing entry points (CLI, MIDI, file I/O) do
  validate.
- **No `unwrap()` in the audio thread.** Audio callbacks must not
  panic. The hot path uses early-return with `let Some(x) = ... else`,
  or default values. The analysis crate is allowed to panic on
  pathological input (it runs off-thread).

## Hot-path discipline (audio crate)

The cpal callback runs at real-time priority. Specific rules:

- **No allocations.** All per-deck buffers (PV input, OLA accumulator,
  scratch, EQ state) are pre-allocated at construction. The mixer's
  shared scratch buffer grows the first time it sees a larger output
  size and stays that size.
- **No locks.** The command ring is a lock-free SPSC (`rtrb`). The
  producer side has a `Mutex` but it's held off-thread by senders only;
  the audio thread holds the consumer alone.
- **No I/O.** No `eprintln!` in the callback or anything it calls. The
  closest violation is the `err_fn` passed to cpal's
  `build_output_stream`, which logs stream errors — those are rare and
  acceptable.
- **Float precision matters.** Phase accumulators wrap to ±π each
  frame to avoid drift. The PV's `hop_a_accum` tracks fractional
  analysis hops so non-integer speeds don't drift over a track.

When extending the engine: if you find yourself wanting a `Vec::new()`
or `format!` in `Mixer::apply` or `render_*`, you've taken a wrong turn.

## Where things live

```
src/main.rs              CLI parse, MIDI thread spawn, eframe entry
src/midi.rs              LPD8 hardcoded mapping (pads, knobs)
crates/control/          DeckCommand enum + shared types
crates/decode/           symphonia → TrackBuffer
crates/analysis/         BPM/beat/key detection (pure DSP)
crates/audio/            cpal stream, mixer, PV, EQ — the hot path
crates/audio/src/pvoc.rs Phase vocoder
crates/audio/src/eq.rs   Biquad EQ
crates/ui/               eframe app
crates/ui/src/persistence.rs   favourites + analysis cache files
```

## Conventions that matter when editing

- **Biquad coefficient updates preserve state.** `set_low_shelf` /
  `set_high_shelf` replace `b0..a2` but leave `s1`/`s2` alone. This is
  why dragging an EQ knob doesn't click. Don't add a "reset on update"
  unless you genuinely want the click.
- **`speed_ratio` is the user-set tempo; `nudge_offset` is the
  while-held push/pull.** Effective playback rate is the sum. Telemetry
  publishes the base (`speed_ratio` only) so the BPM readout doesn't
  flicker during nudge.
- **`cue_frame` is the stable beat anchor; `playhead` carries phase
  offsets.** When Beat Align nudges a deck on play-start, it shifts
  `playhead` only. This way `CueRelease` returns to a clean B-beat
  position and the cue marker on screen stays put.
- **Pitch lock default is ON.** This was an explicit user choice — DJ
  workflows usually want pitch-locked tempo nudging.
- **Beat Align default is ON.** Ditto. Auto-corrects small cue-press
  mis-timing.
- **Track list sort: title ascending by default.** Click headers to
  change. The arrow ▲ / ▼ marks the active column.

## Persistence files (gitignored)

Two files in the user's music directory, both line-based plain text:

- `.favourites` — one absolute path per line.
- `.analysis-cache` — `path|bpm|tonic|is_minor|beats…` per line.

Both are user data; never commit them. The `.gitignore` already excludes
the `music/` directory entirely.

## MIDI development

Default mapping targets an AKAI LPD8 (PROG-1 factory layout). Every
incoming MIDI message is logged to stderr — when adding a new controller,
press buttons / turn knobs and read off the note/CC numbers from stderr,
then edit `src/midi.rs`. Schema-driven TOML mapping is on the TODO.

## Phase-2 hardware on the horizon

The user is planning a custom RP2040 / ESP32-S3 hardware controller
speaking USB MIDI. The engine is already designed for this:
- `Sender` is cloneable; the new controller is just another MIDI
  producer.
- 14-bit CC pairs are on the TODO for the pitch fader.
- Rotary encoders will map to relative-CC `SetNudge` events. The
  while-held `SetNudge` was designed with encoders in mind, not pads
  (pads are the stand-in for now).

If you're adding features, check that they don't preclude this — the
command surface should stay producer-agnostic.

## Things to not change without thinking

- The cpal device selection logic. Picking `default_output_device()`
  doesn't work on PipeWire systems; the name-based fallback is there
  for a reason. See `pick_device` in `crates/audio/src/lib.rs`.
- The Pioneer CUE state machine. It's a small piece of code but it
  encodes the "tap vs hold" + "Cue Play commit" semantics carefully.
  Read the table in DESIGN.md §9 before touching it.
- The analysis pipeline order: spectral flux → autocorr → parabolic →
  half/double bias → brute-force phase-aligned refinement. The
  refinement step is what got the BPM precision from "drifts visibly
  over 16 beats" down to "rock solid". Don't drop it.

## When in doubt

- Check TODO.md before starting bigger features — many ideas have
  prior thinking attached.
- For audio-thread changes, build + run + listen. Type checks alone
  don't catch clicks, drift, or phase artifacts.
- Memory of why each design choice was made lives in the author's
  Claude memory store (not in the repo) and isn't required to make
  changes, but is referenced from DESIGN.md when relevant.
