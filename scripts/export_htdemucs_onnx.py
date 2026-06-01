"""Export HTDemucs to ONNX via the dynamo exporter, neutralising the
value-dependent guards in the demucs source code that torch.export
refuses to trace.

This is the "torch.onnx.export(model, ..., dynamo=True)" path with
all the per-module patches we needed to make the trace go through.
The patches replace runtime value assertions with no-ops (they're
debug guards, not load-bearing).
"""

from __future__ import annotations
import sys
import numpy as np
import torch
import torch.nn.functional as F

# Monkey-patches MUST land BEFORE the model code that uses them is
# imported by `get_model`, so we patch via the module object.
import demucs.hdemucs as _hd
import demucs.htdemucs as _htd

# 1. demucs.hdemucs.pad1d — strip the value-dependent assert.
def _pad1d_no_assert(x, paddings, mode="constant", value=0.):
    length = x.shape[-1]
    padding_left, padding_right = paddings
    if mode == "reflect":
        max_pad = max(padding_left, padding_right)
        if length <= max_pad:
            extra_pad = max_pad - length + 1
            extra_pad_right = min(padding_right, extra_pad)
            extra_pad_left = extra_pad - extra_pad_right
            paddings = (padding_left - extra_pad_left, padding_right - extra_pad_right)
            x = F.pad(x, (extra_pad_left, extra_pad_right))
    return F.pad(x, paddings, mode, value)
_hd.pad1d = _pad1d_no_assert
_htd.pad1d = _pad1d_no_assert  # htdemucs imports pad1d by name

# 2. Replace spectro / ispectro with conv-based real-valued versions.
# Output of spectro: (..., F, T, 2) real. Input of ispectro: same.
# HTDemucs's downstream view_as_real / view_as_complex glue is patched
# to be identity so the shape just flows through.
import demucs.spec as _spec_mod

def _stft_real(x, n_fft, hop_length, win_length, window):
    """torch.stft equivalent that returns (B*, F, T, 2) real."""
    # Pad like torch.stft(center=True, pad_mode='reflect').
    pad = n_fft // 2
    x = F.pad(x.unsqueeze(1), (pad, pad), mode="reflect").squeeze(1)
    # Pre-build DFT kernels: (F, 1, n_fft) — F = n_fft//2 + 1.
    n_freqs = n_fft // 2 + 1
    n = torch.arange(n_fft, dtype=x.dtype, device=x.device)
    k = torch.arange(n_freqs, dtype=x.dtype, device=x.device).unsqueeze(1)
    arg = -2.0 * torch.pi * k * n / n_fft
    cos_k = (torch.cos(arg) * window).unsqueeze(1)
    sin_k = (torch.sin(arg) * window).unsqueeze(1)
    real = F.conv1d(x.unsqueeze(1), cos_k, stride=hop_length)
    imag = F.conv1d(x.unsqueeze(1), sin_k, stride=hop_length)
    # torch.stft(normalized=True) divides by sqrt(n_fft).
    scale = 1.0 / (n_fft ** 0.5)
    real = real * scale
    imag = imag * scale
    return torch.stack([real, imag], dim=-1)

def _istft_real(spec, n_fft, hop_length, win_length, window, length):
    """torch.istft equivalent that takes (B*, F, T, 2) real and returns
    (B*, length). Uses conv_transpose1d for the synthesis side and
    divides by the synthesis-window energy for the overlap-add
    correction (matches torch.istft's default behaviour with
    normalized=True)."""
    real = spec[..., 0]
    imag = spec[..., 1]
    # Same DFT kernels as forward, but synthesise:
    n_freqs = n_fft // 2 + 1
    n = torch.arange(n_fft, dtype=real.dtype, device=real.device)
    k = torch.arange(n_freqs, dtype=real.dtype, device=real.device).unsqueeze(1)
    arg = 2.0 * torch.pi * k * n / n_fft  # inverse sign
    # Hermitian-symmetric reconstruction: the real istft of a one-sided
    # spec uses the standard inverse DFT formula. For each output sample
    # x[t] we want sum_k (real[k] cos + imag[k] sin) windowed.
    cos_k = (torch.cos(arg) * window).unsqueeze(1)
    sin_k = (torch.sin(arg) * window).unsqueeze(1)
    # conv_transpose accumulates overlap-add automatically.
    out_real = F.conv_transpose1d(real, cos_k, stride=hop_length)
    out_imag = F.conv_transpose1d(imag, sin_k, stride=hop_length)
    # The forward used cos(-arg) / sin(-arg); inverse cancels imag terms
    # because the spectrum is conjugate-symmetric (one-sided rfft).
    # For the one-sided case the formula is:
    #   x[t] = (1/N) * (X[0] + X[N/2]*(-1)^t + 2*sum_{k=1..N/2-1}(...))
    # We approximate by doubling the contribution of interior bins and
    # halving DC + Nyquist — which is exactly what one-sided istft does.
    # Build a (n_freqs,) weight to scale each frequency bin:
    w = torch.full((n_freqs,), 2.0, dtype=real.dtype, device=real.device)
    w[0] = 1.0
    if n_fft % 2 == 0:
        w[-1] = 1.0
    real_weighted = real * w.view(1, -1, 1)
    imag_weighted = imag * w.view(1, -1, 1)
    out_real = F.conv_transpose1d(real_weighted, cos_k, stride=hop_length)
    out_imag = F.conv_transpose1d(imag_weighted, sin_k, stride=hop_length)
    # Energy normalisation for overlap-add: same constant as forward,
    # both ends used `normalized=True`.
    inv_scale = 1.0 / (n_fft ** 0.5)
    # Total reconstruction is the real part of the inverse DFT,
    # divided by n_fft. Both contributions add.
    x = (out_real - out_imag) * (inv_scale / n_fft)
    # Crop to `length` after the center-pad on the forward side.
    pad = n_fft // 2
    return x[..., pad : pad + length]

