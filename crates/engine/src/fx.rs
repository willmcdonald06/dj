//! The beat FX unit: one effect at a time, on deck A, deck B or the main mix, timed in
//! beats.
//!
//! Effects: echo, reverb, filter sweep, flanger and roll (stutter). Their time is a
//! number of beats ([`FX_BEATS`], 1/4 to 16) at the FX tempo, which follows the target
//! deck's beatgrid (for the main mix: the sync master, or the louder playing deck) and
//! can be overridden with TAP. Effects that move in time (the filter sweep, flanger and
//! roll) lock to the deck's beat phase, so a 1-beat sweep peaks on every beat.
//!
//! The unit sits after the channel fader. Echo and reverb are *sends*: switching the
//! FX off stops feeding them, and their tails ring out. The others are *inserts* that
//! fade back to the dry signal. LEVEL is how much effect: the echo or reverb level over
//! the unchanged dry signal, or the dry/wet balance of an insert.
//!
//! Changing the effect or its target while it runs fades the old one out over a few
//! milliseconds first, so nothing clicks.
//!
//! # RT safety
//!
//! [`FxUnit::new`] allocates the delay lines and is not RT-safe; everything else is.

use std::f32::consts::{PI, TAU};

use crate::deck::DeckId;
use crate::dsp::{Smoothed, Svf, SvfCoefficients};

/// Effect lengths, in beats.
pub const FX_BEATS: [f32; 8] = [0.25, 0.5, 0.75, 1.0, 2.0, 4.0, 8.0, 16.0];
/// The FX tempo without a grid or taps.
pub const DEFAULT_FX_BPM: f32 = 120.0;
/// Longest delay the echo and roll can hold.
const MAX_DELAY_SECONDS: f32 = 16.0;
/// Fade time when switching the FX on or off, or changing effect or target.
const SWITCH_SECONDS: f32 = 0.006;
/// Taps further apart than this start a new count.
const TAP_RESET_SECONDS: f64 = 2.0;
const TAPS: usize = 4;
const ECHO_FEEDBACK: f32 = 0.55;
/// How far the dry signal dips under a send effect at full level.
const SEND_DUCK: f32 = 0.4;
/// Echo output gain. A beat-length echo of a steady beat adds up in phase with the
/// music, so its repeats are kept well under the input.
const ECHO_OUTPUT: f32 = 0.6;
const FLANGER_FEEDBACK: f32 = 0.6;
/// Flanger delay sweep, in seconds.
const FLANGER_DELAY: (f32, f32) = (0.0005, 0.004);
/// Filter sweep range, in Hz, and resonance.
const SWEEP_HZ: (f32, f32) = (180.0, 9_000.0);
const SWEEP_Q: f32 = 2.2;
/// Filter coefficients are recomputed every this many frames.
const SWEEP_UPDATE: u32 = 16;
/// Crossfade at the loop point of a roll.
const ROLL_FADE_SECONDS: f32 = 0.003;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FxKind {
    #[default]
    Echo,
    Reverb,
    /// A resonant low-pass swept up and down once per length.
    Filter,
    Flanger,
    /// Repeats the last division of the beat (a loop roll on the FX bus).
    Roll,
}

impl FxKind {
    pub const ALL: [FxKind; 5] = [
        FxKind::Echo,
        FxKind::Reverb,
        FxKind::Filter,
        FxKind::Flanger,
        FxKind::Roll,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            FxKind::Echo => "ECHO",
            FxKind::Reverb => "REVERB",
            FxKind::Filter => "FILTER",
            FxKind::Flanger => "FLANGER",
            FxKind::Roll => "ROLL",
        }
    }

    /// Echo and reverb are sends with tails; the rest are inserts.
    pub const fn has_tail(self) -> bool {
        matches!(self, FxKind::Echo | FxKind::Reverb)
    }

    fn index(self) -> usize {
        match self {
            FxKind::Echo => 0,
            FxKind::Reverb => 1,
            FxKind::Filter => 2,
            FxKind::Flanger => 3,
            FxKind::Roll => 4,
        }
    }

    /// The next (or, for negative `step`, previous) kind, wrapping around.
    pub fn step(self, step: i32) -> Self {
        let n = Self::ALL.len() as i32;
        Self::ALL[(self.index() as i32 + step).rem_euclid(n) as usize]
    }
}

