//! End-to-end sanity check: run the full analysis pipeline on Moonlight
//! and confirm the first downbeat lands around 0.52 s (matching the
//! Python spike's pick). Skipped automatically when either the MP3 or
//! the ONNX model isn't present.

use std::path::Path;

const MOONLIGHT: &str =
    "/home/ben/Documents/DJ/music/Mandragora, Beltran (BR) - Moonlight (Original Mix).mp3";

#[test]
#[ignore = "depends on a local MP3 + the model; run with --ignored to validate"]
fn first_downbeat_at_about_half_a_second() {
    let path = Path::new(MOONLIGHT);
    if !path.exists() {
        eprintln!("skipping: Moonlight not at {}", path.display());
        return;
    }
    let buf = decode::load_to_buffer(path).expect("decode Moonlight");
    let r = analysis::analyse(&buf);
    assert_eq!(r.analysis_version, 2, "expected model-driven v2 result");
    assert!(
        !r.downbeats.is_empty(),
        "no downbeats — model probably not loaded"
    );
    let first_idx = r.downbeats[0] as usize;
    let first_time = r.beat_grid[first_idx];
    println!(
        "Moonlight: first downbeat at beats[{first_idx}] = {first_time:.3} s, \
         bpm = {:.2}, {} beats, {} downbeats",
        r.bpm,
        r.beat_grid.len(),
        r.downbeats.len(),
    );
    // Python spike's global postproc picked offset=1, so first
    // downbeat = beat index 1 ≈ 0.52 s. Accept ±100 ms for FP drift.
    assert!(
        (first_time - 0.52).abs() < 0.10,
        "first downbeat {first_time:.3} s is too far from spike's 0.52 s",
    );
}
