#!/usr/bin/env python3
"""ODJ controller — two-deck carrier/breakout PCB, described in SKiDL.

Generates a KiCad netlist (`odj_controller.net`) you import into Pcbnew to
place + route the board. One Raspberry Pi Pico drives BOTH decks:

    USB ── Pico ─┬─ Deck A: jog encoder, 4 buttons, Play+Cue LEDs, mux A (GP26)
                 └─ Deck B: jog encoder, 4 buttons, Play+Cue LEDs, mux B (GP27)
                 shared mux select S0/S1/S2 (GP6/7/8)

Everything off-board (pots, faders, buttons, encoders, LEDs) lands on 2.54 mm
pin headers so the panel-mounted controls plug in — no soldering to the board.
All 8 channels of each mux and every spare Pico GPIO are broken out for
expansion (Sync/Load buttons, a second encoder detent, etc.).

Run it:
    python3 odj_controller.py          # writes odj_controller.net

The parts are defined with explicit pins (no KiCad symbol libraries needed to
generate the netlist). All footprints are stock KiCad parts (verified against
KiCad 10's `kicad-library`); the Pico is two 1x20 socket headers rather than a
Pico-specific footprint, which KiCad doesn't ship. Panel controls use JST-XH;
bench/expansion points use 2.54 mm pin headers.

Pin map mirrors hardware/firmware/src/main.c. Keep the two in sync.
"""

from skidl import (
    KICAD8,
    SKIDL,
    TEMPLATE,
    Net,
    Part,
    Pin,
    generate_netlist,
    set_default_tool,
)

set_default_tool(KICAD8)

PASSIVE = Pin.types.PASSIVE

# --- footprints (standard KiCad 10 libraries) ---
FP_DIP16 = "Package_DIP:DIP-16_W7.62mm"
FP_R = "Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm_P10.16mm_Horizontal"
FP_C = "Capacitor_THT:C_Disc_D5.0mm_W2.5mm_P5.00mm"
FP_CP = "Capacitor_THT:CP_Radial_D5.0mm_P2.50mm"


def _hdr_fp(n):
    # 2.54 mm pin header — bench/expansion connectors (jumpers, I2C modules)
    # and the two sockets the Pico plugs into.
    return f"Connector_PinHeader_2.54mm:PinHeader_1x{n:02d}_P2.54mm_Vertical"


def _jst_fp(n):
    # JST-XH 2.50 mm vertical through-hole — panel-control cables (locking,
    # polarised, can't be plugged in backwards). B2B/B3B/B4B-XH-A = 2/3/4-pin.
    return f"Connector_JST:JST_XH_B{n}B-XH-A_1x{n:02d}_P2.50mm_Vertical"


# ---------------------------------------------------------------------------
# Part builders (explicit-pin, self-contained)
# ---------------------------------------------------------------------------

def make_ic(name, ref_prefix, footprint, pins):
    """pins: list of (pin_number, pin_name)."""
    p = Part(tool="skidl", name=name, dest=TEMPLATE,
             ref_prefix=ref_prefix, footprint=footprint)
    for num, pname in pins:
        p += Pin(num=num, name=pname, func=PASSIVE)
    return p()


def header_template(n, footprint=None, name=None):
    p = Part(tool="skidl", name=name or f"Header_1x{n:02d}", dest=TEMPLATE,
             ref_prefix="J", footprint=footprint or _hdr_fp(n))
    for i in range(1, n + 1):
        p += Pin(num=i, name=f"P{i}", func=PASSIVE)
    return p


# JST-XH panel-control connectors (pots/faders, buttons, LEDs, encoders).
JXH2 = header_template(2, _jst_fp(2), "JST_XH_1x02")
JXH3 = header_template(3, _jst_fp(3), "JST_XH_1x03")
JXH4 = header_template(4, _jst_fp(4), "JST_XH_1x04")
# 2.54 mm pin headers: I2C expansion (1x4), spare-GPIO breakout (1x10), and
# the two 1x20 sockets the Pico plugs into.
H4 = header_template(4)
H10 = header_template(10)
H20 = header_template(20)


