//! Audio callback statistics: xrun counter and callback-duration histogram.
//!
//! Written from the audio thread with relaxed atomics only (RT-safe); read by the UI's
//! debug overlay.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Histogram buckets, each 10% of the period budget wide; the last bucket collects
/// everything at or over 100% (a callback that overran its deadline).
pub const HISTOGRAM_BUCKETS: usize = 11;

#[derive(Debug, Default)]
pub struct CallbackStats {
    callbacks: AtomicU64,
    xruns: AtomicU64,
    other_errors: AtomicU64,
    last_duration_ns: AtomicU64,
    max_duration_ns: AtomicU64,
    last_frames: AtomicU64,
    histogram: [AtomicU64; HISTOGRAM_BUCKETS],
    /// The callback thread's scheduling: 0 not checked yet, 1 real-time, 2 not.
    realtime: std::sync::atomic::AtomicU8,
}

/// A point-in-time copy of [`CallbackStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    pub callbacks: u64,
    pub xruns: u64,
    pub other_errors: u64,
    pub last_duration: Duration,
    pub max_duration: Duration,
    /// Frames in the most recent callback (the actual period size).
    pub last_frames: u64,
    /// Callback count per 10%-of-budget bucket.
    pub histogram: [u64; HISTOGRAM_BUCKETS],
    /// Whether the callback thread runs at real-time priority (`None` until the first
    /// callback has checked).
    pub realtime: Option<bool>,
}

impl CallbackStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// RT-safe.
    pub fn record_callback(&self, elapsed: Duration, frames: usize, sample_rate: u32) {
        let ns = elapsed.as_nanos().min(u64::MAX as u128) as u64;
        self.callbacks.fetch_add(1, Ordering::Relaxed);
        self.last_duration_ns.store(ns, Ordering::Relaxed);
        self.max_duration_ns.fetch_max(ns, Ordering::Relaxed);
        self.last_frames.store(frames as u64, Ordering::Relaxed);

        let budget_ns = (frames as u64 * 1_000_000_000) / u64::from(sample_rate.max(1));
        let bucket = bucket_for(ns, budget_ns);
        self.histogram[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Record whether the callback thread is real-time. RT-safe.
    pub fn record_realtime(&self, realtime: bool) {
        let value = match realtime {
            true => 1,
            false => 2,
        };
        self.realtime.store(value, Ordering::Relaxed);
    }

    /// Called from the stream's error callback.
    pub fn record_error(&self, kind: cpal::ErrorKind) {
        match kind {
            cpal::ErrorKind::Xrun => self.xruns.fetch_add(1, Ordering::Relaxed),
            _ => self.other_errors.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let mut histogram = [0; HISTOGRAM_BUCKETS];
        for (out, bucket) in histogram.iter_mut().zip(&self.histogram) {
            *out = bucket.load(Ordering::Relaxed);
        }
        StatsSnapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
            other_errors: self.other_errors.load(Ordering::Relaxed),
            last_duration: Duration::from_nanos(self.last_duration_ns.load(Ordering::Relaxed)),
            max_duration: Duration::from_nanos(self.max_duration_ns.load(Ordering::Relaxed)),
            last_frames: self.last_frames.load(Ordering::Relaxed),
            histogram,
            realtime: match self.realtime.load(Ordering::Relaxed) {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
        }
    }
}

fn bucket_for(elapsed_ns: u64, budget_ns: u64) -> usize {
    if budget_ns == 0 {
        return HISTOGRAM_BUCKETS - 1;
    }
    let tenths = (elapsed_ns.saturating_mul(10) / budget_ns) as usize;
    tenths.min(HISTOGRAM_BUCKETS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_tenth_of_budget() {
        assert_eq!(bucket_for(0, 1000), 0);
        assert_eq!(bucket_for(99, 1000), 0);
        assert_eq!(bucket_for(100, 1000), 1);
        assert_eq!(bucket_for(999, 1000), 9);
        assert_eq!(bucket_for(1000, 1000), 10);
        assert_eq!(bucket_for(5000, 1000), 10);
    }

    #[test]
    fn records_callbacks_and_errors() {
        let stats = CallbackStats::new();
        // 128 frames at 48 kHz is a 2.67 ms budget; 1 ms is bucket 3.
        stats.record_callback(Duration::from_millis(1), 128, 48_000);
        stats.record_error(cpal::ErrorKind::Xrun);
        stats.record_error(cpal::ErrorKind::DeviceNotAvailable);
        let s = stats.snapshot();
        assert_eq!(s.callbacks, 1);
        assert_eq!(s.xruns, 1);
        assert_eq!(s.other_errors, 1);
        assert_eq!(s.histogram[3], 1);
        assert_eq!(s.last_frames, 128);
    }
}
