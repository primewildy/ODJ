# ODJ

A from-scratch DJ controller for Linux, written in Rust. Two decks, sub-3ms
output latency on PipeWire, MIDI controller support, in-app BPM + beat-grid
+ key detection, harmonic-mix filtering, and a phase vocoder for key-locked
tempo control.

Built as a side project; on the road to driving a custom microcontroller
hardware controller (RP2040 / ESP32-S3) speaking USB MIDI.

## Status

Working end-to-end as a two-deck mixing tool you can DJ a set on. Tested
with an AKAI LPD8 as the dev controller against a library of ~550 mp3/flac
tracks.

Built features:
- Two decks, cpal/PipeWire output at 128-frame buffers (~2.9 ms latency).
- Optional second cpal stream for **PFL / cue monitoring** on a separate
  output device (headphones).
- Symphonia-based decode (mp3, flac, wav, aac/m4a).
- Phase-vocoder key-lock (with identity phase-locking for cleaner highs)
  + vinyl-style coupled-pitch mode per deck.
- 2-band shelving EQ per deck (low ≈250 Hz, high ≈4 kHz, ±25..+6 dB).
- Per-deck play-envelope (5 ms fade) — no click on transitions.
- BPM + beat-grid analysis (pure-Rust spectral flux + autocorr + brute
  force phase-aligned refinement, ~0.05 BPM precision).
- Musical key detection (Krumhansl-Schmuckler) in Camelot notation.
- Auto Sync (BPM match) + auto Beat Align (phase-snap on play start).
- Pioneer-style CUE state machine including the "Cue Play" commit.
- Vinyl-style temporary push/pull nudge while held.
- Paused jog = audible scrub (forward + reverse). Click-to-seek on
  either waveform.
- Sortable track list (Title / Artist / Key / BPM), favourites, harmonic
  compatibility filter, background analysis worker with disk persistence.
- LPD8 hardcoded mapping (pads + 8 knobs) with on-screen mirroring.
- **Homebrew hardware controller** prototype under `hardware/` — RP2040
  + optical encoder + arcade buttons + 74HC4051 analog mux. Class-
  compliant USB MIDI; sysex-triggered reboot for one-command re-flash
  (`hardware/flash.sh`).

See [DESIGN.md](./DESIGN.md) for architecture and [TODO.md](./TODO.md) for
roadmap.

## Running

```
cargo run --release -- [--device <cpal-name>] [--cue-device <cpal-name>] \
                      [--midi <port-substring>] [--music-dir <path>]
```

Defaults:
- `--device` unset → uses the cpal `pipewire` device for the master out.
  Override to target a specific sink directly (e.g. an analog speaker
  sink that bypasses a default-sink loopback). When the cpal device name
  is `pipewire`, the `PIPEWIRE_NODE` env var (if set) is honoured.
- `--cue-device` unset → no cue stream is opened; deck `cue` toggles
  have no audible effect. Set this to a second cpal device name (e.g.
  `hw:CARD=USB,DEV=0` for a USB DAC dongle) to enable PFL monitoring.
  The two streams have independent clocks; drift is irrelevant for
  cue since master and cue are heard on separate transducers.
- `--midi LPD8` matches any port whose name contains "LPD8". Set to `""`
  to disable MIDI entirely. The custom hardware enumerates as
  "ODJ Controller", so `--midi "ODJ Controller"` selects it.
- `--music-dir music` (relative to CWD). Audio files in this directory
  (non-recursive) populate the in-app track list.

The first launch with a populated music directory will spend a few minutes
in the background pre-analysing every track (BPM + beat grid + key).
Progress shows in the top bar. Cache file `<music-dir>/.analysis-cache`
makes subsequent launches instant.

## Controls

### Keyboard
- `space` Deck A play/pause toggle
- `c` (hold) Deck A CUE (Pioneer state machine; press while paused at cue
  starts preview, release returns to cue)
- `b` Deck B play/pause toggle
- `n` (hold) Deck B CUE

### LPD8 (factory PROG-1 defaults)

Layout — top row pads/knobs = Deck A, bottom row = Deck B:

```
[K1 K2 K3 K4]    [Pad 5 Pad 6 Pad 7 Pad 8]    ← Deck A
[K5 K6 K7 K8]    [Pad 1 Pad 2 Pad 3 Pad 4]    ← Deck B
```

| Pad | Deck | Function                                    |
|-----|------|---------------------------------------------|
| 5   | A    | Play / pause toggle                         |
| 6   | A    | CUE (Pioneer state machine, hold for preview)|
| 7   | A    | Pull (slower while held — vinyl push/pull)  |
| 8   | A    | Push (faster while held)                    |
| 1   | B    | Play / pause toggle                         |
| 2   | B    | CUE                                         |
| 3   | B    | Pull                                        |
| 4   | B    | Push                                        |

| Knob | Deck | Function                                       |
|------|------|------------------------------------------------|
| K1   | A    | Pitch (CC 64 = unity, ±8% range)              |
| K2   | A    | Volume (linear 0..1)                           |
| K3   | A    | EQ high (CC 64 = flat, ±25..+6 dB)            |
| K4   | A    | EQ low                                         |
| K5   | B    | Pitch                                          |
| K6   | B    | Volume                                         |
| K7   | B    | EQ high                                        |
| K8   | B    | EQ low                                         |

If your LPD8 sends different note/CC numbers, every incoming MIDI message
is logged to stderr — read them off and edit the `match` arms in
`src/midi.rs`.

## Project layout

```
/
├── Cargo.toml          # workspace + binary
├── src/
│   ├── main.rs         # eframe entry, CLI parse, MIDI thread spawn
│   └── midi.rs         # LPD8 / ODJ Controller MIDI input + jog handler
├── crates/
│   ├── control/        # DeckCommand enum, MusicalKey, ring buffer types
│   ├── decode/         # symphonia → TrackBuffer
│   ├── analysis/       # BPM + beat grid (DSP) + key (Krumhansl)
│   ├── audio/          # cpal stream(s), mixer, EQ, phase vocoder,
│   │                   #   beat align, cue ring buffer
│   └── ui/             # eframe app, track table, persistence
└── hardware/           # RP2040 firmware + schematic + flash.sh
    ├── firmware/       # Pico SDK C project
    ├── SCHEMATIC.md    # pinout, wiring tables
    └── flash.sh        # sysex-reboot auto-flash workflow
```

## Acknowledgements

Beat-detection pipeline drew on the standard MIR cookbook (spectral flux
+ autocorrelation, with phase-aligned brute-force refinement). Key
detection uses the Krumhansl-Kessler profiles. Pioneer CDJ CUE behaviour
was researched against the CDJ-3000 / CDJ-2000NXS instruction manuals.