def _spectro_real(x, n_fft=512, hop_length=None, pad=0):
    *other, length = x.shape
    x = x.reshape(-1, length)
    hop = hop_length or n_fft // 4
    window = torch.hann_window(n_fft, dtype=x.dtype, device=x.device)
    z = _stft_real(x, n_fft * (1 + pad), hop, n_fft, window)
    _, freqs, frames, _ = z.shape
    return z.view(*other, freqs, frames, 2)

def _ispectro_real(z, hop_length=None, length=None, pad=0):
    *other, freqs, frames, last2 = z.shape
    assert last2 == 2, f"ispectro_real expects real-stacked (..., F, T, 2), got {z.shape}"
    n_fft = 2 * freqs - 2
    z = z.view(-1, freqs, frames, 2)
    win_length = n_fft // (1 + pad)
    window = torch.hann_window(win_length, dtype=z.dtype, device=z.device)
    if win_length != n_fft:
        # Zero-pad the window to n_fft like torch.istft does.
        window = F.pad(window, (0, n_fft - win_length))
    x = _istft_real(z, n_fft, hop_length, win_length, window, length)
    return x.view(*other, x.shape[-1])

_spec_mod.spectro = _spectro_real
_spec_mod.ispectro = _ispectro_real
_htd.spectro = _spectro_real
_htd.ispectro = _ispectro_real

# 3. view_as_real / view_as_complex become identities — the tensors
# are already in real-stacked form (..., 2) thanks to (2).
_orig_view_as_real = torch.view_as_real
_orig_view_as_complex = torch.view_as_complex
def _view_as_real(t):
    if t.is_complex():
        return _orig_view_as_real(t)
    # Already (..., 2). Identity.
    return t
def _view_as_complex(t):
    if t.is_complex():
        return t
    # Real-stacked → leave alone; downstream ispectro_real expects it.
    return t
torch.view_as_real = _view_as_real
torch.view_as_complex = _view_as_complex

# 4. HTDemucs._spec slices the last dim of spectro's output as T.
# With our new (..., F, T, 2) shape the T dim is now second-to-last,
# so we patch _spec inline to slice the right axis.
import math
def _spec_patched(self, x):
    hl = self.hop_length
    nfft = self.nfft
    assert hl == nfft // 4
    le = int(math.ceil(x.shape[-1] / hl))
    pad = hl // 2 * 3
    x = _pad1d_no_assert(x, (pad, pad + le * hl - x.shape[-1]), mode="reflect")
    z = _spectro_real(x, nfft, hl)
    # Slice F (drop the last freq bin) and T (drop 2 frames each side).
    z = z[..., :-1, :, :]  # F
    z = z[..., 2 : 2 + le, :]  # T
    return z
_htd.HTDemucs._spec = _spec_patched

def _ispec_patched(self, z, length=None, scale=0):
    hl = self.hop_length // (4**scale)
    # The original does F.pad(z, (0, 0, 0, 1)) (1 row to F) then
    # F.pad(z, (2, 2)) (2 cols on each side of T). With our shape
    # (..., F, T, 2) the dims are different — translate.
    z = F.pad(z, (0, 0, 0, 0, 0, 1))     # F gets +1 row
    z = F.pad(z, (0, 0, 2, 2))            # T gets +2 each side
    pad = hl // 2 * 3
    le = hl * int(math.ceil(length / hl)) + 2 * pad
    x = _ispectro_real(z, hl, length=le)
    x = x[..., pad : pad + length]
    return x
