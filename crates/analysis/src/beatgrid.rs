//! Constant-tempo beatgrid from kick markers: the in-app fallback until offline beat
//! tracking (Milestone 6) provides better grids.
//!
//! 1. A rough beat period from the most common kick spacing.
//! 2. A fine search around it for the period at which the kicks line up best: each
//!    kick is a unit vector at angle `2π · frame / period`, and kicks on a steady grid
//!    point the same way, so the length of their sum measures how well a period fits.
//! 3. The grid's anchor (phase) is the direction of that sum.
//! 4. If a whole-number BPM fits almost as well, it is used: most electronic music is
//!    produced at one.

use dj_engine::BeatGrid;

/// Range searched around the rough tempo, as a fraction.
const SEARCH_RANGE: f64 = 0.03;
/// A whole-number BPM is used if its fit is at least this fraction of the best.
const ROUND_BPM_TOLERANCE: f64 = 0.97;
/// Fewer kicks than this give no grid.
const MIN_KICKS: usize = 16;
/// Kick spacings accepted for the rough tempo, in seconds.
const SPACING_RANGE: (f64, f64) = (0.2, 2.0);
/// Rough tempos are folded into this BPM range.
const BPM_RANGE: (f64, f64) = (70.0, 180.0);

/// A grid and how well the kicks fit it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridEstimate {
    pub grid: BeatGrid,
    /// 0..1: 1 means every kick sits exactly on a beat.
    pub confidence: f64,
}

/// Fit a constant-tempo grid to kick positions (frames, ascending).
pub fn estimate(kicks: &[u64], sample_rate: u32) -> Option<GridEstimate> {
    if kicks.len() < MIN_KICKS {
        return None;
    }
    let sr = sample_rate as f64;
    let rough_bpm = rough_bpm(kicks, sr)?;
    let rough_period = 60.0 * sr / rough_bpm;
    let times: Vec<f64> = kicks.iter().map(|&k| k as f64).collect();

    // Step size: over the length of the track, a period error of one step moves the
    // last beat by well under a tenth of a beat.
    let span = times[times.len() - 1] - times[0];
    let beats = (span / rough_period).max(1.0);
    let step = rough_period / beats / 20.0;
    let low = rough_period * (1.0 - SEARCH_RANGE);
    let high = rough_period * (1.0 + SEARCH_RANGE);
    let mut best = (rough_period, 0.0);
    let mut period = low;
    while period <= high {
        let fit = fit(&times, period).0;
        if fit > best.1 {
            best = (period, fit);
        }
        period += step;
    }
    // Polish within one step either side.
    let (mut lo, mut hi) = (best.0 - step, best.0 + step);
    for _ in 0..40 {
        let a = lo + (hi - lo) / 3.0;
        let b = hi - (hi - lo) / 3.0;
        match fit(&times, a).0 < fit(&times, b).0 {
            true => lo = a,
            false => hi = b,
        }
    }
    let mut period = (lo + hi) / 2.0;
    let mut score = fit(&times, period).0;

    let bpm = 60.0 * sr / period;
    let round_period = 60.0 * sr / bpm.round();
    let round_score = fit(&times, round_period).0;
    if round_score >= score * ROUND_BPM_TOLERANCE {
        period = round_period;
        score = round_score;
    }

    let (_, angle) = fit(&times, period);
    let anchor = (angle / std::f64::consts::TAU * period).rem_euclid(period);
    Some(GridEstimate {
        grid: BeatGrid { anchor, period },
        confidence: score,
    })
}

/// How well `times` fit a grid of `period` (0..1), and the grid's phase angle.
fn fit(times: &[f64], period: f64) -> (f64, f64) {
    let (mut x, mut y) = (0.0, 0.0);
    for &t in times {
        let angle = std::f64::consts::TAU * (t / period).fract();
        x += angle.cos();
        y += angle.sin();
    }
    ((x * x + y * y).sqrt() / times.len() as f64, y.atan2(x))
}

/// Median kick spacing as a BPM, folded into [`BPM_RANGE`].
fn rough_bpm(kicks: &[u64], sr: f64) -> Option<f64> {
    let mut spacings: Vec<f64> = kicks
        .windows(2)
        .map(|w| (w[1] - w[0]) as f64 / sr)
        .filter(|s| (SPACING_RANGE.0..SPACING_RANGE.1).contains(s))
        .collect();
    if spacings.len() < MIN_KICKS / 2 {
        return None;
    }
    spacings.sort_by(f64::total_cmp);
    let mut bpm = 60.0 / spacings[spacings.len() / 2];
    while bpm < BPM_RANGE.0 {
        bpm *= 2.0;
    }
    while bpm > BPM_RANGE.1 {
        bpm /= 2.0;
    }
    Some(bpm)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Kicks at `bpm` from `first` frames, skipping a breakdown and every 7th kick,
    /// with deterministic jitter of up to `jitter_ms`.
    fn kicks(bpm: f64, first: f64, beats: usize, jitter_ms: f64) -> Vec<u64> {
        let period = 60.0 * SR as f64 / bpm;
        (0..beats)
            .filter(|&b| b % 7 != 3 && !(100..164).contains(&b))
            .map(|b| {
                let wobble = ((b * 7919) % 13) as f64 / 12.0 * 2.0 - 1.0;
                (first + b as f64 * period + wobble * jitter_ms * SR as f64 / 1000.0) as u64
            })
            .collect()
    }

    fn check(bpm: f64, first: f64, jitter_ms: f64) -> GridEstimate {
        let estimate = estimate(&kicks(bpm, first, 600, jitter_ms), SR).unwrap();
        let found = estimate.grid.bpm(SR);
        assert!((found - bpm).abs() < 0.005, "bpm {found}, expected {bpm}");
        // The anchor is on a beat of the true grid.
        let period = 60.0 * SR as f64 / bpm;
        let offset = ((estimate.grid.anchor - first) / period).rem_euclid(1.0);
        let offset_ms = offset.min(1.0 - offset) * period / SR as f64 * 1000.0;
        assert!(offset_ms < 1.0, "anchor off by {offset_ms} ms");
        estimate
    }

    #[test]
    fn whole_number_tempo_is_exact() {
        let estimate = check(128.0, 12_345.0, 2.0);
        assert_eq!(estimate.grid.bpm(SR), 128.0);
        assert!(estimate.confidence > 0.9);
    }

    #[test]
    fn fractional_tempo_is_kept() {
        check(123.45, 999.0, 1.0);
        check(87.3, 40_000.0, 1.0);
    }

    #[test]
    fn slow_kicks_keep_their_tempo() {
        // Kicks at 80 BPM (e.g. every other beat of a 160 BPM track): within the
        // folding range, so they stay at 80. Halving or doubling is the user's call.
        let estimate = estimate(&kicks(80.0, 0.0, 400, 0.5), SR).unwrap();
        let bpm = estimate.grid.bpm(SR);
        assert!((bpm - 80.0).abs() < 0.01, "{bpm}");
    }

    #[test]
    fn too_few_or_random_kicks_give_nothing_or_low_confidence() {
        assert!(estimate(&[1, 2, 3], SR).is_none());
        let random: Vec<u64> = (0..200u64).map(|i| i * i * 7919 % 10_000_000).collect();
        let mut random = random;
        random.sort();
        if let Some(e) = estimate(&random, SR) {
            assert!(e.confidence < 0.5, "{}", e.confidence);
        }
    }
}