def passive2(ref_prefix, footprint, value):
    p = Part(tool="skidl", name=value, dest=TEMPLATE,
             ref_prefix=ref_prefix, footprint=footprint)
    p += Pin(num=1, name="1", func=PASSIVE)
    p += Pin(num=2, name="2", func=PASSIVE)
    return p(value=value)


# ---------------------------------------------------------------------------
# Power nets
# ---------------------------------------------------------------------------
GND = Net("GND")
V3 = Net("+3V3")
V5 = Net("+5V")

# ---------------------------------------------------------------------------
# Pico — socketed on two 1x20 pin headers. No Pico module footprint ships with
# KiCad, and this matches the plug-in design: solder male headers to the Pico,
# then socket it into two female 1x20 headers on the board. In layout, place
# the two rows 17.78 mm (0.7") apart, same orientation (pin 1 at top), and the
# Pico seats. pp(n) maps a Pico *physical* pin (1..40) to a header pad:
#   PICO-LEFT  pad k = physical pin k        (1..20,  top→bottom)
#   PICO-RIGHT pad k = physical pin (41 - k)  (40..21, top→bottom)
# so both headers sit pin-1-at-top and the Pico's right-side pins line up.
# ---------------------------------------------------------------------------
pico_l = H20(value="PICO-LEFT")
pico_r = H20(value="PICO-RIGHT")


def pp(n):
    return pico_l[n] if n <= 20 else pico_r[41 - n]


# Power + ground.
V3 += pp(36)                                  # 3V3
V5 += pp(40)                                  # VBUS
for gnd_pin in (3, 8, 13, 18, 23, 28, 33, 38):
    GND += pp(gnd_pin)

# Shared mux select lines.
S0 = Net("MUX_S0"); S0 += pp(9)    # GP6
S1 = Net("MUX_S1"); S1 += pp(10)   # GP7
S2 = Net("MUX_S2"); S2 += pp(11)   # GP8

# Mux commons → the two ADC inputs.
MUXA_Z = Net("MUXA_Z"); MUXA_Z += pp(31)   # GP26 / ADC0 — Deck A analog
MUXB_Z = Net("MUXB_Z"); MUXB_Z += pp(32)   # GP27 / ADC1 — Deck B analog

# Jog encoders (NPN open-collector; Pico internal pull-ups, no level shifter).
ENCA_A = Net("ENCA_A"); ENCA_A += pp(19)   # GP14
ENCA_B = Net("ENCA_B"); ENCA_B += pp(20)   # GP15
ENCB_A = Net("ENCB_A"); ENCB_A += pp(21)   # GP16
ENCB_B = Net("ENCB_B"); ENCB_B += pp(22)   # GP17

# Buttons (active-low, internal pull-ups). note numbers are the host mapping.
BTN_A_PLAY = Net("BTN_A_PLAY"); BTN_A_PLAY += pp(14)    # GP10  note 40
BTN_A_CUE = Net("BTN_A_CUE"); BTN_A_CUE += pp(15)       # GP11  note 41
BTN_A_HPCUE = Net("BTN_A_HPCUE"); BTN_A_HPCUE += pp(16)  # GP12  note 44
BTN_B_PLAY = Net("BTN_B_PLAY"); BTN_B_PLAY += pp(17)    # GP13  note 36
BTN_B_CUE = Net("BTN_B_CUE"); BTN_B_CUE += pp(25)       # GP19  note 37
BTN_B_HPCUE = Net("BTN_B_HPCUE"); BTN_B_HPCUE += pp(26)  # GP20  note 45
BTN_A_SYNC = Net("BTN_A_SYNC"); BTN_A_SYNC += pp(4)     # GP2   note 46
BTN_B_SYNC = Net("BTN_B_SYNC"); BTN_B_SYNC += pp(5)     # GP3   note 47

