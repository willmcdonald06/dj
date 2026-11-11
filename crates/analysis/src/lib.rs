//! In-app track analysis: waveform summaries and kick (transient) markers.
//!
//! [`analyze`] reads a decoded track once and runs every analyser on it. It is meant
//! for a background thread (the loader runs it right after decoding).
//!
//! Not RT-safe.

pub mod beatgrid;
pub mod key;
pub mod onsets;
pub mod tempo;
pub mod transients;
pub mod waveform;

pub use beatgrid::GridEstimate;
pub use key::{Key, KeyDetector, KeyEstimate, KeyNotation, Mode};
pub use onsets::KickDetector;
pub use transients::Transient;
pub use waveform::{WaveformBuilder, WaveformSummary};

use std::sync::Arc;

use dj_engine::Track;

/// Bumped whenever an analyser changes its output, so stored results can be redone.
pub const ANALYSIS_VERSION: u32 = 4;

/// Level (peak, linear) that counts as the track's first sound: -40 dB.
pub const FIRST_SOUND_LEVEL: f32 = 0.01;
/// The auto cue moves to a beat no more than this many beats from the first sound.
const AUTO_CUE_SNAP_BEATS: f64 = 0.25;

#[derive(Debug, Clone, PartialEq)]
pub struct TrackAnalysis {
    pub version: u32,
    pub sample_rate: u32,
    pub frames: u64,
    /// Shared so the UI can hand it to the renderer without copying.
    pub waveform: Arc<WaveformSummary>,
    /// Kick onsets in frames, ascending.
    pub kicks: Vec<u64>,
    /// Constant-tempo grid fitted to the kicks, if there are enough of them.
    pub grid: Option<GridEstimate>,
    /// Musical key, unless the track is silent.
    pub key: Option<KeyEstimate>,
}

