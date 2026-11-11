//! Stems: a track's vocals, melody, bass and drums, muted and unmuted live.
//!
//! A [`StemSet`] holds the four stems of one loaded track, compressed with
//! [`stem_codec`](crate::stem_codec). The loader builds it off the audio thread and
//! hands it over like a track; the deck decodes blocks on demand into a small cache.
//!
//! # Playing stems by subtraction
//!
//! The deck plays the full mix as usual and *subtracts* each muted stem from it:
//! `out = mix − Σ (1 − gainᵢ) · stemᵢ`. With every stem on, you hear the original file
//! untouched (separation never colours a track you aren't using stems on), only muted
//! stems are decoded (so the usual case costs nothing), and a stem fades in or out
//! smoothly. Muting everything leaves the separator's residual, the small part of the
//! mix no stem claimed, which is usually inaudible under the next track.
//!
//! Stems are applied to the source frames, before time-stretching, so key lock costs
//! the same with stems as without.
//!
//! # RT safety
//!
//! [`StemSet`] reads and everything on [`StemPlayer`] except `new` are **RT-safe**:
//! the cache is preallocated and each miss decodes one fixed-size block. Dropping a
//! `StemSet` frees memory, so the loader keeps a reference to every set it hands over,
//! as for tracks.

use std::sync::Arc;

use crate::stem_codec::{BLOCK_FRAMES, EncodedStem};
use crate::track::TrackId;

/// Stems per track.
pub const STEM_COUNT: usize = 4;
/// Fade time when a stem is muted or unmuted.
pub const STEM_RAMP_SECONDS: f32 = 0.012;
/// Decoded blocks cached per stem: enough for an interpolation window across a block
/// boundary at both ends of a loop.
const CACHE_SLOTS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stem {
    Vocals,
    Melody,
    Bass,
    Drums,
}

impl Stem {
    /// In storage order (the order of channel pairs in a stems file).
    pub const ALL: [Stem; STEM_COUNT] = [Stem::Vocals, Stem::Melody, Stem::Bass, Stem::Drums];

    pub const fn index(self) -> usize {
        match self {
            Stem::Vocals => 0,
            Stem::Melody => 1,
            Stem::Bass => 2,
            Stem::Drums => 3,
        }
    }

    /// Short upper-case label for buttons.
    pub const fn label(self) -> &'static str {
        match self {
            Stem::Vocals => "VOCAL",
            Stem::Melody => "MELODY",
            Stem::Bass => "BASS",
            Stem::Drums => "DRUMS",
        }
    }
}

/// The four stems of one loaded track.
#[derive(Debug)]
pub struct StemSet {
    track: TrackId,
    stems: [EncodedStem; STEM_COUNT],
    /// Track frame `f` lines up with stem frame `f + offset`.
    offset: i64,
}

impl StemSet {
    /// `offset`: stem frames ahead of the track (see [`StemSet::offset`]).
    pub fn new(track: TrackId, stems: [EncodedStem; STEM_COUNT], offset: i64) -> Self {
        Self {
            track,
            stems,
            offset,
        }
    }

    pub fn track(&self) -> TrackId {
        self.track
    }

    /// The same stems shifted to a new offset.
    pub fn with_offset(self, offset: i64) -> Self {
        Self { offset, ..self }
    }

    /// Track frame `f` lines up with stem frame `f + offset`.
    pub fn offset(&self) -> i64 {
        self.offset
    }

    pub fn stem(&self, stem: Stem) -> &EncodedStem {
        &self.stems[stem.index()]
    }

    /// Memory used by the compressed stems.
    pub fn compressed_bytes(&self) -> usize {
        self.stems.iter().map(EncodedStem::compressed_bytes).sum()
    }

    pub fn frames(&self) -> u64 {
        self.stems
            .iter()
            .map(EncodedStem::frames)
            .max()
            .unwrap_or(0)
    }
}

/// Decoded blocks of one stem. RT-safe after creation.
pub struct StemCache {
    blocks: [i64; CACHE_SLOTS],
    lengths: [usize; CACHE_SLOTS],
    used: [u64; CACHE_SLOTS],
    data: Box<[f32]>,
    clock: u64,
    last: usize,
}

impl Default for StemCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StemCache {
    /// Not RT-safe: allocates the cache.
    pub fn new() -> Self {
        Self {
            blocks: [-1; CACHE_SLOTS],
            lengths: [0; CACHE_SLOTS],
            used: [0; CACHE_SLOTS],
            data: vec![0.0; CACHE_SLOTS * BLOCK_FRAMES * 2].into_boxed_slice(),
            clock: 0,
            last: 0,
        }
    }

    pub fn clear(&mut self) {
        self.blocks = [-1; CACHE_SLOTS];
    }