# Button-LED drives (each through a series resistor to its LED header).
# Two per deck: Play (lit while playing) and 🎧-cue / PFL (lit while the deck
# is routed to the headphones). The host echoes the matching note back to
# light them: Play = 40/36, 🎧-cue = 44/45. The transport-Cue button has no
# Pico-driven LED.
LED_A_PLAY_GP = Net("LED_A_PLAY_GP"); LED_A_PLAY_GP += pp(24)    # GP18  note 40
LED_A_HPCUE_GP = Net("LED_A_HPCUE_GP"); LED_A_HPCUE_GP += pp(29)  # GP22  note 44
LED_B_PLAY_GP = Net("LED_B_PLAY_GP"); LED_B_PLAY_GP += pp(27)    # GP21  note 36
LED_B_HPCUE_GP = Net("LED_B_HPCUE_GP"); LED_B_HPCUE_GP += pp(12)  # GP9   note 45

# ---------------------------------------------------------------------------
# U2 / U3 — 74HC4051 analog muxes (DIP-16, by datasheet pin number)
# ---------------------------------------------------------------------------
MUX_PINS = [
    (1, "Y4"), (2, "Y6"), (3, "Z"), (4, "Y7"), (5, "Y5"),
    (6, "E"), (7, "VEE"), (8, "VSS"), (9, "S2"), (10, "S1"),
    (11, "S0"), (12, "Y3"), (13, "Y0"), (14, "Y1"), (15, "Y2"), (16, "VDD"),
]


def wire_mux(mux, z_net):
    V3.connect(mux[16])              # VDD
    GND.connect(mux[8], mux[7], mux[6])  # VSS, VEE, E (active-low enable on)
    S0.connect(mux[11]); S1.connect(mux[10]); S2.connect(mux[9])
    z_net += mux[3]


muxA = make_ic("74HC4051", "U", FP_DIP16, MUX_PINS)
muxB = make_ic("74HC4051", "U", FP_DIP16, MUX_PINS)
wire_mux(muxA, MUXA_Z)
wire_mux(muxB, MUXB_Z)

# Channel wiper nets. Mux channel → Y-pin map (datasheet pin number):
#   ch0=Y0=13  ch1=Y1=14  ch2=Y2=15  ch3=Y3=12
#   ch4=Y4=1   ch5=Y5=5   ch6=Y6=2   ch7=Y7=4
CH_YPIN = {0: 13, 1: 14, 2: 15, 3: 12, 4: 1, 5: 5, 6: 2, 7: 4}

# Per-deck channel labels (matches firmware MUX_CC order). 5 used + 3 spare.
CH_LABEL = {
    0: "EQ_LOW", 1: "EQ_MID", 2: "EQ_HIGH", 3: "VOLUME", 4: "PITCH",
    5: "SPARE5", 6: "SPARE6", 7: "SPARE7",
}

# ---------------------------------------------------------------------------
# Analog headers — one JST-XH 3-pin per mux channel (3V3, wiper, GND).
# 8 per deck = every channel broken out for plug-in pots/faders.
# ---------------------------------------------------------------------------
def analog_headers(mux, deck):
    for ch in range(8):
        wiper = Net(f"AN_{deck}{ch}_{CH_LABEL[ch]}")
        wiper += mux[CH_YPIN[ch]]
        j = JXH3(value=f"{deck}-{CH_LABEL[ch]}")
        V3.connect(j[1])
        wiper += j[2]
        GND.connect(j[3])


analog_headers(muxA, "A")
analog_headers(muxB, "B")

# ---------------------------------------------------------------------------
# Button headers — JST-XH 2-pin (signal, GND). 4 per deck.
# ---------------------------------------------------------------------------
def button_header(sig_net, label):
    j = JXH2(value=label)
    sig_net += j[1]
    GND.connect(j[2])


button_header(BTN_A_PLAY, "A-PLAY")
button_header(BTN_A_CUE, "A-CUE")
button_header(BTN_A_HPCUE, "A-HPCUE")
button_header(BTN_B_PLAY, "B-PLAY")
button_header(BTN_B_CUE, "B-CUE")
button_header(BTN_B_HPCUE, "B-HPCUE")
button_header(BTN_A_SYNC, "A-SYNC")
button_header(BTN_B_SYNC, "B-SYNC")

