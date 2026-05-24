# cpal + PipeWire audio findings

Verified by a latency spike: **128 frames @ 44.1 kHz F32 stereo = ~2.90 ms
output latency**, measured on an Arch + PipeWire 1.6.4 setup, even when
routing through a custom default-sink filter (a mono-summing loopback).
cpal's ALSA backend negotiates this without complaint when targeting
the `pipewire` device. Play/pause via `AtomicBool` in the audio callback
is responsive.

## Critical cpal/ALSA device-selection workaround on PipeWire systems

- `cpal::default_host().default_output_device()` returns a device
  literally named `"default"` (the ALSA default PCM). On
  PipeWire-via-ALSA-shim, this device's `default_output_config()`
  succeeds but `build_output_stream` returns `DeviceNotAvailable`.
- **Fix**: enumerate `host.output_devices()` and pick the one whose
  name is `"pipewire"`. That device has a working default config
  (44.1 kHz F32 stereo, buffer range 1..4194304) and accepts
  `BufferSize::Fixed(128)`.
- See `crates/audio/src/lib.rs::pick_device` for the pattern: prefer
  `name == "pipewire"`, fall back to any device whose
  `default_output_config()` succeeds.

## Per-process routing without modifying global state

- `PIPEWIRE_NODE=<sink-name> ./binary` routes that process's PipeWire
  stream to a specific sink. Does NOT change the default sink for
  other apps.
- Surface this as a `--device` CLI flag (sets the env var before
  audio init) rather than asking users to fiddle with environment
  variables.

## Engine format

F32, 2 channels, 44.1 kHz — kept consistent throughout. No format
conversion in the hot path.

## Why this matters

This setup-specific workaround is exactly the kind of thing that
wastes a debugging session if forgotten. The "default" cpal device
being broken on PipeWire is a sharp edge that took two iterations to
resolve during the spike.
