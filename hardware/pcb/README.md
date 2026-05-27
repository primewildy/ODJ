# ODJ controller PCB (two-deck carrier board)

A single-board carrier that drives **both decks** from one Raspberry Pi Pico.
Everything off-board (pots, faders, buttons, encoders, LEDs) lands on **JST-XH**
connectors — locking and polarised, so panel cables can't be plugged in
backwards. Bench/expansion points (spare GPIO, I2C) use 2.54 mm pin headers for
jumper wires and plug-in modules. The Pico itself **sockets onto two 1×20
headers**. Nothing solders to the PCB except the connectors, the two mux DIPs,
and the decoupling parts.

The board is described in code (`odj_controller.py`, using
[SKiDL](https://github.com/devbisme/skidl)) and the KiCad netlist is generated
from it. The script is the source of truth; the `.net` file is a build
artifact (regenerate it, don't hand-edit).

```
USB ── Pico ─┬─ Deck A: jog encoder, 4 buttons, Play+🎧Cue LEDs, mux A (GP26/ADC0)
             └─ Deck B: jog encoder, 4 buttons, Play+🎧Cue LEDs, mux B (GP27/ADC1)
             shared mux select S0/S1/S2 (GP6/7/8)
```

Why one Pico for two decks: the host (`src/midi.rs`) already maps both decks
from a single MIDI stream, and the RP2040 has the I/O (≈21 of 26 usable GPIO).
One USB cable, one firmware, no inter-board sync.

## Generate the netlist

SKiDL is a Python package; KiCad need not be installed to *generate* the
netlist (the parts carry explicit pin definitions). You only need KiCad for the
layout step.

```
python3 -m venv venv
. venv/bin/activate
pip install skidl
python odj_controller.py        # writes odj_controller.net
```

## Lay out + route in KiCad → Gerbers

The schematic capture is done (it's the netlist). The remaining work — placing
parts and routing copper — is the visual part KiCad is for. A 2-layer board is
plenty.

1. Install **KiCad (10.x)** *and its library package* — the footprints and
   symbols are a separate ~370 MB package. On Arch this bites everyone:

   ```
   sudo pacman -S kicad kicad-library kicad-library-3d
   ```

   Installing `kicad` alone leaves `/usr/share/kicad/{symbols,footprints}`
   empty and every library "missing". KiCad provides sane defaults for the
   `KICAD10_FOOTPRINT_DIR` / `KICAD10_SYMBOL_DIR` path variables internally;
   if a stale `~/.config/kicad` from an older version points them wrong, check
   **Preferences → Configure Paths…** (they should be `/usr/share/kicad/...`).
   Then open the **PCB editor** (skip Eeschema; we import the netlist straight
   into the board).
2. **File → Import → Netlist…** → pick `odj_controller.net` → set *Link Method*
   to **reference designators** (SKiDL randomises the unique-id tstamps each
   regeneration, so refdes is the stable choice for re-imports) → *Update PCB*.
   Footprints land in a heap with a ratsnest showing every connection. Every
   footprint is a stock KiCad 10 part, so they all resolve — including the
   Pico, which is two `PinHeader_1x20` sockets (`PICO-LEFT` / `PICO-RIGHT`)
   rather than a Pico-specific footprint that KiCad doesn't ship.
   - **Placing the Pico:** set the two 1×20 sockets 17.78 mm (0.7") apart,
     same orientation (pin 1 at top on both), and a Pico with male headers
     drops straight in.
3. Draw the board outline, place parts (see layout tips below), route, add
   ground pours on both layers, run **DRC**.
4. **File → Plot** Gerbers + **Generate Drill Files**, zip them, upload to
   JLCPCB / PCBWay / OSHPark. ~£5 for 5 boards from JLCPCB.

## Layout tips that matter

- **Decoupling close.** Put each 100 nF right at its mux's pin 16↔8, and one at
  the Pico's 3V3↔GND. The 10 µF bulk near the 3V3 entry.
- **Don't filter the ADC line.** No cap on `MUXA_Z`/`MUXB_Z` — the firmware
  settles the mux in 5 µs, and a cap + the pot's ~10 kΩ source would smear one
  channel's reading into the next. Keep these two traces short instead, and
  away from the USB and encoder lines.
- **Star the analog ground** back to a Pico GND pin (33/38) if you can; keeps
  pot noise out of the ADC.
- Encoders are NPN open-collector → no level shifter; the 4-pin header carries
  +5V (VBUS), A, B, GND.

## Bill of materials

| Qty | Part | Notes |
|-----|------|-------|
| 1 | Raspberry Pi Pico | with male pin headers soldered on |
| 2 | 1×20 female socket header | sockets the Pico (rows 0.7"/17.78 mm apart) |
| 2 | 74HC4051 (DIP-16) | + 2× DIP-16 sockets |
| 2 | Rotary encoder, 600 P/R, NPN open-collector | jog wheels |
| 8 | Illuminated arcade button, 24 mm | Play/Cue/🎧Cue/Sync × 2 decks |
| 4 | Slide fader (10 kΩ) | pitch + volume × 2 decks |
| 6 | Rotary pot B10K | 3-band EQ × 2 decks |
| 3 | Ceramic cap 100 nF | mux ×2 + Pico decoupling |
| 1 | Electrolytic cap 10 µF | 3V3 bulk |
| 4 | Resistor | 0 Ω link (arcade LED has its own R) or ~150 Ω for a bare LED |
| 16 | JST-XH 1×3 (B3B-XH-A) | analog channels (8 per mux) |
| 12 | JST-XH 1×2 (B2B-XH-A) | 8 buttons + 4 LEDs (Play + 🎧Cue × 2 decks) |
| 2 | JST-XH 1×4 (B4B-XH-A) | jog encoders |
| 1 | 1×4 pin header | I2C expansion |
| 1 | 1×10 pin header | spare-GPIO breakout |

Plus the JST-XH cable side: matching 2/3/4-way housings + crimp contacts (or
pre-crimped leads) to wire each panel control back to its board connector —
16× 3-way, 12× 2-way, 2× 4-way.

## Pin map (mirrors `hardware/firmware/src/main.c`)

Keep this and the firmware in sync.

| Signal | GPIO | Pico pin | Connects to |
|--------|------|----------|-------------|
| Mux select S0 | GP6 | 9 | both muxes pin 11 |
| Mux select S1 | GP7 | 10 | both muxes pin 10 |
| Mux select S2 | GP8 | 11 | both muxes pin 9 |
| Mux A common Z | GP26/ADC0 | 31 | mux A pin 3 — Deck A analog |
| Mux B common Z | GP27/ADC1 | 32 | mux B pin 3 — Deck B analog |
| Deck A jog A / B | GP14 / GP15 | 19 / 20 | A-ENCODER header |
| Deck B jog A / B | GP16 / GP17 | 21 / 22 | B-ENCODER header |
| Deck A Play btn | GP10 | 14 | A-PLAY (note 40) |
| Deck A Cue btn | GP11 | 15 | A-CUE (note 41) |
| Deck A 🎧Cue btn | GP12 | 16 | A-HPCUE (note 44) |
| Deck B Play btn | GP13 | 17 | B-PLAY (note 36) |
| Deck B Cue btn | GP19 | 25 | B-CUE (note 37) |
| Deck B 🎧Cue btn | GP20 | 26 | B-HPCUE (note 45) |
| Deck A Sync btn | GP2 | 4 | A-SYNC (note 46) |
| Deck B Sync btn | GP3 | 5 | B-SYNC (note 47) |
| Deck A Play LED | GP18 | 24 | A_PLAY-LED via R1 (host note 40) |
| Deck A 🎧Cue LED (PFL) | GP22 | 29 | A_HPCUE-LED via R2 (host note 44) |
| Deck B Play LED | GP21 | 27 | B_PLAY-LED via R3 (host note 36) |
| Deck B 🎧Cue LED (PFL) | GP9 | 12 | B_HPCUE-LED via R4 (host note 45) |

The 🎧Cue LED is a **PFL indicator** — lit while that deck is routed to the
headphones, not tied to the transport-cue point. The transport-Cue button
(notes 41 / 37) has no Pico-driven LED.

Both muxes use the same channel layout (channel = `S2 S1 S0`):

| Ch | Y-pin | Function | Deck A CC | Deck B CC |
|----|-------|----------|-----------|-----------|
| 0 | Y0 (13) | EQ low | 4 | 8 |
| 1 | Y1 (14) | EQ mid | 9 | *(new)* |
| 2 | Y2 (15) | EQ high | 3 | 7 |
| 3 | Y3 (12) | Volume | 2 | 6 |
| 4 | Y4 (1) | Pitch | 1 | 5 |
| 5–7 | Y5/Y6/Y7 | spare (broken out) | — | — |

## Expansion (extra button banks: effects, hot cues)

Two independent routes — they coexist, use either or both:

**A. Standalone USB-MIDI pad controllers.** A cheap 16-pad USB controller just
plugs into the laptop as its own MIDI device — nothing touches this PCB. This
is a *host* change, not a hardware one: `src/midi.rs` currently connects to a
single MIDI input port; supporting extra controllers means opening multiple
ports and mapping the pad notes to hot-cue / FX commands. The engine is already
ready for it — `Sender` is cloneable, so any MIDI producer can drive a deck.

**B. Raw buttons wired to this board.** For tightly-integrated pads, the board
is built to grow without a respin:

- **I2C-EXP header** taps I2C0 (GP0=SDA, GP1=SCL) + 3V3 + GND. Hang an
  **MCP23017-style I/O expander** off it: 16 buttons per chip on two wires,
  chain up to 8 (128 buttons). Firmware polls it over I2C.
- **Spare-GPIO header** breaks out GP0–GP5, GP9, GP22, GP28 (ADC2) + power —
  enough for a **4×4 matrix scan** (16 buttons on 8 GPIO) for one cluster.

Either way the new pads emit MIDI the host maps to hot-cue / FX commands — no
change to this PCB.
