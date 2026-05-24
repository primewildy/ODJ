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
- [ ] **Relative-CC jog wheel handling.** Custom encoders will emit
  value 0x40 ± delta. Map to a magnitude-aware `SetNudge` (faster spin
  → larger temporary offset) or a `Seek` for paused decks.
- [ ] **TOML mapping file** instead of the hardcoded LPD8 layout. The
  v1 plan had a schema sketch — pick that up so different controllers
  can be added without code changes.

## Audio routing

- [ ] **Stereo cue / preview output.** Useful when a second output
  device is available. Without it, the existing CUE preview just routes
  through the main mix.
- [ ] **UI for `--device` selection.** Currently CLI-only.

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