/// What the FX unit processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FxTarget {
    Deck(DeckId),
    #[default]
    Main,
}

/// A control on the FX unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FxControl {
    SetOn(bool),
    ToggleOn,
    SetKind(FxKind),
    /// Next (+1) or previous (-1) effect.
    StepKind(i32),
    SetTarget(FxTarget),
    /// Length in beats; snapped to the nearest of [`FX_BEATS`].
    SetBeats(f32),
    /// Longer (+1) or shorter (-1) along [`FX_BEATS`].
    StepBeats(i32),
    /// Effect level, 0..1.
    SetLevel(f32),
    /// TAP: set the FX tempo from the time between taps.
    Tap,
    /// Forget tapped tempo and follow the decks again.
    AutoTempo,
}

/// FX state for the UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FxSnapshot {
    pub on: bool,
    pub kind: FxKind,
    pub target: FxTarget,
    pub beats: f32,
    pub level: f32,
    /// The FX tempo in use.
    pub bpm: f32,
    /// The tempo comes from TAP rather than a deck.
    pub tapped: bool,
}

impl Default for FxSnapshot {
    fn default() -> Self {
        Self {
            on: false,
            kind: FxKind::default(),
            target: FxTarget::default(),
            beats: 1.0,
            level: 0.5,
            bpm: DEFAULT_FX_BPM,
            tapped: false,
        }
    }
}

/// The tempo and beat phase the FX should follow, from a deck's grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FxClock {
    pub bpm: f32,
    /// Beats since the grid's anchor at the first frame of the block, while the deck
    /// moves (a stopped deck gives its tempo but no phase to follow).
    pub beat: Option<f64>,
}

pub(crate) struct FxUnit {
    sample_rate: f32,
    controls: FxSnapshot,
    /// The kind and target running now; they trail `controls` while switching.
    kind: FxKind,
    target: FxTarget,
    /// Fade for switching kind or target: 1 when settled.
    switch_gain: f32,
    switch_step: f32,
    /// Ramps 0..1 with FX ON: the send into echo and reverb, the mix of inserts.
    on_gain: f32,
    level: Smoothed,
    /// Beats elapsed, advanced every frame and resynced to the deck once per block.
    beat: f64,
    /// Frames counted, for tap timing.
    frame: u64,
    taps: [u64; TAPS],
    tap_count: usize,
    tapped_bpm: Option<f32>,
    echo: Echo,
    reverb: Reverb,
    sweep: Sweep,
    flanger: Flanger,
    roll: Roll,
}

