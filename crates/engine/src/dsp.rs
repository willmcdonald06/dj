//! Small DSP building blocks: biquads, a state-variable filter, a parameter smoother and
//! a 3-band isolator EQ.
//!
//! # RT safety
//!
//! Everything here is **RT-safe**: plain arithmetic on fixed-size state.

use std::f32::consts::{FRAC_1_SQRT_2, PI};

/// Butterworth Q, used for Linkwitz-Riley crossovers.
const BUTTERWORTH_Q: f32 = FRAC_1_SQRT_2;

/// A second-order IIR filter (transposed direct form II), RBJ cookbook coefficients.
#[derive(Debug, Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    s1: f32,
    s2: f32,
}

impl Biquad {
    fn from_coefficients(b: [f32; 3], a: [f32; 3]) -> Self {
        Self {
            b0: b[0] / a[0],
            b1: b[1] / a[0],
            b2: b[2] / a[0],
            a1: a[1] / a[0],
            a2: a[2] / a[0],
            s1: 0.0,
            s2: 0.0,
        }
    }

    fn omega(sample_rate: f32, hz: f32, q: f32) -> (f32, f32) {
        let w0 = 2.0 * PI * hz / sample_rate;
        (w0.cos(), w0.sin() / (2.0 * q))
    }

    pub fn lowpass(sample_rate: f32, hz: f32, q: f32) -> Self {
        let (cos, alpha) = Self::omega(sample_rate, hz, q);
        Self::from_coefficients(
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    pub fn highpass(sample_rate: f32, hz: f32, q: f32) -> Self {
        let (cos, alpha) = Self::omega(sample_rate, hz, q);
        Self::from_coefficients(
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    pub fn allpass(sample_rate: f32, hz: f32, q: f32) -> Self {
        let (cos, alpha) = Self::omega(sample_rate, hz, q);
        Self::from_coefficients(
            [1.0 - alpha, -2.0 * cos, 1.0 + alpha],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Fourth-order Linkwitz-Riley section: two identical Butterworth biquads.
#[derive(Debug, Clone, Copy)]
struct Lr4([Biquad; 2]);

impl Lr4 {
    fn lowpass(sample_rate: f32, hz: f32) -> Self {
        let b = Biquad::lowpass(sample_rate, hz, BUTTERWORTH_Q);
        Self([b, b])
    }

    fn highpass(sample_rate: f32, hz: f32) -> Self {
        let b = Biquad::highpass(sample_rate, hz, BUTTERWORTH_Q);
        Self([b, b])
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.0[0].process(x);
        self.0[1].process(y)
    }
}

/// Crossover frequencies of the 3-band EQ.
pub const EQ_LOW_MID_HZ: f32 = 250.0;
pub const EQ_MID_HIGH_HZ: f32 = 2500.0;

/// A 3-band isolator EQ for one channel of audio, built from Linkwitz-Riley crossovers.
///
/// With all bands at unity the bands sum to an all-pass: the magnitude response is flat
/// and only the phase changes, so the EQ is transparent at its centre position. Any band
/// can be cut to silence (a "kill").
#[derive(Debug, Clone, Copy)]
pub struct IsolatorEq {
    low_lp: Lr4,
    /// Matches the low band's phase to the mid/high split.
    low_ap: Biquad,
    rest_hp: Lr4,
    mid_lp: Lr4,
    high_hp: Lr4,
}

impl IsolatorEq {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            low_lp: Lr4::lowpass(sample_rate, EQ_LOW_MID_HZ),
            low_ap: Biquad::allpass(sample_rate, EQ_MID_HIGH_HZ, BUTTERWORTH_Q),
            rest_hp: Lr4::highpass(sample_rate, EQ_LOW_MID_HZ),
            mid_lp: Lr4::lowpass(sample_rate, EQ_MID_HIGH_HZ),
            high_hp: Lr4::highpass(sample_rate, EQ_MID_HIGH_HZ),
        }
    }

    /// Process one sample with linear band gains `[low, mid, high]`.
    #[inline]
    pub fn process(&mut self, x: f32, gains: [f32; 3]) -> f32 {
        let [low, mid, high] = self.split(x);
        low * gains[0] + mid * gains[1] + high * gains[2]
    }

    /// Split one sample into its `[low, mid, high]` bands, which sum to an all-pass
    /// of the input.
    #[inline]
    pub fn split(&mut self, x: f32) -> [f32; 3] {
        let low = self.low_ap.process(self.low_lp.process(x));
        let rest = self.rest_hp.process(x);
        [low, self.mid_lp.process(rest), self.high_hp.process(rest)]
    }
}

/// Zero-delay-feedback state-variable filter (Simper's TPT form). Stable while its
/// cutoff is being swept, which a DJ filter knob does constantly.
#[derive(Debug, Clone, Copy, Default)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
}

/// Coefficients for [`Svf`], shared by the left and right channels.
#[derive(Debug, Clone, Copy)]
pub struct SvfCoefficients {
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
}

impl SvfCoefficients {
    pub fn new(sample_rate: f32, hz: f32, q: f32) -> Self {
        let hz = hz.clamp(10.0, sample_rate * 0.49);
        let g = (PI * hz / sample_rate).tan();
        let k = 1.0 / q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        Self { k, a1, a2, a3 }
    }
}

impl Svf {
    /// Process one sample; returns `(lowpass, highpass)`.
    #[inline]
    pub fn process(&mut self, x: f32, c: &SvfCoefficients) -> (f32, f32) {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        (v2, x - c.k * v1 - v2)
    }
}

/// One-pole smoother that moves a parameter towards its target, so steps from a MIDI
/// knob don't cause zipper noise.
#[derive(Debug, Clone, Copy)]
pub struct Smoothed {
    current: f32,
    target: f32,
    coefficient: f32,
}

/// Time for a smoothed parameter to cover ~63% of a step.
pub const SMOOTHING_SECONDS: f32 = 0.005;

impl Smoothed {
    pub fn new(sample_rate: f32, value: f32) -> Self {
        Self {
            current: value,
            target: value,
            coefficient: 1.0 - (-1.0 / (SMOOTHING_SECONDS * sample_rate)).exp(),
        }
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    #[inline]
    pub fn tick(&mut self) -> f32 {
        let next = self.current + (self.target - self.current) * self.coefficient;
        // Snap once the step is too small to change an f32, or it would stall just
        // short of the target forever.
        if next == self.current {
            self.current = self.target;
        } else {
            self.current = next;
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Steady-state gain of a filter for a sine at `hz` (output RMS over input RMS,
    /// which unlike the sampled peak doesn't depend on where samples land).
    fn response(hz: f32, mut f: impl FnMut(f32) -> f32) -> f32 {
        let total = SR as usize;
        let (mut input, mut output) = (0.0f64, 0.0f64);
        for i in 0..total {
            let x = (2.0 * PI * hz * i as f32 / SR).sin();
            let y = f(x);
            if i >= total / 2 {
                input += (x * x) as f64;
                output += (y * y) as f64;
            }
        }
        (output / input).sqrt() as f32
    }

    fn db(gain: f32) -> f32 {
        20.0 * gain.log10()
    }

    #[test]
    fn isolator_is_flat_at_unity() {
        for hz in [40.0, 120.0, 250.0, 700.0, 2500.0, 6000.0, 15000.0] {
            let mut eq = IsolatorEq::new(SR);
            let gain = response(hz, |x| eq.process(x, [1.0, 1.0, 1.0]));
            assert!(db(gain).abs() < 0.1, "{hz} Hz: {} dB", db(gain));
        }
    }

    #[test]
    fn isolator_kills_each_band() {
        let cases = [
            (60.0, [0.0, 1.0, 1.0]),
            (800.0, [1.0, 0.0, 1.0]),
            (10_000.0, [1.0, 1.0, 0.0]),
        ];
        for (hz, gains) in cases {
            let mut eq = IsolatorEq::new(SR);
            let gain = response(hz, |x| eq.process(x, gains));
            assert!(db(gain) < -24.0, "{hz} Hz with {gains:?}: {} dB", db(gain));
        }
    }

    #[test]
    fn svf_lowpass_and_highpass() {
        let c = SvfCoefficients::new(SR, 1000.0, FRAC_1_SQRT_2);
        let mut svf = Svf::default();
        assert!(db(response(100.0, |x| svf.process(x, &c).0)).abs() < 0.5);
        let mut svf = Svf::default();
        assert!(db(response(10_000.0, |x| svf.process(x, &c).0)) < -30.0);
        let mut svf = Svf::default();
        assert!(db(response(10_000.0, |x| svf.process(x, &c).1)).abs() < 0.5);
        let mut svf = Svf::default();
        assert!(db(response(100.0, |x| svf.process(x, &c).1)) < -30.0);
    }

    #[test]
    fn smoother_converges() {
        let mut s = Smoothed::new(SR, 0.0);
        s.set(1.0);
        let first = s.tick();
        assert!(first > 0.0 && first < 0.01);
        for _ in 0..(SR as usize / 10) {
            s.tick();
        }
        assert_eq!(s.tick(), 1.0);
    }
}