    /// Stem frame `frame` (silence outside the stem). RT-safe; a cache miss decodes one
    /// block.
    #[inline]
    pub fn frame(&mut self, stem: &EncodedStem, frame: i64) -> (f32, f32) {
        if frame < 0 {
            return (0.0, 0.0);
        }
        let block = frame / BLOCK_FRAMES as i64;
        let within = (frame % BLOCK_FRAMES as i64) as usize;
        let slot = match self.blocks[self.last] == block {
            true => self.last,
            false => self.find_or_decode(stem, block),
        };
        if within >= self.lengths[slot] {
            return (0.0, 0.0);
        }
        let base = slot * BLOCK_FRAMES * 2 + within * 2;
        (self.data[base], self.data[base + 1])
    }

    fn find_or_decode(&mut self, stem: &EncodedStem, block: i64) -> usize {
        self.clock += 1;
        let slot = match self.blocks.iter().position(|&b| b == block) {
            Some(slot) => slot,
            None => {
                let mut oldest = 0;
                for slot in 1..CACHE_SLOTS {
                    if self.used[slot] < self.used[oldest] {
                        oldest = slot;
                    }
                }
                let out =
                    &mut self.data[oldest * BLOCK_FRAMES * 2..(oldest + 1) * BLOCK_FRAMES * 2];
                self.lengths[oldest] = stem.block(block as usize, out);
                self.blocks[oldest] = block;
                oldest
            }
        };
        self.used[slot] = self.clock;
        self.last = slot;
        slot
    }
}

/// Mute state of a deck's stems, as the UI sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StemsSnapshot {
    /// Stems are loaded for the current track.
    pub available: bool,
    pub muted: [bool; STEM_COUNT],
}

/// A deck's stems: the loaded set, mute state, gain ramps and decode caches.
pub(crate) struct StemPlayer {
    set: Option<Arc<StemSet>>,
    muted: [bool; STEM_COUNT],
    /// Current gain of each stem (1 = playing, 0 = muted).
    gain: [f32; STEM_COUNT],
    /// Gain at the start and end of the frames being read now.
    from: [f32; STEM_COUNT],
    to: [f32; STEM_COUNT],
    /// Some stem is (or is fading) below full gain: subtraction is needed.
    engaged: bool,
    step: f32,
    caches: [StemCache; STEM_COUNT],
}