impl FxUnit {
    /// Not RT-safe: allocates about 13 MB of delay lines at 48 kHz.
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        Self {
            sample_rate: sr,
            controls: FxSnapshot::default(),
            kind: FxKind::default(),
            target: FxTarget::default(),
            switch_gain: 1.0,
            switch_step: 1.0 / (SWITCH_SECONDS * sr),
            on_gain: 0.0,
            level: Smoothed::new(sr, FxSnapshot::default().level),
            beat: 0.0,
            frame: 0,
            taps: [0; TAPS],
            tap_count: 0,
            tapped_bpm: None,
            echo: Echo::new(sr),
            reverb: Reverb::new(sr),
            sweep: Sweep::new(sr),
            flanger: Flanger::new(sr),
            roll: Roll::new(sr),
        }
    }

    pub fn apply(&mut self, control: FxControl) {
        let c = &mut self.controls;
        match control {
            FxControl::SetOn(on) => c.on = on,
            FxControl::ToggleOn => c.on = !c.on,
            FxControl::SetKind(kind) => c.kind = kind,
            FxControl::StepKind(step) => c.kind = c.kind.step(step),
            FxControl::SetTarget(target) => c.target = target,
            FxControl::SetBeats(beats) => c.beats = nearest_beats(beats),
            FxControl::StepBeats(step) => {
                let at = FX_BEATS.iter().position(|&b| b == c.beats).unwrap_or(3) as i32;
                let to = (at + step).clamp(0, FX_BEATS.len() as i32 - 1);
                c.beats = FX_BEATS[to as usize];
            }
            FxControl::SetLevel(level) => {
                c.level = level.clamp(0.0, 1.0);
                self.level.set(c.level);
            }
            FxControl::Tap => self.tap(),
            FxControl::AutoTempo => {
                self.tapped_bpm = None;
                self.tap_count = 0;
            }
        }
    }

    fn tap(&mut self) {
        let gap = (TAP_RESET_SECONDS * self.sample_rate as f64) as u64;
        let last = self.taps[(self.tap_count + TAPS - 1) % TAPS];
        if self.tap_count > 0 && self.frame.saturating_sub(last) > gap {
            self.tap_count = 0;
        }
        self.taps[self.tap_count % TAPS] = self.frame;
        self.tap_count += 1;
        // The tap is a beat.
        self.beat = self.beat.round();
        let counted = self.tap_count.min(TAPS);
        if counted >= 2 {
            let first = self.taps[(self.tap_count - counted) % TAPS];
            let interval = (self.frame - first) as f32 / (counted - 1) as f32;
            if interval > 0.0 {
                let bpm = 60.0 * self.sample_rate / interval;
                self.tapped_bpm = Some(bpm.clamp(40.0, 250.0));
            }
        }
    }

    /// Follow `clock` (from the target deck's grid) for the next block, unless the
    /// tempo was tapped. `None`: no deck to follow; keep the current tempo.
    pub fn set_clock(&mut self, clock: Option<FxClock>) {
        if self.tapped_bpm.is_some() {
            return;
        }
        if let Some(clock) = clock {
            self.controls.bpm = clock.bpm.clamp(20.0, 400.0);
            if let Some(beat) = clock.beat {
                self.beat = beat;
            }
        }
    }

    /// The target the next frames should go through: decided by the engine before
    /// mixing each frame.
    pub fn target(&self) -> FxTarget {
        self.target
    }

    pub fn snapshot(&self) -> FxSnapshot {
        FxSnapshot {
            bpm: self.bpm(),
            tapped: self.tapped_bpm.is_some(),
            ..self.controls
        }
    }

    fn bpm(&self) -> f32 {
        self.tapped_bpm.unwrap_or(self.controls.bpm)
    }

    /// Process one frame of the target's signal.
    #[inline]
    pub fn process(&mut self, x: (f32, f32)) -> (f32, f32) {
        self.frame += 1;
        let bpm = self.bpm();
        let beat_frames = 60.0 * self.sample_rate / bpm;
        self.beat += 1.0 / beat_frames as f64;

        // Switching kind or target: fade out, swap (clearing the new effect), fade in.
        let settled = self.kind == self.controls.kind && self.target == self.controls.target;
        if settled {
            self.switch_gain = (self.switch_gain + self.switch_step).min(1.0);
        } else {
            self.switch_gain -= self.switch_step;
            if self.switch_gain <= 0.0 {
                self.switch_gain = 0.0;
                self.kind = self.controls.kind;
                self.target = self.controls.target;
                self.clear(self.kind);
            }
        }
        let on_target = match self.controls.on {
            true => 1.0,
            false => 0.0,
        };
        let delta = on_target - self.on_gain;
        self.on_gain += delta.clamp(-self.switch_step, self.switch_step);
        let level = self.level.tick();
        let length = self.controls.beats * beat_frames;
        // Where we are in the effect's cycle, locked to the beat.
        let phase = (self.beat / self.controls.beats as f64).rem_euclid(1.0) as f32;

        match self.kind {
            FxKind::Echo | FxKind::Reverb => {
                let send = (x.0 * self.on_gain, x.1 * self.on_gain);
                let wet = match self.kind {
                    FxKind::Echo => self.echo.process(send, length),
                    _ => self.reverb.process(send, length / self.sample_rate),
                };
                let gain = level * self.switch_gain;
                // Dip the dry signal as the effect comes up, so the sum stays about
                // as loud (echo and reverb at full level would add ~3 dB and clip a
                // loud track). The dip follows FX ON; the tail does not.
                let dry = 1.0 - SEND_DUCK * level * self.on_gain * self.switch_gain;
                (x.0 * dry + wet.0 * gain, x.1 * dry + wet.1 * gain)
            }
            FxKind::Filter | FxKind::Flanger | FxKind::Roll => {
                let active = self.on_gain > 0.0 || self.roll.engaged;
                if !active {
                    // Keep the roll's history recording so it can start on the beat.
                    self.roll.record(x);
                    return x;
                }
                let wet = match self.kind {
                    FxKind::Filter => self.sweep.process(x, phase),
                    FxKind::Flanger => self.flanger.process(x, phase),
                    _ => {
                        let into = phase * length;
                        self.roll
                            .process(x, self.controls.on, length, into, self.on_gain)
                    }
                };
                let mix = level * self.on_gain * self.switch_gain;
                (x.0 + (wet.0 - x.0) * mix, x.1 + (wet.1 - x.1) * mix)
            }
        }
    }

    fn clear(&mut self, kind: FxKind) {
        match kind {
            FxKind::Echo => self.echo.clear(),
            FxKind::Reverb => self.reverb.clear(),
            FxKind::Filter => self.sweep.clear(),
            FxKind::Flanger => self.flanger.clear(),
            FxKind::Roll => self.roll.release(),
        }
    }
}

