# HTDemucs → ONNX export — working

Branch: `stems-onnx-export`. Goal: drop the Python `demucs`
subprocess and run HTDemucs natively from Rust via `ort`. Status:
**ONNX export works**; output matches PyTorch within float32
rounding (max abs diff ~7e-05 on a real clip). Rust integration is
the remaining work.

## What it took

`stem-spike/export_demucs_onnx.py` carries four monkey-patches that
have to be in place before `demucs.pretrained.get_model("htdemucs")`
is called. Run via the `stem-spike/venv/`.

### 1. Strip a value-dependent assert from `demucs.hdemucs.pad1d`

```python
assert (out[..., padding_left: padding_left + length] == x0).all()
```

`torch.export.export` refuses to trace value-dependent control flow:
"Could not guard on data-dependent expression". The assert is a
debug check, not load-bearing — patch the function to a no-op.

### 2. Replace `spectro` / `ispectro` with conv1d-based STFT/ISTFT

`demucs.spec.spectro` calls `torch.stft(..., return_complex=True)`.
ONNX opset 17 has an STFT op but the PyTorch symbolic doesn't bridge
complex types. Replace forward STFT with `conv1d` against sin/cos
kernels, output `(..., F, T, 2)` real-stacked. Replace inverse STFT
with `conv_transpose1d` + one-sided Hermitian weighting on each bin
(double interior bins, DC and Nyquist as-is) and `1/n_fft` scaling.

### 3. Stub out `torch.view_as_real` / `view_as_complex`

The CAC (complex-as-channels) path in `_magnitude` and `_mask` calls
these to reshape between complex `(F, T)` and real-stacked
`(F, T, 2)`. With our patched STFT the tensors are already
real-stacked, so both become identity passthroughs.

### 4. Inline-patch `_spec`, `_ispec`, `_magnitude`, `_mask`

Each one assumes complex-shape `(B, C, F, T)` and slices / pads the
last dim as T. With our new `(B, C, F, T, 2)` shape:

- `_spec`: slice `[..., :-1, :, :]` for F and `[..., 2:2+le, :]` for T.
- `_ispec`: pad spec changes from `(0,0,0,1)` → `(0,0,0,0,0,1)` and
  `(2,2)` → `(0,0,2,2)`.
- `_magnitude`: drop the `view_as_real` step (already real-stacked),
  permute and reshape directly.
- `_mask`: drop the `view_as_complex`, return the real-stacked tensor.

The patches are applied to the class methods before
`get_model` instantiates HTDemucs.

## Validation

```
=== torch.onnx.export(dynamo=True) ===
[torch.onnx] Obtain model graph for HTDemucs ✅
[torch.onnx] Run decompositions...           ✅
[torch.onnx] Translate the graph into ONNX...✅
[torch.onnx] Optimize the ONNX graph...      ✅
OK → htdemucs.onnx

max abs diff: 7.0304e-05, rms: 4.2388e-06
```

Tested on a 7.8 s clip from the Epic.mp3 in the spike dir. Output
shape `(1, 4, 2, 343980)` — `(batch, drums/bass/other/vocals, stereo,
samples at 44.1 kHz)`. Bit-for-bit equivalent to PyTorch's output
within FP32 rounding noise.

The bare ONNX is 2.3 MB header + 168 MB `.data` sidecar. Inlined
to a single file: 171 MB. Bigger than beat_this (85 MB) but
manageable as a cached download.

## Next steps (still on this branch)

1. Move the inlined `htdemucs.onnx` to `~/.cache/dj/htdemucs.onnx`.
2. Port `crates/stems` to use `ort` instead of the Python subprocess:
   - Load the model once via `OnceLock<Session>` (same pattern as
     `crates/analysis::downbeat::model()`).
   - Resample input to 44.1 kHz stereo if needed.
   - Chunk into 7.8 s segments; demucs.apply uses an overlap-add
     wrapper with ~3 s border per side at training time. For a
     first pass we can use non-overlapping chunks and accept slight
     edge artefacts, then come back for overlap-add later.
3. Drop the Python venv / subprocess dependency from `start-dj.sh`.

## See also

- [stem_separation.md](stem_separation.md) — the original spike that
  established HTDemucs as the right model.
