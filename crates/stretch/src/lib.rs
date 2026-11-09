//! Time-stretching for key lock, behind the [`TimeStretcher`] trait so the algorithm can
//! be swapped (the spec leaves Rubber Band vs Signalsmith Stretch open until it has
//! been benchmarked on the CM5).
//!
//! [`Signalsmith`] wraps Signalsmith Stretch (MIT), vendored in `third_party/` and
//! compiled by this crate's build script.
//!
//! # RT safety
//!
//! Creating a stretcher allocates and is **not** RT-safe. [`TimeStretcher::process`],
//! [`TimeStretcher::seek`] and [`TimeStretcher::reset`] are **RT-safe** once created: the
//! C++ sizes all its buffers when it is configured and processes in place after that.

use std::ffi::c_void;

/// Changes playback speed without changing pitch.
///
/// Streams are planar stereo. Feeding `input` frames and asking for `output` frames
/// plays at `input / output` times the original speed.
pub trait TimeStretcher: Send {
    /// Clear all internal state.
    fn reset(&mut self);

    /// Input the stretcher needs ahead of the point being processed, in input frames.
    fn input_latency(&self) -> usize;

    /// Delay from the point being processed to the output, in output frames.
    fn output_latency(&self) -> usize;

    /// Most pre-roll [`seek`](Self::seek) can use, in input frames.
    fn max_preroll(&self) -> usize;

    /// Prime the stretcher with the input just before where playback continues, after
    /// a [`reset`](Self::reset). Ideally [`max_preroll`](Self::max_preroll) frames.
    fn seek(&mut self, left: &[f32], right: &[f32], rate: f64);

    /// Consume all of `input`, write all of `output`. Each pair must be equal length.
    fn process(&mut self, input: (&[f32], &[f32]), output: (&mut [f32], &mut [f32]));
}

unsafe extern "C" {
    fn dj_stretch_create(channels: i32, sample_rate: f32, cheaper: i32, split: i32) -> *mut c_void;
    fn dj_stretch_destroy(handle: *mut c_void);
    fn dj_stretch_reset(handle: *mut c_void);
    fn dj_stretch_input_latency(handle: *mut c_void) -> i32;
    fn dj_stretch_output_latency(handle: *mut c_void) -> i32;
    fn dj_stretch_block_samples(handle: *mut c_void) -> i32;
    fn dj_stretch_interval_samples(handle: *mut c_void) -> i32;
    fn dj_stretch_set_transpose(handle: *mut c_void, factor: f32);
    fn dj_stretch_seek(handle: *mut c_void, inputs: *const *const f32, frames: i32, rate: f64);
    fn dj_stretch_process(
        handle: *mut c_void,
        inputs: *const *const f32,
        input_frames: i32,
        outputs: *const *mut f32,
        output_frames: i32,
    );
}

/// Quality / CPU trade-off for [`Signalsmith`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    #[default]
    Default,
    /// Smaller FFT blocks: less CPU and latency, slightly rougher sound.
    Cheaper,
}

/// Signalsmith Stretch, stereo.
pub struct Signalsmith {
    handle: *mut c_void,
}

// SAFETY: the C++ object is only ever used through `&mut self` (or `&self` for the
// read-only latency queries), so it is never touched from two threads at once.
unsafe impl Send for Signalsmith {}

impl Signalsmith {
    /// Create a stereo stretcher. Uses split computation, which spreads the FFT work
    /// evenly across calls instead of doing it in bursts, at the cost of one interval
    /// of extra latency: the audio callback needs steady timing more than low latency.
    ///
    /// Not RT-safe.
    pub fn new(sample_rate: u32, quality: Quality) -> Self {
        let cheaper = match quality {
            Quality::Default => 0,
            Quality::Cheaper => 1,
        };
        // SAFETY: plain constructor; returns an owned pointer freed in `Drop`.
        let handle = unsafe { dj_stretch_create(2, sample_rate as f32, cheaper, 1) };
        assert!(!handle.is_null(), "Signalsmith Stretch allocation failed");
        Self { handle }
    }

    /// Pitch shift factor (1.0 = none). Key lock keeps this at 1.0.
    pub fn set_transpose(&mut self, factor: f32) {
        // SAFETY: valid handle owned by self.
        unsafe { dj_stretch_set_transpose(self.handle, factor) }
    }
}

