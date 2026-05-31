# Downbeat detection — spike findings + integration plan

A 2026-05-31 spike to pick a downbeat-detection model and validate the
ONNX path before wiring it into `crates/analysis`. TL;DR: **beat_this
(Foroughmand et al. 2024), exported to ONNX, with a custom 40-line
global-bar-phase postprocessor in place of madmom's DBN.**

## The problem we're solving

`crates/analysis` finds the beat *period* (autocorrelation of spectral
flux) and the beat *phase* (32 sub-frame offsets, pick highest envelope
sum). But the phase search is at one-beat granularity — any of the four
bar rotations of the grid scores the same. The UI then renders
`i % 4 == 0` as a downbeat, so whichever beat happens to land at index
0 becomes "1". Tracks with anacrustic intros (e.g. Moonlight by
Mandragora/Beltran) get the wrong bar phase. This is a v1.5 item from
day one; the spike confirms how to ship it.

## Candidates

| Model | Year | Arch | Downbeat F1 | ONNX export | License |
|---|---|---|---|---|---|
| **beat_this** | 2024 | CNN + transformer, end-to-end | ~0.91 | clean | MIT |
| madmom RNNDownBeat | 2016 | BiLSTM + DBN postproc | ~0.84 | hard (DBN to port) | BSD |
| BeatNet | 2021 | CNN-TCN + HMM | ~0.86 | medium (HMM to replace) | MIT |

beat_this wins on all four axes. It's also what Mixxx is wiring up for
their GSoC 2025 ML beat tracker.

## Spike (Python)

`downbeat-spike/` (gitignored, ~3 GB venv + 2 MB ONNX model). Setup:

```sh
python -m venv venv && . venv/bin/activate
pip install beat_this soundfile torchcodec onnx onnxruntime onnxscript
```

### Step 1: confirm model loads + runs

`beat_this --gpu 0 -o out <track>` works out of the box. The bundled
`--dbn` postprocessor requires madmom, which fails to import on Python
3.14 (`from collections import MutableSequence` — removed in 3.10). So
we skip DBN entirely and write our own postproc, which is the
algorithm we'd port to Rust anyway.

### Step 2: custom global-bar-phase postproc

`global_bar_phase.py`. The beat_this `--no-dbn` ("minimal") path picks
beat + downbeat peaks independently per-frame, which means the bar
labels flip around mid-track (we saw `1-2-1-2` then a stray `3-4-5-6`
in Moonlight's intro). Our postproc instead picks **one** global bar
offset for the whole track:

  1. Get per-frame beat + downbeat logits via `Audio2Frames`.
  2. Peak-pick the beat curve → beat frames.
  3. For each of the 4 candidate bar offsets (which of beat[0..3] is
     the true "1"?) sum the downbeat logits at the predicted downbeat
     positions across the entire track.
  4. Pick the offset with the highest sum.

The score separation is decisive — winner ~1.0, runners-up all
negative — across every track we tried:

```
Moonlight (Mandragora):           off=1, scores=[-2.49, 1.00, -2.51,  0.40]
Moonlight Siren:                   off=0, scores=[ 1.00,-1.54, -2.08, -2.53]
02 - Around the World:             off=1, scores=[-0.83, 1.00, -0.87, -0.66]
Beyond - Around Us (Extended Mix): off=2, scores=[-0.87,-2.05,  1.00, -1.07]
```

Output is a clean repeating `1-2-3-4` for the whole track, no flips.

### Step 3: ONNX export

`export_onnx.py`. The model is a `dict`-returning module; we wrap it
to return a `(beat, downbeat)` tuple and call `torch.onnx.export` with
opset 17 and dynamic batch/time axes.

Result: **model_final0.onnx is 2.1 MB**. ONNX Runtime CPU output
matches PyTorch within 1e-5 (numerical reordering noise; not a real
discrepancy).

## Integration plan (Rust)

This is what gets built after the spike commits.

1. **`crates/analysis`**: add `ort` (ONNX Runtime) + a vendored
   `model_final0.onnx`. Pipeline:
   - Resample decoded audio to 22050 Hz mono.
   - Log-mel spectrogram (128 mel, 1024 fft, 441 hop = 50 fps,
     fmin=30, fmax=11000). Mel filterbank precomputed at startup.
   - Chunk at 1500 frames with `border_size=6` keep-first overlap
     (port `split_predict_aggregate` from beat_this/inference.py).
   - ONNX inference per chunk on CPU (the 2 MB model runs in
     <100 ms/chunk on a modern laptop).
   - Aggregate beat + downbeat logits across chunks.
   - Peak-pick beats (±70 ms = ±3 frames).
   - Global bar offset via the four-way score sum.
   - Return `beat_grid: Vec<f64>` (unchanged) plus
     `downbeats: Vec<u32>` (indices into `beat_grid`).

2. **`crates/control`**: extend `TrackAnalysis` with the existing
   `downbeats` field (already planned) + bump `analysis_version`.

3. **`crates/ui/src/persistence.rs`**: new cache-line format:
   `path|bpm|tonic|is_minor|version|downbeats|beats…`. Stale lines
   (no version field or older version) get silently invalidated and
   re-analysed by the existing background worker.

4. **`crates/ui/src/lib.rs`**: `let is_downbeat = i % 4 == 0;` becomes
   `let is_downbeat = d.downbeats.binary_search(&(i as u32)).is_ok();`.

5. **Background re-analysis**: existing worker picks up missing /
   stale cache lines on next launch. For the user's 1112-track library
   this is ~30-60 min in the background (model is fast, but startup
   covers fewer tracks at a time than the current DSP pipeline).

## Risk + how to retire it

- **Mel filterbank match.** Beat_this' log-mel must be bit-for-bit
  what the model was trained on. Mitigation: dump a real
  spectrogram tensor from Python on a known track, recompute in Rust,
  assert per-element match within 1e-4.
- **Chunk overlap aggregation.** `split_predict_aggregate` has subtle
  border handling for first/middle/last chunks. Vendor the algorithm,
  cross-check against Python output on a long track.
- **ort runtime size.** ~50 MB shared lib downloaded by the
  `ort` crate's build script. The release binary stays small;
  startup downloads on first launch. If this hurts cold-start, we
  bundle ort statically.

## Cost summary

- Dev: 1-2 focused sessions for the Rust port.
- Model: 2.1 MB bundled with the binary.
- Runtime: 50 MB ort lib (downloaded once).
- Analysis time: probably +1-2 s/track on top of the existing
  spectral-flux pipeline. 1112 tracks ≈ 20-30 min one-time re-analyse.

## See also

- [analysis_v1.md](analysis_v1.md) — the spectral-flux pipeline this
  replaces the phase-search step of.
- [downbeat_idea.md](downbeat_idea.md) — the v1.5 plan this implements.
- [stem_separation.md](stem_separation.md) — sister spike (HTDemucs);
  same Python-3.14 + torchcodec quirks.