fn nearest_beats(beats: f32) -> f32 {
    let mut best = FX_BEATS[0];
    for b in FX_BEATS {
        if (b.ln() - beats.max(0.01).ln()).abs() < (best.ln() - beats.max(0.01).ln()).abs() {
            best = b;
        }
    }
    best
}

/// Write every page of a freshly allocated zeroed buffer. Large zeroed allocations
/// come straight from the OS, which maps their pages lazily, so the audio thread's
/// first write to each would page-fault.
fn prefault<T: Copy>(data: &mut [T]) {
    if let Some(&first) = data.first() {
        data.fill(first);
    }
    std::hint::black_box(data);
}

/// A stereo circular buffer.
struct DelayLine {
    data: Box<[(f32, f32)]>,
    write: usize,
}

impl DelayLine {
    fn new(frames: usize) -> Self {
        let mut data = vec![(0.0, 0.0); frames.max(4)].into_boxed_slice();
        prefault(&mut data);
        Self { data, write: 0 }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn push(&mut self, x: (f32, f32)) {
        self.data[self.write] = x;
        self.write = (self.write + 1) % self.data.len();
    }

    /// The frame written `delay` frames ago (fractional: linear interpolation).
    /// `delay` ≥ 1.
    #[inline]
    fn read(&self, delay: f32) -> (f32, f32) {
        let len = self.data.len();
        let delay = delay.clamp(1.0, (len - 2) as f32);
        let whole = delay.floor();
        let frac = delay - whole;
        let a = self.data[(self.write + len - whole as usize) % len];
        let b = self.data[(self.write + len - whole as usize - 1) % len];
        (a.0 + (b.0 - a.0) * frac, a.1 + (b.1 - a.1) * frac)
    }

    /// The frame at absolute index `index` (wrapped).
    #[inline]
    fn at(&self, index: usize) -> (f32, f32) {
        self.data[index % self.data.len()]
    }

    fn clear(&mut self) {
        self.data.fill((0.0, 0.0));
    }
}

/// One-pole low-pass, for damping feedback loops.
#[derive(Clone, Copy, Default)]
struct OnePole {
    state: f32,
}

impl OnePole {
    #[inline]
    fn process(&mut self, x: f32, coefficient: f32) -> f32 {
        self.state += (x - self.state) * coefficient;
        self.state
    }
}

fn one_pole_coefficient(sample_rate: f32, hz: f32) -> f32 {
    1.0 - (-TAU * hz / sample_rate).exp()
}

/// Tempo-synced echo with darkening repeats.
struct Echo {
    line: DelayLine,
    /// Delay now, gliding to the target (a tempo change bends the repeats like tape).
    delay: f32,
    glide: f32,
    damp: [OnePole; 2],
    low_cut: [OnePole; 2],
    damp_c: f32,
    low_cut_c: f32,
}

impl Echo {
    fn new(sr: f32) -> Self {
        Self {
            line: DelayLine::new((MAX_DELAY_SECONDS * sr) as usize),
            delay: sr / 2.0,
            glide: one_pole_coefficient(sr, 4.0),
            damp: [OnePole::default(); 2],
            low_cut: [OnePole::default(); 2],
            damp_c: one_pole_coefficient(sr, 5_000.0),
            low_cut_c: one_pole_coefficient(sr, 120.0),
        }
    }

