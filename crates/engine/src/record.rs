//! A copy of the main output for recording.
//!
//! The audio thread writes the main bus (interleaved stereo, exactly as sent to the
//! device) into a lock-free queue while recording is armed; a writer thread elsewhere
//! drains it to disk through [`RecordSource`].
//!
//! # RT safety
//!
//! [`RecordTap`] lives on the audio thread and is **RT-safe**: the queue is allocated in
//! [`record_link`] and [`RecordTap::write`] does one atomic load, one bounded copy into
//! preallocated queue space and, when the queue is full, one atomic add. It never waits:
//! if the writer falls behind, the block is dropped and counted.
//!
//! [`RecordSource`] is for the writer thread and is not RT-safe to create.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Seconds of audio the queue holds, so the writer can stall on a slow disk this long
/// without losing anything.
pub const RECORD_QUEUE_SECONDS: usize = 4;

/// Interleaved channels in the recorded stream: the main bus, left and right.
pub const RECORD_CHANNELS: usize = 2;

struct Shared {
    armed: AtomicBool,
    /// Frames the audio thread had to drop because the queue was full, ever.
    dropped: AtomicU64,
}

/// Audio-thread end: copies the main bus into the queue while armed.
pub struct RecordTap {
    queue: rtrb::Producer<f32>,
    shared: Arc<Shared>,
}

/// Writer-thread end: arms and disarms recording and drains the recorded samples.
pub struct RecordSource {
    queue: rtrb::Consumer<f32>,
    shared: Arc<Shared>,
    sample_rate: u32,
}

/// Create the two ends, with room for [`RECORD_QUEUE_SECONDS`] at `sample_rate`. Not
/// RT-safe.
pub(crate) fn record_link(sample_rate: u32) -> (RecordTap, RecordSource) {
    let capacity = sample_rate as usize * RECORD_QUEUE_SECONDS * RECORD_CHANNELS;
    let (producer, consumer) = rtrb::RingBuffer::new(capacity);
    let shared = Arc::new(Shared {
        armed: AtomicBool::new(false),
        dropped: AtomicU64::new(0),
    });
    let tap = RecordTap {
        queue: producer,
        shared: Arc::clone(&shared),
    };
    let source = RecordSource {
        queue: consumer,
        shared,
        sample_rate,
    };
    (tap, source)
}

impl RecordTap {
    /// Whether to collect the main bus for this callback. Read once per callback, so a
    /// callback records either all of its frames or none.
    pub(crate) fn armed(&self) -> bool {
        self.shared.armed.load(Ordering::Acquire)
    }

    /// Queue `samples` (interleaved stereo, whole frames). RT-safe; never blocks. A
    /// block that doesn't fit is dropped whole, so left and right never swap.
    pub(crate) fn write(&mut self, samples: &[f32]) {
        match self.queue.write_chunk_uninit(samples.len()) {
            Ok(chunk) => {
                chunk.fill_from_iter(samples.iter().copied());
            }
            Err(_) => {
                let frames = (samples.len() / RECORD_CHANNELS) as u64;
                self.shared.dropped.fetch_add(frames, Ordering::Relaxed);
            }
        }
    }
}

impl RecordSource {
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Start or stop collecting the main bus. After disarming, the callback running at
    /// that moment may still queue one more block; drain after it has finished.
    pub fn set_armed(&self, armed: bool) {
        self.shared.armed.store(armed, Ordering::Release);
    }

    pub fn armed(&self) -> bool {
        self.shared.armed.load(Ordering::Acquire)
    }

    /// Frames dropped so far because the queue was full (a running total).
    pub fn dropped_frames(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Move every queued sample into `out` (appending), returning how many were added.
    /// Always whole frames: the audio thread only queues whole blocks.
    pub fn drain_into(&mut self, out: &mut Vec<f32>) -> usize {
        let available = self.queue.slots();
        if available == 0 {
            return 0;
        }
        let Ok(chunk) = self.queue.read_chunk(available) else {
            return 0;
        };
        let (first, second) = chunk.as_slices();
        out.extend_from_slice(first);
        out.extend_from_slice(second);
        chunk.commit_all();
        available
    }

    /// Throw away anything queued (e.g. left over from a previous recording).
    pub fn discard(&mut self) {
        let available = self.queue.slots();
        if let Ok(chunk) = self.queue.read_chunk(available) {
            chunk.commit_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_queue_drops_whole_blocks_and_counts_them() {
        let (mut tap, mut source) = record_link(10);
        // Room for 10 Hz × 4 s × 2 ch = 80 samples.
        tap.write(&[0.5; 60]);
        tap.write(&[0.25; 40]);
        assert_eq!(source.dropped_frames(), 20);
        let mut out = Vec::new();
        assert_eq!(source.drain_into(&mut out), 60);
        assert!(out.iter().all(|&s| s == 0.5));
        tap.write(&[0.25; 40]);
        assert_eq!(source.drain_into(&mut out), 40);
        assert_eq!(out.len(), 100);
    }
}
