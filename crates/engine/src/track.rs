//! Decoded track audio shared between a loader thread (writer) and the audio thread
//! (reader).
//!
//! A [`Track`] is allocated once, at its final size, before any audio is decoded. The
//! loader then appends frames through the single [`TrackWriter`] and publishes how many
//! are ready; the audio thread can start playing as soon as the first frames are in.
//! Samples are stored as `AtomicU32` bit patterns so the writer and reader can share the
//! buffer without `unsafe`. Relaxed atomic loads and stores of `u32` compile to plain
//! loads and stores on x86-64 and AArch64, so this costs nothing over a `[f32]`.
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

/// Identifies one load of a track. Increases with every load, so the UI can tell which
/// load a deck snapshot refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u64);

pub struct Track {
    id: TrackId,
    sample_rate: u32,
    /// Interleaved stereo `f32` samples, stored as bits.
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
        let samples: Box<[AtomicU32]> = (0..capacity_frames * TRACK_CHANNELS)
            .map(|_| AtomicU32::new(0))
            .collect();
        let track = Arc::new(Track {
            id,
            sample_rate,
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
        self.track.samples.len() / TRACK_CHANNELS
    }

    /// Append interleaved stereo frames and make them visible to readers. Frames that
    /// don't fit are dropped; returns how many were written.
    pub fn push_interleaved(&mut self, stereo: &[f32]) -> usize {
        let start = self.written * TRACK_CHANNELS;
        let room = self.track.samples.len() - start;
        let count = (stereo.len() / TRACK_CHANNELS * TRACK_CHANNELS).min(room);
        for (slot, &sample) in self.track.samples[start..start + count].iter().zip(stereo) {
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        let frames = count / TRACK_CHANNELS;
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
    fn frames_become_visible_as_written() {
        let (track, mut writer) = Track::new(TrackId(1), 48_000, 4);
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
