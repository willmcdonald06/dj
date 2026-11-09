//! Streaming stereo sample-rate conversion to the engine rate.
//!
//! Wraps rubato's synchronous FFT resampler (fast, high quality, fixed ratio) so audio
//! can be fed in packet-sized pieces as it is decoded. The resampler's start-up delay is
//! trimmed, and [`StereoResampler::finish`] flushes the tail so the output is exactly
//! `ceil(input × ratio)` frames long and aligned with the input.
//!
//! Not RT-safe; runs on loader threads.

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Indexing, Resampler};

const CHANNELS: usize = 2;
/// Input frames per resampler call.
const CHUNK_FRAMES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum ResampleError {
    #[error("cannot build resampler from {from} Hz to {to} Hz: {message}")]
    Construction { from: u32, to: u32, message: String },
    #[error("resampling failed: {0}")]
    Process(String),
}

pub struct StereoResampler {
    inner: Fft<f32>,
    from: u64,
    to: u64,
    /// Input not yet consumed, one `Vec` per channel.
    pending: Vec<Vec<f32>>,
    /// Output scratch, one `Vec` per channel, `output_frames_max` long.
    out: Vec<Vec<f32>>,
    /// Start-up delay frames still to drop.
    to_trim: usize,
    input_total: u64,
    output_total: u64,
}

impl StereoResampler {
    pub fn new(from: u32, to: u32) -> Result<Self, ResampleError> {
        let inner = Fft::<f32>::new(
            from as usize,
            to as usize,
            CHUNK_FRAMES,
            CHANNELS,
            FixedSync::Input,
        )
        .map_err(|err| ResampleError::Construction {
            from,
            to,
            message: err.to_string(),
        })?;
        let out_max = inner.output_frames_max();
        let to_trim = inner.output_delay();
        Ok(Self {
            inner,
            from: from as u64,
            to: to as u64,
            pending: (0..CHANNELS)
                .map(|_| Vec::with_capacity(CHUNK_FRAMES * 4))
                .collect(),
            out: vec![vec![0.0; out_max]; CHANNELS],
            to_trim,
            input_total: 0,
            output_total: 0,
        })
    }

    /// Feed planar stereo input. Resampled output is passed to `emit` as planar
    /// `(left, right)` slices of equal length.
    pub fn push(
        &mut self,
        left: &[f32],
        right: &[f32],
        emit: &mut dyn FnMut(&[f32], &[f32]),
    ) -> Result<(), ResampleError> {
        self.pending[0].extend_from_slice(left);
        self.pending[1].extend_from_slice(right);
        self.input_total += left.len() as u64;
        while self.pending[0].len() >= self.inner.input_frames_next() {
            self.process_chunk(None, emit)?;
        }
        Ok(())
    }

    /// Flush the remaining input and the resampler's tail.
    pub fn finish(&mut self, emit: &mut dyn FnMut(&[f32], &[f32])) -> Result<(), ResampleError> {
        let remaining = self.pending[0].len();
        if remaining > 0 {
            self.process_chunk(Some(remaining), emit)?;
        }
        // Push silence until the delayed tail has come out.
        while self.output_total < self.expected_output() {
            self.process_chunk(Some(0), emit)?;
        }
        Ok(())
    }

    /// Output length matching all input pushed so far.
    fn expected_output(&self) -> u64 {
        // Integer maths: the float ratio can land a hair above a whole number and round
        // up one frame too many.
        (self.input_total * self.to).div_ceil(self.from)
    }

    fn process_chunk(
        &mut self,
        partial_len: Option<usize>,
        emit: &mut dyn FnMut(&[f32], &[f32]),
    ) -> Result<(), ResampleError> {
        let needed = self.inner.input_frames_next();
        // The adapter needs `needed` frames even for a partial chunk; pad with zeros.
        for channel in &mut self.pending {
            if channel.len() < needed {
                channel.resize(needed, 0.0);
            }
        }
        let out_max = self.out[0].len();
        let input = SequentialSliceOfVecs::new(&self.pending, CHANNELS, needed)
            .map_err(|err| ResampleError::Process(err.to_string()))?;
        let mut output = SequentialSliceOfVecs::new_mut(&mut self.out, CHANNELS, out_max)
            .map_err(|err| ResampleError::Process(err.to_string()))?;
        let indexing = Indexing {
            partial_len,
            ..Indexing::default()
        };
        let (consumed, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|err| ResampleError::Process(err.to_string()))?;

        let consumed = match partial_len {
            // The whole partial chunk (and its zero padding) is used up.
            Some(_) => self.pending[0].len(),
            None => consumed,
        };
        for channel in &mut self.pending {
            channel.drain(..consumed);
        }

        let skip = self.to_trim.min(produced);
        self.to_trim -= skip;
        // Never emit more than the input so far accounts for; this also cuts the
        // flushed silence at the exact end.
        let room = self.expected_output().saturating_sub(self.output_total) as usize;
        let count = (produced - skip).min(room);
        if count > 0 {
            emit(
                &self.out[0][skip..skip + count],
                &self.out[1][skip..skip + count],
            );
            self.output_total += count as u64;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (i as f32 / rate as f32 * hz * std::f32::consts::TAU).sin() * 0.5)
            .collect()
    }

    fn resample_all(from: u32, to: u32, input: &[f32], piece: usize) -> (Vec<f32>, Vec<f32>) {
        let mut resampler = StereoResampler::new(from, to).unwrap();
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut emit = |l: &[f32], r: &[f32]| {
            left.extend_from_slice(l);
            right.extend_from_slice(r);
        };
        for chunk in input.chunks(piece) {
            let negated: Vec<f32> = chunk.iter().map(|s| -s).collect();
            resampler.push(chunk, &negated, &mut emit).unwrap();
        }
        resampler.finish(&mut emit).unwrap();
        (left, right)
    }

    #[test]
    fn output_length_is_exact() {
        for (from, frames) in [(44_100, 44_100), (44_100, 1000), (96_000, 12_345)] {
            let input = sine(from, 440.0, frames);
            let (left, right) = resample_all(from, 48_000, &input, 1152);
            let expected = (frames as f64 * 48_000.0 / from as f64).ceil() as usize;
            assert_eq!(left.len(), expected, "{from} Hz, {frames} frames");
            assert_eq!(right.len(), expected);
        }
    }

    #[test]
    fn sine_keeps_pitch_amplitude_and_alignment() {
        // 1 s of 1 kHz at 44.1 kHz becomes 1 s of 1 kHz at 48 kHz.
        let input = sine(44_100, 1000.0, 44_100);
        let (left, right) = resample_all(44_100, 48_000, &input, 777);
        let expected = sine(48_000, 1000.0, 48_000);
        // Ignore the edges, where the filter sees the signal start and stop.
        let max_err = left[2000..46_000]
            .iter()
            .zip(&expected[2000..46_000])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 0.01, "max error {max_err}");
        assert!(
            right[2000..46_000]
                .iter()
                .zip(&left[2000..46_000])
                .all(|(r, l)| *r == -*l)
        );
    }
}
