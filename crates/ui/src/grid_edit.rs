//! Pure grid-edit operations on `TrackAnalysis`.
//!
//! Each op takes the current analysis and returns a new one with the
//! beat_grid / bpm / downbeats adjusted. No I/O, no engine calls — the
//! UI layer rebuilds an `Arc<TrackAnalysis>` from the result and sends
//! `DeckCommand::UpdateAnalysis` to swap it in live.
//!
//! Why pure functions: they're trivially testable (synthetic input →
//! expected output) — exactly the kind of testing TODO.md asks for.

use control::TrackAnalysis;

/// Shift every beat time by `delta_secs`. Positive = later in the track,
/// negative = earlier. Beats that fall before 0 after the shift are
/// dropped; downbeat indices are adjusted to point at the same beat
/// they referred to before the shift (so "bar 1" stays on the same
/// musical event).
pub fn shifted(an: &TrackAnalysis, delta_secs: f64) -> TrackAnalysis {
    let shifted: Vec<(usize, f64)> = an
        .beat_grid
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let nt = t + delta_secs;
            if nt >= 0.0 { Some((i, nt)) } else { None }
        })
        .collect();
    let drop_count = an.beat_grid.len() - shifted.len();
    let new_grid: Vec<f64> = shifted.iter().map(|(_, t)| *t).collect();
    // Old beat index → new beat index. Old indices that survived
    // re-number 0..new_grid.len() in order; dropped indices have no
    // mapping (their downbeat references are removed).
    let new_downbeats: Vec<u32> = an
        .downbeats
        .iter()
        .filter_map(|&db_old| {
            let new_idx = (db_old as i64) - (drop_count as i64);
            if new_idx >= 0 && new_idx < new_grid.len() as i64 {
                Some(new_idx as u32)
            } else {
                None
            }
        })
        .collect();
    rebuild(an, new_grid, new_downbeats, an.bpm)
}

/// Re-anchor the grid by `n_beats`. Positive → grid moves later by N
/// beats (every beat shifts +N × period); negative → earlier. Same
/// effect as `shifted(an, n_beats * 60.0 / bpm)` but uses the
/// existing grid spacing so it survives small BPM detection error.
pub fn skip_beats(an: &TrackAnalysis, n_beats: i32) -> TrackAnalysis {
    if an.beat_grid.is_empty() || an.bpm <= 0.0 || n_beats == 0 {
        return clone_analysis(an);
    }
    let period = 60.0 / an.bpm as f64;
    shifted(an, n_beats as f64 * period)
}

/// Halve the BPM by dropping every other beat. The downbeat anchored
/// by `downbeats[0]` (or beat 0 if none) stays at the same time.
pub fn bpm_halved(an: &TrackAnalysis) -> TrackAnalysis {
    if an.beat_grid.len() < 2 {
        return clone_analysis(an);
    }
    // Keep beats whose offset-from-anchor is even.
    let anchor = an.downbeats.first().copied().unwrap_or(0) as usize;
    let new_grid: Vec<f64> = an
        .beat_grid
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let offset = i as i64 - anchor as i64;
            if offset.rem_euclid(2) == 0 { Some(*t) } else { None }
        })
        .collect();
    // Downbeats: every old downbeat that survived becomes a downbeat
    // at half cadence — easier to just re-derive `i % 4 == 0` from
    // the new anchor.
    let new_db = derive_downbeats(&new_grid, new_anchor_index(anchor, &an.beat_grid, &new_grid));
    rebuild(an, new_grid, new_db, an.bpm * 0.5)
}

/// Double the BPM by inserting a midpoint beat between every adjacent
/// pair. New BPM = old × 2. Downbeats are re-derived from the same
/// anchor beat (which is now at index 2× its old position).
pub fn bpm_doubled(an: &TrackAnalysis) -> TrackAnalysis {
    if an.beat_grid.len() < 2 {
        return clone_analysis(an);
    }
    let mut new_grid: Vec<f64> = Vec::with_capacity(an.beat_grid.len() * 2);
    for w in an.beat_grid.windows(2) {
        new_grid.push(w[0]);
        new_grid.push(0.5 * (w[0] + w[1]));
    }
    // Last beat doesn't pair with anything — push it solo.
    new_grid.push(*an.beat_grid.last().unwrap());

    let anchor_old = an.downbeats.first().copied().unwrap_or(0) as usize;
    let new_anchor = (anchor_old * 2).min(new_grid.len().saturating_sub(1));
    let new_db = derive_downbeats(&new_grid, new_anchor);
    rebuild(an, new_grid, new_db, an.bpm * 2.0)
}

