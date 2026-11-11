//! Decoded track audio shared between a loader thread (writer) and the audio thread
//! (reader).
//!
//! A [`Track`] is allocated once, at its final size, before any audio is decoded. The
//! loader then appends frames through the single [`TrackWriter`] and publishes how many
//! are ready; the audio thread can start playing as soon as the first frames are in.
//! Frames are stored as `AtomicU32`s so the writer and reader can share the buffer
//! without `unsafe`. Relaxed atomic loads and stores of `u32` compile to plain loads and
//! stores on x86-64 and AArch64, so this costs nothing over a plain slice.
//!
//! Normally each frame is one `u32` holding two 16-bit samples ([`Track::new`]): half
//! the memory of `f32`s (a six-minute track at 48 kHz is 69 MB rather than 138 MB),
//! and half the memory the audio thread reads. Samples are scaled with 2× headroom, so
//! decoded audio a little over full scale (common in loud masters) isn't clipped; that
//! leaves about 90 dB of dynamic range. [`Track::new_exact`] keeps `f32`s, for tests
//! that need sample values back exactly.
//!
//! # RT safety
//!
//! Reading ([`Track::frame`], [`Track::ready_frames`], [`Track::len_frames`],
//! [`Track::is_complete`]) is **RT-safe**. Creating a track and writing to it are not.
//!
//! Dropping the last `Arc<Track>` frees the buffer, so the audio thread must never hold
//! the last reference. The loader keeps a reference to every track it hands out and
//! frees a track on its own thread once nobody else holds it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Channels stored per frame. Tracks are always stored as interleaved stereo.
pub const TRACK_CHANNELS: usize = 2;

/// 16-bit value of a sample of 1.0 (full scale): ±2.0 fits.
const I16_SCALE: f32 = 16_384.0;

/// How a track keeps its samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// One `u32` per frame: left in the low 16 bits, right in the high.
    PackedI16,
    /// One `u32` per sample: the `f32`'s bits.
    F32,
}

#[inline]
fn to_i16(sample: f32) -> u32 {
    ((sample * I16_SCALE).round().clamp(-32_768.0, 32_767.0) as i16) as u16 as u32
}

#[inline]
fn from_i16(bits: u32) -> f32 {
    (bits as u16 as i16) as f32 * (1.0 / I16_SCALE)
}

/// Identifies one load of a track. Increases with every load, so the UI can tell which
/// load a deck snapshot refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u64);

pub struct Track {
    id: TrackId,
    sample_rate: u32,
    format: Format,
    /// The frames, as [`Format`] says.
    samples: Box<[AtomicU32]>,
    /// Frames written and visible to readers. Only grows.
    ready: AtomicU64,
    /// Final length in frames. Equals the capacity until the writer finishes.
    len: AtomicU64,
    complete: AtomicBool,
}

impl Track {
    /// Allocate a track with room for `capacity_frames` frames, and its writer.
    ///
    /// Not RT-safe.
    pub fn new(id: TrackId, sample_rate: u32, capacity_frames: usize) -> (Arc<Track>, TrackWriter) {
        Self::with_format(id, sample_rate, capacity_frames, Format::PackedI16)
    }

    /// Like [`new`](Self::new), but keeping `f32` samples exactly (twice the memory).
    /// For tests that read sample values back.
    pub fn new_exact(
        id: TrackId,
        sample_rate: u32,
        capacity_frames: usize,
    ) -> (Arc<Track>, TrackWriter) {
        Self::with_format(id, sample_rate, capacity_frames, Format::F32)
    }

    fn with_format(
        id: TrackId,
        sample_rate: u32,
        capacity_frames: usize,
        format: Format,
    ) -> (Arc<Track>, TrackWriter) {
        let per_frame = match format {
            Format::PackedI16 => 1,
            Format::F32 => TRACK_CHANNELS,
        };
        let samples: Box<[AtomicU32]> = (0..capacity_frames * per_frame)
            .map(|_| AtomicU32::new(0))
            .collect();
        let track = Arc::new(Track {
            id,
            sample_rate,
            format,
            samples,
            ready: AtomicU64::new(0),
            len: AtomicU64::new(capacity_frames as u64),
            complete: AtomicBool::new(false),
        });
        let writer = TrackWriter {
            track: Arc::clone(&track),
            written: 0,
        };
        (track, writer)
    }

    pub fn id(&self) -> TrackId {
        self.id
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Frames that can be read now.
    #[inline]
    pub fn ready_frames(&self) -> u64 {
        self.ready.load(Ordering::Acquire)
    }

    /// Length of the track in frames. While loading this is the expected length; once
    /// [`is_complete`](Self::is_complete) it is exact.
    #[inline]
    pub fn len_frames(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// The stereo frame at `index`, or `None` if it isn't decoded yet (or is past the end).
    ///
    /// RT-safe. Callers reading many frames should load [`ready_frames`](Self::ready_frames)
    /// once and use [`frame_unchecked_ready`](Self::frame_unchecked_ready).
    #[inline]
    pub fn frame(&self, index: u64) -> Option<(f32, f32)> {
        match index < self.ready_frames() {
            true => Some(self.frame_unchecked_ready(index)),
            false => None,
        }
    }

    /// The stereo frame at `index`, which the caller has checked is below a value
    /// returned by [`ready_frames`](Self::ready_frames). Returns silence if it is out of
    /// bounds rather than panicking.
    ///
    /// RT-safe.
    #[inline]
    pub fn frame_unchecked_ready(&self, index: u64) -> (f32, f32) {
        // The format never changes, so this branch is always predicted.
        match self.format {
            Format::PackedI16 => match self.samples.get(index as usize) {
                Some(frame) => {
                    let bits = frame.load(Ordering::Relaxed);
                    (from_i16(bits), from_i16(bits >> 16))
                }
                None => (0.0, 0.0),
            },
            Format::F32 => {
                let base = index as usize * TRACK_CHANNELS;
                match self.samples.get(base..base + TRACK_CHANNELS) {
                    Some(pair) => (
                        f32::from_bits(pair[0].load(Ordering::Relaxed)),
                        f32::from_bits(pair[1].load(Ordering::Relaxed)),
                    ),
                    None => (0.0, 0.0),
                }
            }
        }
    }

    /// Bytes of audio the track holds (its capacity).
    pub fn memory_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<AtomicU32>()
    }
}

impl std::fmt::Debug for Track {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Track")
            .field("id", &self.id)
            .field("sample_rate", &self.sample_rate)
            .field("ready_frames", &self.ready_frames())
            .field("len_frames", &self.len_frames())
            .field("complete", &self.is_complete())
            .finish()
    }
}

