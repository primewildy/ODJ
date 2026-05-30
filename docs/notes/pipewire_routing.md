# PipeWire routing for the cue / headphone output

How the cue stream gets to the right sink, and why `start-dj.sh` is more
than just `cargo run --release`. Real lessons from bringing up the
UGREEN USB DAC as the headphone output.

## The problem

We open two cpal output streams in one process:
- **Master** — the main mix, should go to whatever the user uses for
  their main monitors (laptop speakers via a mono-summing loopback in
  the author's case).
- **Cue / PFL** — pre-fader sum of decks that have `🎧 CUE` toggled,
  should go to the **headphone DAC** (UGREEN dongle).

On a PipeWire system this turns out to be a surprising amount of work.

## Gotcha 1 — cpal doesn't always enumerate USB DACs

cpal's ALSA backend uses `snd_device_name_hint` to list output devices.
On PipeWire-managed systems this gives you `pipewire`, `default:CARD=…`,
`hdmi:CARD=…` for built-in cards — but USB DACs that came up after init
often don't appear. The UGREEN shows in `aplay -L` as `default:CARD=Audio`
and in `wpctl status` as `alsa_output.usb-KTMicro_KT_USB_Audio_…`, but
cpal's `host.output_devices()` doesn't list it.

**Fix (`crates/audio/src/lib.rs::pick_device` + `find_pipewire_sink`):**
if the requested `--cue-device` doesn't match cpal's enumeration, we
shell out to `pactl list short sinks` and look for a sink whose name
contains the requested substring (with `_`/`-` normalised to spaces, so
"KT USB Audio" matches "alsa_output.usb-KTMicro_KT_USB_Audio_…"). If
found, we open cpal's `pipewire` device and route this one stream to
the matching PipeWire node via `PIPEWIRE_NODE`.

## Gotcha 2 — PIPEWIRE_NODE has a timing race

cpal's ALSA `Stream::play()` is async — it sends a signal to the audio
thread, which calls `snd_pcm_start`, which is when pipewire-alsa
actually creates the PipeWire stream and reads the `PIPEWIRE_NODE` env
var. If we set the env var to route the *cue* and the *master* audio
thread reads it later (still in its startup), master ends up on the cue
sink too.

**Fix:** sleep 200 ms after `master.play()` before changing
`PIPEWIRE_NODE` for the cue. By then the master's audio thread has
done its `snd_pcm_start` and is bound to its sink; subsequent env var
changes don't affect it. We don't restore the env var — master is
already connected and we don't open more streams.

## Gotcha 3 — WirePlumber's auto-routing policy can override the hint

Even with `PIPEWIRE_NODE` set correctly, WirePlumber may still move a
stream to the user-default sink after it connects ("restore previous
routing" policy). The env var is a *hint*, not a command.

**Fix (in `start-dj.sh`):** after the app is running, identify each
dj stream by `application.name` and `pactl move-sink-input` it
explicitly to the right sink. An explicit move is treated as user
intent and held by WirePlumber.

## Gotcha 4 — PIPEWIRE_PROP_* vs PIPEWIRE_PROPS

To label the streams as "DJ Master" and "DJ Cue" in pw-top /
pavucontrol, you set PipeWire stream properties. The correct env var
is **`PIPEWIRE_PROPS`** (singular, value is an SPA-JSON properties
block: `{ application.name = "DJ Master" node.description = "DJ Master" media.name = "DJ Master" }`).
`PIPEWIRE_PROP_<key>` does **not** work — pipewire-alsa just ignores it.

## Gotcha 5 — never restart wireplumber while the dj app is running

If you `systemctl --user restart wireplumber` while the app holds
ALSA-via-PipeWire streams, those streams' PipeWire backings get torn
down and aren't reconnected cleanly. The app keeps running and you'll
see MIDI events arrive, but **no audio reaches any sink**. Recovery is
to close the app and relaunch. Apply WirePlumber config tweaks (e.g.
short HDA device names in `~/.config/wireplumber/wireplumber.conf.d/`)
*before* launching the app, not during.

## `start-dj.sh` puts it all together

1. Find the user's `mono-playback` loopback (the one that mono-sums for
   their "hears in one ear" setup) and remember its current sink target.
2. Move that loopback to the laptop **Speaker** so master (default →
   Mono → loopback → Speaker) reaches the speakers, not the headphones.
3. Launch `cargo run --release -- --cue-device "KT USB Audio" --midi "ODJ"`.
4. Wait for the two `DJ Master` / `DJ Cue` sink-inputs to appear, then
   `pactl move-sink-input` each to its target sink explicitly.
5. On exit (Ctrl-C / kill / normal quit) restore the loopback's
   original target so day-to-day mono-summed audio is intact.

If any piece is missing (no mono loopback, no Speaker sink, no UGREEN),
the script logs what's skipped and still launches the app.

See also: [audio_findings.md](audio_findings.md) for the original
cpal-on-PipeWire findings, [cue_routing.md](cue_routing.md) for the
engine-side cue bus design.