    fn clear(&mut self) {
        self.line.clear();
        self.damp = [OnePole::default(); 2];
        self.low_cut = [OnePole::default(); 2];
    }

    #[inline]
    fn process(&mut self, x: (f32, f32), length: f32) -> (f32, f32) {
        let target = length.clamp(1.0, (self.line.len() - 2) as f32);
        // Jump on big changes (a new length), glide on small ones (tempo drift).
        if (target - self.delay).abs() > target * 0.1 {
            self.delay = target;
        } else {
            self.delay += (target - self.delay) * self.glide;
        }
        let wet = self.line.read(self.delay);
        let mut shape = |ch: usize, s: f32| {
            let dark = self.damp[ch].process(s, self.damp_c);
            dark - self.low_cut[ch].process(dark, self.low_cut_c)
        };
        let fed = (shape(0, wet.0), shape(1, wet.1));
        self.line
            .push((x.0 + fed.0 * ECHO_FEEDBACK, x.1 + fed.1 * ECHO_FEEDBACK));
        (wet.0 * ECHO_OUTPUT, wet.1 * ECHO_OUTPUT)
    }
}

/// Freeverb-style reverb: parallel damped combs into series all-passes, per channel.
struct Reverb {
    sample_rate: f32,
    combs: [[Comb; 8]; 2],
    allpasses: [[Allpass; 4]; 2],
    /// Decay time the comb feedback was computed for.
    decay_for: f32,
}

const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;
const REVERB_INPUT: f32 = 0.015;
/// Output gain: brings the wet signal to a few dB under the input.
const REVERB_OUTPUT: f32 = 2.0;
const REVERB_DAMP: f32 = 0.3;

struct Comb {
    buffer: Box<[f32]>,
    index: usize,
    feedback: f32,
    filter: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self {
            buffer: vec![0.0; len.max(1)].into_boxed_slice(),
            index: 0,
            feedback: 0.8,
            filter: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let out = self.buffer[self.index];
        self.filter = out * (1.0 - REVERB_DAMP) + self.filter * REVERB_DAMP;
        self.buffer[self.index] = x + self.filter * self.feedback;
        self.index = (self.index + 1) % self.buffer.len();
        out
    }
}

struct Allpass {
    buffer: Box<[f32]>,
    index: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self {
            buffer: vec![0.0; len.max(1)].into_boxed_slice(),
            index: 0,
        }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let delayed = self.buffer[self.index];
        let out = delayed - x;
        self.buffer[self.index] = x + delayed * 0.5;
        self.index = (self.index + 1) % self.buffer.len();
        out
    }
}

impl Reverb {
    fn new(sr: f32) -> Self {
        let scale = sr / 44_100.0;
        let tuned = |n: usize, spread: usize| ((n + spread) as f32 * scale) as usize;
        Self {
            sample_rate: sr,
            combs: [0, STEREO_SPREAD].map(|s| COMB_TUNING.map(|n| Comb::new(tuned(n, s)))),
            allpasses: [0, STEREO_SPREAD]
                .map(|s| ALLPASS_TUNING.map(|n| Allpass::new(tuned(n, s)))),
            decay_for: 0.0,
        }
    }

    fn clear(&mut self) {
        for comb in self.combs.iter_mut().flatten() {
            comb.buffer.fill(0.0);
            comb.filter = 0.0;
        }
        for allpass in self.allpasses.iter_mut().flatten() {
            allpass.buffer.fill(0.0);
        }
    }

