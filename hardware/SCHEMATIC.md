# ODJ controller — prototype schematic (Deck A, Phase 1+2)

> This documents the **Deck-A breadboard prototype** (what the firmware in
> `firmware/` currently targets). The **two-deck PCB** — one Pico driving
> both decks, all controls on plug-in headers — is generated from
> [`pcb/odj_controller.py`](./pcb/odj_controller.py); see
> [`pcb/README.md`](./pcb/README.md) for its pin map and BOM.

Wiring for the first hardware build using:

- Raspberry Pi Pico (RP2040)
- 1× 600 P/R optical incremental encoder (NPN open-collector)
- 4× 24 mm illuminated arcade buttons (LEDs not yet driven by Pico)
- 1× Aexit 60 mm slide potentiometer (volume) — buy a 2nd later for pitch
- 3× B10K rotary potentiometers (EQ low / mid / high — user-supplied)
- 1× 74HC4051 8-channel analog multiplexer
- (no level shifter needed — see encoder note)

## Pico pin assignments

Numbering refers to the **physical pin number** on the Pico board (1-40),
with the GPIO label in parentheses. Physical pin 1 is top-left when the USB
connector points up and you're looking at the component side.

| Pico pin | GPIO | Function                       | To                       |
|----------|------|--------------------------------|--------------------------|
| 36       | 3V3  | 3.3 V power                    | Mux VDD, pot outer pins  |
| 38       | GND  | Ground                         | Common ground            |
| 40       | VBUS | +5 V from USB                  | Encoder V+, LED commons  |
| 19       | GP14 | Encoder A (in, pull-up)        | Encoder green wire       |
| 20       | GP15 | Encoder B (in, pull-up)        | Encoder white wire       |
| 14       | GP10 | Button 1 — Play/Pause (in, pull-up) | Arcade button switch terminal |
| 15       | GP11 | Button 2 — CUE (in, pull-up)   | Arcade button switch     |
| 16       | GP12 | Button 3 — Nudge-down (in, pull-up) | Arcade button switch |
| 17       | GP13 | Button 4 — Nudge-up (in, pull-up)   | Arcade button switch |
| 24       | GP18 | Play LED out (active high)     | Button 1 LED `+`         |
| 9        | GP6  | Mux S0 (out)                   | 74HC4051 pin 11 (S0)     |
| 10       | GP7  | Mux S1 (out)                   | 74HC4051 pin 10 (S1)     |
| 11       | GP8  | Mux S2 (out)                   | 74HC4051 pin 9 (S2)      |
| 31       | GP26 | ADC0 (in, analog)              | 74HC4051 pin 3 (Z output)|

Other Pico pins unused for now.

## Encoder wiring

NPN open-collector outputs. We use the Pico's internal pull-ups to 3.3 V,
so the encoder output lines never exceed 3.3 V — no level shifter needed.

| Encoder wire | Colour     | To Pico        |
|--------------|------------|----------------|
| V+           | red        | VBUS (pin 40)  |
| V0           | black      | GND (any GND)  |
| A            | green      | GP14 (pin 19)  |
| B            | white      | GP15 (pin 20)  |
| Z (index)    | orange     | not connected  |

**Important:** never connect A, B, or Z directly to V+ — they're open-collector
outputs and will burn the encoder's internal transistor.

## 74HC4051 multiplexer wiring

The 4051 is an 8-channel analog mux. Three select pins (S0/S1/S2) pick
which input (Y0–Y7) is routed to the common Z output. Z connects to the
Pico's ADC0.

| 4051 pin | Name | To                          |
|----------|------|-----------------------------|
| 1        | Y4   | Pitch fader wiper (Phase 2) |
| 2        | Y6   | (unused, future)            |
| 3        | Z    | Pico GP26 (ADC0)            |
| 4        | Y7   | (unused, future)            |
| 5        | Y5   | (unused, future)            |
| 6        | E    | GND (active-low enable)     |
| 7        | VEE  | GND                         |
| 8        | GND  | GND                         |
| 9        | S2   | Pico GP8                    |
| 10       | S1   | Pico GP7                    |
| 11       | S0   | Pico GP6                    |
| 12       | Y3   | Volume fader wiper          |
| 13       | Y0   | EQ low pot wiper            |
| 14       | Y1   | EQ mid pot wiper            |
| 15       | Y2   | EQ high pot wiper           |
| 16       | VDD  | Pico 3V3 (pin 36)           |

Channel mapping summary:

| Mux ch (S2 S1 S0) | Connected | App MIDI mapping  |
|-------------------|-----------|-------------------|
| 0 (0 0 0) Y0      | EQ low    | CC 4 (Deck A low) |
| 1 (0 0 1) Y1      | EQ mid    | CC 9 (unused in engine for now) |
| 2 (0 1 0) Y2      | EQ high   | CC 3 (Deck A high)|
| 3 (0 1 1) Y3      | Volume    | CC 2 (Deck A gain)|
| 4 (1 0 0) Y4      | Pitch     | CC 1 (Deck A pitch)|
| 5 (1 0 1) Y5      | —         | —                 |
| 6 (1 1 0) Y6      | —         | —                 |
| 7 (1 1 1) Y7      | —         | —                 |

