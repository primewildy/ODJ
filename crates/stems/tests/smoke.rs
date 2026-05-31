//! End-to-end smoke test: run HTDemucs ONNX on a real track and
//! sanity-check the four stems came out non-trivial.
//! Requires `~/.cache/dj/htdemucs.onnx` to be in place — see
//! docs/notes/htdemucs_onnx_export.md.

use std::path::Path;

#[test]
#[ignore = "depends on local audio + ONNX model — run with --ignored"]
fn separate_real_track() {
    let track = "/home/ben/Documents/DJ/stem-spike/Epic.mp3";
    if !Path::new(track).exists() {
        eprintln!("skipping: {track} not present");
        return;
    }
    let cache = stems::SessionCache::new().expect("cache");
    let t0 = std::time::Instant::now();
    let s = cache.separate(Path::new(track)).expect("separate");
    let elapsed = t0.elapsed().as_secs_f32();
    println!(
        "separated in {elapsed:.1}s: drums={} bass={} vocals={} other={} samples \
         @ {} Hz {} ch",
        s.drums.len(),
        s.bass.len(),
        s.vocals.len(),
        s.other.len(),
        s.sample_rate,
        s.channels,
    );
    let energy = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
    println!(
        "RMS: drums={:.4} bass={:.4} vocals={:.4} other={:.4}",
        energy(&s.drums),
        energy(&s.bass),
        energy(&s.vocals),
        energy(&s.other),
    );
    assert_eq!(s.channels, 2);
    assert_eq!(s.sample_rate, 44_100);
    // Each stem should have at least SOME signal.
    assert!(energy(&s.drums) > 1e-5, "drums stem is silent");
    assert!(energy(&s.bass) > 1e-5, "bass stem is silent");
    assert!(energy(&s.vocals) > 1e-5, "vocals stem is silent");
    assert!(energy(&s.other) > 1e-5, "other stem is silent");
}
