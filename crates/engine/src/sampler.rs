//! The sampler: one bank of eight slots shared by both decks' pads, played into the
//! main output.
//!
//! Each slot holds a short decoded sample and plays it as a one-shot (press restarts
//! it), gated (plays while held) or loop (press starts, press again stops). With
//! quantize on and a deck playing, a press starts on the next beat of the main clock.
//! With sync on, loops play at the main tempo: they are sped up or slowed down by the
//! ratio of the tempos (so their pitch follows, as with key lock off). One-shots and
//! gated samples always play at their own speed.
//!
//! # RT safety
//!
//! [`Sampler`] lives on the audio thread and is **RT-safe**: samples arrive as
//! `Arc<Track>`s that the loader also holds (so dropping one here never frees it), and
//! rendering is a bounded loop over the slots per frame.

use std::sync::Arc;

use crate::fx::FxClock;
use crate::track::{Track, TrackId};

pub const SAMPLER_SLOTS: usize = 8;
/// Fade in and out over this long, so starts and stops don't click.
const FADE_SECONDS: f32 = 0.004;
/// A press this soon after a beat counts as on the beat (starts at once).
const LATE_BEAT: f64 = 1.0 / 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleMode {
    /// Plays to the end; a press while playing restarts it.
    #[default]
    OneShot,
    /// Plays while the pad is held.
    Gated,
    /// Repeats until pressed again.
    Loop,
}

impl SampleMode {
    pub const ALL: [SampleMode; 3] = [SampleMode::OneShot, SampleMode::Gated, SampleMode::Loop];

    pub fn next(self) -> Self {
        match self {
            SampleMode::OneShot => SampleMode::Gated,
            SampleMode::Gated => SampleMode::Loop,
            SampleMode::Loop => SampleMode::OneShot,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SampleMode::OneShot => "ONE SHOT",
            SampleMode::Gated => "GATE",
            SampleMode::Loop => "LOOP",
        }
    }
}

