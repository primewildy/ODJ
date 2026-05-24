# AKAI LPD8 (development MIDI controller)

An **AKAI LPD8** was used as the development MIDI controller during the
build.

## Detection

- USB: `09e8:0075` AKAI Professional M.I. Corp. LPD8
- ALSA sequencer client: `20:0 LPD8 MIDI 1`
- ALSA rawmidi: `hw:1,0,0`
- midir picks it up via either the alsaseq or alsaraw backend.

## Hardware

8 velocity-sensitive pads (note on/off), 8 knobs (CC), 4 programmable
banks. Class-compliant USB MIDI, no driver needed.

## Constraints worth knowing

It is **not** a DJ controller — no jog wheels, no faders. It's
sufficient as a development surrogate for "play/pause" (pad →
note_on) and "tempo nudge / gain" (knob → CC), but the realistic
control surface for jog/pitch is still keyboard for now. Long-term
plan is the project's own phase-2 hardware with rotary encoders for
jog + pitch.

## Why hardcode the mapping (for now)

Avoids guessing what controller assumptions to bake into a schema
abstraction. The schema-driven TOML mapping is on the TODO list — it
needs to work for an LPD8 *and* the future custom hardware. Both
produce standard MIDI messages and both consume the same
`DeckCommand` enum.

## Default bindings

See the [main README](../../README.md) for the full table of pad/knob
bindings. They follow factory PROG-1 defaults.

Pads 4 and 8 (top-right of each row) are unmapped — held as reserved
slots for future features.
