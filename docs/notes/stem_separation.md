# Stem separation — spike findings + integration plan

A 2026-05-31 spike to pick a model and shape the integration. TL;DR:
**HTDemucs 4-stem on the GPU, exposed as 3 controls per deck.**

## Models tested

All on a 5 min 1080p MP3 ("Epic", downloaded via yt-dlp), RTX 4060
Laptop GPU, in `stem-spike/` (gitignored).

| Model | Run | Stems out | Notes |
|---|---|---|---|
| **HTDemucs** (`htdemucs`, v4) | **~15 s** | drums, bass, vocals, other | Hybrid Transformer Demucs v4, MIT-licensed, the de facto open default. Mixxx is wiring this up via ONNX in GSoC 2025. |
| **HTDemucs 6-stem** (`htdemucs_6s`) | ~20 s | drums, bass, vocals, other, piano, guitar | Same architecture, more output heads. Slight quality drop on the core 4 stems; piano/guitar useful if the track has them. |
| **Mel-Band RoFormer** (Kim Jensen, audio-separator) | ~77 s | vocals, instrumental (2-stem only) | SDR 12.6 dB for vocals (HTDemucs is ~9). Heavier model, slower. Would need a cascade to a second model for drums/bass. |

## Decision

**HTDemucs 4-stem.** Not worth the cascade time penalty for slightly
cleaner vocals when DJing — the master-vs-cue blend in the headphones
already gives plenty of margin to hear what's there. Mixing 6 stems live
is past human capacity ("for us mere humans in real time"); 3 is the
sweet spot.

UI design: **3 stem faders per deck** —
- **drums** ← the drums stem
- **bass**  ← the bass stem
- **melody** ← vocals + other summed at playback

Source on disk stays as 4 stems so we can rearrange/expose all 4 later
without re-running separation.

## Why HTDemucs over RoFormer for this project

- **Inference cost.** 15 s vs 77 s per track for ~20-track libraries is
  insignificant; for the user's 550-track library that's ~2 hours of
  pre-compute vs ~12 hours.
- **Single model, single pass.** Cascade ensembles add complexity for a
  use-case (DJ stem manipulation) where ~9 dB vocal SDR is already plenty.
- **Mixxx is going this way.** Their GSoC 2025 ONNX export is HTDemucs,
  meaning when we want to ditch the Python dependency and run inference
  natively in Rust via `ort`, the work is already done upstream.
- **MIT licence**, no entanglement.

If the project ever cares about vocal-isolation quality for a specific
use case (acapella export, karaoke), revisit RoFormer + cascade.

## Storage path

- Raw WAV: 4 × ~50 MB per ~5-min track ≈ 200 MB/track. Too much.
- **FLAC** (lossless): ~50 % of WAV ≈ 100 MB/track. Sensible default.
- Opus 192 k: ~5–10 MB/stem ≈ 30 MB/track. Lossy but audibly fine for
  the DJ stem-mute use case. Probably the right tradeoff if a 550-track
  library hits storage limits.

Cache directory parallel to the analysis cache:
`<music-dir>/.stems-cache/<track-hash>/{drums,bass,vocals,other}.flac`.
Invalidate on track-file mtime change (same trigger as the analysis-cache
invalidation already on the TODO).

## Integration sketch (not built yet)

1. **Analysis worker** (`crates/ui/src/persistence.rs`?) gains a stem
   step after BPM/key: shell out to `demucs` (or a future `ort`-based
   in-process model) when stems aren't cached. Slow, off-thread,
   non-blocking; surfaces a "stems X/Y" progress label.
2. **decode** crate: `TrackBuffer` grows from one stereo buffer to four
   (drums, bass, vocals, other). If a cached stem set is found alongside
   a track, load those; otherwise fall back to the single buffer
   (gain controls just no-op until separation finishes).
3. **audio** crate (`Mixer`): per-deck stem gains as 3 `f32`s
   (`gain_drums`, `gain_bass`, `gain_melody`). Render becomes
   `gain_drums·drums + gain_bass·bass + gain_melody·(vocals+other)`.
   No new allocs, lock-free reads from atomics (same pattern as the
   existing per-deck `gain_linear`).
4. **control**: 3 new `DeckCommand` variants — `SetStemDrums`,
   `SetStemBass`, `SetStemMelody`.
5. **ui** crate: 3 mini-faders per deck panel, between the EQ row and
   the pitch slider.
6. **MIDI**: once the PCB is fabbed and Deck B is firmware-supported,
   wire pads/encoders to stem-kill commands. Out of scope for the
   first integration pass.

## Python 3.14 quirks hit during the spike

- `torchaudio` 2.11 needs the **`torchcodec`** package to save WAVs
  (its old SoX/FFmpeg backends are deprecated).
- `audio-separator` pins `beartype<0.19`, which doesn't parse
  `collections.abc.Callable | None` under Python 3.14. Force-upgrading
  to `beartype>=0.22` makes it run (pip warns about the pin clash; no
  functional issue). If we ever package this for non-developers, pin
  Python to 3.12 in the venv setup.

## How to re-run the spike

```sh
cd stem-spike
. venv/bin/activate
python -m demucs.separate --device cuda -o stems <track>           # 4-stem
python -m demucs.separate -n htdemucs_6s --device cuda -o stems <track>  # 6-stem
audio-separator <track> --model_filename vocals_mel_band_roformer.ckpt \
    --output_dir stems/roformer                                    # 2-stem RoFormer
```

See also: [overview.md](overview.md), [analysis_v1.md](analysis_v1.md)
(the existing offline-analysis pipeline this would plug into).
