# BPM + beat-grid analysis (v1)

Pure-Rust pipeline in `crates/analysis/`:

1. Block-average to mono at ~11025 Hz (no proper anti-alias — fine for
   onset detection).
2. STFT via rustfft (N=1024, hop=512, Hann window).
3. Spectral-flux onset envelope (sum of positive bin differences).
4. Subtract local mean (~1 s window) to detrend.
5. Autocorrelate envelope; peak in lag range corresponding to 60..200 BPM.
6. Phase: try 32 sub-frame offsets, pick the one maximising envelope
   sum at predicted beat positions.
7. Generate beat times in seconds from phase + period.

Runtime: ~100 ms per 5-minute track. Runs in the background load
thread; never touches the audio or UI thread.

## Refinements

Added after observing a real-world drift report:

- **Parabolic interpolation** around the autocorr peak gives sub-frame
  precision in the lag. Without it, BPM was quantised to integer-lag
  values (e.g. 127 BPM → lag 10 → reported as 129.2 BPM, drifting
  ~1.7%). With it, the refined lag is ~10.17 → 127 BPM. See
  `crates/analysis/src/lib.rs` around the `refined_lag` block.
- **Soft half/double bias**: any detected BPM < 80 gets doubled, > 180
  gets halved. Catches the easy half/double-tempo failure mode without
  affecting in-range tracks.
- **Brute-force phase-aligned refinement**: parabolic interpolation
  alone wasn't accurate enough (real autocorr peaks aren't true
  parabolas). After the rough estimate, search ±5 BPM in 0.05 BPM
  steps; for each candidate compute the best phase score and pick the
  global maximum. This is what got the BPM precision rock-solid.

## Remaining v1 failure modes

- **Phase off by a half-beat**: onsets in EDM are usually on the beat,
  but offbeat onsets (snares, hats) can pull the phase.
- **Non-percussive material**: spectral flux finds little to lock onto;
  tempo becomes random.
- **Tempo changes mid-track**: assumed constant; not handled.
- **Strong half/double conviction**: if the autocorr peak is genuinely
  at the half/double (e.g. half-time techno where the snare-on-the-3
  dominates), the bias may not fix it.

## Why this approach (not aubio)

- Pure Rust = no C deps, builds cleanly without system libraries.
- Algorithm is small enough (~200 lines) to be understood and modified.
- v1.5 may replace this with a learned downbeat tracker (see
  [downbeat_idea.md](downbeat_idea.md)), at which point this code
  becomes a fallback for non-analysed tracks.

## How quantise uses it

The engine stores `Arc<TrackAnalysis>` per deck. On `CuePress` while
paused, if `quantize` is on and `beat_grid` is non-empty, the new
`cue_frame` snaps to the nearest beat via binary search (in seconds,
then converted to source frames). Code: `audio::snap_to_beat` and
`audio::nearest_beat_secs`.

## Cache versioning

`TrackAnalysis.analysis_version` is currently 1. Bump this when the
algorithm changes meaningfully so cached results get re-computed. The
on-disk cache doesn't yet track this version (TODO).
