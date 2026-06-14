//! Per-deck post-EQ pre-fader FX chain.
//!
//! For now this is just a beat-synced Echo. The chain is shaped so
//! adding Reverb / Filter later is mechanical: each new effect
//! interprets the existing `colour` / `time` / `mix` parameters
//! its own way, and `apply()` dispatches on `kind`.
//!
//! Hot-path discipline (CLAUDE.md): everything is pre-allocated at
//! construction. `apply` does no allocs, no I/O, no panics, no
//! locks. The delay-line buffer is sized for the worst case at deck
//! construction and never grows.
//!
//! "Off" still runs the chain — Echo's feedback tail decays
//! gracefully through silence rather than getting hard-zeroed on
//! toggle, which would click. The dry signal is always preserved.
//!
//! Beat sync: caller passes the deck's effective BPM (analysis BPM
//! × speed_ratio × nudge), which we convert to a sample-accurate
//! delay length. Changing `beats` smoothly crossfades the read tap
//! across ~10 ms so beat-picker clicks don't pop.

const TAIL_CROSSFADE_FRAMES: usize = 480; // ~10 ms at 48 kHz; cheap.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxKind {
    /// Tempo-synced delay (Type B in the design — beat picker).
    Echo,
    /// Schroeder reverb (Type A — continuous Time knob).
    Reverb,
}

impl Default for FxKind {
    fn default() -> Self {
        FxKind::Echo
    }
}

/// Schroeder reverb tunings — comb delays in milliseconds and
/// allpass delays. These are the Freeverb defaults; small,
/// well-spread prime-ish values that read as "natural room".
/// Multiplied by sample_rate at construction to get frame counts.
const COMB_DELAYS_MS: [f32; 4] = [29.7, 37.1, 41.1, 43.7];
const ALLPASS_DELAYS_MS: [f32; 2] = [5.0, 1.7];
const ALLPASS_FEEDBACK: f32 = 0.5;

/// One per deck. Lives inside `DeckState` and gets fed the
/// post-EQ buffer for in-place processing.
pub struct FxChain {
    pub kind: FxKind,
    pub on: bool,
    /// Effect "Colour" — Echo treats this as feedback (0..0.9).
    pub colour: f32,
    /// Effect "Time" — Echo ignores (uses `beats`+BPM); Reverb will
    /// use it later.
    pub time: f32,
    /// Wet/dry mix. 0 = dry, 1 = fully wet.
    pub mix: f32,
    /// Tempo-synced delay length in beats (Echo).
    pub beats: f32,

    /// Current effective BPM (analysis × speed_ratio × nudge);
    /// updated each callback by the deck render before `apply`.
    bpm: f32,

    // ----- Echo state ---------------------------------------------
    /// Stereo delay line, interleaved. Capacity = 2 s × sample_rate.
    delay: Vec<f32>,
    /// Sample rate the delay buffer was sized for.
    sample_rate: u32,
    /// Maximum addressable delay in *frames* (delay.len() / channels).
    max_delay_frames: usize,
    /// Write head, in frames. Wraps modulo max_delay_frames.
    write_idx: usize,
    /// Read offset, in *frames*. Computed each callback from `beats`
    /// and `bpm`. Stored so a beat-change crossfade can interpolate.
    read_offset: usize,
    /// When `beats` changes mid-callback, we crossfade the read tap
    /// from the previous offset to the new one over the next
    /// TAIL_CROSSFADE_FRAMES frames. None = not crossfading.
    crossfade_from: Option<usize>,
    crossfade_remaining: usize,

    // ----- Reverb state -------------------------------------------
    /// Four parallel comb filters per channel. Sized at construction
    /// from COMB_DELAYS_MS × sample_rate. Each comb owns its own
    /// state — feedback samples + a one-pole LP filter memory for
    /// damping.
    combs_l: [Comb; 4],
    combs_r: [Comb; 4],
    /// Two series allpass filters per channel — diffusion stage
    /// that prevents the parallel-comb output from sounding like a
    /// metallic flutter.
    apl_l: [AllPass; 2],
    apl_r: [AllPass; 2],
}

/// Single comb filter for the Schroeder reverb. Pre-allocated delay
/// line + a one-pole LP in the feedback path for damping.
struct Comb {
    buf: Vec<f32>,
    idx: usize,
    /// Last-sample memory for the one-pole damping LP.
    lp_last: f32,
}

impl Comb {
    fn new(delay_frames: usize) -> Self {
        Self {
            buf: vec![0.0; delay_frames.max(1)],
            idx: 0,
            lp_last: 0.0,
        }
    }

