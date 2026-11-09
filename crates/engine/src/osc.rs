//! Sine oscillator with a de-click gain ramp. RT-safe.

use std::f32::consts::TAU;

/// Time for the gain ramp to travel from 0 to full scale, in seconds.
const RAMP_SECONDS: f32 = 0.005;

pub struct SineOsc {
    sample_rate: f32,
    phase: f32,
    phase_inc: f32,
    gain: f32,
    enabled: bool,
    current_gain: f32,
    ramp_step: f32,
}

impl SineOsc {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate,
            phase: 0.0,
            phase_inc: 0.0,
            gain: 0.0,
            enabled: false,
            current_gain: 0.0,
            ramp_step: 1.0 / (RAMP_SECONDS * sample_rate),
        }
    }

    pub fn set_frequency(&mut self, hz: f32) {
        self.phase_inc = hz / self.sample_rate;
    }

    pub fn set_gain(&mut self, gain: f32) {
        self.gain = gain;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn target_gain(&self) -> f32 {
        if self.enabled {
            self.gain
        } else {
            0.0
        }
    }

    #[inline]
    pub fn next_sample(&mut self) -> f32 {
        let target = self.target_gain();
        let delta = target - self.current_gain;
        if delta.abs() <= self.ramp_step {
            self.current_gain = target;
        } else {
            self.current_gain += self.ramp_step * delta.signum();
        }

        if self.current_gain == 0.0 {
            return 0.0;
        }

        let sample = (self.phase * TAU).sin() * self.current_gain;
        self.phase += self.phase_inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        sample
    }
}
