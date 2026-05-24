# Pioneer CDJ CUE state machine

The CUE button on Pioneer CDJs (CDJ-2000NXS / CDJ-3000) is a single
button with state-dependent behaviour. Researched against the VirtualDJ
CDJ-3000 control mapping doc, the Pioneer DJ blog, and the CDJ-2000NXS
manual (section 26).

## Unified state machine

| State on press | Press action | Release action |
|---|---|---|
| Playing | playhead := cue, pause | no-op |
| Paused (anywhere) | cue := current playhead, start playing (preview) | playhead := cue (the one just set), pause |

**The elegant bit:** a quick tap and a long hold of CUE follow the
same code path. While paused:

- A tap sets the cue with a near-inaudible blip of preview before
  snapping back.
- A hold lets you scrub-preview from cue while held, then snaps back
  on release.

No tap-vs-hold timer is needed. The engine needs an `in_preview: bool`
flag set by `CuePress` when paused, cleared by `CueRelease` (or any
explicit `Play`/`Pause`/`Stop` so a normal play doesn't trigger a
phantom snap-back).

## Pioneer "Cue Play" (commit) behaviour

Pressing PLAY mid-preview commits the preview to actual playback:

- While `in_preview == true`, `PlayToggle` does **not** toggle
  `playing`. It clears `in_preview` only.
- `CueRelease` is then a no-op (in_preview is false) and the track
  continues playing seamlessly.
- Implemented in `PlayToggle`'s match arm in `Mixer::apply`.

This is the "let go of cue while the track keeps playing" behaviour
— a common DJ move when you're previewing and decide yes-this-is-the-spot.

## Where it lives

`crates/audio/src/lib.rs` in `Mixer::apply` — the match arms for
`DeckCommand::CuePress`, `DeckCommand::CueRelease`, and
`DeckCommand::PlayToggle`. State flag is `DeckState::in_preview`.

## Sources

- VirtualDJ CDJ-3000 controls manual
- Pioneer CDJ-2000NXS operating instructions (ManualsLib)
- AlphaTheta Help Center "How to use the 8 Hot Cue buttons" (background)

## Why mimic Pioneer faithfully

Muscle memory transfers. The unified state machine is also small and
elegant enough to be worth defending in code review.

## Extending

When adding a new feature that involves cue interaction (e.g. hot cues),
add a separate `HotCuePress(n)` command — don't add another branch in
`CuePress` and don't fork the state machine. The whole point of the
unified design is that it's small.
