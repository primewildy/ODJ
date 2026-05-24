# Beat Align (phase-snap on play start)

Distinct from `Sync` (which matches tempo / BPM only). Beat Align is
**phase** matching: corrects small cue mis-timing automatically so two
decks' beat grids land on the same instant in real time.

## Trigger

Any command that transitions a deck from `playing=false` to
`playing=true`. In practice that's `PlayToggle` and `CuePress` (paused
branch).

Implemented in `Mixer::apply` as a *post-apply step* — after the
per-deck mutation finishes, both decks are re-borrowed and
`beat_align_to(this, other)` is called if:

- `this.beat_align == true`, and
- `other.playing == true`, and
- both decks have non-empty `beat_grid` in their analyses.

## Math

For each deck, compute source-time phase = `playhead_secs -
nearest_beat_secs`. Convert to real-time by dividing by `speed_ratio`
(so post-Sync, both decks share a real-time beat period). Shift `this`
by the difference in real-time phases, wrapped to ±half a beat period
for the smallest signed shift. Convert back to `this`'s source-frames
and update `playhead`.

**Important**: only `playhead` is updated, **not** `cue_frame`. The cue
marker stays anchored to wherever Q-quantise (or the user) put it — on
a beat marker. A subsequent `CueRelease` returns to a clean beat
position. Commit (Cue Play) keeps the phase-shifted playhead, so the
track continues in beat lock.

## Defaults

`beat_align` defaults to `true` in `DeckState::new` and
`DeckTelemetry::new`. So does `pitch_lock` — small tempo adjustments
to align beats shouldn't change pitch.

## UI

Single global checkbox in the top bar labelled "Beat Align". Toggling
sends `SetBeatAlign` to both decks. The UI reads `deck_a` telemetry
for the displayed state on the assumption both are in lockstep.

## Why this design (vs. continuous sync)

Continuous correction would constantly nudge the playhead which would
either glitch the audio (instant jumps) or require complex tempo-bending.
Snapping only on play-start avoids both, and matches the typical DJ
workflow: "correct any slight mistake in my press of the cue button".
Phase doesn't drift afterwards because `Sync` has already matched the
effective BPMs.

## Known limits

- Assumes analysis BPMs / beat grids are accurate. If a deck's
  analysis is half-tempo or phase-shifted by a beat, alignment will
  be off.
- Doesn't reset if BPMs drift apart while playing (e.g. user wiggles
  pitch knob mid-mix). That's by design — they're driving.
- A small click is possible when the playhead jumps. In PV mode it's
  softer (PV smooths transitions); in vinyl mode it's a direct sample
  jump.
