//! Per-deck shelving EQ: low shelf + high shelf, RBJ coefficients, Direct
//! Form II Transposed. Stereo (2 channels of state).
//!
//! Coefficient updates preserve state, so smoothly twisting an EQ knob from
//! -25 dB to +6 dB doesn't click.

use std::f32::consts::PI;

const MAX_CHANNELS: usize = 2;

pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32, // a0 normalised to 1
    s1: [f32; MAX_CHANNELS],
    s2: [f32; MAX_CHANNELS],
}

#[derive(Clone, Copy)]
struct Coeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    /// Identity filter; safe initial state.
    pub fn passthrough() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            s1: [0.0; MAX_CHANNELS],
            s2: [0.0; MAX_CHANNELS],
        }
    }

    /// Replace coefficients with low-shelf at fc. Q = 0.7071. State preserved.
    pub fn set_low_shelf(&mut self, fs: f32, fc: f32, gain_db: f32) {
        let c = low_shelf_coeffs(fs, fc, gain_db);
        self.b0 = c.b0;
        self.b1 = c.b1;
        self.b2 = c.b2;
        self.a1 = c.a1;
        self.a2 = c.a2;
    }

    /// Replace coefficients with high-shelf at fc. Q = 0.7071. State preserved.
    pub fn set_high_shelf(&mut self, fs: f32, fc: f32, gain_db: f32) {
        let c = high_shelf_coeffs(fs, fc, gain_db);
        self.b0 = c.b0;
        self.b1 = c.b1;
        self.b2 = c.b2;
        self.a1 = c.a1;
        self.a2 = c.a2;
    }

    /// Replace coefficients with a peaking (bell) filter at fc. State
    /// preserved. Used for the mid EQ band.
    pub fn set_peaking(&mut self, fs: f32, fc: f32, q: f32, gain_db: f32) {
        let c = peaking_coeffs(fs, fc, q, gain_db);
        self.b0 = c.b0;
        self.b1 = c.b1;
        self.b2 = c.b2;
        self.a1 = c.a1;
        self.a2 = c.a2;
    }

    /// Replace coefficients with an RBJ low-pass at fc with Q. State
    /// preserved. Scaffolding for the upcoming Filter FX (FEATURES.md §4).
    #[allow(dead_code)]
    pub fn set_lowpass(&mut self, fs: f32, fc: f32, q: f32) {
        let c = lowpass_coeffs(fs, fc, q);
        self.b0 = c.b0;
        self.b1 = c.b1;
        self.b2 = c.b2;
        self.a1 = c.a1;
        self.a2 = c.a2;
    }

    /// Replace coefficients with an RBJ high-pass at fc with Q. State
    /// preserved. Scaffolding for the upcoming Filter FX.
    #[allow(dead_code)]
    pub fn set_highpass(&mut self, fs: f32, fc: f32, q: f32) {
        let c = highpass_coeffs(fs, fc, q);
        self.b0 = c.b0;
        self.b1 = c.b1;
        self.b2 = c.b2;
        self.a1 = c.a1;
        self.a2 = c.a2;
    }

    /// Pass-through reset: drop the filter to identity coefficients.
    /// State (`s1`/`s2`) preserved so a sweep through "off" doesn't
    /// audibly snap, but new input passes unchanged. Scaffolding for
    /// the upcoming Filter FX.
    #[allow(dead_code)]
    pub fn set_passthrough(&mut self) {
        let c = identity();
        self.b0 = c.b0;
        self.b1 = c.b1;
        self.b2 = c.b2;
        self.a1 = c.a1;
        self.a2 = c.a2;
    }

    /// DF2T per-sample. `ch` indexes stereo state (0 or 1).
    #[inline]
    pub fn process(&mut self, ch: usize, x: f32) -> f32 {
        let y = self.b0 * x + self.s1[ch];
        self.s1[ch] = self.b1 * x - self.a1 * y + self.s2[ch];
        self.s2[ch] = self.b2 * x - self.a2 * y;
        y
    }
}

fn low_shelf_coeffs(fs: f32, fc: f32, gain_db: f32) -> Coeffs {
    if gain_db.abs() < 0.01 {
        return identity();
    }
    let q = 0.7071_f32;
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * fc / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q);
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha);
    let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cw);
    let b2 = a * ((a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha);
    let a0 = (a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha;
    let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cw);
    let a2 = (a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha;
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn high_shelf_coeffs(fs: f32, fc: f32, gain_db: f32) -> Coeffs {
    if gain_db.abs() < 0.01 {
        return identity();
    }
    let q = 0.7071_f32;
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * fc / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q);
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
    let b0 = a * ((a + 1.0) + (a - 1.0) * cw + two_sqrt_a_alpha);
    let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cw);
    let b2 = a * ((a + 1.0) + (a - 1.0) * cw - two_sqrt_a_alpha);
    let a0 = (a + 1.0) - (a - 1.0) * cw + two_sqrt_a_alpha;
    let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cw);
    let a2 = (a + 1.0) - (a - 1.0) * cw - two_sqrt_a_alpha;
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn peaking_coeffs(fs: f32, fc: f32, q: f32, gain_db: f32) -> Coeffs {
    if gain_db.abs() < 0.01 {
        return identity();
    }
    let a = 10f32.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * fc / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q);
    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cw;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha / a;
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

