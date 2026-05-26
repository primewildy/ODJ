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
