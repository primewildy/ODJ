//! One-off diagnostic — run the full pipeline on "Are We Truly Free"
//! and print the model's bar-offset scores for the chosen window.
//! Skipped unless `--ignored` is passed.

use std::path::Path;

const TRACK_TRULY_FREE: &str =
    "/home/ben/Documents/DJ/music/KAS_ST - Are We Truly Free_ (Original Mix).mp3";
const TRACK_TOUCH_DIAL: &str =
    "/home/ben/Documents/DJ/music/Rossi. - Don_t Touch That Dial (Original Mix).mp3";

fn dump_one(track: &str) {
    let path = Path::new(track);
    if !path.exists() {
        eprintln!("skipping: track not at {}", path.display());
        return;
    }
    let buf = decode::load_to_buffer(path).expect("decode");
    let r = analysis::analyse(&buf);
    let first_idx = r.downbeats.first().copied().unwrap_or(0) as usize;
    let first_time = r.beat_grid.get(first_idx).copied().unwrap_or(0.0);
    println!(
        "{}: bpm={:.2}, first downbeat = beat[{}] @ {:.3}s ({} beats, {} downbeats)",
        Path::new(track).file_name().unwrap().to_string_lossy(),
        r.bpm,
        first_idx,
        first_time,
        r.beat_grid.len(),
        r.downbeats.len(),
    );
}

#[test]
#[ignore = "diagnostic only — run with --ignored"]
fn dump_touch_dial() {
    dump_one(TRACK_TOUCH_DIAL);
}

#[test]
#[ignore = "diagnostic only — run with --ignored"]
fn dump_emotion() {
    dump_one("/home/ben/Documents/DJ/music/112. Toman - Emotion (Extended Mix).mp3");
    dump_one("/home/ben/Documents/DJ/music/Eli Fola - Emotion (Extended Mix).mp3");
    dump_one("/home/ben/Documents/DJ/music/Emotion - Toman (Extended Mix) 132.mp3");
}

#[test]
#[ignore = "diagnostic only — run with --ignored"]
fn dump() {
    let TRACK = TRACK_TRULY_FREE;
    let path = Path::new(TRACK);
    if !path.exists() {
        eprintln!("skipping: track not at {}", path.display());
        return;
    }
    let buf = decode::load_to_buffer(path).expect("decode");
    let r = analysis::analyse(&buf);
    let first_idx = r.downbeats.first().copied().unwrap_or(0) as usize;
    let first_time = r.beat_grid.get(first_idx).copied().unwrap_or(0.0);
    println!(
        "RESULT: bpm={:.2}, version={}, first downbeat = beat[{}] @ {:.3}s (track has {} beats, {} downbeats)",
        r.bpm,
        r.analysis_version,
        first_idx,
        first_time,
        r.beat_grid.len(),
        r.downbeats.len(),
    );
    println!(
        "RESULT: beats[0..6] = {:?}",
        r.beat_grid.iter().take(6).copied().collect::<Vec<_>>()
    );
}