/// The single writer for a [`Track`]. Owned by the loader thread. Not RT-safe.
pub struct TrackWriter {
    track: Arc<Track>,
    written: usize,
}

impl TrackWriter {
    pub fn track(&self) -> &Arc<Track> {
        &self.track
    }

    /// Frames written so far.
    pub fn written_frames(&self) -> usize {
        self.written
    }

    pub fn capacity_frames(&self) -> usize {
        match self.track.format {
            Format::PackedI16 => self.track.samples.len(),
            Format::F32 => self.track.samples.len() / TRACK_CHANNELS,
        }
    }

    /// Append interleaved stereo frames and make them visible to readers. Frames that
    /// don't fit are dropped; returns how many were written.
    pub fn push_interleaved(&mut self, stereo: &[f32]) -> usize {
        let room = self.capacity_frames() - self.written;
        let frames = (stereo.len() / TRACK_CHANNELS).min(room);
        let input = &stereo[..frames * TRACK_CHANNELS];
        match self.track.format {
            Format::PackedI16 => {
                let slots = &self.track.samples[self.written..self.written + frames];
                for (slot, pair) in slots.iter().zip(input.chunks_exact(TRACK_CHANNELS)) {
                    slot.store(to_i16(pair[0]) | to_i16(pair[1]) << 16, Ordering::Relaxed);
                }
            }
            Format::F32 => {
                let start = self.written * TRACK_CHANNELS;
                let slots = &self.track.samples[start..start + input.len()];
                for (slot, &sample) in slots.iter().zip(input) {
                    slot.store(sample.to_bits(), Ordering::Relaxed);
                }
            }
        }
        self.written += frames;
        // Release: readers that see the new count also see the samples stored above.
        self.track
            .ready
            .store(self.written as u64, Ordering::Release);
        frames
    }

    /// Set the length reported while loading (it defaults to the capacity). Use it when
    /// the capacity includes slack for an uncertain frame count.
    pub fn set_expected_len(&mut self, frames: usize) {
        let frames = frames.min(self.capacity_frames());
        self.track.len.store(frames as u64, Ordering::Release);
    }

    /// Mark the track fully decoded. Its length becomes the number of frames written.
    pub fn finish(self) {
        self.track.len.store(self.written as u64, Ordering::Release);
        self.track.complete.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_bit_frames_keep_their_value_and_headroom() {
        let (track, mut writer) = Track::new(TrackId(1), 48_000, 4);
        writer.push_interleaved(&[0.25, -0.5, 1.5, -1.9, 3.0, -3.0, 0.0001, -0.0001]);
        let close = |a: f32, b: f32| (a - b).abs() <= 0.5 / I16_SCALE;
        let (l, r) = track.frame(0).unwrap();
        assert!(close(l, 0.25) && close(r, -0.5), "{l} {r}");
        // Over full scale: kept, up to 2.
        let (l, r) = track.frame(1).unwrap();
        assert!(close(l, 1.5) && close(r, -1.9), "{l} {r}");
        let (l, r) = track.frame(2).unwrap();
        assert!(
            close(l, 32_767.0 / I16_SCALE) && close(r, -2.0),
            "clipped at ±2"
        );
        let (l, r) = track.frame(3).unwrap();
        assert!(close(l, 0.0001) && close(r, -0.0001));
        assert_eq!(track.memory_bytes(), 4 * 4, "one u32 per frame");
        let (exact, _) = Track::new_exact(TrackId(2), 48_000, 4);
        assert_eq!(exact.memory_bytes(), 4 * 8);
    }

    #[test]
    fn frames_become_visible_as_written() {
        let (track, mut writer) = Track::new_exact(TrackId(1), 48_000, 4);
        assert_eq!(track.frame(0), None);
        assert_eq!(writer.push_interleaved(&[0.1, 0.2, 0.3, 0.4]), 2);
        assert_eq!(track.ready_frames(), 2);
        assert_eq!(track.frame(1), Some((0.3, 0.4)));
        assert_eq!(track.frame(2), None);
        assert_eq!(track.len_frames(), 4);
        assert!(!track.is_complete());
    }

    #[test]
    fn overflow_is_dropped_and_finish_sets_length() {
        let (track, mut writer) = Track::new(TrackId(1), 48_000, 2);
        assert_eq!(writer.push_interleaved(&[1.0; 10]), 2);
        assert_eq!(writer.push_interleaved(&[1.0; 2]), 0);
        writer.finish();
        assert!(track.is_complete());
        assert_eq!(track.len_frames(), 2);
    }

    #[test]
    fn short_finish_shrinks_length() {
        let (track, mut writer) = Track::new(TrackId(1), 48_000, 100);
        writer.push_interleaved(&[0.5; 6]);
        writer.finish();
        assert_eq!(track.len_frames(), 3);
        assert_eq!(track.frame_unchecked_ready(500), (0.0, 0.0));
    }
}
