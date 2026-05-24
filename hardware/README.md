# ODJ hardware controller

Firmware + wiring for a homebrew DJ controller around the Raspberry Pi
Pico (RP2040). Companion to the [main project](../README.md).

Phase 1 target: Deck A only — 1 jog encoder, 4 arcade buttons, 4 analog
inputs (3 EQ + 1 volume fader) via a 74HC4051 mux.

Looks like a class-compliant USB MIDI device to the host. Plug it in and
the existing app picks it up via `midir` the same way it does the LPD8.

## Files

- [`SCHEMATIC.md`](./SCHEMATIC.md) — pin assignments, mux channel map,
  wiring diagram, build order.
- [`firmware/`](./firmware/) — Pico SDK C project.
  - `CMakeLists.txt` — build configuration.
  - `src/main.c` — main loop: encoder polling, button debounce, mux
    scan, MIDI send.
  - `src/usb_descriptors.c` — USB device + MIDI interface descriptors.
  - `src/tusb_config.h` — TinyUSB config (MIDI device, no host).

## Building the firmware

### One-time setup

Install the Pico C/C++ SDK and toolchain. On Arch:

```
sudo pacman -S arm-none-eabi-gcc arm-none-eabi-newlib cmake
git clone --depth 1 -b master https://github.com/raspberrypi/pico-sdk.git ~/pico-sdk
cd ~/pico-sdk
git submodule update --init
echo 'export PICO_SDK_PATH=$HOME/pico-sdk' >> ~/.bashrc   # or ~/.zshrc
source ~/.bashrc
```

(Debian/Ubuntu: `apt install gcc-arm-none-eabi libnewlib-arm-none-eabi
cmake build-essential` — same SDK clone otherwise.)

### Build

```
cd hardware/firmware
mkdir build && cd build
cmake ..
make -j
```

Output: `odj_controller.uf2` in the build directory.

### Flash

1. Hold the **BOOTSEL** button on the Pico while plugging in USB.
2. The Pico mounts as a USB drive called `RPI-RP2`.
3. Drag `odj_controller.uf2` onto it.
4. The Pico reboots into the firmware and now enumerates as a USB MIDI
   device named "ODJ Controller".

To re-flash after code changes, repeat (BOOTSEL on plug-in, copy
`.uf2`). Or use `picotool` if you've got it installed:

```
picotool load -f -x odj_controller.uf2
```

## Testing without the host app

Verify the device shows up and sends sensible MIDI:

```
# Check Linux sees a new MIDI port:
amidi -l

# Dump everything coming from it:
amidi -p hw:<n>,0,0 -d
# or via the ALSA sequencer:
aconnect -i -l
aseqdump -p "ODJ Controller"
```

Turn the encoder slowly — should see CC 16 messages with values just
above or below 64. Press a button — `noteon`/`noteoff` on notes 40-43.
Move a fader / turn a knob — CC on 1, 2, 3, 4, or 9 depending on which
mux channel is connected.

## MIDI mapping (host-side perspective)

Designed to look like an LPD8 in PROG-1 mode so the existing
`src/midi.rs` mapping in the host app handles it with **zero code
changes** to start with:

| Hardware  | MIDI                              | App reads as                |
|-----------|-----------------------------------|-----------------------------|
| Button 1  | note 40, ch 1                     | Deck A play/pause toggle    |
| Button 2  | note 41 on press / off on release | Deck A CUE (Pioneer state)  |
| Button 3  | note 42                           | Deck A nudge − (while held) |
| Button 4  | note 43                           | Deck A nudge + (while held) |
| Mux ch 0 (EQ low)  | CC 4                     | Deck A EQ low               |
| Mux ch 1 (EQ mid)  | CC 9                     | (no engine handler yet)     |
| Mux ch 2 (EQ high) | CC 3                     | Deck A EQ high              |
| Mux ch 3 (Volume)  | CC 2                     | Deck A gain                 |
| Mux ch 4 (Pitch)   | CC 1                     | Deck A pitch                |
| **Encoder**       | CC 16 (relative)                   | **no engine handler yet**   |

The encoder needs a handler in `src/midi.rs` on the app side — that's a
next-session task. For now you can verify it via `aseqdump` that the CC
values change as expected.

## Run the host app against the new device

```
cargo run --release -- --midi "ODJ Controller"
```

(If the substring `LPD8` is what's plugged in, it'd still match — both
work simultaneously if both are connected.)

## When something doesn't work

- **Device not appearing**: re-flash with BOOTSEL. Check `dmesg` for USB
  errors. Confirm the Pico isn't in mass-storage mode (it would have a
  `RPI-RP2` drive mounted if so).
- **Encoder doesn't count or counts wrong direction**: swap the A and B
  wires (green ↔ white). Or flip in firmware.
- **Button reads inverted**: probably wired to 3V3 instead of GND. The
  firmware expects active-low with internal pull-ups.
- **Pots read backward**: swap the two outer terminals on that pot.
- **One pot reads garbage**: check the corresponding mux channel
  binding in `SCHEMATIC.md` — the firmware assumes Y0=low EQ, Y1=mid,
  Y2=high, Y3=vol, Y4=pitch.
- **Pico won't boot after a flash**: hold BOOTSEL, plug in, drag a
  blank `.uf2` (or `flash_nuke.uf2` from the Pico docs) to wipe.

## Roadmap from here

See [`../TODO.md`](../TODO.md) under "Phase-2 hardware prep". Short
version: PIO quadrature decoder (replaces the polled one), capacitive
jog touch, 74HC165 button expansion for the full pad grid, LED feedback
via shift registers / WS2812, OLED for browse.
