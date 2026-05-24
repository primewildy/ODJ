# Project overview

A from-scratch DJ controller, built as a side project on the road to
driving a custom microcontroller hardware controller (RP2040 or
ESP32-S3 likely) over USB MIDI.

## Locked-in decisions

- **Language**: Rust. Chosen over Go because of GC concerns plus the
  lack of audio DSP libraries; over C++/JUCE for iteration speed.
- **Platform v1**: Linux only (developed on Arch + PipeWire 1.6.4).
- **Beat metadata**: skip Pioneer/rekordbox import; build own
  BPM/beat detection.
- **GUI**: egui (via eframe).
- **Phase 2**: custom hardware controller with a microcontroller
  speaking USB MIDI. This means **MIDI is a first-class input from
  v1**, not a later addition.

## Architecture principle

One `DeckCommand` enum, multiple producers (keyboard, MIDI, GUI). A
lock-free SPSC ring buffer sits between control producers and the
audio thread. The phase-2 hardware controller becomes just another
MIDI producer — no engine changes required.

Core crate choices: cpal (audio I/O), symphonia (decode), rustfft
(FFTs for analysis + PV), midir (MIDI input), eframe/egui (UI),
egui_extras (sortable table). No C-library dependencies.

**Why these choices:** the goal is a working hobby tool with a clear
upgrade path to real hardware. Keeping MIDI first-class in v1 is what
makes phase-2 hardware drop in cleanly.

**How to apply when extending:** keep the command surface MIDI-friendly
(think CC ranges, note triggers, 14-bit values for high-resolution
faders/jog wheels). Avoid putting GUI-specific assumptions in core
types. Cache analysis results so reloads are instant.
