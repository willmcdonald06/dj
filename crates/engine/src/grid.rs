//! Constant-tempo beatgrid. RT-safe (plain arithmetic).

/// Beats at `anchor + n * period` frames for every integer `n`, at the engine sample
/// rate. Small and `Copy`, so it can travel in a [`Command`](crate::Command).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeatGrid {
    /// Frame of one beat (any beat; not necessarily a downbeat).
    pub anchor: f64,
    /// Frames per beat.
    pub period: f64,
}

impl BeatGrid {
    pub fn from_bpm(bpm: f64, anchor: f64, sample_rate: u32) -> Self {
        Self {
            anchor,
            period: 60.0 * sample_rate as f64 / bpm,
        }
    }

    pub fn bpm(&self, sample_rate: u32) -> f64 {
        60.0 * sample_rate as f64 / self.period
    }

    /// Beat number at `frame`, fractional (e.g. 12.25 is a quarter of the way from
    /// beat 12 to beat 13).
    pub fn beat_at(&self, frame: f64) -> f64 {
        (frame - self.anchor) / self.period
    }

    pub fn frame_of(&self, beat: f64) -> f64 {
        self.anchor + beat * self.period
    }

    /// Where in its beat `frame` is, 0..1.
    pub fn phase(&self, frame: f64) -> f64 {
        self.beat_at(frame).rem_euclid(1.0)
    }

    /// The beat nearest to `frame`.
    pub fn snap(&self, frame: f64) -> f64 {
        self.frame_of(self.beat_at(frame).round())
    }
}

/// `x` wrapped into `[-0.5, 0.5)`: the shortest way round a beat.
pub fn wrap_half(x: f64) -> f64 {
    (x + 0.5).rem_euclid(1.0) - 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_phase_and_snap() {
        let grid = BeatGrid::from_bpm(120.0, 1000.0, 48_000);
        assert_eq!(grid.period, 24_000.0);
        assert!((grid.bpm(48_000) - 120.0).abs() < 1e-9);
        assert_eq!(grid.beat_at(25_000.0), 1.0);
        assert_eq!(grid.phase(13_000.0), 0.5);
        assert_eq!(grid.phase(0.0), 1.0 - 1000.0 / 24_000.0);
        assert_eq!(grid.snap(30_000.0), 25_000.0);
        assert_eq!(grid.snap(40_000.0), 49_000.0);
    }

    #[test]
    fn wrap_half_takes_the_short_way() {
        assert_eq!(wrap_half(0.25), 0.25);
        assert_eq!(wrap_half(0.75), -0.25);
        assert_eq!(wrap_half(-0.75), 0.25);
        assert_eq!(wrap_half(0.0), 0.0);
    }
}