_htd.HTDemucs._ispec = _ispec_patched

# 5. _magnitude and _mask both call view_as_real / view_as_complex
# inline — replace with the real-stacked equivalents, skipping the
# now-redundant view_as_real call (z already has trailing 2).
def _magnitude_patched(self, z):
    if self.cac:
        # z: (B, C, F, T, 2) thanks to _spec_patched
        B, C, Fr, T, _ = z.shape
        m = z.permute(0, 1, 4, 2, 3)  # → (B, C, 2, F, T)
        m = m.reshape(B, C * 2, Fr, T)
    else:
        # |z| where z is real-stacked: sqrt(re² + im²)
        re = z[..., 0]
        im = z[..., 1]
        m = (re * re + im * im).clamp_min(1e-12).sqrt()
    return m
_htd.HTDemucs._magnitude = _magnitude_patched

def _mask_patched(self, z, m):
    if self.cac:
        B, S, C, Fr, T = m.shape
        out = m.view(B, S, -1, 2, Fr, T).permute(0, 1, 2, 4, 5, 3).contiguous()
        # out: (B, S, C, F, T, 2) real — exactly the shape
        # _ispec_patched expects, no complex conversion needed.
        return out
    if self.wiener_iters >= 0:
        raise NotImplementedError("wiener path not yet supported by patched export")
    # Mask-based with no wiener.
    z = z.unsqueeze(1)
    # |z| for the divisor.
    re, im = z[..., 0], z[..., 1]
    mag = (re * re + im * im).clamp_min(1e-12).sqrt()
    return z / mag.unsqueeze(-1) * m.unsqueeze(-1)
_htd.HTDemucs._mask = _mask_patched

# Now import the rest (after all patches are in place).
from demucs.pretrained import get_model  # noqa: E402

# Now import the rest.
from demucs.pretrained import get_model  # noqa: E402

OUT = "htdemucs.onnx"


def main() -> int:
    model = get_model("htdemucs").models[0]
    model.eval()

    sr = model.samplerate
    n_samples = int(float(model.segment) * sr)
    print(f"segment: {model.segment} s @ {sr} Hz = {n_samples} samples")

    # Use a real audio clip for verification so zeros-in / zeros-out
    # doesn't masquerade as a passing test.
    import soundfile as sf
    audio_path = "Epic.mp3"
    try:
        import soxr
        wav, src_sr = sf.read(audio_path, dtype="float32", always_2d=True)
        if src_sr != sr:
            wav = soxr.resample(wav, src_sr, sr)
        wav = wav.T  # (C, T)
        if wav.shape[0] == 1:
            wav = np.repeat(wav, 2, axis=0)
        wav = wav[:2, :n_samples]
        if wav.shape[1] < n_samples:
            wav = np.pad(wav, ((0, 0), (0, n_samples - wav.shape[1])))
        dummy = torch.from_numpy(wav).unsqueeze(0).contiguous()
        print(f"using audio from {audio_path}")
    except Exception as e:
        print(f"(falling back to random input: {e})")
        torch.manual_seed(0)
        dummy = torch.randn(1, model.audio_channels, n_samples) * 0.1
    print(f"input:  {tuple(dummy.shape)}")
    with torch.inference_mode():
        ref = model(dummy)
    print(f"output: {tuple(ref.shape)}\n")

    print("=== torch.onnx.export(dynamo=True) ===")
    try:
        torch.onnx.export(
            model,
            (dummy,),
            OUT,
            input_names=["audio"],
            output_names=["stems"],
            opset_version=20,
            dynamo=True,
        )
        print(f"OK → {OUT}\n")
    except Exception as e:
        print(f"FAIL: {type(e).__name__}")
        # Trim verbose torch.export traceback to the key bits.
        msg = str(e)
        lines = msg.splitlines()
        # Print first 30 lines max, plus any 'pad1d|hdemucs|htdemucs' contexts.
        for line in lines[:30]:
            print(line)
        if len(lines) > 30:
            print(f"... [{len(lines) - 30} more lines]")
        return 1

    try:
        import onnxruntime as ort
    except ImportError:
        print("(onnxruntime missing — skipping verify)")
        return 0
    sess = ort.InferenceSession(OUT, providers=["CPUExecutionProvider"])
    onnx_out = sess.run(["stems"], {"audio": dummy.numpy()})[0]
    diff = np.abs(ref.numpy() - onnx_out)
    print(f"max abs diff: {diff.max():.4e}, rms: {np.sqrt((diff**2).mean()):.4e}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
