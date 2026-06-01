# ODJ

A from-scratch DJ controller for Linux, written in Rust. Two decks, sub-3ms
output latency on PipeWire, MIDI controller support, in-app BPM + beat-grid
+ key detection, harmonic-mix filtering, and a phase vocoder for key-locked
tempo control.

Built as a side project; on the road to driving a custom microcontroller
hardware controller (RP2040 / ESP32-S3) speaking USB MIDI.

## Status

Working end-to-end as a two-deck mixing tool you can DJ a set on. Tested
with an AKAI LPD8 + homebrew **ODJ Controller** (`hardware/`) against a
library of ~550 mp3/flac tracks. The breadboard prototype of the
controller (Deck A: jog, transport buttons, 3-band EQ + volume + pitch
faders, Play LED) is fully working. A **two-deck carrier PCB**
(`hardware/pcb/`) is ready for fab — single Pico, JST-XH plug-in panel
connectors for all controls, plug-in CJMCU-4051 mux modules. A
**3D-printable faceplate** (`hardware/enclosure/`, parametric OpenSCAD)
matches it. Cue/PFL output works via a USB DAC dongle with PipeWire
routing handled automatically by `start-dj.sh`.

Built features:
- Two decks, cpal/PipeWire output at 256-frame buffers (~5.8 ms latency
  end-to-end; drops to ~2.9 ms at 128 if you run master only).
- Optional second cpal stream for **PFL / cue monitoring** on a separate
  output device (e.g. a USB DAC). **Headphone bus** with its own volume
  and a Pioneer-style **CUE↔MASTER blend** so you can mix the cued decks
  with the main mix in your headphones.
- Symphonia-based decode (mp3, flac, wav, aac/m4a).
- Phase-vocoder key-lock (with identity phase-locking for cleaner highs)
  + vinyl-style coupled-pitch mode per deck.
- 3-band EQ per deck (low 250 Hz shelf, mid 1 kHz bell, high 4 kHz shelf,
  ±25..+6 dB).
- Per-deck play-envelope (5 ms fade) — no click on transitions.
- BPM + beat-grid analysis (pure-Rust spectral flux + autocorr + brute
  force phase-aligned refinement, ~0.05 BPM precision).