# ---------------------------------------------------------------------------
# Encoder headers — JST-XH 4-pin (+5V, A, B, GND). NPN open-collector encoder.
# ---------------------------------------------------------------------------
def encoder_header(a_net, b_net, label):
    j = JXH4(value=label)
    V5.connect(j[1])
    a_net += j[2]
    b_net += j[3]
    GND.connect(j[4])


encoder_header(ENCA_A, ENCA_B, "A-ENCODER")
encoder_header(ENCB_A, ENCB_B, "B-ENCODER")

# ---------------------------------------------------------------------------
# Button-LED series resistors + headers. R = 0R (arcade button has its own
# series resistor) or ~150R for a bare LED off 3V3. JST-XH 2-pin (LED, GND).
# Play + 🎧-cue (PFL) LED per deck.
# ---------------------------------------------------------------------------
def led_chain(gp_net, label):
    r = passive2("R", FP_R, "0R")
    gp_net += r[1]
    out = Net(f"LED_{label}")
    out += r[2]
    j = JXH2(value=f"{label}-LED")
    out += j[1]
    GND.connect(j[2])


led_chain(LED_A_PLAY_GP, "A_PLAY")
led_chain(LED_A_HPCUE_GP, "A_HPCUE")
led_chain(LED_B_PLAY_GP, "B_PLAY")
led_chain(LED_B_HPCUE_GP, "B_HPCUE")

# ---------------------------------------------------------------------------
# Decoupling — 100nF at each mux VDD + at the Pico, plus a 10uF bulk.
# ---------------------------------------------------------------------------
for _ in range(3):
    c = passive2("C", FP_C, "100nF")
    V3 += c[1]
    GND += c[2]
cbulk = passive2("C", FP_CP, "10uF")
V3 += cbulk[1]
GND += cbulk[2]

# ---------------------------------------------------------------------------
# Spare GPIO breakout — every unused Pico pin + power/ground, on one header.
#   GP0-GP5, GP9, GP22, GP28(ADC2) are free for expansion.
# ---------------------------------------------------------------------------
spare = H10(value="SPARE-GPIO")
V3 += spare[1], spare[10]
V5 += spare[2]
GND += spare[3], spare[9]

# Spare Pico GPIO → breakout header pins (greppable per-pin nets).
#   GP0/GP1 = I2C0 (SDA/SCL) — also on the I2C-EXP header below.
#   GP28 = ADC2, a third analog input if ever needed.
#   GP4/GP5 are free spares.
# (GP2/GP3 → Sync buttons; GP22/GP9 → Deck A / Deck B 🎧-cue (PFL) LEDs.)
spare_map = [
    ("SP_GP0", 1, 4), ("SP_GP1", 2, 5),
    ("SP_GP4", 6, 6), ("SP_GP5", 7, 7), ("SP_GP28", 34, 8),
]
spare_nets = {}
for name, pico_pin, hdr_pin in spare_map:
    n = Net(name)
    n += pp(pico_pin)
    n += spare[hdr_pin]
    spare_nets[name] = n

# ---------------------------------------------------------------------------
# I2C expansion header — for future button banks (effects, hot cues) via an
# MCP23017-style I/O expander: 16 buttons per chip on 2 wires, and you can
# chain up to 8 expanders on the same bus. Taps I2C0 = GP0(SDA)/GP1(SCL);
# these are the same nets as the spare breakout, exposed here as a tidy
# plug-in point. 4-pin: 3V3, SDA, SCL, GND.
# ---------------------------------------------------------------------------
exp = H4(value="I2C-EXP")
V3 += exp[1]
spare_nets["SP_GP0"] += exp[2]   # SDA
spare_nets["SP_GP1"] += exp[3]   # SCL
GND += exp[4]

if __name__ == "__main__":
    import os

    out = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                       "odj_controller.net")
    generate_netlist(file_=out)
    print(f"wrote {out}")
