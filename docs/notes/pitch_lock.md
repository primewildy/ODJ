# Pitch lock (key-lock) via streaming phase vocoder

Per-deck pitch-lock toggle: when on, tempo can be changed via the pitch
knob/slider without altering the audible pitch. Implemented with a
streaming phase vocoder, not an external library.

## Module

`crates/audio/src/pvoc.rs`.

## Parameters

- N_FFT = 1024 (≈23 ms at 44.1k)
- HOP_S = 256 (synthesis hop)
- HOP_A = HOP_S × speed_ratio, fractional accumulator avoids long-term
  drift
- Hann window applied at both analysis and synthesis (Hann² OLA gain =
  1.5 at hop = N/4; output scaled by 1/(N_FFT × 1.5))
- MAX_HOP_A = 512 (covers up to ~2× speed)

## Dispatch

`render_one` in `lib.rs` calls `render_deck_pv` when `deck.pitch_lock`
is true, else `render_deck` (vinyl). The same playhead advances at
`(src_sr / eng_sr) × speed_ratio` per output sample in both modes, so
the UI shows identical position info.

## Hot-path discipline

PV is allocated at `DeckState::new()`; `process_frame` and `consume`
do no allocations. cpal callback stays alloc-free.

## Known limitations

- ~23 ms warmup transient when first toggling pitch_lock on — OLA
  accumulator is initially zero, output fades in. Reset on each toggle
  (intentional).
- Standard phase vocoder smears transients (kicks get slightly softer).
  Acceptable at ±8% range; would need spectral pinning or a hybrid
  (PV + WSOLA) to fix properly.
- No transient detection / phase-locking improvements yet.
- Speed range clamped to MAX_HOP_A=512; anything > ~2× will saturate.

## Default

ON in this project (user preference). Toggle with the "🔒 key" checkbox
next to Q on each deck panel.

## Why phase vocoder and not Rubber Band

- Pure Rust, no C dep. Matches the project's "no system libraries"
  stance.
- Small enough (~200 lines) to debug and modify.
- Quality is good enough for ±8% DJ-style tempo adjustment.