/// Mark the beat nearest `t` as bar-position-1. Rewrites `downbeats`
/// as every 4th index starting from that beat. Grid + BPM unchanged.
pub fn set_downbeat_at(an: &TrackAnalysis, t: f64) -> TrackAnalysis {
    if an.beat_grid.is_empty() {
        return clone_analysis(an);
    }
    let anchor = nearest_beat_index(&an.beat_grid, t);
    let new_db = derive_downbeats(&an.beat_grid, anchor);
    rebuild(an, an.beat_grid.clone(), new_db, an.bpm)
}

// ---- helpers ---------------------------------------------------------

fn clone_analysis(an: &TrackAnalysis) -> TrackAnalysis {
    TrackAnalysis {
        analysis_version: an.analysis_version,
        bpm: an.bpm,
        beat_grid: an.beat_grid.clone(),
        downbeats: an.downbeats.clone(),
        duration_secs: an.duration_secs,
        sample_rate: an.sample_rate,
        key: an.key,
    }
}

fn rebuild(
    src: &TrackAnalysis,
    beat_grid: Vec<f64>,
    downbeats: Vec<u32>,
    bpm: f32,
) -> TrackAnalysis {
    TrackAnalysis {
        analysis_version: src.analysis_version,
        bpm,
        beat_grid,
        downbeats,
        duration_secs: src.duration_secs,
        sample_rate: src.sample_rate,
        key: src.key,
    }
}

fn nearest_beat_index(beats: &[f64], t: f64) -> usize {
    match beats.binary_search_by(|b| {
        b.partial_cmp(&t).unwrap_or(std::cmp::Ordering::Equal)
    }) {
        Ok(i) => i,
        Err(i) => {
            if i == 0 { 0 }
            else if i >= beats.len() { beats.len() - 1 }
            else {
                let a = beats[i - 1];
                let b = beats[i];
                if (t - a).abs() <= (t - b).abs() { i - 1 } else { i }
            }
        }
    }
}

/// `new_grid` is a sub-sequence of `old_grid` (every 2nd, every 4th, …).
/// Find the position of `old_grid[old_anchor]` in `new_grid` (or the
/// closest surviving beat if the anchor itself was dropped).
fn new_anchor_index(old_anchor: usize, old_grid: &[f64], new_grid: &[f64]) -> usize {
    let Some(t) = old_grid.get(old_anchor).copied() else { return 0; };
    nearest_beat_index(new_grid, t)
}

/// Every 4th index starting from `anchor`. 4/4-ish music — same fallback
/// the rest of the app uses when the model didn't fire.
fn derive_downbeats(grid: &[f64], anchor: usize) -> Vec<u32> {
    if grid.is_empty() { return Vec::new(); }
    let anchor = anchor.min(grid.len() - 1);
    // Walk back to the earliest index that's still in phase with the
    // anchor so bar 1 marker appears at the start of the track when
    // possible. anchor - 4k for k = 0,1,2,... until we'd go negative.
    let first = anchor % 4;
    let mut out = Vec::new();
    let mut i = first;
    while i < grid.len() {
        out.push(i as u32);
        i += 4;
    }
    out
}

