# Design notes

These are the design notes accumulated during the build. They explain
*why* particular choices were made — the kind of context that's easy to
lose between sessions but useful when revisiting a piece of code months
later.

Not required reading to use or extend the project — [README.md](../../README.md)
and [DESIGN.md](../../DESIGN.md) cover that. These notes are for when
you want the rationale behind a specific decision.

## Index

- [overview.md](overview.md) — project goals, decisions, principles
- [build_sequence.md](build_sequence.md) — original build plan (historical)
- [audio_findings.md](audio_findings.md) — cpal + PipeWire latency spike + workarounds
- [lpd8.md](lpd8.md) — AKAI LPD8 controller used during dev
- [pioneer_cue.md](pioneer_cue.md) — Pioneer CDJ CUE state machine
- [analysis_v1.md](analysis_v1.md) — BPM + beat-grid pipeline
- [pitch_lock.md](pitch_lock.md) — phase vocoder for key-lock
- [beat_align.md](beat_align.md) — phase-aligned auto-cue
- [key_detection.md](key_detection.md) — Krumhansl-Schmuckler + Camelot
- [persistence.md](persistence.md) — favourites + analysis cache file format
- [downbeat_idea.md](downbeat_idea.md) — v1.5 downbeat detection plan