## Potentiometer wiring (3× B10K + 1× 60 mm slide)

Each pot has 3 terminals. Looking at the back of a rotary pot with shaft
up, the leftmost terminal goes to GND, rightmost to 3V3 (rotates CW =
higher value). For a slide pot, the orientation depends on the model —
flip the outer two pins if direction feels backward.

```
B10K pot           Slide fader 10K
  outer1 → 3V3       outer1 → 3V3
  wiper  → mux Y     wiper  → mux Y
  outer2 → GND       outer2 → GND
```

## Arcade button wiring

Each button has 4 terminals: 2 for the switch contacts, 2 for the LED
(usually marked `+` and `−`). One switch terminal goes to a Pico GPIO,
the other to GND; the Pico's internal pull-ups make a press an active-low
signal.

For the **Play button only**, the LED is driven by the Pico so it
mirrors the deck's playing state. The firmware listens for MIDI
`note_on 40` / `note_off 40` from the host and drives GP18 accordingly.

```
Arcade button (Play, button 1)     Pico
  switch term 1                  → GP10 (pin 14)
  switch term 2                  → GND
  LED +                          → GP18 (pin 24)   ← driven by firmware
  LED −                          → GND
```

For the **other buttons** (Cue, Nudge ±), the LED is just left
disconnected or hard-wired always-on to VBUS via the button's built-in
series resistor. We're not lighting those from firmware yet.

3.3 V GPIO source mode works for the supplied "5 V arcade LED" buttons —
the LED's internal series resistor is sized for 5 V, so at 3.3 V it
sits at roughly 60-70% of rated brightness. Visible in daylight; bright
enough for stage. If you really need full brightness, switch to:

```
LED + → VBUS (5 V)
LED − → GP18 via a small (~150 Ω) resistor
```

…and flip the firmware to active-low (negate `gpio_put` in
`handle_note`). Not necessary for the prototype.

## Layout overview (block diagram)

```
                    ┌─────────────────────────┐
                    │   Raspberry Pi Pico     │
   USB → host       │                         │
                    │     GP14/15 (in) ◄──────┼──── encoder A/B
                    │     GP10..13 (in) ◄─────┼──── 4× buttons → GND
                    │     GP6/7/8 (out) ──────┼──── 4051 S0/S1/S2
                    │     GP26 ADC ◄──────────┼──── 4051 Z
                    │     3V3 ────────────────┼──── 4051 VDD, pot tops
                    │     VBUS ───────────────┼──── encoder V+, LED+
                    │     GND ────────────────┼──── everything's GND
                    └─────────────────────────┘
                                                    ┌───────────┐
                                                    │ 74HC4051  │
                                                    │           │
                                          Y0 ───────┤            │
                                          Y1 ───────┤  EQ pots  │
                                          Y2 ───────┤            │
                                          Y3 ───────┤  Vol pot  │
                                          Y4 ───────┤  Pitch pot│
                                                    └───────────┘
```

## Build order I'd recommend

1. **Encoder only.** Pico + encoder, 4 wires. Verify polling reads A/B
   correctly and counts ticks both directions. Watch via USB serial.
2. **Add 1 button.** Confirm note_on / note_off comes through USB MIDI
   on the host. Use `aseqdump -p <port>` to watch the MIDI bytes.
3. **Add the 4051 + 1 pot.** Verify ADC reads varying voltage. Send as
   CC, watch in the host app.
4. **Wire all 4 buttons + remaining pots.** Cosmetic — same code path,
   just more channels.

Don't put the LEDs on until everything else works — fewer things to
debug.

## Pinout reference card

Pico physical pinout (top view, USB at top):

```
                    ┌────[USB]────┐
       GP0  TX   1 ─┤             ├─ 40  VBUS
       GP1  RX   2 ─┤             ├─ 39  VSYS
              GND 3 ─┤             ├─ 38  GND
       GP2       4 ─┤             ├─ 37  3V3_EN
       GP3       5 ─┤             ├─ 36  3V3      ← mux VDD, pot tops
       GP4       6 ─┤             ├─ 35  ADC_VREF
       GP5       7 ─┤             ├─ 34  GP28 ADC2
              GND 8 ─┤             ├─ 33  GND
       GP6       9 ─┤             ├─ 32  GP27 ADC1
       GP7      10 ─┤    Pico     ├─ 31  GP26 ADC0  ← mux Z
       GP8      11 ─┤             ├─ 30  RUN
       GP9      12 ─┤             ├─ 29  GP22
              GND 13 ─┤             ├─ 28  GND
       GP10     14 ─┤             ├─ 27  GP21
       GP11     15 ─┤             ├─ 26  GP20
       GP12     16 ─┤             ├─ 25  GP19
       GP13     17 ─┤             ├─ 24  GP18
              GND 18 ─┤             ├─ 23  GND
       GP14     19 ─┤             ├─ 22  GP17
       GP15     20 ─┤             ├─ 21  GP16
                    └─────────────┘
```