// ---- tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(bpm: f32, n: usize) -> TrackAnalysis {
        let period = 60.0 / bpm as f64;
        let beat_grid: Vec<f64> = (0..n).map(|i| i as f64 * period).collect();
        // Every 4th = downbeat, starting at 0.
        let downbeats: Vec<u32> = (0..n as u32).step_by(4).collect();
        TrackAnalysis {
            analysis_version: 2,
            bpm,
            beat_grid,
            downbeats,
            duration_secs: n as f64 * period,
            sample_rate: 44_100,
            key: None,
        }
    }

    #[test]
    fn shifted_positive_pushes_grid_later() {
        let an = synth(120.0, 16);
        let out = shifted(&an, 0.010); // 10 ms
        assert_eq!(out.beat_grid.len(), 16);
        assert!((out.beat_grid[0] - 0.010).abs() < 1e-9);
        assert!((out.beat_grid[1] - (0.500 + 0.010)).abs() < 1e-9);
        assert_eq!(out.downbeats, an.downbeats);
        assert_eq!(out.bpm, 120.0);
    }

    #[test]
    fn shifted_negative_drops_beats_before_zero() {
        let an = synth(120.0, 16);
        // Beats are at 0, 0.5, 1.0, … Shift back by 0.6 s — beats at
        // t=0 (−0.6) and t=0.5 (−0.1) drop, beat at t=1.0 (0.4)
        // survives. 14 beats remain.
        let out = shifted(&an, -0.6);
        assert_eq!(out.beat_grid.len(), 14);
        assert!(out.beat_grid[0] >= 0.0);
        // Original downbeats [0,4,8,12]. Drops two beats → new
        // indices [-2,2,6,10]; the negative one disappears so the
        // first surviving downbeat is at new index 2.
        assert_eq!(out.downbeats.first().copied(), Some(2));
    }

    #[test]
    fn skip_one_beat_equals_one_period_shift() {
        let an = synth(120.0, 16);
        let out = skip_beats(&an, 1);
        let period = 60.0 / 120.0;
        for (a, b) in an.beat_grid.iter().zip(out.beat_grid.iter()) {
            assert!((b - (a + period)).abs() < 1e-9);
        }
    }

    #[test]
    fn skip_negative_beats_walks_grid_back() {
        let an = synth(120.0, 16);
        let out = skip_beats(&an, -1);
        // First beat (t=0) shifts to -0.5 s → dropped.
        assert_eq!(out.beat_grid.len(), 15);
    }

    #[test]
    fn halve_bpm_keeps_every_other_beat() {
        let an = synth(120.0, 16);
        let out = bpm_halved(&an);
        assert_eq!(out.bpm, 60.0);
        assert_eq!(out.beat_grid.len(), 8);
        // The kept beats are at 0.5-s spacing × 2 = 1.0 s apart.
        for w in out.beat_grid.windows(2) {
            assert!((w[1] - w[0] - 1.0).abs() < 1e-9);
        }
        // Anchor stays — bar 1 is still beat 0.
        assert_eq!(out.downbeats.first().copied(), Some(0));
    }

    #[test]
    fn double_bpm_inserts_midpoints() {
        let an = synth(120.0, 4);
        let out = bpm_doubled(&an);
        assert_eq!(out.bpm, 240.0);
        // 4 original beats → 4 + 3 midpoints = 7 new beats.
        assert_eq!(out.beat_grid.len(), 7);
        // Verify spacing = 0.25 s.
        for w in out.beat_grid.windows(2) {
            assert!((w[1] - w[0] - 0.25).abs() < 1e-9);
        }
    }

    #[test]
    fn halve_then_double_round_trip_preserves_grid_size() {
        let an = synth(120.0, 16);
        let halved = bpm_halved(&an);
        let back = bpm_doubled(&halved);
        // Round-trip recovers same beat count (one off at the edge is
        // fine — halve drops to 8, double brings it to 15).
        assert!((back.bpm - an.bpm).abs() < 1e-3);
        assert!(back.beat_grid.len() >= an.beat_grid.len() - 2);
    }

    #[test]
    fn set_downbeat_at_picks_nearest_beat() {
        let an = synth(120.0, 16);
        // Beat times: 0, 0.5, 1.0, 1.5, 2.0, …  Ask for 1.45 → snaps
        // to beat 3 (t=1.5).
        let out = set_downbeat_at(&an, 1.45);
        assert_eq!(out.downbeats.first().copied(), Some(3));
        // Subsequent downbeats every 4 beats: 3, 7, 11, 15.
        assert_eq!(out.downbeats, vec![3, 7, 11, 15]);
        // Grid + BPM unchanged.
        assert_eq!(out.beat_grid, an.beat_grid);
        assert_eq!(out.bpm, an.bpm);
    }

    #[test]
    fn empty_grid_safe_for_all_ops() {
        let an = TrackAnalysis {
            analysis_version: 2,
            bpm: 0.0,
            beat_grid: Vec::new(),
            downbeats: Vec::new(),
            duration_secs: 0.0,
            sample_rate: 44_100,
            key: None,
        };
        let _ = shifted(&an, 1.0);
        let _ = skip_beats(&an, 4);
        let _ = bpm_halved(&an);
        let _ = bpm_doubled(&an);
        let _ = set_downbeat_at(&an, 1.0);
    }
}
