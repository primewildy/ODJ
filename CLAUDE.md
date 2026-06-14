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

Test coverage is targeted, not comprehensive: pure DSP/data helpers in
`crates/ui/src/grid_edit.rs`, the persistence parsers in
`crates/ui/src/persistence.rs`, the auto-mix state machine in
`crates/ui/src/auto_mix.rs`, and the analysis crate's pipeline pieces
(autocorr, key detection). `cargo test --workspace` is fast (<5 s) and
gates the things most likely to silently regress.

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
src/main.rs                     CLI parse, settings/CLI/default
                                  resolution, MIDI + heartbeat + theme
                                  watcher thread spawn, eframe entry
src/midi.rs                     ODJ + LPD8 hardcoded mapping

crates/control/                 DeckCommand, DeckId, TrackAnalysis,
                                  TrackBuffer, MusicalKey
crates/decode/                  symphonia → TrackBuffer (lenient on
                                  m4a quirks; falls back to first-
                                  packet spec when codec params lack
                                  the channel count)
crates/analysis/                spectral-flux BPM + brute-force phase
                                  refinement, ONNX downbeat model,
                                  Krumhansl key, kick-trough alignment
crates/stems/                   HTDemucs ONNX wrapper (ort + CUDA/CPU)
crates/audio/                   cpal streams, Mixer, the hot path
  src/lib.rs                    DeckState + render loop, retire ring
  src/eq.rs                     RBJ biquads (low/mid/high)
  src/pvoc.rs                   streaming phase vocoder (key-lock)
  src/fx.rs                     post-EQ FX — Echo + Schroeder Reverb
crates/ui/                      eframe app + all UI state
  src/lib.rs                    DjApp, panels, widgets, command emit
  src/auto_mix.rs               per-track load worker (decode +
                                  analysis + stems + kick-align) +
                                  AutoMixController
  src/grid_edit.rs              pure grid ops (shifted / skip_beats /
                                  bpm_halved / bpm_doubled /
                                  set_downbeat_at) + unit tests
  src/history.rs                .history file + session grouping
  src/palette.rs                design tokens (accents, neutrals,
                                  stems, hot_cue); light + dark
  src/persistence.rs            AnalysisCache (.analysis-cache),
                                  Favourites (.favourites),
                                  TrackMetaStore (.track-meta —
                                  hot cues + grid overrides)
  src/settings.rs               XDG settings.toml load/save
  src/theme.rs                  gdbus/gsettings system-theme watcher
  src/fonts.rs                  bundled Roboto + JetBrains Mono
