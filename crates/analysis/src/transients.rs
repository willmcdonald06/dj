//! Transient markers: every sharp attack (kicks, snares, hats, plucks), found in a
//! waveform summary, for the spike markers between the scrolling waveforms. Unlike the
//! kick detector this keeps everything percussive, and it is cheap enough to run in
//! the UI when a track's analysis arrives.

use crate::WaveformSummary;
use crate::waveform::{HIGH, LOW, MID};

/// One attack: where it starts, and the loudest summary bin in the next
/// [`BODY_SECONDS`] (for its colour and size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transient {
    pub frame: u64,
    pub bins: [u8; 4],
}

/// Energy is smoothed over this long before looking for rises.
const SMOOTH_SECONDS: f64 = 0.004;
/// A rise is measured against the quietest level in this window before it.
const LOOKBACK_SECONDS: (f64, f64) = (0.025, 0.004);
/// How much louder (natural log of the ratio) counts as an attack: about 5 dB.
const MIN_RISE: f32 = 0.55;
/// Ignore attacks quieter than this fraction of the loudest level nearby, so busy
/// quiet passages don't fill up with markers.
const RELATIVE_FLOOR: f32 = 0.18;
const NEIGHBOURHOOD_SECONDS: f64 = 1.0;
/// Absolute floor (summed band peaks, 0..3).
const ABSOLUTE_FLOOR: f32 = 0.03;
/// Attacks closer than this keep only the stronger.
const MIN_GAP_SECONDS: f64 = 0.06;
pub const BODY_SECONDS: f64 = 0.02;

/// Find the attacks in `summary`, in order.
pub fn detect(summary: &WaveformSummary) -> Vec<Transient> {
    let Some(bins) = summary.levels.first() else {
        return Vec::new();
    };
    let per_bin = f64::from(summary.frames_per_bin.max(1));
    let rate = f64::from(summary.sample_rate.max(1));
    let to_bins = |seconds: f64| ((seconds * rate / per_bin).round() as usize).max(1);

    // Band-summed peak energy, smoothed.
    let raw: Vec<f32> = bins
        .iter()
        .map(|b| (f32::from(b[LOW]) + f32::from(b[MID]) + f32::from(b[HIGH])) / 255.0)
        .collect();
    let smooth = to_bins(SMOOTH_SECONDS);
    let energy: Vec<f32> = (0..raw.len())
        .map(|i| {
            let window = &raw[i.saturating_sub(smooth - 1)..=i];
            window.iter().sum::<f32>() / window.len() as f32
        })
        .collect();

    // Loudest level per neighbourhood block, for the relative floor.
    let block = to_bins(NEIGHBOURHOOD_SECONDS / 2.0);
    let block_max: Vec<f32> = energy
        .chunks(block)
        .map(|c| c.iter().copied().fold(0.0, f32::max))
        .collect();
    let loud_near = |i: usize| {
        let b = i / block;
        let from = b.saturating_sub(1);
        let to = (b + 1).min(block_max.len() - 1);
        block_max[from..=to].iter().copied().fold(0.0, f32::max)
    };

    let (back_far, back_near) = (to_bins(LOOKBACK_SECONDS.0), to_bins(LOOKBACK_SECONDS.1));
    let rise: Vec<f32> = (0..energy.len())
        .map(|i| {
            if i < back_far {
                return 0.0;
            }
            let before = energy[i - back_far..=i - back_near]
                .iter()
                .copied()
                .fold(f32::MAX, f32::min);
            let e = energy[i];
            match e >= ABSOLUTE_FLOOR && e >= RELATIVE_FLOOR * loud_near(i) {
                true => (e + 0.01).ln() - (before + 0.01).ln(),
                false => 0.0,
            }
        })
        .collect();

    // Peaks of the rise, at least MIN_GAP apart (the stronger wins).
    let gap = to_bins(MIN_GAP_SECONDS);
    let body = to_bins(BODY_SECONDS);
    let mut found: Vec<(usize, f32)> = Vec::new();
    for i in 1..rise.len().saturating_sub(1) {
        let r = rise[i];
        if r < MIN_RISE || r < rise[i - 1] || r < rise[i + 1] {
            continue;
        }
        match found.last_mut() {
            Some(last) if i - last.0 < gap => {
                if r > last.1 {
                    *last = (i, r);
                }
            }
            _ => found.push((i, r)),
        }
    }
    found
        .into_iter()
        .map(|(i, _)| {
            // The attack starts where the smoothing window began to rise.
            let start = i.saturating_sub(smooth / 2);
            let bins = bins[start..(start + body).min(bins.len())]
                .iter()
                .fold([0u8; 4], |acc, b| std::array::from_fn(|k| acc[k].max(b[k])));
            Transient {
                frame: start as u64 * u64::from(summary.frames_per_bin),
                bins,
            }
        })
        .collect()
}

/// The transients from `from` (inclusive) to `to` (exclusive), in frames.
pub fn between(transients: &[Transient], from: f64, to: f64) -> &[Transient] {
    let start = transients.partition_point(|t| (t.frame as f64) < from);
    let end = transients.partition_point(|t| (t.frame as f64) < to);
    &transients[start..end.max(start)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WaveformBuilder;

    const SR: u32 = 44_100;

    /// Clicks of decaying noise every `interval` seconds, over a quiet hum.
    fn clicks(interval: f64, seconds: f64) -> WaveformSummary {
        let frames = (seconds * f64::from(SR)) as u64;
        let every = (interval * f64::from(SR)) as u64;
        let mut builder = WaveformBuilder::new(SR, frames);
        let mut noise = 12345u32;
        for i in 0..frames {
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let white = (noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            let since = (i % every) as f32 / SR as f32;
            let hum = (std::f32::consts::TAU * 220.0 * i as f32 / SR as f32).sin() * 0.01;
            let s = white * 0.8 * (-since * 60.0).exp() + hum;
            builder.push(s, s);
        }
        builder.finish()
    }

    #[test]
    fn finds_each_click_once_near_its_start() {
        let summary = clicks(0.25, 4.0);
        let found = detect(&summary);
        // The click at frame 0 has nothing before it to rise from.
        assert_eq!(
            found.len(),
            15,
            "{:?}",
            found.iter().map(|t| t.frame).collect::<Vec<_>>()
        );
        let every = (0.25 * f64::from(SR)) as u64;
        for (n, t) in found.iter().enumerate() {
            let expected = (n as u64 + 1) * every;
            let error_ms = (t.frame as f64 - expected as f64).abs() / f64::from(SR) * 1000.0;
            assert!(error_ms < 5.0, "click {n} at {} ms off", error_ms);
            assert!(t.bins[3] > 100, "{:?}", t.bins);
        }
    }

    #[test]
    fn steady_tone_has_no_transients_after_its_start() {
        let frames = 3 * SR as u64;
        let mut builder = WaveformBuilder::new(SR, frames);
        for i in 0..frames {
            let s = (std::f32::consts::TAU * 440.0 * i as f32 / SR as f32).sin() * 0.5;
            builder.push(s, s);
        }
        let found = detect(&builder.finish());
        assert!(found.len() <= 1, "{found:?}");
    }

    #[test]
    fn between_selects_a_range() {
        let t = |frame| Transient {
            frame,
            bins: [0; 4],
        };
        let all = [t(10), t(20), t(30)];
        assert_eq!(between(&all, 15.0, 30.0), &[t(20)]);
        assert!(between(&all, 31.0, 40.0).is_empty());
    }
}