fn identity() -> Coeffs {
    Coeffs {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    }
}

/// RBJ Audio EQ Cookbook low-pass coefficients.
#[allow(dead_code)]
fn lowpass_coeffs(fs: f32, fc: f32, q: f32) -> Coeffs {
    let w0 = 2.0 * PI * fc / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q.max(0.1));
    let b0 = (1.0 - cw) * 0.5;
    let b1 = 1.0 - cw;
    let b2 = (1.0 - cw) * 0.5;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

/// RBJ high-pass coefficients.
#[allow(dead_code)]
fn highpass_coeffs(fs: f32, fc: f32, q: f32) -> Coeffs {
    let w0 = 2.0 * PI * fc / fs;
    let cw = w0.cos();
    let sw = w0.sin();
    let alpha = sw / (2.0 * q.max(0.1));
    let b0 = (1.0 + cw) * 0.5;
    let b1 = -(1.0 + cw);
    let b2 = (1.0 + cw) * 0.5;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cw;
    let a2 = 1.0 - alpha;
    Coeffs {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: a1 / a0,
        a2: a2 / a0,
    }
}

#[cfg(test)]
mod tests {
    //! Biquad invariants. The big one — state is preserved on
    //! coefficient changes — is what makes dragging an EQ knob
    //! click-free. If a future refactor accidentally resets s1/s2
    //! on every `set_*` call, the existing audio path would start
    //! pop'ing on every knob touch; this test catches that.
    use super::*;

    /// Compare two f32 values "audibly close" — anything under a
    /// fraction of a quantisation step at f32 precision is fine.
    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() < tol
    }

    #[test]
    fn passthrough_returns_input_unchanged() {
        let mut bq = Biquad::passthrough();
        for x in [-1.0, -0.5, 0.0, 0.25, 1.0] {
            let y = bq.process(0, x);
            assert!(close(y, x, 1e-6), "passthrough altered {x} → {y}");
        }
    }

    #[test]
    fn set_low_shelf_preserves_state() {
        // The no-click invariant: re-setting coefficients must not
        // reset s1/s2. We push samples through one filter, snapshot
        // state, re-set coefficients, and verify state hasn't moved.
        let mut bq = Biquad::passthrough();
        bq.set_low_shelf(44_100.0, 120.0, -6.0);
        for i in 0..256 {
            let _ = bq.process(0, (i as f32 * 0.01).sin());
        }
        let s1_before = bq.s1[0];
        let s2_before = bq.s2[0];
        bq.set_low_shelf(44_100.0, 120.0, 3.0); // different gain
        assert_eq!(bq.s1[0], s1_before, "set_low_shelf zeroed s1");
        assert_eq!(bq.s2[0], s2_before, "set_low_shelf zeroed s2");
    }

    #[test]
    fn set_high_shelf_preserves_state() {
        let mut bq = Biquad::passthrough();
        bq.set_high_shelf(44_100.0, 8000.0, -6.0);
        for i in 0..256 {
            let _ = bq.process(0, (i as f32 * 0.01).sin());
        }
        let s1 = bq.s1[0];
        let s2 = bq.s2[0];
        bq.set_high_shelf(44_100.0, 8000.0, 6.0);
        assert_eq!(bq.s1[0], s1);
        assert_eq!(bq.s2[0], s2);
    }

    #[test]
    fn set_peaking_preserves_state() {
        let mut bq = Biquad::passthrough();
        bq.set_peaking(44_100.0, 1000.0, 1.0, -6.0);
        for i in 0..256 {
            let _ = bq.process(0, (i as f32 * 0.01).sin());
        }
        let s1 = bq.s1[0];
        let s2 = bq.s2[0];
        bq.set_peaking(44_100.0, 1000.0, 1.0, 6.0);
        assert_eq!(bq.s1[0], s1);
        assert_eq!(bq.s2[0], s2);
    }

    #[test]
    fn zero_gain_shelves_are_passthrough() {
        // The identity() shortcut at gain ≈ 0 dB. Avoids tiny
        // floating-point coefficient noise riding through.
        let mut bq = Biquad::passthrough();
        bq.set_low_shelf(44_100.0, 120.0, 0.0);
        for x in [0.1, -0.4, 0.7] {
            let y = bq.process(0, x);
            assert!(close(y, x, 1e-6), "0 dB shelf coloured {x} → {y}");
        }
    }

    #[test]
    fn channels_are_independent() {
        // s1[0] and s1[1] must not be aliased — left and right
        // channels filter independently with their own state.
        let mut bq = Biquad::passthrough();
        bq.set_low_shelf(44_100.0, 200.0, -6.0);
        let _ = bq.process(0, 1.0); // left only
        assert_ne!(bq.s1[0], 0.0, "left state should have moved");
        assert_eq!(bq.s1[1], 0.0, "right state should be untouched");
    }
}
