# Musical key detection (Krumhansl-Schmuckler, Camelot output)

Classical Krumhansl-Schmuckler key-finding, layered onto the existing
analysis STFT pass.

## Algorithm

1. During the STFT loop (1024 frame, 512 hop, ~11025 Hz mono),
   accumulate chroma: for each FFT bin in the 60–4000 Hz range, map to
   its nearest pitch class via MIDI note = 69 + 12·log2(freq/440), sum
   magnitudes per pitch class.
2. After all frames: normalise chroma.
3. Pearson-correlate against 24 key profiles (12 major + 12 minor),
   using the Krumhansl-Kessler weight vectors rotated to each candidate
   tonic.
4. Pick the (tonic, mode) with the highest correlation.

## Output

`Option<MusicalKey { tonic: u8, is_minor: bool }>` in `AnalysisResult`.
Wired through `TrackAnalysis` so both engine and UI see the same value.
UI shows it via `MusicalKey::label()` in **Camelot Wheel notation** —
e.g. `8A` for A minor, `5B` for D♯/E♭ major.

## Camelot mapping

Numbers 1..12 around the circle of fifths starting at 8B = C major.
Tonic→number lookup for *major* tonics: `[8, 3, 10, 5, 12, 7, 2, 9, 4,
11, 6, 1]` indexed by tonic 0..11 (C, C#, D, …, B). For *minor* tonics,
route via the relative major (tonic + 3 mod 12) and use suffix `A`.
Implementation: `MusicalKey::label` in `crates/control/src/lib.rs`.

## Why Camelot

It's the DJ-standard, used by Mixed In Key, rekordbox, etc. Harmonic
mixing rules (same number, ±1 number, or A↔B at same number) become
trivial to read.

## Known limits

- Krumhansl-Schmuckler can pick the wrong mode (major vs minor) on
  tracks with strong V-chord emphasis or modal/atonal material.
- No equivalent of a "swap" suggestion (could be a v1.5 UI thing —
  show compatible neighbours).
- Chroma summed across the whole track; tracks with a key change
  won't be detected correctly. Could be addressed by windowing.
