# Downbeat detection (v1.5 plan)

Most DJ software gets the "1 of bar 1" wrong on tracks with anacrustic
intros (e.g. a 3-beat pickup before the first downbeat, or FX-only
intros). Idea:

1. Pick a stable mid-track analysis window (skip first/last ~10%).
2. Beat-track that window for tempo + phase (the existing v1 analyser
   handles this).
3. Run a *downbeat* model (not just beat detection) to find "the 1"
   within the bar.
4. Find a phrase boundary (break/drop on a 4/8/16/32-bar grid) to
   confirm the bar assignment.
5. Back-project the bar grid to t=0 and store the projected first
   downbeat as the auto-cue point — even if no actual onset exists
   there.

This is how a human DJ ears it out.

## Library landscape

- **madmom** (Python, BiLSTM-based) is the reference for downbeat
  detection. RNNDownBeatProcessor + DBNDownBeatTrackingProcessor.
- **BeatNet**, **beat_this** (2024 CNN+transformer) are newer options.
- All exportable to ONNX → run in-process from Rust via the `ort`
  crate (ONNX Runtime).
- Classical DSP handles beat tracking fine but downbeat detection is
  much weaker without ML.

## v1 design choice

The per-track analysis cache should be **structured** to support this
addition without re-running v1 analysis: separate `bpm`, `beat_grid`,
`downbeats` (indices into `beat_grid`), `phrase_boundaries`,
`auto_cue`, and `analysis_version`. v1 fills `bpm` + `beat_grid` via
DSP; v1.5 fills the rest via an ONNX downbeat model. The audio engine
reads the cache and doesn't care which version produced it.

Status as of writing: schema is partially there (the engine's
`TrackAnalysis` has `bpm` + `beat_grid` + `key`). `downbeats`,
`phrase_boundaries`, and `auto_cue` are still TODO additions.

## Constraints to document

- Assumes 4/4 (fine for ~99% of dance music; mis-handles 3/4, 6/8,
  odd meters).
- Assumes roughly constant tempo in the analysis window.

## Why this is worth doing

It's a real differentiator vs. existing DJ software, and only easy to
do if the data model supports it from the start. v1.5 layering ML on
top of an existing data structure is much less work than a refactor.
