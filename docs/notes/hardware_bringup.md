# Hardware bring-up lessons

Gotchas from wiring up the first RP2040 controller (Deck A). Recorded so
the next board / deck goes faster.

## ALSA card number is not stable

The Pico's card number floats between `hw:0` and `hw:1` across reflashes
and replugs. Several debug captures showed "nothing" purely because they
were pointed at the old card number. Always resolve it dynamically:

```
PORT=$(amidi -l | awk '/ODJ Controller/ {print $2; exit}')
amidi -p "$PORT" -d
```

The host app is unaffected — `midir` matches the port by *name*
("ODJ Controller"), not card number. Only raw `amidi`/`aseqdump`
debugging needs the lookup. (`aseqdump -p "ODJ Controller"` also didn't
auto-subscribe reliably on this box; `amidi -p hw:N,0,0 -d` was the
dependable path.)

## NPN open-collector encoder → no level shifter

The 600 P/R optical encoder is 5 V NPN open-collector. Powered from VBUS
(5 V), its A/B outputs only ever pull *low*; the Pico's internal pull-ups
(to 3.3 V) define the high level, so the lines never exceed 3.3 V. Four
wires, no external level shifter, no resistors. (A push-pull encoder
*would* need a shifter.)

## Hot pots = a short across the resistive track

Symptom: pots warm to the touch, two pots' wipers affecting each other.
A 10 kΩ pot across 3.3 V draws ~0.3 mA — it can't get warm. Heat means
3.3 V is shorted to GND across part of a track: typically a solder
bridge putting a wiper onto its own supply leg (or a wiper-to-wiper
bridge plus a rail short). **Power down immediately**, then meter
(power off):

- 3V3 ↔ GND should be high resistance. Near 0 Ω = the dangerous short.
- Wiper ↔ wiper of different pots should be open.
- Wiper ↔ each rail should *vary* with rotation, never stick near 0 Ω.

Fix is reflow/wick the bridge. Nothing in firmware can cause pot heat —
it's always a wiring short.

## ADC bring-up: prove the core first

When all mux channels read 0, isolate the fault by reading the on-chip
temperature sensor (ADC channel 4, no external pins). Non-zero temp ⇒
the ADC core converts and the fault is in the input path (mux / pots /
the chosen GPIO). The `MUX_DEBUG` flag in `firmware/src/main.c` streams
all channels (deadband off) + the temp sensor for exactly this.

## Fast iteration: sysex reboot

After the first manual BOOTSEL flash of firmware that includes the
sysex-reboot hook, `hardware/flash.sh` does rebuild → sysex reboot to
BOOTSEL → copy `.uf2` → device restarts, with no button presses. ~5 s
per iteration.

See also: [audio_findings.md](audio_findings.md), [lpd8.md](lpd8.md).
