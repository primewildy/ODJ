"""Custom global-bar-phase postprocessor for beat_this.

beat_this' built-in "minimal" postproc picks per-frame beat + downbeat peaks
independently, which means the bar grid flips around mid-track (we saw labels
like 1-2-1-2 then a stray 3-4-5-6 in Moonlight). The DBN postproc fixes this
but needs madmom, which doesn't load on Python 3.14.

This script implements a simpler global-mode postproc:
  1. Get per-frame beat + downbeat activation logits from beat_this.
  2. Pick beat peaks (this is the beat grid).
  3. For each beat, sample the downbeat logit at that frame.
  4. Try all 4 candidate bar offsets (which of beat[0..3] is the true "1"?)
     and pick the offset where the sum of downbeat logits at predicted
     downbeats is highest.
  5. Emit a (time, bar_position) TSV the same shape as the beat_this CLI.

This is the exact algorithm we'd port to Rust.
"""

from __future__ import annotations
import argparse
import sys
from pathlib import Path

import numpy as np
import torch
import torch.nn.functional as F

from beat_this.inference import Audio2Frames
from beat_this.preprocessing import load_audio

FPS = 50  # beat_this' fixed frame rate (1 frame = 20 ms)


def peak_pick(logits: np.ndarray, half_win: int = 3, thresh: float = 0.0) -> np.ndarray:
    """Return frame indices of peaks within a +/- half_win sliding window."""
    t = torch.tensor(logits[None, None, :], dtype=torch.float32)
    pooled = F.max_pool1d(t, kernel_size=2 * half_win + 1, stride=1, padding=half_win)
    is_peak = (t == pooled) & (t > thresh)
    return is_peak.squeeze().nonzero().squeeze(-1).cpu().numpy()


def global_bar_offset(
    beat_frames: np.ndarray, downbeat_logits: np.ndarray
) -> tuple[int, list[float]]:
    """Among the four candidate bar offsets (0..3 beats into the grid), pick
    the one where sum of downbeat logits at the predicted downbeats is highest.
    Returns (best_offset, [score_per_offset])."""
    scores = []
    for off in range(4):
        idx = beat_frames[off::4]
        scores.append(float(downbeat_logits[idx].sum()))
    best = int(np.argmax(scores))
    return best, scores


def process(path: Path) -> list[tuple[float, int]]:
    signal, sr = load_audio(str(path))
    model = Audio2Frames(device="cuda" if torch.cuda.is_available() else "cpu")
    beat_logits, downbeat_logits = model(signal, sr)
    # logits are (T,) tensors.
    bl = beat_logits.float().cpu().numpy()
    dl = downbeat_logits.float().cpu().numpy()

    beat_frames = peak_pick(bl, half_win=3)
    if len(beat_frames) < 4:
        return []

    off, scores = global_bar_offset(beat_frames, dl)
    process.last_scores = scores
    process.last_off = off
    out = []
    for i, fi in enumerate(beat_frames):
        bar_pos = ((i - off) % 4) + 1
        out.append((fi / FPS, bar_pos))
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("inputs", nargs="+", type=Path)
    ap.add_argument("--outdir", "-o", type=Path, default=Path("out_global"))
    args = ap.parse_args()
    args.outdir.mkdir(exist_ok=True)

    for p in args.inputs:
        print(f"-> {p.name}")
        beats = process(p)
        if not beats:
            print(f"  (no beats)")
            continue
        out_path = args.outdir / (p.stem + ".beats")
        with out_path.open("w") as f:
            for t, b in beats:
                f.write(f"{t:.4f}\t{b}\n")
        ones = [t for t, b in beats if b == 1]
        firstone = ones[0] if ones else float("nan")
        scores = getattr(process, "last_scores", [])
        s_norm = [f"{s/max(scores):.2f}" for s in scores] if scores else []
        print(f"  {len(beats)} beats, first '1' @ {firstone:.3f}s, off={process.last_off}, scores={s_norm}, wrote {out_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
