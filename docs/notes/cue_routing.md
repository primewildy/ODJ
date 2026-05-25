# Cue / PFL routing

Added late in the hardware-prototype branch. Opens a second cpal stream
on a secondary audio device (typically a USB DAC dongle) and routes a
pre-fader-listen mix into it.

## Why

A real DJ workflow needs to monitor an incoming track in headphones
while a different track plays on speakers. Without a separate cue
output you can only listen to the master mix, which kills the workflow:
you can't preview a track at low fader before bringing it up.

Standard DJ convention is "PFL" (pre-fader-listen): the cue bus picks
up the deck's signal **after EQ but before the channel fader**. That
means:

- You can keep a deck's channel fader down (master doesn't hear it)
  while still listening to it in headphones.
- EQ knobs affect both master and cue, so EQ prep work has audible
  feedback in headphones.

## How

When `--cue-device <name>` is set, the engine opens a second cpal
output stream alongside master. A lock-free SPSC ring buffer
(`rtrb`, 4096 stereo samples ≈ 23 ms) sits between them:

```
master callback                            cue callback
─────────────────                          ─────────────────
for each deck:                             pop samples from
  scratch = deck audio (post-EQ, post-      ring; write to
           envelope)                        cue_stream's out;
  master += scratch * gain                  underrun → 0.0
  if deck.cue_on: cue_mix += scratch
push cue_mix → ring
```

The master callback is the only thread that runs the engine. Cue
callback is trivial — pop and write.

## Drift

Two cpal devices have independent clocks. Over an hour the cue
stream's read position drifts against master's write position by tens
of ms. We don't compensate (no resampling). It doesn't matter because:

- Master and cue are heard on separate transducers (speakers and
  headphones); never directly compared in time.
- Underrun (cue clock faster than master) → silence in headphones for
  a few samples. Inaudible.
- Overrun (cue clock slower) → producer drops excess. Cue lags master
  by a bit more over time, but resets effectively the next time the
  ring drains.

## Why not one fancy interface

A proper DJ audio interface (e.g. Native Instruments Audio 4 DJ) has a
single USB device with 4 output channels — master on 1/2, cue on 3/4.
Same clock, no drift. That's the next hardware upgrade but the
two-cheap-dongles approach delivers a working cue path today for £10.

The host-side code is identical either way. With a single multi-channel
device, the cue stream's `--cue-device` could route to a different
channel range of the same device — implementation detail.

## When `--cue-device` is unset

`Mixer::cue_producer` is `None`. The cue mix accumulation is skipped
entirely (`if self.cue_producer.is_some()` guards). Zero overhead.
Deck `cue` toggles in the UI are no-ops audibly but still propagate
through telemetry (so the visual indicator works).

## Where it lives

- `crates/audio/src/lib.rs` — `Engine::start` opens the optional
  second device; `Mixer::render` does the cue accumulation and push;
  `build_cue_stream` is the consumer callback.
- `crates/control/src/lib.rs` — `DeckCommand::SetCueOn { deck, on }`.
- `crates/ui/src/lib.rs` — per-deck `🎧 CUE` toggle.
- `src/main.rs` — `--cue-device` CLI flag.

See also: [audio_findings.md](audio_findings.md), [overview.md](overview.md).
