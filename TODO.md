# TODO

Picks up where the last session ended. Organised by theme, roughly in
priority order within each section.

## Polish & quick wins

- [ ] **Cache invalidation.** `.analysis-cache` doesn't track file mtime,
  so a replaced track keeps its old analysis. Add an `mtime` column to
  the cache schema; re-analyse on mismatch. Bump `analysis_version` and
  invalidate when the analyser changes.
- [ ] **Multiple music directories.** Currently one non-recursive
  directory. Either accept multiple `--music-dir` flags, or walk
  recursively, or both.
- [ ] **Persist `pitch_lock` / `beat_align` / per-deck quantize defaults
  between sessions.** Right now they reset to `true` on every launch.
- [ ] **Cheat-sheet overlay.** A `?` shortcut that pops a panel listing
  every keyboard + MIDI binding (so you don't have to grep `midi.rs`).
- [ ] **Drag a `.mp3` onto a deck** as a quick way to load without going
  through the picker.
- [ ] **Parallelise the analysis worker.** Currently one decode+analyse
  at a time. `num_cpus / 2` workers would halve cold-start time.

## Known limitations to fix

- [ ] **Half / double tempo errors.** The soft bias toward [80..180] BPM
  catches most cases but a real half-time tempo (e.g. a 70 BPM dub) will
  get doubled to 140. Need a confidence-based decision: compare phase
  scores at the candidate, ×2, ÷2 and pick the highest *normalised*.
- [ ] **Phase off by a half-beat on some tracks.** When the snare /
  off-beat is louder than the kick, autocorr can lock to the wrong phase.
  Bias the chroma / phase scoring toward low-frequency energy.
- [ ] **Tempo changes mid-track.** Analysis assumes constant tempo and
  a track with a 2 BPM ramp ends up with a grid that drifts. Detection
  would have to window the autocorr.
- [ ] **Key detection on tracks with key changes.** Same problem,
  different feature; one global chroma summary picks the dominant key.
- [ ] **Key detection mode flips.** Krumhansl can pick relative
  major/minor wrong on V-chord-heavy material. Could try a smarter
  profile (Temperley, Sapp) or run two passes.
- [ ] **PV warmup transient.** ~23 ms fade-in when first toggling
  pitch-lock on (OLA accumulator starts empty). Prime by running a
  silent frame or two before the first real audio.
- [ ] **PV transient smearing.** Standard phase vocoder softens kicks
  at ±8% stretch. Either (a) accept it, (b) add transient detection +
  phase-reset, or (c) swap in a WSOLA layer for transient blocks.
- [ ] **Track-number prefix over-strips.** Titles legitimately starting
  with digits (e.g. "1999 - Prince") will be parsed wrong. Could be
  smarter — only strip if remainder still starts with a letter.

## v1.5 features (schema already supports)

- [ ] **Downbeat detection.** The user's design idea: pick a stable
  mid-track window, find a confident downbeat, confirm with a phrase
  boundary (break / drop on a 4/8/16/32-bar grid), back-project to t=0.
  Likely needs an ONNX model (madmom port or beat_this). Schema fields
  `downbeats`, `phrase_boundaries`, `auto_cue` already reserved in the
  original DESIGN.md draft — wire them through TrackAnalysis.
- [ ] **Phrase-boundary highlighting on the zoom waveform.** Faint
  vertical lines at detected section breaks.
- [ ] **Auto cue at musical "1".** Use the back-projected first downbeat
  as the default cue on load, instead of t=0.

## Bigger features

- [ ] **Effects beyond EQ.** At minimum: high-pass / low-pass filter
  (single knob, biquad), simple delay, reverb. Wire as another per-deck
  pre-mix stage.
- [ ] **Master crossfader.** Currently per-deck gain serves as a manual
  crossfade. A real X-fader would be a single -1..+1 control modulating
  both gains in opposite directions with a configurable curve.
- [ ] **Stem-based mixing.** Pre-compute Demucs / hybrid-demucs stems
  per track on the worker thread, cache them, render with per-stem gain
  controls (kill the kick from one deck while letting another's play).
- [ ] **Loop control.** 4/8/16-beat loops mapped to pads. Engine support
  is a small addition to the render loop.
- [ ] **Hot cues.** Multiple stored cue points per track, mapped to
  pads (4–8 typical).
- [ ] **Recording the mix.** Tap the master mix into a file (wav/flac)
  while playing.
- [ ] **Ghost beat overlay on the zoom view.** Draw the *other* deck's
  beat grid as faint lines on this deck's zoom view, so you can see how
  far off-phase you are at a glance.

## Phase-2 hardware prep

- [ ] **14-bit CC support in the MIDI parser.** The pitch fader on the
  custom controller will be MSB+LSB pairs (CC m, CC m+32). Currently we
  only treat single CCs. Engine takes `f32` directly so only the parse
  side needs the work.
- [ ] **Relative-CC jog wheel handling.** The firmware (`hardware/firmware/`)
  emits CC 16 with value `64 ± delta` per scan tick. Host side needs:
  - Translate each CC 16 message into a `SetNudge { delta * scale }`
    proportional to spin speed.
  - A 50–100 ms timer that clears the nudge back to 0 when CC 16
    stops arriving (deck returns to set tempo when you let go of the
    jog).
- [ ] **TOML mapping file** instead of the hardcoded LPD8 layout. The
  v1 plan had a schema sketch — pick that up so different controllers
  can be added without code changes. Becomes more urgent as the hardware
  controller starts adding controls.
- [x] **EQ-mid in the engine.** Added a 1 kHz peaking filter + SetEqMid;
  mid knob (CC 9) drives it. Now a proper 3-band EQ.
- [ ] **Full TOML control mapping.** `controls.toml` currently only does
  per-CC invert. Extend to CC→action remapping + note→action so a new
  controller can be configured without touching `src/midi.rs`.

## Hardware build

See [`hardware/`](./hardware/). (The `hardware-prototype` branch is merged
into `main`.)

- [x] Pico SDK firmware (encoder + buttons + mux ADC → USB MIDI).
- [x] Schematic + build/flash docs.
- [x] Sysex-triggered reboot to BOOTSEL + `flash.sh` workflow.
- [x] Encoder wired and validated. Polled quadrature decoder is fine
  at human spin rates.
- [x] Bidirectional MIDI for Play-button LED feedback.
- [x] Host-side encoder → nudge (playing) + audible scrub (paused).
- [x] All Deck A controls wired + bring-up complete: jog encoder,
  4 buttons, 3-band EQ + volume + pitch via the 74HC4051 mux. Full
  0–127 range verified on every analog channel.
- [ ] **PIO quadrature decoder.** Polled decoder is plenty fast for
  the 600 P/R encoder at human spin rates, but a PIO program would
  free the CPU and survive higher RPMs. Pico SDK has a reference
  example.
- [ ] **Capacitive jog touch** sensor on the platter top (cap-touch
  module or RC charge-time measurement). Required for proper scratch
  detection later.
- [ ] **More LEDs.** Currently only the Play button. Cue, sync, etc.
  follow the same pattern.

## Two-deck PCB

The single-board carrier that drives **both decks** from one Pico. See
[`hardware/pcb/`](./hardware/pcb/). Schematic/netlist is done; layout +
firmware/host extension are pending.

- [x] **Netlist** (`pcb/odj_controller.py`, SKiDL → KiCad). One Pico,
  both decks, two 74HC4051 (shared select, GP26/GP27), 4 buttons/deck
  (Play/Cue/🎧Cue/Sync), jog encoders, Play + 🎧Cue(PFL) LEDs per deck.
  Everything on plug-in headers; all 8 channels of each mux + spare GPIO +
  an I2C expansion header broken out. 43 components, 49 nets, generates clean.
- [ ] **PCB layout + route** in KiCad → Gerbers → JLCPCB. 2-layer.
- [ ] **Firmware: two decks.** Extend `firmware/src/main.c` to a 2nd
  encoder (emit CC 17), a 2nd mux on GP27/ADC1, Deck B buttons (notes
  36/37), the headphone-cue buttons (notes 44 = A, 45 = B), the Sync
  buttons (notes 46 = A, 47 = B), and 4 LED outputs — Play + 🎧Cue(PFL)
  per deck (GP18/GP22 deck A, GP21/GP9 deck B), mirrored from host notes
  40/44 (A) and 36/45 (B).
- [ ] **Host: headphone-cue (PFL) buttons.** Map note 44 → toggle
  `SetCueOn(A)`, note 45 → `SetCueOn(B)`, reading current state from
  telemetry. (Engine + UI already have the toggle; just no MIDI binding.)
- [ ] **Host: PFL-LED feedback.** Extend the `led_watcher` to emit the
  🎧-cue notes (44 = A, 45 = B) when a deck's `cue_on` toggles, so the PFL
  LED tracks "this deck is in the headphones" — plus the Play notes (40/36)
  for both decks. Today it only sends note 40.
- [ ] **Host: Sync buttons.** Map note 46 → `Sync(A)`, note 47 →
  `Sync(B)`. (Engine + UI already have Sync; just no MIDI binding.)
- [ ] **Host: Deck B jog (CC 17).** Generalise the jog/scrub machinery
  in `src/midi.rs` (currently Deck-A-only `JogState`) to both decks.
- [ ] **Host: Deck B mid-EQ CC.** Deck B has low (CC 8) and high (CC 7)
  but no mid; add a CC for the 2nd mux's mid channel.
- [ ] **Multi-port MIDI input.** `midi::start` connects to one port;
  open several so a cheap USB pad controller can drive hot-cues / FX
  alongside the main board. (`Sender` is already cloneable.)
- [ ] **Button-bank expansion** for a full pad grid: either an MCP23017
  I/O expander on the I2C-EXP header, or a standalone USB-MIDI pad
  controller (needs the multi-port input above). 74HC165 shift-register
  scanning is a third option if pin-count ever forces it.

## Audio routing

- [x] **Stereo cue / preview output.** Done. `--cue-device <name>`
  opens a second cpal stream; per-deck `cue` toggles route post-EQ
  pre-fader audio into it. See DESIGN.md §14.
- [ ] **Cue mix bus master volume.** Currently the cue mix is summed at
  unit gain. Add a host-side `cue_gain` and a UI slider / MIDI knob.
- [ ] **Cue clock drift compensation.** Two USB audio devices drift by
  tens of ms over an hour. Resampling the cue stream to match master's
  rate would eliminate this. Not yet noticeable in practice.
- [ ] **UI for `--device` selection.** Currently CLI-only. Could
  enumerate cpal devices at startup with a dropdown.

## Tech debt

- [ ] **Tests.** None right now. Even smoke tests over the analyser
  (synthetic clicks at known BPM → expect that BPM back) would help.
- [ ] **GitHub Actions CI.** Build on push to verify.
- [ ] **Cross-platform.** Linux only. cpal supports macOS/Windows
  natively — should be a small lift once tests exist.
- [ ] **Sender mutex on the producer side.** Cheap but not lock-free.
  If multiple high-rate producers (e.g. real jog encoders sending CCs
  at hundreds of Hz) become an issue, switch to one ring per producer
  and drain N rings on the audio side.