impl StemPlayer {
    /// Not RT-safe: allocates the caches.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            set: None,
            muted: [false; STEM_COUNT],
            gain: [1.0; STEM_COUNT],
            from: [1.0; STEM_COUNT],
            to: [1.0; STEM_COUNT],
            engaged: false,
            step: 1.0 / (STEM_RAMP_SECONDS * sample_rate as f32).max(1.0),
            caches: std::array::from_fn(|_| StemCache::new()),
        }
    }

    /// A new track is loading: drop the old stems and unmute everything, ready for the
    /// new track's stems.
    pub fn reset(&mut self) {
        self.set = None;
        self.muted = [false; STEM_COUNT];
        self.gain = [1.0; STEM_COUNT];
        self.from = self.gain;
        self.to = self.gain;
        self.engaged = false;
        for cache in &mut self.caches {
            cache.clear();
        }
    }

    /// Stems for the loaded track arrived. Stems muted while they were loading fade
    /// out from here.
    pub fn attach(&mut self, set: Arc<StemSet>) {
        self.set = Some(set);
        self.gain = [1.0; STEM_COUNT];
        for cache in &mut self.caches {
            cache.clear();
        }
    }

    pub fn track(&self) -> Option<TrackId> {
        self.set.as_ref().map(|set| set.track())
    }

    pub fn set_muted(&mut self, stem: Stem, muted: bool) {
        self.muted[stem.index()] = muted;
    }

    pub fn is_muted(&self, stem: Stem) -> bool {
        self.muted[stem.index()]
    }

    /// Mute every stem but `stem`; if that is already how it is, unmute them all.
    pub fn solo(&mut self, stem: Stem) {
        let alone = Stem::ALL
            .iter()
            .all(|&other| self.is_muted(other) == (other != stem));
        for other in Stem::ALL {
            let muted = !alone && other != stem;
            self.set_muted(other, muted);
        }
    }

    pub fn snapshot(&self) -> StemsSnapshot {
        StemsSnapshot {
            available: self.set.is_some(),
            muted: self.muted,
        }
    }

    /// Advance the gain ramps over the next `frames` output frames. Call once before
    /// reading the source frames for them.
    pub fn begin(&mut self, frames: usize) {
        self.from = self.gain;
        let span = self.step * frames as f32;
        for (gain, &muted) in self.gain.iter_mut().zip(&self.muted) {
            let target = match muted {
                true => 0.0,
                false => 1.0,
            };
            let delta = target - *gain;
            if delta.abs() <= span {
                *gain = target;
            } else {
                *gain += span * delta.signum();
            }
        }
        self.to = self.gain;
        self.engaged = self.set.is_some() && self.from.iter().chain(&self.to).any(|&g| g < 1.0);
    }

    /// Track frame `frame` of the mix (`mix`) with the muted stems taken out. `t` is how
    /// far through the current ramp (0..1) this frame is heard.
    #[inline]
    pub fn apply(&mut self, frame: i64, t: f32, mix: (f32, f32)) -> (f32, f32) {
        if !self.engaged {
            return mix;
        }
        let Some(set) = &self.set else {
            return mix;
        };
        let (mut l, mut r) = mix;
        let at = frame + set.offset;
        for s in 0..STEM_COUNT {
            let gain = self.from[s] + (self.to[s] - self.from[s]) * t;
            let cut = 1.0 - gain;
            if cut <= 0.0 {
                continue;
            }
            let (sl, sr) = self.caches[s].frame(&set.stems[s], at);
            l -= cut * sl;
            r -= cut * sr;
        }
        (l, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stem_codec::StemEncoder;

    fn encoded(samples: &[f32]) -> EncodedStem {
        let mut encoder = StemEncoder::new(1.0);
        encoder.push(samples, samples);
        encoder.finish()
    }

    /// Four stems, each a constant level, so the mix is their sum.
    fn set(offset: i64) -> Arc<StemSet> {
        let n = BLOCK_FRAMES * 3;
        let stems = [0.1, 0.2, 0.3, 0.4].map(|level| encoded(&vec![level; n]));
        Arc::new(StemSet::new(TrackId(1), stems, offset))
    }

    #[test]
    fn unmuted_stems_leave_the_mix_alone() {
        let mut player = StemPlayer::new(48_000);
        player.attach(set(0));
        player.begin(64);
        assert_eq!(player.apply(10, 0.0, (1.0, 1.0)), (1.0, 1.0));
    }

    #[test]
    fn muting_fades_the_stem_out_of_the_mix() {
        let mut player = StemPlayer::new(48_000);
        player.attach(set(0));
        player.set_muted(Stem::Bass, true);
        let ramp_frames = (STEM_RAMP_SECONDS * 48_000.0) as usize;
        let mut last = 1.0;
        for block in 0..(ramp_frames / 64 + 2) {
            player.begin(64);
            let (l, _) = player.apply(block as i64 * 64, 1.0, (1.0, 1.0));
            assert!(l <= last + 1e-6 && last - l < 0.3 * 64.0 / ramp_frames as f32 + 1e-3);
            last = l;
        }
        assert!((last - 0.7).abs() < 1e-3, "{last}");
        // Past the end of the stems nothing is subtracted.
        assert_eq!(
            player.apply(BLOCK_FRAMES as i64 * 5, 1.0, (1.0, 1.0)),
            (1.0, 1.0)
        );
        player.reset();
        player.begin(64);
        assert_eq!(player.apply(10, 1.0, (1.0, 1.0)), (1.0, 1.0));
        assert!(!player.is_muted(Stem::Bass));
    }

    #[test]
    fn solo_toggles_between_one_stem_and_all() {
        let mut player = StemPlayer::new(48_000);
        player.solo(Stem::Vocals);
        assert_eq!(player.snapshot().muted, [false, true, true, true]);
        player.solo(Stem::Vocals);
        assert_eq!(player.snapshot().muted, [false; 4]);
        player.set_muted(Stem::Vocals, true);
        player.solo(Stem::Drums);
        assert_eq!(player.snapshot().muted, [true, true, true, false]);
    }

    #[test]
    fn offset_shifts_the_stems_against_the_track() {
        let mut player = StemPlayer::new(48_000);
        player.attach(set(-100));
        player.set_muted(Stem::Drums, true);
        for _ in 0..100 {
            player.begin(64);
        }
        // Track frame 50 is stem frame -50: before the stems start.
        assert_eq!(player.apply(50, 1.0, (1.0, 1.0)), (1.0, 1.0));
        let (l, _) = player.apply(150, 1.0, (1.0, 1.0));
        assert!((l - 0.6).abs() < 1e-3);
    }

    #[test]
    fn cache_survives_jumping_between_blocks() {
        let n = BLOCK_FRAMES * 8;
        let ramp: Vec<f32> = (0..n).map(|i| i as f32 / n as f32).collect();
        let stem = encoded(&ramp);
        let mut cache = StemCache::new();
        for &frame in &[0, 5000, 1, 8000, 1023, 1024, 7000, 0, n as i64 - 1] {
            let (l, _) = cache.frame(&stem, frame);
            let want = (ramp[frame as usize] * 32767.0).round() * (1.0 / 32767.0);
            assert!((l - want).abs() < 1e-6, "frame {frame}");
        }
        assert_eq!(cache.frame(&stem, n as i64), (0.0, 0.0));
        assert_eq!(cache.frame(&stem, -1), (0.0, 0.0));
    }
}
