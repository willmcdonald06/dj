//! Multi-resolution waveform summaries, coloured by frequency band.
//!
//! Level 0 holds one bin per [`BASE_FRAMES_PER_BIN`] frames; each level above halves
//! the resolution (each bin is the max of two below). A renderer picks the level whose
//! bins are about one pixel wide, so drawing costs the same at any zoom. Each bin keeps
//! the peak of the low, mid and high bands and of the full signal, as `u8`.

use dj_engine::dsp::IsolatorEq;

/// Frames per bin at level 0. At 48 kHz that is 750 bins a second, enough for about
/// 1.7 s across 1280 pixels at the closest zoom.
pub const BASE_FRAMES_PER_BIN: u32 = 64;

/// Index into a bin.
pub const LOW: usize = 0;
pub const MID: usize = 1;
pub const HIGH: usize = 2;
pub const PEAK: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct WaveformSummary {
    pub sample_rate: u32,
    pub frames: u64,
    /// Frames per bin at level 0; a bin at level `n` covers `frames_per_bin << n`.
    pub frames_per_bin: u32,
    /// Finest first. Each bin is `[low, mid, high, peak]`, peak amplitudes scaled to
    /// 0..=255.
    pub levels: Vec<Vec<[u8; 4]>>,
}

impl WaveformSummary {
    /// The coarsest level whose bins are no wider than `frames_per_pixel`, so a pixel
    /// spans one or two bins.
    pub fn level_for(&self, frames_per_pixel: f64) -> usize {
        let ratio = frames_per_pixel / self.frames_per_bin as f64;
        let level = match ratio > 1.0 {
            true => ratio.log2().floor() as usize,
            false => 0,
        };
        level.min(self.levels.len().saturating_sub(1))
    }

    pub fn bin_frames(&self, level: usize) -> u64 {
        (self.frames_per_bin as u64) << level
    }

    /// Total bins over all levels.
    pub fn total_bins(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }
}

/// Builds a [`WaveformSummary`] from frames pushed one at a time.
pub struct WaveformBuilder {
    sample_rate: u32,
    split: IsolatorEq,
    frames: u64,
    in_bin: u32,
    current: [f32; 4],
    bins: Vec<[u8; 4]>,
}

impl WaveformBuilder {
    pub fn new(sample_rate: u32, expected_frames: u64) -> Self {
        let bins = expected_frames / BASE_FRAMES_PER_BIN as u64 + 1;
        Self {
            sample_rate,
            split: IsolatorEq::new(sample_rate as f32),
            frames: 0,
            in_bin: 0,
            current: [0.0; 4],
            bins: Vec::with_capacity(bins as usize),
        }
    }

    #[inline]
    pub fn push(&mut self, left: f32, right: f32) {
        let mono = (left + right) * 0.5;
        let bands = self.split.split(mono);
        for (peak, value) in self.current.iter_mut().zip(bands) {
            *peak = peak.max(value.abs());
        }
        self.current[PEAK] = self.current[PEAK].max(mono.abs());
        self.frames += 1;
        self.in_bin += 1;
        if self.in_bin == BASE_FRAMES_PER_BIN {
            self.flush_bin();
        }
    }

    fn flush_bin(&mut self) {
        self.bins.push(self.current.map(quantise));
        self.current = [0.0; 4];
        self.in_bin = 0;
    }

    pub fn finish(mut self) -> WaveformSummary {
        if self.in_bin > 0 {
            self.flush_bin();
        }
        let mut levels = vec![self.bins];
        while levels.last().is_some_and(|level| level.len() > 1) {
            let below = levels.last().map(Vec::as_slice).unwrap_or_default();
            let next = below
                .chunks(2)
                .map(|pair| match pair {
                    [a, b] => [0, 1, 2, 3].map(|i| a[i].max(b[i])),
                    [a] => *a,
                    _ => [0; 4],
                })
                .collect();
            levels.push(next);
        }
        WaveformSummary {
            sample_rate: self.sample_rate,
            frames: self.frames,
            frames_per_bin: BASE_FRAMES_PER_BIN,
            levels,
        }
    }
}

fn quantise(peak: f32) -> u8 {
    (peak.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn summary_of(hz: f32, seconds: f32) -> WaveformSummary {
        let frames = (SR as f32 * seconds) as u64;
        let mut builder = WaveformBuilder::new(SR, frames);
        for i in 0..frames {
            let s = (std::f32::consts::TAU * hz * i as f32 / SR as f32).sin() * 0.8;
            builder.push(s, s);
        }
        builder.finish()
    }

    /// Peak of one band over the middle of level 0. (Individual bins follow the shape
    /// of a low note's waveform, so the mean would be lower.)
    fn band(summary: &WaveformSummary, band: usize) -> f32 {
        let bins = &summary.levels[0];
        let middle = &bins[bins.len() / 4..bins.len() * 3 / 4];
        middle.iter().map(|b| b[band]).max().unwrap_or(0) as f32 / 255.0
    }

    #[test]
    fn bands_follow_frequency() {
        let bass = summary_of(50.0, 1.0);
        assert!(band(&bass, LOW) > 0.7, "low {}", band(&bass, LOW));
        assert!(band(&bass, HIGH) < 0.05);

        let mid = summary_of(800.0, 1.0);
        assert!(band(&mid, MID) > 0.7);
        assert!(band(&mid, LOW) < 0.1 && band(&mid, HIGH) < 0.1);

        let treble = summary_of(9000.0, 1.0);
        assert!(band(&treble, HIGH) > 0.7);
        assert!(band(&treble, LOW) < 0.05);
        assert!((band(&treble, PEAK) - 0.8).abs() < 0.02);
    }

    #[test]
    fn levels_halve_down_to_one_bin() {
        let summary = summary_of(440.0, 2.0);
        assert_eq!(summary.frames, 96_000);
        assert_eq!(summary.levels[0].len(), 1500);
        assert_eq!(summary.levels[1].len(), 750);
        assert_eq!(summary.levels.last().unwrap().len(), 1);
        // Max-reduction keeps the peak at every level.
        let top = summary.levels.last().unwrap()[0][PEAK];
        assert_eq!(
            top,
            summary.levels[0].iter().map(|b| b[PEAK]).max().unwrap()
        );
    }

    #[test]
    fn level_for_picks_bins_about_a_pixel_wide() {
        let summary = summary_of(440.0, 2.0);
        assert_eq!(summary.level_for(10.0), 0);
        assert_eq!(summary.level_for(64.0), 0);
        assert_eq!(summary.level_for(128.0), 1);
        assert_eq!(summary.level_for(250.0), 1);
        assert_eq!(summary.level_for(1e12), summary.levels.len() - 1);
    }
}