impl Drop for Signalsmith {
    fn drop(&mut self) {
        // SAFETY: the handle came from `dj_stretch_create` and is dropped once.
        unsafe { dj_stretch_destroy(self.handle) }
    }
}

fn to_i32(n: usize) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

impl TimeStretcher for Signalsmith {
    fn reset(&mut self) {
        // SAFETY: valid handle owned by self.
        unsafe { dj_stretch_reset(self.handle) }
    }

    fn input_latency(&self) -> usize {
        // SAFETY: valid handle; read-only query.
        unsafe { dj_stretch_input_latency(self.handle) }.max(0) as usize
    }

    fn output_latency(&self) -> usize {
        // SAFETY: valid handle; read-only query.
        unsafe { dj_stretch_output_latency(self.handle) }.max(0) as usize
    }

    fn max_preroll(&self) -> usize {
        // SAFETY: valid handle; read-only queries.
        let (block, interval) = unsafe {
            (
                dj_stretch_block_samples(self.handle),
                dj_stretch_interval_samples(self.handle),
            )
        };
        (block + interval).max(0) as usize
    }

    fn seek(&mut self, left: &[f32], right: &[f32], rate: f64) {
        let frames = left.len().min(right.len());
        let inputs = [left.as_ptr(), right.as_ptr()];
        // SAFETY: both channel pointers are valid for `frames` reads.
        unsafe { dj_stretch_seek(self.handle, inputs.as_ptr(), to_i32(frames), rate) }
    }

    fn process(&mut self, input: (&[f32], &[f32]), output: (&mut [f32], &mut [f32])) {
        let in_frames = input.0.len().min(input.1.len());
        let out_frames = output.0.len().min(output.1.len());
        let inputs = [input.0.as_ptr(), input.1.as_ptr()];
        let outputs = [output.0.as_mut_ptr(), output.1.as_mut_ptr()];
        // SAFETY: input pointers are valid for `in_frames` reads and output pointers for
        // `out_frames` writes; the input and output buffers are distinct borrows.
        unsafe {
            dj_stretch_process(
                self.handle,
                inputs.as_ptr(),
                to_i32(in_frames),
                outputs.as_ptr(),
                to_i32(out_frames),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn sine(hz: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (std::f32::consts::TAU * hz * i as f32 / SR as f32).sin() * 0.5)
            .collect()
    }

    /// Stretch `input` at `rate` in 64-frame output blocks, as the deck does.
    fn stretch(input: &[f32], rate: f64, out_frames: usize) -> Vec<f32> {
        let mut s = Signalsmith::new(SR, Quality::Default);
        let mut out = vec![0.0; out_frames];
        let mut scratch = vec![0.0; 64];
        let mut read = 0.0f64;
        for chunk in out.chunks_mut(64) {
            let n = chunk.len();
            let start = read as usize;
            read += n as f64 * rate;
            let end = (read as usize).min(input.len());
            let slice = &input[start.min(end)..end];
            s.process((slice, slice), (chunk, &mut scratch[..n]));
        }
        out
    }

    fn rising_crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    #[test]
    fn faster_playback_keeps_pitch() {
        let input = sine(440.0, SR as usize * 3);
        // 1.08x for two seconds of output.
        let out = stretch(&input, 1.08, SR as usize * 2);
        let middle = &out[SR as usize / 2..SR as usize * 3 / 2];
        let hz = rising_crossings(middle) as f32;
        assert!((hz - 440.0).abs() <= 3.0, "pitch {hz} Hz");
        let peak = middle.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.1, "level {peak}");
    }

    #[test]
    fn latencies_and_preroll_are_sane() {
        let s = Signalsmith::new(SR, Quality::Default);
        let total = s.input_latency() + s.output_latency();
        // Tens of milliseconds, not seconds.
        assert!(total > 0 && total < SR as usize / 5, "latency {total}");
        assert!(s.max_preroll() >= s.input_latency());
        let cheaper = Signalsmith::new(SR, Quality::Cheaper);
        assert!(cheaper.input_latency() + cheaper.output_latency() <= total);
    }
}