    /// Process one sample. `feedback` 0..1, `damp` 0..1 (higher =
    /// more high-frequency loss in the tail).
    fn tick(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let out = self.buf[self.idx];
        // One-pole LP on the feedback signal — gives the "warm"
        // decay; without it the reverb sounds metallic.
        self.lp_last = out * (1.0 - damp) + self.lp_last * damp;
        // Write input + LP-filtered feedback back into the buffer.
        // Denormal-safety constant prevents the feedback ring from
        // landing in slow-path f32 once it gets quiet.
        self.buf[self.idx] = x + self.lp_last * feedback + 1e-25;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

/// Single allpass — preserves spectrum but smears phase, used as
/// the diffusion stage at the output of the comb bank.
struct AllPass {
    buf: Vec<f32>,
    idx: usize,
}

impl AllPass {
    fn new(delay_frames: usize) -> Self {
        Self {
            buf: vec![0.0; delay_frames.max(1)],
            idx: 0,
        }
    }

    fn tick(&mut self, x: f32, feedback: f32) -> f32 {
        let buf_out = self.buf[self.idx];
        let out = -x + buf_out;
        self.buf[self.idx] = x + buf_out * feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

impl FxChain {
    pub fn new(sample_rate: u32) -> Self {
        // 2 seconds of stereo headroom — covers any sane tempo-synced
        // delay (4 beats @ 30 BPM = 8 s would NOT fit, but BPMs below
        // 60 are not realistic for DJ material; the longest beat
        // value we expose is 2 beats, which at 60 BPM is 2 s exactly).
        let max_delay_frames = (sample_rate as usize) * 2;
        let channels = 2;
        let comb_frames = |ms: f32| -> usize {
            (ms * 0.001 * sample_rate as f32) as usize
        };
        // Right-channel deltas: tiny prime offsets per Freeverb so
        // left and right reverb tails decorrelate slightly, giving
        // the stereo image some width.
        let r_stereo_spread: f32 = 23.0 / sample_rate as f32 * 1000.0;
        let mk_comb_pair = |ms: f32| (
            Comb::new(comb_frames(ms)),
            Comb::new(comb_frames(ms + r_stereo_spread)),
        );
        let (cl0, cr0) = mk_comb_pair(COMB_DELAYS_MS[0]);
        let (cl1, cr1) = mk_comb_pair(COMB_DELAYS_MS[1]);
        let (cl2, cr2) = mk_comb_pair(COMB_DELAYS_MS[2]);
        let (cl3, cr3) = mk_comb_pair(COMB_DELAYS_MS[3]);
        let mk_ap_pair = |ms: f32| (
            AllPass::new(comb_frames(ms)),
            AllPass::new(comb_frames(ms + r_stereo_spread)),
        );
        let (al0, ar0) = mk_ap_pair(ALLPASS_DELAYS_MS[0]);
        let (al1, ar1) = mk_ap_pair(ALLPASS_DELAYS_MS[1]);
        Self {
            kind: FxKind::Echo,
            on: false,
            colour: 0.45, // moderate feedback / damping
            time: 0.5,
            mix: 0.35,
            beats: 0.5, // 1/2 beat — classic dub-echo division
            bpm: 120.0,
            delay: vec![0.0; max_delay_frames * channels],
            sample_rate,
            max_delay_frames,
            write_idx: 0,
            read_offset: 0,
            crossfade_from: None,
            crossfade_remaining: 0,
            combs_l: [cl0, cl1, cl2, cl3],
            combs_r: [cr0, cr1, cr2, cr3],
            apl_l: [al0, al1],
            apl_r: [ar0, ar1],
        }
    }

    /// Caller provides the deck's effective BPM (analysis × speed
    /// + nudge). We use it to compute the current echo delay.
    pub fn set_bpm(&mut self, bpm: f32) {
        self.bpm = bpm.clamp(40.0, 240.0);
    }

    /// Process `buf` (interleaved L,R,L,R,...) in place. `channels`
    /// is the OUTPUT channel count (1 or 2 — we treat mono by
    /// fanning out and folding back). Caller has already applied EQ
    /// and the play envelope; FX sits between EQ and the cue tap.
    pub fn apply(&mut self, buf: &mut [f32], channels: usize) {
        if channels == 0 || buf.is_empty() {
            return;
        }
        match self.kind {
            FxKind::Echo => self.apply_echo(buf, channels),
            FxKind::Reverb => self.apply_reverb(buf, channels),
        }
    }

    fn target_delay_frames(&self) -> usize {
        if self.bpm <= 0.0 || self.beats <= 0.0 {
            return 0;
        }
        let secs = 60.0 / self.bpm as f64 * self.beats as f64;
        let frames = (secs * self.sample_rate as f64).round() as i64;
        frames.clamp(1, self.max_delay_frames as i64 - 1) as usize
    }

    /// Schroeder reverb: 4 parallel damped combs → 2 series
    /// allpasses → wet mix into the dry buffer.
    /// - `colour` → damping (0 = bright tail, 1 = very dark)
    /// - `time`   → comb feedback (longer time = higher feedback)
    /// - `mix`    → wet/dry blend (0 = dry, 1 = fully wet)
    fn apply_reverb(&mut self, buf: &mut [f32], channels: usize) {
        let mix = if self.on { self.mix.clamp(0.0, 1.0) } else { 0.0 };
        let dry = 1.0 - mix;
        // Map time 0..1 → feedback 0.4..0.95. Below 0.4 the tail
        // dies inside one comb cycle (no audible reverb); above
        // 0.95 the loop runs away.
        let feedback = 0.4 + self.time.clamp(0.0, 1.0) * 0.55;
        let damp = self.colour.clamp(0.0, 0.95);
        // Each comb runs at roughly 1/4 of the total wet contribution
        // (4 parallel combs summed); normalise so peak wet doesn't
        // blow past unity.
        let comb_norm = 0.25;
        let frames = buf.len() / channels;
        for f in 0..frames {
            let i_l = f * channels;
            let i_r = if channels >= 2 { i_l + 1 } else { i_l };
            let dry_l = buf[i_l];
            let dry_r = buf[i_r];

            // Sum 4 parallel combs per side.
            let mut wet_l = 0.0_f32;
            let mut wet_r = 0.0_f32;
            for i in 0..4 {
                wet_l += self.combs_l[i].tick(dry_l, feedback, damp);
                wet_r += self.combs_r[i].tick(dry_r, feedback, damp);
            }
            wet_l *= comb_norm;
            wet_r *= comb_norm;
            // Diffuse through 2 series allpasses per side.
            wet_l = self.apl_l[0].tick(wet_l, ALLPASS_FEEDBACK);
            wet_l = self.apl_l[1].tick(wet_l, ALLPASS_FEEDBACK);
            wet_r = self.apl_r[0].tick(wet_r, ALLPASS_FEEDBACK);
            wet_r = self.apl_r[1].tick(wet_r, ALLPASS_FEEDBACK);

            buf[i_l] = dry * dry_l + mix * wet_l;
            if channels >= 2 {
                buf[i_r] = dry * dry_r + mix * wet_r;
            }
        }
    }

    fn apply_echo(&mut self, buf: &mut [f32], channels: usize) {
        let want = self.target_delay_frames();
        // Beat-change crossfade: if the read offset moved noticeably,
        // arm a short fade from the old offset to the new one so the
        // tap-jump doesn't click.
        if want != self.read_offset {
            if want.abs_diff(self.read_offset) > 32 {
                self.crossfade_from = Some(self.read_offset);
                self.crossfade_remaining = TAIL_CROSSFADE_FRAMES;
            }
            self.read_offset = want;
        }

        // Feedback / mix are read once per frame. Clamping here
        // means the user can drag knobs without us re-validating.
        let feedback = self.colour.clamp(0.0, 0.9);
        let mix = if self.on { self.mix.clamp(0.0, 1.0) } else { 0.0 };
        let dry = 1.0 - mix;

        let frames = buf.len() / channels;
        let n_ch_stored = 2; // delay line is stereo-interleaved
        for f in 0..frames {
            let i_l = f * channels;
            let i_r = if channels >= 2 { i_l + 1 } else { i_l };

            // Wet sample from the delay line at the current tap.
            let read_idx = (self.write_idx + self.max_delay_frames - self.read_offset)
                % self.max_delay_frames;
            let mut wet_l = self.delay[read_idx * n_ch_stored];
            let mut wet_r = self.delay[read_idx * n_ch_stored + 1];

            // If we're crossfading from a previous tap, blend.
            if self.crossfade_remaining > 0 {
                if let Some(prev) = self.crossfade_from {
                    let prev_idx = (self.write_idx + self.max_delay_frames - prev)
                        % self.max_delay_frames;
                    let pl = self.delay[prev_idx * n_ch_stored];
                    let pr = self.delay[prev_idx * n_ch_stored + 1];
                    let t = self.crossfade_remaining as f32 / TAIL_CROSSFADE_FRAMES as f32;
                    wet_l = wet_l * (1.0 - t) + pl * t;
                    wet_r = wet_r * (1.0 - t) + pr * t;
                }
                self.crossfade_remaining -= 1;
                if self.crossfade_remaining == 0 {
                    self.crossfade_from = None;
                }
            }

            // Source-frame snapshot (the post-EQ dry signal).
            let dry_l = buf[i_l];
            let dry_r = buf[i_r];

            // Output = dry + mix * wet.
            buf[i_l] = dry * dry_l + mix * wet_l;
            if channels >= 2 {
                buf[i_r] = dry * dry_r + mix * wet_r;
            }

            // Write back to delay line: input + feedback × wet.
            // Subtle denormal-safety: small constant offset prevents
            // f32 denormals when the feedback ring is "silent" with
            // a tiny residue. Cheaper than an FTZ flag toggle and
            // perceptually inaudible.
            let in_l = dry_l + feedback * wet_l + 1e-25;
            let in_r = dry_r + feedback * wet_r + 1e-25;
            self.delay[self.write_idx * n_ch_stored] = in_l;
            self.delay[self.write_idx * n_ch_stored + 1] = in_r;
            self.write_idx = (self.write_idx + 1) % self.max_delay_frames;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_passes_signal_through_unchanged() {
        // Off means mix=0, so output should equal input bit-for-bit
        // (modulo the denormal-safety add, but it lands in the
        // delay line, not the dry path).
        let mut fx = FxChain::new(48_000);
        fx.on = false;
        fx.beats = 0.5;
        fx.set_bpm(120.0);
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut buf = input.clone();
        fx.apply(&mut buf, 2);
        for (i, (a, b)) in input.iter().zip(buf.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "off shouldn't alter sample {i}: {a} vs {b}");
        }
    }

    #[test]
    fn echo_delays_signal_by_target() {
        // Impulse in → impulse out 1 beat later, attenuated by mix.
        // 120 BPM, 1 beat = 0.5 s = 24000 frames at 48k.
        let mut fx = FxChain::new(48_000);
        fx.on = true;
        fx.beats = 1.0;
        fx.set_bpm(120.0);
        fx.colour = 0.0; // no feedback — single echo
        fx.mix = 1.0;     // fully wet
        let mut buf = vec![0.0_f32; 48_000 * 2]; // 24000 stereo frames
        // Stereo impulse at frame 0.
        buf[0] = 1.0;
        buf[1] = 1.0;
        fx.apply(&mut buf, 2);
        // The dry frame 0 should be wiped (mix=1 → dry=0).
        assert!(buf[0].abs() < 1e-6);
        // 24000 frames later we expect the wet impulse.
        let echo_l = buf[24_000 * 2];
        let echo_r = buf[24_000 * 2 + 1];
        assert!(echo_l > 0.99, "L echo expected near 1.0, got {echo_l}");
        assert!(echo_r > 0.99, "R echo expected near 1.0, got {echo_r}");
    }

    #[test]
    fn feedback_creates_repeating_echoes() {
        let mut fx = FxChain::new(48_000);
        fx.on = true;
        fx.beats = 0.5;
        fx.set_bpm(120.0); // 0.5 beat = 0.25 s = 12000 frames
        fx.colour = 0.5; // 50% feedback
        fx.mix = 0.5;
        // Run a 2-second buffer with a single click at frame 0.
        let mut buf = vec![0.0_f32; 48_000 * 2 * 2];
        buf[0] = 1.0;
        buf[1] = 1.0;
        fx.apply(&mut buf, 2);
        // We should see successively quieter echoes at 12k, 24k,
        // 36k, 48k... Each one's amplitude is feedback × previous.
        let peaks: Vec<f32> = (1..6)
            .map(|n| buf[(12_000 * n) * 2].abs())
            .collect();
        for w in peaks.windows(2) {
            assert!(w[0] > w[1], "echoes should decay: {w:?}");
            assert!(w[1] > 0.0, "all echoes should be audible: {w:?}");
        }
    }

    #[test]
    fn beats_change_does_not_panic() {
        // Sweeping the beat divisions while audio is flowing should
        // not produce NaN / out-of-bounds reads.
        let mut fx = FxChain::new(48_000);
        fx.on = true;
        fx.set_bpm(126.0);
        fx.mix = 0.4;
        fx.colour = 0.4;
        let mut buf = vec![0.5_f32; 4096];
        for &b in &[0.25_f32, 0.5, 1.0, 2.0, 0.5, 0.25] {
            fx.beats = b;
            fx.apply(&mut buf, 2);
            for s in &buf {
                assert!(s.is_finite(), "FX produced non-finite sample");
            }
        }
    }
}