```

## Conventions that matter when editing

- **Biquad coefficient updates preserve state.** `set_low_shelf` /
  `set_peaking` / `set_high_shelf` replace `b0..a2` but leave
  `s1`/`s2` alone. This is why dragging an EQ knob doesn't click.
  Don't add a "reset on update" unless you genuinely want the click.
- **`speed_ratio` is the user-set tempo; `nudge_offset` is the
  while-held push/pull.** Effective playback rate is the sum. Telemetry
  publishes the base (`speed_ratio` only) so the BPM readout doesn't
  flicker during nudge.
- **`cue_frame` is the stable beat anchor; `playhead` carries phase
  offsets.** When Beat Align nudges a deck on play-start, it shifts
  `playhead` only. This way `CueRelease` returns to a clean B-beat
  position and the cue marker on screen stays put.
- **Hot-cue state is separate from CUE state.** `hot_cue_preview:
  Option<u8>` and `in_preview: bool` are mutually exclusive engine-side
  — pressing CUE cancels any hot-cue preview and vice versa. Don't
  collapse them; the two presses have different commit semantics.
- **`pending_hot_cue` is the quantised-jump scheduler.** It's checked
  at the top of `render_into` once per audio callback (~5–10 ms
  resolution; way under one beat). Anything that mutates the playhead
  (CuePress, Stop, Seek, LoadTrack, HotCueClear of the target slot,
  another HotCueSetOrJump) must clear it — otherwise the deck will
  teleport mid-mix when the playhead next crosses the stale fire-at
  frame.
- **`UpdateAnalysis` re-runs `beat_align_to` when both decks are
  playing.** Lets the user nudge a wonky grid against a known-good
  reference and audibly converge step-by-step. Don't make
  `UpdateAnalysis` a silent state swap — the audible re-phase is the
  whole point of Grid Adjust.
- **Manual `.track-meta` grid wins over the analyser.** When a track
  has a grid override the load-event path drops both the refined
  analyser swap AND the kick-trough phase shift for that path. If you
  add a new background grid-touching worker, add the same suppression
  check or you'll re-introduce the three-writer race.
- **Pitch lock + Beat Align defaults are ON.** Explicit user choices.
- **Quantize default is ON, per deck.** Hot-cue jumps wait for the
  next beat; CUE-press-while-paused snaps to nearest beat.
- **Track list sort: title ascending by default.** Click headers to
  change. The arrow ▲ / ▼ marks the active column. The filter+sort
  result is cached behind a fingerprint (search text, favourites
  count, analysis cache generation, history generation, sort state) so
  the per-frame `.to_lowercase()` doesn't show up on a profiler.

## Persistence files (gitignored)

Four files in the user's music directory, all line-based plain text:

- `.favourites` — one absolute path per line.
- `.analysis-cache` — versioned `v3|path|bpm|tonic|is_minor|beats|downbeats|duration`
  per line; older v1/v2 entries parse cleanly.
- `.history` — `timestamp|deck|path` per line; grouped into sessions by
  2-hour gap in the History tab.
- `.track-meta` — user-authored: hot cues (frames + labels + colours)
  and manual beat-grid overrides. Format
  `v1|path|key=value;key=value;…`. Forward-compat: unknown keys are
  silently skipped.

A fifth file, **`settings.toml`**, lives at XDG `~/.config/odj/` (NOT
the music dir) — audio device, MIDI port, music dir path, per-deck
defaults. CLI flags > settings > built-in defaults.

All are user data; never commit them. The `.gitignore` already excludes
the `music/` directory entirely. The persistence layer uses atomic
write-temp + rename for the rewrite-on-change files (favourites,
track-meta, settings) so a kill mid-write can't corrupt the on-disk
state. The analysis cache is append-only.

When touching these schemas:

- `.analysis-cache` writer always emits the newest `v<N>` line; parser
  accepts all older versions. Bump `CACHE_VERSION` only when you want
  to force re-analysis (e.g. new field that has to come from the DSP);
  leave it alone for additive on-disk fields and let v1/v2 entries
  read as `Option::None` for the new bit.
- `.track-meta` parser is *deliberately* lenient — unknown fields are
  dropped, partial overrides (`grid_beats` without `grid_bpm`, say)
  are rejected at parse time. Add new fields with the same shape so
  older builds skip them cleanly.

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

## Gotchas (dev environment)

- **Hyprland "Application Not Responding" popup.** On Hyprland 0.48+
  the compositor pops an ANR dialog when a client doesn't pong
  `xdg_wm_base.ping` within `misc.anr_missed_pings × 1.5 s` (default
  1 → ≈3 s). eframe/winit on Wayland stops painting when the window
  is unfocused or on another workspace (compositor stops sending
  `wl_surface::frame` callbacks → paints stall → pongs land late);
  the `ctx.request_repaint()` heartbeat in `src/main.rs` masks
  on-focus blocks but doesn't reliably sidestep the off-focus path,
  even at very high `anr_missed_pings` values. Upstream tracker:
  [egui #5112].
  **Verified June 2026 on eframe 0.34**: the upgrade ships several
  Wayland fixes but doesn't resolve this one — the popup still
  triggers when the window is moved off-workspace. Until #5112
  lands the working fix is to disable the dialog entirely:
  `misc { enable_anr_dialog = false }`. `pkill` /
  `hyprctl killactive` remain available for any real hang.
  Don't reach for in-app hacks — they cost more than they buy until
  eframe ships its own resolution. (We did still ship the
  kick-trough off-thread fix from CODE_REVIEW.md, which was a
  separate on-focus source of the same dialog.)

[egui #5112]: https://github.com/emilk/egui/issues/5112

## Things to not change without thinking

- The cpal device selection logic. Picking `default_output_device()`
  doesn't work on PipeWire systems; the name-based fallback is there
  for a reason. See `pick_device` in `crates/audio/src/lib.rs`.
- The Pioneer CUE state machine. It's a small piece of code but it
  encodes the "tap vs hold" + "Cue Play commit" semantics carefully.
  Read the table in DESIGN.md §9 before touching it.
- The hot-cue / pending-jump state machine. Tabulated in DESIGN.md §9;
  has six callers that can cancel a pending jump and every one of them
  matters. Adding a seventh playhead-mutator? Cancel the pending jump
  there too.
- The analysis pipeline order: spectral flux → autocorr → parabolic →
  half/double bias → brute-force phase-aligned refinement. The
  refinement step is what got the BPM precision from "drifts visibly
  over 16 beats" down to "rock solid". Don't drop it.
- The load-event ordering in `drain_loads`. Initial → (Refined |
  KickAligned) → Stems → KickAligned. The manual-override suppression
  hooks live at the top of each branch — keep them there so a future
  fourth writer for the grid can't slip past.
- The atomic write-temp + rename pattern in `persistence.rs`. A kill
  mid-write on `.favourites` / `.track-meta` / `settings.toml` must
  not corrupt the on-disk file. If you add a new rewrite-on-change
  store, use the same pattern.

## When in doubt

- Check TODO.md before starting bigger features — many ideas have
  prior thinking attached.
- For audio-thread changes, build + run + listen. Type checks alone
  don't catch clicks, drift, or phase artifacts.
- Memory of why each design choice was made lives in the author's
  Claude memory store (not in the repo) and isn't required to make
  changes, but is referenced from DESIGN.md when relevant.
