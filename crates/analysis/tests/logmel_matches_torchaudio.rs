//! Compares our Rust log-mel against a Python tensor dumped from
//! torchaudio.MelSpectrogram with the exact beat_this settings.
//!
//! Reference data (5 s of Moonlight, audio + log-mel) lives in
//! `tests/fixtures/`. Regenerate via
//! `python downbeat-spike/dump_reference.py`.

use std::fs;
use std::path::PathBuf;

use analysis::logmel::{LogMel, N_MELS};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read_f32(path: &PathBuf) -> Vec<f32> {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    assert_eq!(bytes.len() % 4, 0, "{path:?} length not a multiple of 4");
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[test]
fn matches_torchaudio_within_tolerance() {
    let audio = read_f32(&fixture("moonlight_audio.f32"));
    let reference = read_f32(&fixture("moonlight_logmel.f32"));

    let shape_str = fs::read_to_string(fixture("moonlight_logmel.shape")).unwrap();
    let mut it = shape_str.split_whitespace();
    let n_frames: usize = it.next().unwrap().parse().unwrap();
    let n_mels: usize = it.next().unwrap().parse().unwrap();
    assert_eq!(n_mels, N_MELS, "reference n_mels {n_mels} != {N_MELS}");
    assert_eq!(
        reference.len(),
        n_frames * n_mels,
        "reference logmel length mismatch"
    );

    let lm = LogMel::new();
    let ours = lm.compute(&audio);
    assert_eq!(ours.len(), reference.len(), "frame-count mismatch");

    let mut max_abs = 0.0_f32;
    let mut sum_sq = 0.0_f64;
    for (&a, &b) in ours.iter().zip(reference.iter()) {
        let d = (a - b).abs();
        if d > max_abs {
            max_abs = d;
        }
        sum_sq += (d as f64) * (d as f64);
    }
    let rms = (sum_sq / ours.len() as f64).sqrt() as f32;
    println!("logmel diff: max_abs={max_abs:.4e}, rms={rms:.4e}");
    // Tolerance: 1e-2 max-abs is roomy enough for FP reordering and
    // the slightly-different STFT implementations; the model only
    // cares about the broad envelope.
    assert!(
        max_abs < 1e-2,
        "logmel max_abs diff {max_abs:.4e} too large"
    );
}