impl TrackAnalysis {
    /// Index of the kick nearest to `frame`, if there are any.
    pub fn nearest_kick(&self, frame: f64) -> Option<u64> {
        let split = self.kicks.partition_point(|&k| (k as f64) < frame);
        let before = split.checked_sub(1).map(|i| self.kicks[i]);
        let after = self.kicks.get(split).copied();
        match (before, after) {
            (Some(b), Some(a)) => match frame - b as f64 <= a as f64 - frame {
                true => Some(b),
                false => Some(a),
            },
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// Tempo implied by the most common kick spacing, folded into 70..=180 BPM. A rough
    /// figure for display and sanity checks; the beatgrid comes later (Milestone 4).
    pub fn kick_bpm(&self) -> Option<f64> {
        let mut intervals: Vec<f64> = self
            .kicks
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / self.sample_rate as f64)
            .filter(|&s| s > 0.2 && s < 2.0)
            .collect();
        if intervals.len() < 4 {
            return None;
        }
        intervals.sort_by(f64::total_cmp);
        let mut bpm = 60.0 / intervals[intervals.len() / 2];
        while bpm < 70.0 {
            bpm *= 2.0;
        }
        while bpm > 180.0 {
            bpm /= 2.0;
        }
        Some(bpm)
    }

    /// The first frame louder than [`FIRST_SOUND_LEVEL`], from the waveform summary.
    pub fn first_sound(&self) -> Option<u64> {
        let threshold = (FIRST_SOUND_LEVEL * 255.0).ceil() as u8;
        let bins = self.waveform.levels.first()?;
        let bin = bins.iter().position(|b| b[waveform::PEAK] >= threshold)?;
        Some(bin as u64 * self.waveform.frames_per_bin as u64)
    }

    /// Where a freshly loaded track should be cued: the first sound, moved onto the
    /// beatgrid's nearest beat when it is close to one (a soft attack or a short lead
    /// in shouldn't put the cue off the beat).
    pub fn auto_cue(&self, grid: Option<dj_engine::BeatGrid>) -> Option<f64> {
        let sound = self.first_sound()? as f64;
        let Some(grid) = grid else {
            return Some(sound);
        };
        let beat = grid.snap(sound);
        match (beat - sound).abs() <= grid.period * AUTO_CUE_SNAP_BEATS {
            true => Some(beat.max(0.0)),
            false => Some(sound),
        }
    }

    /// Kicks in `[from, to)` frames.
    pub fn kicks_between(&self, from: f64, to: f64) -> &[u64] {
        let start = self.kicks.partition_point(|&k| (k as f64) < from);
        let end = self.kicks.partition_point(|&k| (k as f64) < to);
        &self.kicks[start..end.max(start)]
    }
}

/// Analyse frames given as interleaved-stereo pairs.
pub fn analyze_frames(
    frames: impl IntoIterator<Item = (f32, f32)>,
    sample_rate: u32,
    expected_frames: u64,
) -> TrackAnalysis {
    let mut waveform = WaveformBuilder::new(sample_rate, expected_frames);
    let mut kicks = KickDetector::new(sample_rate, expected_frames);
    let mut key = KeyDetector::new(sample_rate);
    let mut onsets = tempo::OnsetAnalyser::new(sample_rate, expected_frames);
    for (left, right) in frames {
        waveform.push(left, right);
        kicks.push(left, right);
        key.push(left, right);
        onsets.push(left, right);
    }
    let waveform = waveform.finish();
    let kicks = kicks.finish();
    let onsets = onsets.finish();
    TrackAnalysis {
        version: ANALYSIS_VERSION,
        sample_rate,
        frames: waveform.frames,
        waveform: Arc::new(waveform),
        grid: tempo::estimate(&onsets, &kicks, sample_rate),
        key: key.finish(),
        kicks,
    }
}

/// Analyse the decoded part of `track` (all of it once loading has finished).
pub fn analyze(track: &Track) -> TrackAnalysis {
    let frames = track.ready_frames();
    analyze_frames(
        (0..frames).map(|i| track.frame_unchecked_ready(i)),
        track.sample_rate(),
        frames,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis_with_kicks(kicks: Vec<u64>) -> TrackAnalysis {
        let mut a = analyze_frames(std::iter::empty(), 48_000, 0);
        a.kicks = kicks;
        a
    }

    #[test]
    fn nearest_and_range_queries() {
        let a = analysis_with_kicks(vec![100, 200, 300]);
        assert_eq!(a.nearest_kick(0.0), Some(100));
        assert_eq!(a.nearest_kick(149.0), Some(100));
        assert_eq!(a.nearest_kick(151.0), Some(200));
        assert_eq!(a.nearest_kick(1e9), Some(300));
        assert_eq!(a.kicks_between(150.0, 300.0), &[200]);
        assert_eq!(a.kicks_between(400.0, 500.0), &[] as &[u64]);
        assert_eq!(analysis_with_kicks(vec![]).nearest_kick(5.0), None);
    }

    #[test]
    fn auto_cue_skips_silence_and_snaps_to_the_beat() {
        // 0.5 s of silence, then a tone.
        let frames: Vec<(f32, f32)> = (0..96_000)
            .map(|i| match i >= 24_000 {
                true => (0.5, 0.5),
                false => (0.0, 0.0),
            })
            .collect();
        let a = analyze_frames(frames, 48_000, 96_000);
        assert_eq!(a.first_sound(), Some(24_000));
        assert_eq!(a.auto_cue(None), Some(24_000.0));
        // A beat 1000 frames later (well within a quarter beat): the cue moves to it.
        let grid = dj_engine::BeatGrid::from_bpm(120.0, 25_000.0, 48_000);
        assert_eq!(a.auto_cue(Some(grid)), Some(25_000.0));
        // A beat half a beat away: stays on the sound.
        let grid = dj_engine::BeatGrid::from_bpm(120.0, 36_000.0, 48_000);
        assert_eq!(a.auto_cue(Some(grid)), Some(24_000.0));
    }

    #[test]
    fn analyze_reads_a_track() {
        let (track, mut writer) = Track::new(dj_engine::TrackId(1), 48_000, 48_000);
        let (samples, truth) = onsets::tests::kick_track(120.0, 0.1, 1.0, |_| 0.0);
        let stereo: Vec<f32> = samples.iter().flat_map(|&s| [s, s]).collect();
        writer.push_interleaved(&stereo);
        writer.finish();
        let a = analyze(&track);
        assert_eq!(a.frames, 48_000);
        assert_eq!(a.kicks.len(), truth.len());
    }
}