- Downbeat detection via [beat_this](https://github.com/CPJKU/beat_this)
  exported to ONNX and run in-process from Rust via `ort` (CUDA EP).
  Red 4-bar lines on the beat grid; auto-snap of the playhead and cue
  point to the first downbeat on load. Optional — falls back to
  `i%4==0` if the ONNX file isn't present.
- **Live stem separation** via HTDemucs (
  [MansfieldPlumbing/Demucs_v4_TRT](https://huggingface.co/MansfieldPlumbing/Demucs_v4_TRT)
  ONNX) on the GPU through `ort` + TensorRT + FP16. ~7 s/track on an
  RTX 4060 Laptop. Three per-deck rotary knobs (DRUMS / VOCALS /
  INSTRUMENTS), 3-colour overlayed stem waveforms, in-memory
  session cache (not persisted to disk — too big). Tracks load and
  play immediately; the mixer transparently switches from the single
  buffer to the 4-stem mix when the worker finishes.
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
  (`hardware/flash.sh`). Two-deck carrier PCB (one Pico, both decks,
  JST-XH plug-in headers, CJMCU-4051 mux modules) generated from a
  SKiDL netlist (`hardware/pcb/odj_controller.py`) → KiCad → Gerbers.
  Parametric 3D-printed faceplate in `hardware/enclosure/`.

See [DESIGN.md](./DESIGN.md) for architecture and [TODO.md](./TODO.md) for
roadmap.

## Setup

Beyond `cargo` + the system audio stack, two of the bigger features need
out-of-band model files. The app runs fine without either — they're
gracefully optional — but you'll be missing red downbeat lines and stem
controls until they're in place.

All caches live under `~/.cache/dj/` (override with `$XDG_CACHE_HOME`).

### Models — runtime cache layout

| Path | What it is | Size | How to get it |
|---|---|---|---|
| `~/.cache/dj/model_final0.onnx` | beat_this downbeat model | ~85 MB | run [`scripts/export_beat_this_onnx.py`](./scripts/export_beat_this_onnx.py) in a Python venv that has `beat_this` installed, then `cp model_final0.onnx ~/.cache/dj/` |
| `~/.cache/dj/htdemucs.onnx` | HTDemucs stem-separation model | 246 MB | `curl -L -o ~/.cache/dj/htdemucs.onnx https://huggingface.co/MansfieldPlumbing/Demucs_v4_TRT/resolve/main/demucsv4.onnx` |
| `~/.cache/dj/trt-engines/` | TensorRT engine cache | ~190 MB after first build | created automatically on first stem separation (~15 min compile, cached after) |

Both files are runtime-loaded; nothing is bundled into the binary.

### TensorRT — for the fast stem path

The stems crate uses `ort`'s TensorRT execution provider. That needs
TensorRT 10.x libs (`libnvinfer.so.10`, `libnvinfer_plugin.so.10`,
`libnvonnxparser.so.10`) discoverable on `LD_LIBRARY_PATH`. The
easiest way: extract them from PyPI's `tensorrt-cu12-libs` wheel:

```sh
curl -L -o /tmp/trt.whl \
    https://pypi.nvidia.com/tensorrt-cu12-libs/tensorrt_cu12_libs-10.16.1.11-py3-none-manylinux_2_28_x86_64.whl
mkdir -p stem-spike/trt-libs && unzip -q /tmp/trt.whl -d stem-spike/trt-libs
```

`start-dj.sh` picks them up from `stem-spike/trt-libs/tensorrt_libs/`
automatically.

### CUDA — what ort needs at runtime

`ort` ships against CUDA 12 but ships a CUDA EP that's flexible.
On Arch with system CUDA 13.2 you can either:
- Install CUDA 12 alongside (e.g. via the matching `cuda-12.x` AUR
  package), or
- Use PyTorch's bundled CUDA-12 libs from a venv (set up via either
  spike script; PyTorch pulls them as `nvidia-*` packages).

`start-dj.sh` auto-prepends `stem-spike/venv/lib/python*/site-packages/nvidia/*/lib/`
to `LD_LIBRARY_PATH` if it exists, so the venv path "just works".

If neither is in place, `ort` silently falls back to CPU (stems take
~80 s/track instead of ~7 s; downbeat detection still works in
seconds).

### Music

`--music-dir music` by default (relative to CWD). Drop your library
in there; first launch pre-analyses every track in the background
(BPM, beat grid, downbeats, key) and caches results to
`<music-dir>/.analysis-cache`.

## Running

The convenient way (handles PipeWire routing for the cue stream and
restores your normal audio setup on exit):

```
./start-dj.sh
```

That's a thin wrapper around `cargo run --release` with sensible flags
and some PipeWire choreography (see [`docs/notes/pipewire_routing.md`](docs/notes/pipewire_routing.md)).
Or run the binary directly:

```
cargo run --release -- [--device <cpal-name>] [--cue-device <cpal-name>] \
                      [--midi <port-substring>] [--music-dir <path>]
```

Defaults / behaviour:
- `--device` unset → uses the cpal `pipewire` device for the master out.
  Override to target a specific sink directly. When the cpal device name
  is `pipewire`, the `PIPEWIRE_NODE` env var (if set) is honoured.
- `--cue-device` unset → no cue stream is opened; deck `cue` toggles
  have no audible effect. Set this to a cpal device name to enable PFL
  monitoring. If cpal can't find that name in its ALSA enumeration
  (common for USB DACs on PipeWire systems), it falls back to looking up
  a PipeWire sink whose name contains the same substring and routes the
  cue stream to it via `PIPEWIRE_NODE` — so e.g. `--cue-device "KT USB"`
  finds `alsa_output.usb-…KT_USB_Audio…` without needing the full name.
  The two streams have independent clocks; drift is irrelevant for cue
  since master and cue are heard on separate transducers.
- `--midi "ODJ"` is what `start-dj.sh` passes; matches the homebrew
  controller's USB-MIDI port name. The binary's default is `"LPD8"` for
  legacy dev with an AKAI pad; either substring works. Set to `""` to
  disable MIDI entirely.
- `--music-dir music` (relative to CWD). Audio files in this directory
  (non-recursive) populate the in-app track list.

Per-build controller quirks (e.g. a backward-wired fader) go in
`controls.toml` in the working directory — see `controls.toml.example`.
It's read at startup; currently supports `invert_cc = [...]` to flip
specific MIDI CCs. Gitignored since it's hardware-specific.

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
├── start-dj.sh         # launch wrapper — PipeWire routing for the cue
│                       #   stream, auto-restores your normal setup
├── src/
│   ├── main.rs         # eframe entry, CLI parse, MIDI thread spawn
│   └── midi.rs         # LPD8 / ODJ Controller MIDI input + jog handler
├── crates/
│   ├── control/        # DeckCommand enum, MusicalKey, ring buffer types
│   ├── decode/         # symphonia → TrackBuffer
│   ├── analysis/       # BPM + beat grid (DSP) + key (Krumhansl)
│   ├── audio/          # cpal stream(s), mixer, EQ, phase vocoder,
│   │                   #   beat align, cue ring + headphone bus
│   └── ui/             # eframe app, track table, persistence
└── hardware/
    ├── firmware/       # Pico SDK C project (breadboard Deck A)
    ├── pcb/            # two-deck carrier — SKiDL netlist + KiCad
    │   ├── odj_controller.py   # SKiDL → odj_controller.net
    │   └── odj.pretty/         # project-local footprints (Pico,
    │                           #   CJMCU-4051 mux module)
    ├── enclosure/      # 3D-printable faceplate — parametric OpenSCAD
    ├── SCHEMATIC.md    # Deck-A breadboard wiring reference
    └── flash.sh        # sysex-reboot auto-flash workflow
```

## Acknowledgements

Beat-detection pipeline drew on the standard MIR cookbook (spectral flux
+ autocorrelation, with phase-aligned brute-force refinement). Key
detection uses the Krumhansl-Kessler profiles. Pioneer CDJ CUE behaviour
was researched against the CDJ-3000 / CDJ-2000NXS instruction manuals.
