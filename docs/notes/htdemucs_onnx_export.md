# HTDemucs → ONNX export — feature-branch attempt (failed)

Branch: `stems-onnx-export`. Goal: drop the Python `demucs` subprocess
and run HTDemucs natively from Rust via `ort`. Status:
**aborted** after two layered failures; bailing back to the Python
subprocess until upstream (Mixxx GSoC 2025) ships an ONNX export.

## Setup

- Pretrained model: `demucs.pretrained.get_model("htdemucs").models[0]`
- Input shape: `(1, 2, 343980)` — `(batch, channels, time)`, 7.8 s
  segment at 44.1 kHz.
- Output: `(1, 4, 2, 343980)` — `(batch, sources, channels, time)`.

## Failure 1 — torchscript exporter

```
SymbolicValueError: STFT does not currently support complex types
```

`demucs.spec.spectro` calls `torch.stft(..., return_complex=True)`.
The torchscript exporter's symbolic table maps `aten::stft` to the
ONNX STFT op but only for real-valued I/O — complex tensors get a
`SymbolicValueError` at the first reshape after the STFT.

### Attempted patch

Replace `spectro` with a conv1d-based real STFT that returns
`(..., F, T, 2)` instead of `(..., F, T)` complex. This works for
the immediate `view_as_real` call inside `_magnitude`, but the
*surrounding* slicing in `HTDemucs._spec`:

```python
z = spectro(x, nfft, hl)[..., :-1, :]  # slice F then T
assert z.shape[-1] == le + 4
z = z[..., 2: 2 + le]
```

assumes the last dim is `T`, not `2`. So the patch cascades: now
`_spec`, `_magnitude`, `_mask`, `_ispec` all need rewriting in
lockstep. Roughly 4-6 hooks across `htdemucs.py`.

## Failure 2 — dynamo exporter

`torch.onnx.export(model, ..., dynamo=True)` (after
`pip install onnxscript`) gets past the complex-STFT issue —
torch.export.export does support complex via the new
`torch.aten.stft.center` overload. New failure:

```
GuardOnDataDependentSymNode:
Could not guard on data-dependent expression Eq(u0, 1)
Caused by:  demucs/hdemucs.py:39 in pad1d
    assert (out[..., padding_left: padding_left + length] == x0).all()
```

`pad1d` runs an *assertion comparing tensor values* on the trace
path. `torch.export.export` can't tell whether the assertion holds
under arbitrary input, so it refuses to trace. Removing the assert
is one-line; but it's representative — HTDemucs's source has
several similar value-dependent guards in the time/spec branch glue,
and dynamic-shape behaviour around the segment-length padding.

## Honest estimate

To push through to a working export probably takes ~1-2 days of
PR-quality work:
- Strip / convert value-dependent asserts to symbolic checks (or
  remove them — they're debug guards).
- Replace `spectro`/`ispectro` with conv-based equivalents and update
  the 4-6 downstream call sites that assume complex shape.
- Verify ONNX Runtime output matches PyTorch within tolerance on a
  real clip (cross-checking the STFT replacement is the hard part —
  STFT normalisation conventions are easy to get wrong).
- Probably more issues we haven't tripped on yet (RoPE attention,
  wiener filter fallback path, ispectro's istft op).

## Decision

**Stick with the Python subprocess for now.** The user-facing
behaviour is identical (stems separate on GPU at ~13 s/track and the
audio engine doesn't care how they got there). When Mixxx's GSoC
2025 ONNX export lands upstream (their work is in flight), we can
drop in `ort` and a vendored model and the Python venv goes away.

This branch (`stems-onnx-export`) is kept as a record of the failed
spike; the actual stem code on `main` is unchanged.

## Alternative paths (not tried)

- **candle / burn re-implementation.** Manually port the HTDemucs
  forward pass to a Rust ML framework. Biggest engineering surface
  but no Python and no ONNX. Probably a multi-session project.
- **Mel-Band RoFormer** (which has community ONNX exports) — but
  it's 2-stem only, losing drums and bass isolation.
- **Wait.** Mixxx is doing the same thing we'd be doing, with more
  hands. Their export will be the reference.

See: [stem_separation.md](stem_separation.md) for the full plan
this branched off of.
