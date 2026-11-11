//! Hot cues and loops. RT-safe (plain data).

/// Hot cue slots per deck.
pub const HOT_CUES: usize = 8;
/// Shortest loop, in frames (about 5 ms at 48 kHz): shorter ones would wrap more than
/// once per render sub-block.
pub const MIN_LOOP_FRAMES: f64 = 256.0;

/// A loop between two frames of the track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopRegion {
    pub start: f64,
    pub end: f64,
}

impl LoopRegion {
    /// A region of at least [`MIN_LOOP_FRAMES`], or `None` if `end` isn't after `start`.
    pub fn new(start: f64, end: f64) -> Option<Self> {
        match end - start >= MIN_LOOP_FRAMES {
            true => Some(Self { start, end }),
            false => None,
        }
    }

    pub fn len(&self) -> f64 {
        self.end - self.start
    }

    pub fn contains(&self, frame: f64) -> bool {
        (self.start..self.end).contains(&frame)
    }
}

/// One hot cue slot: empty, a point to jump to, or a saved loop.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum HotCue {
    #[default]
    Empty,
    Point(f64),
    Loop(LoopRegion),
}

impl HotCue {
    pub fn is_set(&self) -> bool {
        !matches!(self, HotCue::Empty)
    }

    /// Where playback goes when the cue is triggered.
    pub fn position(&self) -> Option<f64> {
        match self {
            HotCue::Empty => None,
            HotCue::Point(frame) => Some(*frame),
            HotCue::Loop(region) => Some(region.start),
        }
    }
}

/// Loop wraps the time-stretcher's input has made that the heard playhead hasn't caught
/// up with yet: the stretcher reads ahead of what you hear by its latency, so the
/// playhead must wrap later, at the same point, even if the loop was exited meanwhile.
#[derive(Debug, Clone, Copy)]
pub struct PendingWraps {
    wraps: [(f64, f64); 64],
    count: usize,
}

impl Default for PendingWraps {
    fn default() -> Self {
        Self {
            wraps: [(0.0, 0.0); 64],
            count: 0,
        }
    }
}

impl PendingWraps {
    /// Record a wrap at `end` back by `len` frames. Drops it if full (it can't be: the
    /// shortest loop wraps fewer than 64 times within the stretcher's latency).
    pub fn push(&mut self, end: f64, len: f64) {
        if self.count < self.wraps.len() {
            self.wraps[self.count] = (end, len);
            self.count += 1;
        }
    }

    pub fn front(&self) -> Option<(f64, f64)> {
        match self.count {
            0 => None,
            _ => Some(self.wraps[0]),
        }
    }

    pub fn pop(&mut self) {
        if self.count > 0 {
            self.wraps.copy_within(1..self.count, 0);
            self.count -= 1;
        }
    }

    pub fn clear(&mut self) {
        self.count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_have_a_minimum_length() {
        assert!(LoopRegion::new(0.0, 100.0).is_none());
        assert!(LoopRegion::new(100.0, 0.0).is_none());
        let region = LoopRegion::new(1000.0, 3000.0).unwrap();
        assert_eq!(region.len(), 2000.0);
        assert!(region.contains(1000.0) && !region.contains(3000.0));
    }

    #[test]
    fn pending_wraps_are_first_in_first_out() {
        let mut wraps = PendingWraps::default();
        wraps.push(10.0, 5.0);
        wraps.push(20.0, 5.0);
        assert_eq!(wraps.front(), Some((10.0, 5.0)));
        wraps.pop();
        assert_eq!(wraps.front(), Some((20.0, 5.0)));
        wraps.pop();
        assert_eq!(wraps.front(), None);
    }
}