/// Sampler controls, sent as [`Command::Sampler`](crate::Command::Sampler).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SamplerControl {
    Press(usize),
    Release(usize),
    Stop(usize),
    StopAll,
    /// Remove the slot's sample.
    Eject(usize),
    SetMode(usize, SampleMode),
    CycleMode(usize),
    /// 0..1.
    SetLevel(f32),
    SetSync(bool),
    ToggleSync,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SlotSnapshot {
    /// The loaded sample, if any.
    pub sample: Option<TrackId>,
    pub mode: SampleMode,
    pub playing: bool,
    /// Pressed and waiting for the next beat.
    pub waiting: bool,
    /// How far through the sample, 0..1.
    pub progress: f32,
    /// The sample's own tempo, if known.
    pub bpm: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SamplerSnapshot {
    pub slots: [SlotSnapshot; SAMPLER_SLOTS],
    pub level: f32,
    pub sync: bool,
}

impl Default for SamplerSnapshot {
    fn default() -> Self {
        Self {
            slots: [SlotSnapshot::default(); SAMPLER_SLOTS],
            level: DEFAULT_LEVEL,
            sync: true,
        }
    }
}

pub const DEFAULT_LEVEL: f32 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Play {
    Idle,
    /// Starts after this many more frames.
    Waiting(f64),
    Playing,
}

struct Slot {
    sample: Option<Arc<Track>>,
    bpm: Option<f32>,
    mode: SampleMode,
    play: Play,
    /// Frame position in the sample.
    position: f64,
    /// Fade gain, moving towards `target`.
    gain: f32,
    target: f32,
}

impl Slot {
    fn new() -> Self {
        Self {
            sample: None,
            bpm: None,
            mode: SampleMode::OneShot,
            play: Play::Idle,
            position: 0.0,
            gain: 0.0,
            target: 0.0,
        }
    }

    fn len(&self) -> u64 {
        self.sample.as_ref().map_or(0, |s| s.ready_frames())
    }

    fn start(&mut self, wait: f64) {
        self.position = 0.0;
        // Restarting a playing sample: a quick dip rather than a jump.
        self.gain = 0.0;
        self.target = 1.0;
        self.play = match wait > 0.0 {
            true => Play::Waiting(wait),
            false => Play::Playing,
        };
    }

    fn stop(&mut self) {
        match self.play {
            Play::Waiting(_) => self.play = Play::Idle,
            // Fades out; goes idle when the fade ends.
            Play::Playing => self.target = 0.0,
            Play::Idle => {}
        }
    }
}

pub struct Sampler {
    slots: [Slot; SAMPLER_SLOTS],
    level: f32,
    sync: bool,
    quantize: bool,
    clock: Option<FxClock>,
    sample_rate: f32,
    fade_step: f32,
}

impl Sampler {
    pub fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate as f32;
        Self {
            slots: std::array::from_fn(|_| Slot::new()),
            level: DEFAULT_LEVEL,
            sync: true,
            quantize: false,
            clock: None,
            sample_rate,
            fade_step: 1.0 / (FADE_SECONDS * sample_rate).max(1.0),
        }
    }

    /// Put a sample into `slot` (stopping what was there). The caller keeps a reference
    /// to the track.
    pub fn load(&mut self, slot: usize, track: Arc<Track>, bpm: Option<f32>) {
        if let Some(s) = self.slots.get_mut(slot) {
            s.sample = Some(track);
            s.bpm = bpm;
            s.play = Play::Idle;
            s.gain = 0.0;
            s.target = 0.0;
        }
    }

    /// The tempo and beat to start on and sync to, set once per callback.
    pub fn set_clock(&mut self, clock: Option<FxClock>, quantize: bool) {
        self.clock = clock;
        self.quantize = quantize;
    }

    pub fn apply(&mut self, control: SamplerControl) {
        match control {
            SamplerControl::Press(i) => self.press(i),
            SamplerControl::Release(i) => {
                if let Some(slot) = self.slots.get_mut(i) {
                    if slot.mode == SampleMode::Gated {
                        slot.stop();
                    }
                }
            }
            SamplerControl::Stop(i) => {
                if let Some(slot) = self.slots.get_mut(i) {
                    slot.stop();
                }
            }
            SamplerControl::StopAll => self.slots.iter_mut().for_each(Slot::stop),
            SamplerControl::Eject(i) => {
                if let Some(slot) = self.slots.get_mut(i) {
                    // The loader holds the track too, so this never frees it here.
                    *slot = Slot {
                        mode: slot.mode,
                        ..Slot::new()
                    };
                }
            }
            SamplerControl::SetMode(i, mode) => {
                if let Some(slot) = self.slots.get_mut(i) {
                    slot.mode = mode;
                }
            }
            SamplerControl::CycleMode(i) => {
                if let Some(slot) = self.slots.get_mut(i) {
                    slot.mode = slot.mode.next();
                }
            }
            SamplerControl::SetLevel(level) => self.level = level.clamp(0.0, 1.0),
            SamplerControl::SetSync(on) => self.sync = on,
            SamplerControl::ToggleSync => self.sync = !self.sync,
        }
    }

    fn press(&mut self, i: usize) {
        let wait = self.frames_to_next_beat();
        let Some(slot) = self.slots.get_mut(i) else {
            return;
        };
        if slot.sample.is_none() {
            return;
        }
        let active = matches!(slot.play, Play::Waiting(_))
            || (slot.play == Play::Playing && slot.target > 0.0);
        match (slot.mode, active) {
            // A second press stops a loop.
            (SampleMode::Loop, true) => slot.stop(),
            _ => slot.start(wait),
        }
    }

    /// With quantize on and a deck playing: frames until the next beat (0 just after
    /// one). Otherwise 0.
    fn frames_to_next_beat(&self) -> f64 {
        let Some(clock) = self.clock.filter(|_| self.quantize) else {
            return 0.0;
        };
        let Some(beat) = clock.beat else {
            return 0.0;
        };
        if clock.bpm <= 0.0 {
            return 0.0;
        }
        let phase = beat - beat.floor();
        if phase < LATE_BEAT {
            return 0.0;
        }
        let frames_per_beat = 60.0 / clock.bpm as f64 * self.sample_rate as f64;
        (1.0 - phase) * frames_per_beat
    }

    /// Playback speed of `slot`: the tempo ratio for synced loops, else 1.
    fn rate(&self, slot: &Slot) -> f64 {
        match (slot.mode, self.sync, slot.bpm, self.clock) {
            (SampleMode::Loop, true, Some(bpm), Some(clock)) if bpm > 0.0 => {
                (clock.bpm as f64 / bpm as f64).clamp(0.5, 2.0)
            }
            _ => 1.0,
        }
    }

    /// The next stereo frame of all playing slots, scaled by the level. RT-safe.
    #[inline]
    pub fn next_frame(&mut self) -> (f32, f32) {
        let mut out = (0.0f32, 0.0f32);
        for i in 0..SAMPLER_SLOTS {
            let rate = self.rate(&self.slots[i]);
            let step = self.fade_step;
            let slot = &mut self.slots[i];
            match slot.play {
                Play::Idle => continue,
                Play::Waiting(frames) => {
                    match frames <= 1.0 {
                        true => slot.play = Play::Playing,
                        false => slot.play = Play::Waiting(frames - 1.0),
                    }
                    continue;
                }
                Play::Playing => {}
            }
            let Some(sample) = &slot.sample else {
                slot.play = Play::Idle;
                continue;
            };
            let len = sample.ready_frames();
            if len < 2 {
                slot.play = Play::Idle;
                continue;
            }
            let index = slot.position as u64;
            let frac = (slot.position - index as f64) as f32;
            let next = match index + 1 < len {
                true => index + 1,
                false => match slot.mode {
                    SampleMode::Loop => 0,
                    _ => index,
                },
            };
            let a = sample.frame_unchecked_ready(index);
            let b = sample.frame_unchecked_ready(next);
            let left = a.0 + (b.0 - a.0) * frac;
            let right = a.1 + (b.1 - a.1) * frac;
            out.0 += left * slot.gain;
            out.1 += right * slot.gain;

            slot.gain = match slot.gain < slot.target {
                true => (slot.gain + step).min(slot.target),
                false => (slot.gain - step).max(slot.target),
            };
            slot.position += rate;
            if slot.position >= len as f64 {
                match slot.mode {
                    SampleMode::Loop => slot.position -= len as f64,
                    _ => slot.play = Play::Idle,
                }
            }
            if slot.target == 0.0 && slot.gain == 0.0 {
                slot.play = Play::Idle;
            }
        }
        (out.0 * self.level, out.1 * self.level)
    }

    pub fn snapshot(&self) -> SamplerSnapshot {
        let mut snapshot = SamplerSnapshot {
            slots: [SlotSnapshot::default(); SAMPLER_SLOTS],
            level: self.level,
            sync: self.sync,
        };
        for (out, slot) in snapshot.slots.iter_mut().zip(&self.slots) {
            let len = slot.len();
            *out = SlotSnapshot {
                sample: slot.sample.as_ref().map(|s| s.id()),
                mode: slot.mode,
                playing: slot.play == Play::Playing && slot.target > 0.0,
                waiting: matches!(slot.play, Play::Waiting(_)),
                progress: match len {
                    0 => 0.0,
                    len => (slot.position / len as f64) as f32,
                },
                bpm: slot.bpm,
            };
        }
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// A sample of `frames` frames, each frame's value its index / frames.
    fn ramp(frames: usize) -> Arc<Track> {
        let (track, mut writer) = Track::new(TrackId(1), SR, frames);
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = i as f32 / frames as f32 + 0.1;
                [v, v]
            })
            .collect();
        writer.push_interleaved(&samples);
        writer.finish();
        track
    }

    fn run(sampler: &mut Sampler, frames: usize) -> Vec<f32> {
        (0..frames).map(|_| sampler.next_frame().0).collect()
    }

    #[test]
    fn one_shot_plays_once_and_stops() {
        let mut s = Sampler::new(SR);
        s.apply(SamplerControl::SetLevel(1.0));
        s.load(0, ramp(1000), None);
        s.apply(SamplerControl::Press(0));
        let out = run(&mut s, 1500);
        assert!(out[500] > 0.5, "playing in the middle");
        assert!(out[1100..].iter().all(|&v| v == 0.0), "stopped at the end");
        assert!(!s.snapshot().slots[0].playing);
    }

    #[test]
    fn gated_stops_on_release_with_a_fade() {
        let mut s = Sampler::new(SR);
        s.apply(SamplerControl::SetMode(0, SampleMode::Gated));
        s.load(0, ramp(48_000), None);
        s.apply(SamplerControl::Press(0));
        run(&mut s, 1000);
        s.apply(SamplerControl::Release(0));
        let out = run(&mut s, 1000);
        assert!(out[0] > 0.0, "fades rather than cuts");
        assert!(out[500..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn loop_repeats_until_pressed_again() {
        let mut s = Sampler::new(SR);
        s.apply(SamplerControl::SetMode(0, SampleMode::Loop));
        s.load(0, ramp(1000), None);
        s.apply(SamplerControl::Press(0));
        let out = run(&mut s, 3500);
        assert!(out[2500] > 0.0, "still playing after three passes");
        s.apply(SamplerControl::Press(0));
        let out = run(&mut s, 1000);
        assert!(out[500..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn quantized_press_waits_for_the_next_beat() {
        let mut s = Sampler::new(SR);
        s.load(0, ramp(48_000), None);
        // 120 BPM: a beat every 24 000 frames; we're half way through one.
        let clock = FxClock {
            bpm: 120.0,
            beat: Some(10.5),
        };
        s.set_clock(Some(clock), true);
        s.apply(SamplerControl::Press(0));
        assert!(s.snapshot().slots[0].waiting);
        let out = run(&mut s, 12_100);
        assert!(
            out[..11_990].iter().all(|&v| v == 0.0),
            "silent until the beat"
        );
        assert!(out[12_050] > 0.0, "started on the beat");
    }

    #[test]
    fn synced_loops_follow_the_main_tempo_but_one_shots_dont() {
        let mut s = Sampler::new(SR);
        let clock = FxClock {
            bpm: 130.0,
            beat: Some(0.0),
        };
        s.set_clock(Some(clock), false);
        s.apply(SamplerControl::SetMode(0, SampleMode::Loop));
        s.load(0, ramp(1000), Some(120.0));
        s.load(1, ramp(1000), Some(120.0));
        assert!((s.rate(&s.slots[0]) - 130.0 / 120.0).abs() < 1e-9);
        assert_eq!(s.rate(&s.slots[1]), 1.0);
        s.apply(SamplerControl::SetSync(false));
        assert_eq!(s.rate(&s.slots[0]), 1.0);
    }

    #[test]
    fn empty_slots_and_bad_indexes_are_ignored() {
        let mut s = Sampler::new(SR);
        s.apply(SamplerControl::Press(3));
        s.apply(SamplerControl::Press(99));
        assert!(run(&mut s, 100).iter().all(|&v| v == 0.0));
        s.load(2, ramp(100), None);
        s.apply(SamplerControl::Eject(2));
        assert_eq!(s.snapshot().slots[2].sample, None);
    }
}