    /// `decay`: seconds for the tail to fall 60 dB (the effect length).
    #[inline]
    fn process(&mut self, x: (f32, f32), decay: f32) -> (f32, f32) {
        let decay = decay.clamp(0.3, 10.0);
        if (decay - self.decay_for).abs() > 0.01 {
            self.decay_for = decay;
            for comb in self.combs.iter_mut().flatten() {
                let seconds = comb.buffer.len() as f32 / self.sample_rate;
                comb.feedback = 10f32.powf(-3.0 * seconds / decay).min(0.98);
            }
        }
        let input = (x.0 + x.1) * REVERB_INPUT;
        let mut out = [0.0f32; 2];
        for (ch, o) in out.iter_mut().enumerate() {
            let mut sum: f32 = self.combs[ch].iter_mut().map(|c| c.process(input)).sum();
            for allpass in &mut self.allpasses[ch] {
                sum = allpass.process(sum);
            }
            *o = sum * REVERB_OUTPUT;
        }
        (out[0], out[1])
    }
}

/// A resonant low-pass swept up and down by a beat-locked LFO.
struct Sweep {
    sample_rate: f32,
    svf: [Svf; 2],
    coefficients: SvfCoefficients,
    countdown: u32,
}

impl Sweep {
    fn new(sr: f32) -> Self {
        Self {
            sample_rate: sr,
            svf: [Svf::default(); 2],
            coefficients: SvfCoefficients::new(sr, SWEEP_HZ.1, SWEEP_Q),
            countdown: 0,
        }
    }

    fn clear(&mut self) {
        self.svf = [Svf::default(); 2];
        self.countdown = 0;
    }

    #[inline]
    fn process(&mut self, x: (f32, f32), phase: f32) -> (f32, f32) {
        if self.countdown == 0 {
            self.countdown = SWEEP_UPDATE;
            // Open on the beat, closed half way through.
            let open = 0.5 + 0.5 * (TAU * phase).cos();
            let hz = SWEEP_HZ.0 * (SWEEP_HZ.1 / SWEEP_HZ.0).powf(open);
            self.coefficients = SvfCoefficients::new(self.sample_rate, hz, SWEEP_Q);
        }
        self.countdown -= 1;
        let l = self.svf[0].process(x.0, &self.coefficients).0;
        let r = self.svf[1].process(x.1, &self.coefficients).0;
        (l, r)
    }
}

/// A beat-locked flanger.
struct Flanger {
    sample_rate: f32,
    line: DelayLine,
}

impl Flanger {
    fn new(sr: f32) -> Self {
        Self {
            sample_rate: sr,
            line: DelayLine::new((FLANGER_DELAY.1 * sr) as usize + 8),
        }
    }

    fn clear(&mut self) {
        self.line.clear();
    }

    #[inline]
    fn process(&mut self, x: (f32, f32), phase: f32) -> (f32, f32) {
        let (lo, hi) = FLANGER_DELAY;
        // Right channel a little behind the left, for width.
        let delay = |offset: f32| {
            let s = 0.5 - 0.5 * (TAU * phase + offset).cos();
            (lo + (hi - lo) * s) * self.sample_rate
        };
        let dl = self.line.read(delay(0.0)).0;
        let dr = self.line.read(delay(PI / 4.0)).1;
        self.line
            .push((x.0 + dl * FLANGER_FEEDBACK, x.1 + dr * FLANGER_FEEDBACK));
        ((x.0 + dl) * 0.5, (x.1 + dr) * 0.5)
    }
}

/// Loop roll on the FX bus. It records continuously, so when switched on it can loop
/// the current division of the beat from its start (on the grid), and its output joins
/// the live signal seamlessly: at the moment it starts, the loop is playing exactly
/// the frame that is live.
struct Roll {
    line: DelayLine,
    /// Absolute index of the frame written next.
    written: usize,
    engaged: bool,
    /// Loop start (absolute index) and length in frames.
    start: usize,
    length: usize,
    /// Frames played since `start`.
    played: usize,
    fade: usize,
}

