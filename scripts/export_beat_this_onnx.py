"""Export beat_this' final0 checkpoint to ONNX.

The model takes a fixed-size log-mel spectrogram chunk and outputs per-frame
beat + downbeat logits. The Python `Audio2Frames` wrapper handles chunking +
overlap-aggregation around the model; we'll replicate that in Rust.

What we need from this script:
  1. Confirm export() succeeds (no unsupported ops).
  2. Save model.onnx.
  3. Verify ONNX Runtime output matches PyTorch on a real chunk.
"""

from __future__ import annotations
import numpy as np
import torch

from beat_this.inference import load_model, LogMelSpect
from beat_this.preprocessing import load_audio

CKPT = "final0"
DEVICE = "cpu"
CHUNK = 1500  # frames; what beat_this' inference loop uses
N_MELS = 128
OUT = "model_final0.onnx"


def main() -> None:
    model = load_model(CKPT, torch.device(DEVICE))
    model.eval()

    dummy = torch.randn(1, CHUNK, N_MELS)  # (batch, time, mel)
    print(f"input shape: {tuple(dummy.shape)}")

    # Probe a real forward pass first to capture the actual output keys.
    with torch.inference_mode():
        out = model(dummy)
    print(f"output keys: {list(out.keys()) if isinstance(out, dict) else 'tensor'}")
    if isinstance(out, dict):
        for k, v in out.items():
            print(f"  {k}: {tuple(v.shape)}")

    # The model returns a dict; for ONNX export we wrap it to return a tuple.
    class Wrap(torch.nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, x):
            r = self.m(x)
            if isinstance(r, dict):
                return r["beat"], r["downbeat"]
            return r

    wrapped = Wrap(model).eval()

    # Export with a static (1, 1500, 128) input shape. We always chunk at
    # 1500 frames in our inference loop, so dynamic axes aren't needed —
    # and the static export avoids tract failing on internal shape
    # constants that get baked in by the dynamo exporter when axes are
    # nominally dynamic.
    torch.onnx.export(
        wrapped,
        (dummy,),
        OUT,
        input_names=["spect"],
        output_names=["beat", "downbeat"],
        opset_version=17,
        dynamo=False,
    )
    print(f"wrote {OUT}")

    # Sanity check vs ONNX Runtime
    try:
        import onnxruntime as ort
    except ImportError:
        print("onnxruntime not installed; skipping verification")
        return

    sess = ort.InferenceSession(OUT, providers=["CPUExecutionProvider"])
    with torch.inference_mode():
        pb, pd = wrapped(dummy)
    ob, od = sess.run(["beat", "downbeat"], {"spect": dummy.numpy()})
    print(
        f"pytorch beat[0,:5] = {pb[0, :5].numpy()}, "
        f"onnx beat[0,:5] = {ob[0, :5]}"
    )
    max_b = float(np.abs(pb.numpy() - ob).max())
    max_d = float(np.abs(pd.numpy() - od).max())
    print(f"max abs diff: beat={max_b:.2e}, downbeat={max_d:.2e}")


if __name__ == "__main__":
    main()