impl Roll {
    fn new(sr: f32) -> Self {
        Self {
            line: DelayLine::new((MAX_DELAY_SECONDS * sr) as usize),
            written: 0,
            engaged: false,
            start: 0,
            length: 0,
            played: 0,
            fade: (ROLL_FADE_SECONDS * sr) as usize,
        }
    }

    fn release(&mut self) {
        self.engaged = false;
    }

    #[inline]
    fn record(&mut self, x: (f32, f32)) {
        self.line.push(x);
        self.written += 1;
    }

    /// `on`: FX ON; `length`: loop length in frames; `into`: how far into the current
    /// division the playhead is; `on_gain` reaching 0 releases the roll.
    #[inline]
    fn process(
        &mut self,
        x: (f32, f32),
        on: bool,
        length: f32,
        into: f32,
        on_gain: f32,
    ) -> (f32, f32) {
        let max = self.line.len() - self.fade - 2;
        let length = (length as usize).clamp(self.fade * 2 + 1, max);
        if on && !self.engaged {
            // Start on the division's start, already `into` frames in.
            let into = (into as usize).min(length - 1).min(self.written);
            self.engaged = true;
            self.start = self.written - into;
            self.length = length;
            self.played = into;
        } else if self.engaged && length != self.length {
            // A new length while rolling: restart the loop from here.
            self.start = self.written;
            self.length = length;
            self.played = 0;
        }
        if !on && on_gain <= 0.0 {
            self.engaged = false;
        }
        if !self.engaged {
            self.record(x);
            return x;
        }
        // Record the first pass of the loop; after that the buffer is frozen.
        if self.played < self.length {
            self.record(x);
        }
        let position = self.played % self.length;
        self.played += 1;
        let out = self.line.at(self.start + position);
        // Crossfade the end of the loop into what came just before its start.
        let to_end = self.length - position;
        if self.played > self.length && to_end <= self.fade {
            let before = self
                .line
                .at(self.start + self.line.len() + position - self.length);
            let a = 1.0 - to_end as f32 / self.fade as f32;
            return (
                out.0 + (before.0 - out.0) * a,
                out.1 + (before.1 - out.1) * a,
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn unit() -> FxUnit {
        let mut fx = FxUnit::new(SR);
        fx.set_clock(Some(FxClock {
            bpm: 120.0,
            beat: Some(0.0),
        }));
        fx
    }

    fn impulse_response(fx: &mut FxUnit, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                let x = match i {
                    0 => 1.0,
                    _ => 0.0,
                };
                fx.process((x, x)).0
            })
            .collect()
    }

    #[test]
    fn off_passes_the_signal_untouched() {
        for kind in FxKind::ALL {
            let mut fx = unit();
            fx.apply(FxControl::SetKind(kind));
            for i in 0..2000 {
                let x = (i as f32 * 0.01).sin();
                assert_eq!(fx.process((x, -x)), (x, -x), "{kind:?}");
            }
        }
    }

    #[test]
    fn echo_repeats_after_the_beat_length_and_rings_on_after_off() {
        let mut fx = unit();
        fx.apply(FxControl::SetLevel(1.0));
        fx.apply(FxControl::SetBeats(0.5));
        fx.apply(FxControl::SetOn(true));
        // Let the on ramp finish, then send one click.
        for _ in 0..1000 {
            fx.process((0.0, 0.0));
        }
        let out = impulse_response(&mut fx, SR as usize);
        let half_beat = SR as usize / 4; // 120 BPM
        let echo = out[half_beat - 5..half_beat + 5]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(echo > 0.3, "first repeat {echo}");
        assert!(out[half_beat / 2].abs() < 1e-3);
        // Switch off: the tail keeps going.
        fx.apply(FxControl::SetOn(false));
        let tail = impulse_response(&mut fx, SR as usize / 2);
        let energy: f32 = tail.iter().map(|s| s * s).sum();
        assert!(energy > 0.01, "tail energy {energy}");
    }

    #[test]
    fn tap_sets_the_tempo() {
        let mut fx = unit();
        for _ in 0..4 {
            fx.apply(FxControl::Tap);
            for _ in 0..SR / 2 {
                fx.process((0.0, 0.0));
            }
        }
        let s = fx.snapshot();
        assert!(s.tapped);
        assert!((s.bpm - 120.0).abs() < 0.5, "{}", s.bpm);
        // Deck tempo is ignored while tapped, followed again after AUTO.
        fx.set_clock(Some(FxClock {
            bpm: 128.0,
            beat: None,
        }));
        fx.apply(FxControl::Tap);
        assert!((fx.snapshot().bpm - 120.0).abs() < 1.0);
        fx.apply(FxControl::AutoTempo);
        fx.set_clock(Some(FxClock {
            bpm: 128.0,
            beat: None,
        }));
        assert_eq!(fx.snapshot().bpm, 128.0);
    }

    #[test]
    fn beats_step_and_snap() {
        let mut fx = unit();
        fx.apply(FxControl::SetBeats(3.0));
        assert_eq!(fx.snapshot().beats, 4.0);
        fx.apply(FxControl::StepBeats(1));
        assert_eq!(fx.snapshot().beats, 8.0);
        fx.apply(FxControl::StepBeats(10));
        assert_eq!(fx.snapshot().beats, 16.0);
        fx.apply(FxControl::StepBeats(-7));
        assert_eq!(fx.snapshot().beats, 0.25);
        assert_eq!(FxKind::Echo.step(-1), FxKind::Roll);
    }

    #[test]
    fn roll_repeats_the_current_division_seamlessly() {
        let mut fx = unit();
        fx.apply(FxControl::SetKind(FxKind::Roll));
        fx.apply(FxControl::SetLevel(1.0));
        fx.apply(FxControl::SetBeats(0.25));
        let ramp = |i: usize| (i % 100_000) as f32 / 100_000.0;
        let quarter = SR as usize / 8; // 1/4 beat at 120 BPM
        // Run into the second division, then engage: the output stays continuous.
        let mut i = 0;
        while i < quarter + quarter / 2 {
            fx.process((ramp(i), 0.0));
            i += 1;
        }
        fx.apply(FxControl::SetOn(true));
        let mut out = Vec::new();
        for _ in 0..quarter * 3 {
            out.push(fx.process((ramp(i), 0.0)).0);
            i += 1;
        }
        let step = 1.0 / 100_000.0;
        assert!((out[0] - ramp(i - quarter * 3)).abs() < step * 2.0);
        // Once the division ends it starts again from its start.
        let wrap = quarter / 2;
        assert!(
            out[wrap + 1] < out[wrap - 10],
            "{} {}",
            out[wrap + 1],
            out[wrap - 10]
        );
        assert!((out[wrap + quarter] - out[wrap]).abs() < step * 2.0);
        // Off: back to the live signal.
        fx.apply(FxControl::SetOn(false));
        for _ in 0..1000 {
            fx.process((ramp(i), 0.0));
            i += 1;
        }
        assert!((fx.process((ramp(i), 0.0)).0 - ramp(i)).abs() < 1e-6);
    }

    #[test]
    fn changing_kind_fades_instead_of_clicking() {
        let mut fx = unit();
        fx.apply(FxControl::SetKind(FxKind::Filter));
        fx.apply(FxControl::SetLevel(1.0));
        fx.apply(FxControl::SetOn(true));
        let mut last = 0.0f32;
        let mut biggest = 0.0f32;
        for i in 0..SR as usize {
            if i == SR as usize / 2 {
                fx.apply(FxControl::SetKind(FxKind::Flanger));
            }
            let x = 0.5 * (TAU * 100.0 * i as f32 / SR as f32).sin();
            let y = fx.process((x, x)).0;
            if i > 100 {
                biggest = biggest.max((y - last).abs());
            }
            last = y;
        }
        // A 100 Hz sine at 0.5 moves at most ~0.0065 per frame; allow resonance.
        assert!(biggest < 0.05, "largest step {biggest}");
    }
}
