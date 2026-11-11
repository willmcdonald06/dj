//! The internal mixer: per-channel trim, 3-band EQ, filter, fader and headphone cue
//! (PFL), the beat FX unit (after the faders), plus main and headphone buses.
//!
//! Control values arrive normalised, the way a controller sends them (knobs 0..1 with
//! 0.5 at the centre detent, the filter -1..1, faders 0..1). The curves that turn them
//! into gains live here, so every input device behaves the same.
//!
//! # RT safety
//!
//! **RT-safe**: fixed state, no allocation.

use crate::deck::DeckId;
use crate::dsp::{IsolatorEq, Smoothed, Svf, SvfCoefficients};
use crate::fx::{FxClock, FxControl, FxSnapshot, FxTarget, FxUnit};

/// Boost at the top of the EQ knobs.
pub const EQ_MAX_DB: f32 = 6.0;
/// Boost at the top of the trim knob.
pub const TRIM_MAX_DB: f32 = 9.0;
/// Filter knob dead zone around the centre, as a fraction of each half.
pub const FILTER_DEAD_ZONE: f32 = 0.02;
const FILTER_Q: f32 = 1.0;
/// Low-pass sweep from (knob just left of centre) to (knob fully left).
const FILTER_LP_HZ: (f32, f32) = (20_000.0, 60.0);
/// High-pass sweep from (just right of centre) to (fully right).
const FILTER_HP_HZ: (f32, f32) = (20.0, 8_000.0);
/// Part of each filter half over which the dry signal crossfades into the filter.
const FILTER_BLEND: f32 = 0.05;

/// A mixer control on one channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelControl {
    /// 0..1, centre 0.5 = 0 dB, top = +[`TRIM_MAX_DB`], bottom = silence.
    Trim(f32),
    /// 0..1, centre 0.5 = 0 dB, top = +[`EQ_MAX_DB`], bottom = kill.
    EqHigh(f32),
    EqMid(f32),
    EqLow(f32),
    /// -1..1: left of centre low-passes, right high-passes, centre is flat.
    Filter(f32),
    /// Channel fader, 0..1.
    Fader(f32),
    /// Headphone cue (PFL) on or off.
    Pfl(bool),
    TogglePfl,
}

/// A control on the main and headphone buses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MasterControl {
    /// 0..1.
    MainLevel(f32),
    /// 0..1.
    PhonesLevel(f32),
    /// Headphone blend, 0 = cue only, 1 = main only.
    CueMix(f32),
    /// Cue (mono) in the left ear, main (mono) in the right.
    SplitCue(bool),
}

/// Control positions and meter of one channel, for the UI and LEDs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelSnapshot {
    pub trim: f32,
    /// `[low, mid, high]`, each 0..1.
    pub eq: [f32; 3],
    pub filter: f32,
    pub fader: f32,
    pub pfl: bool,
    /// Pre-fader peak in the last callback.
    pub peak: f32,
}

impl Default for ChannelSnapshot {
    fn default() -> Self {
        Self {
            trim: 0.5,
            eq: [0.5; 3],
            filter: 0.0,
            fader: 1.0,
            pfl: false,
            peak: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MasterSnapshot {
    pub main_level: f32,
    pub phones_level: f32,
    pub cue_mix: f32,
    pub split_cue: bool,
    pub phones_peak: f32,
}

impl Default for MasterSnapshot {
    fn default() -> Self {
        Self {
            main_level: 1.0,
            phones_level: 1.0,
            cue_mix: 0.0,
            split_cue: false,
            phones_peak: 0.0,
        }
    }
}

/// Gain for a knob with a centre detent: unity at 0.5, `max_db` at 1, silence at 0.
pub fn knob_gain(value: f32, max_db: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value >= 0.5 {
        db_to_gain((value - 0.5) * 2.0 * max_db)
    } else {
        // Square law below the centre: -6 dB at 3/4 of the way down to the centre, -12
        // dB halfway, silence at the bottom.
        let x = value * 2.0;
        x * x
    }
}

/// Gain for a fader or level control: square law, 0 at the bottom, unity at the top.
pub fn fader_gain(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    value * value
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum FilterShape {
    Flat,
    /// Cutoff and how much of the filtered signal to use (the rest is dry).
    LowPass {
        hz: f32,
        wet: f32,
    },
    HighPass {
        hz: f32,
        wet: f32,
    },
}

fn filter_shape(knob: f32) -> FilterShape {
    let knob = knob.clamp(-1.0, 1.0);
    let amount = (knob.abs() - FILTER_DEAD_ZONE) / (1.0 - FILTER_DEAD_ZONE);
    if amount <= 0.0 {
        return FilterShape::Flat;
    }
    let wet = (amount / FILTER_BLEND).min(1.0);
    let sweep = |(from, to): (f32, f32)| from * (to / from).powf(amount);
    if knob < 0.0 {
        FilterShape::LowPass {
            hz: sweep(FILTER_LP_HZ),
            wet,
        }
    } else {
        FilterShape::HighPass {
            hz: sweep(FILTER_HP_HZ),
            wet,
        }
    }
}

struct Channel {
    sample_rate: f32,
    controls: ChannelSnapshot,
    trim: Smoothed,
    eq_gains: [Smoothed; 3],
    fader: Smoothed,
    pfl: Smoothed,
    eq: [IsolatorEq; 2],
    filter_knob: Smoothed,
    /// Knob value the filter coefficients were last computed for.
    filter_computed_for: f32,
    filter_shape: FilterShape,
    filter_coefficients: SvfCoefficients,
    svf: [Svf; 2],
    peak: f32,
}

impl Channel {
    fn new(sample_rate: f32) -> Self {
        let controls = ChannelSnapshot::default();
        Self {
            sample_rate,
            controls,
            trim: Smoothed::new(sample_rate, knob_gain(controls.trim, TRIM_MAX_DB)),
            eq_gains: controls
                .eq
                .map(|v| Smoothed::new(sample_rate, knob_gain(v, EQ_MAX_DB))),
            fader: Smoothed::new(sample_rate, fader_gain(controls.fader)),
            pfl: Smoothed::new(sample_rate, 0.0),
            eq: [IsolatorEq::new(sample_rate); 2],
            filter_knob: Smoothed::new(sample_rate, 0.0),
            filter_computed_for: 0.0,
            filter_shape: FilterShape::Flat,
            filter_coefficients: SvfCoefficients::new(sample_rate, 1000.0, FILTER_Q),
            svf: [Svf::default(); 2],
            peak: 0.0,
        }
    }

    fn apply(&mut self, control: ChannelControl) {
        match control {
            ChannelControl::Trim(v) => {
                self.controls.trim = v.clamp(0.0, 1.0);
                self.trim.set(knob_gain(v, TRIM_MAX_DB));
            }
            ChannelControl::EqLow(v) => self.set_eq(0, v),
            ChannelControl::EqMid(v) => self.set_eq(1, v),
            ChannelControl::EqHigh(v) => self.set_eq(2, v),
            ChannelControl::Filter(v) => {
                self.controls.filter = v.clamp(-1.0, 1.0);
                self.filter_knob.set(self.controls.filter);
            }
            ChannelControl::Fader(v) => {
                self.controls.fader = v.clamp(0.0, 1.0);
                self.fader.set(fader_gain(v));
            }
            ChannelControl::Pfl(on) => self.set_pfl(on),
            ChannelControl::TogglePfl => self.set_pfl(!self.controls.pfl),
        }
    }

    fn set_eq(&mut self, band: usize, value: f32) {
        self.controls.eq[band] = value.clamp(0.0, 1.0);
        self.eq_gains[band].set(knob_gain(value, EQ_MAX_DB));
    }

    fn set_pfl(&mut self, on: bool) {
        self.controls.pfl = on;
        let target = match on {
            true => 1.0,
            false => 0.0,
        };
        self.pfl.set(target);
    }

    /// Process one frame. Returns `(pre_fader, post_fader, cue_send)`.
    #[inline]
    #[allow(clippy::type_complexity)]
    fn process(&mut self, (l, r): (f32, f32)) -> ((f32, f32), (f32, f32), (f32, f32)) {
        let trim = self.trim.tick();
        let gains = [
            self.eq_gains[0].tick(),
            self.eq_gains[1].tick(),
            self.eq_gains[2].tick(),
        ];
        let mut l = self.eq[0].process(l * trim, gains);
        let mut r = self.eq[1].process(r * trim, gains);

        let knob = self.filter_knob.tick();
        if knob != self.filter_computed_for {
            self.filter_computed_for = knob;
            self.filter_shape = filter_shape(knob);
            let hz = match self.filter_shape {
                FilterShape::Flat => None,
                FilterShape::LowPass { hz, .. } | FilterShape::HighPass { hz, .. } => Some(hz),
            };
            if let Some(hz) = hz {
                self.filter_coefficients = SvfCoefficients::new(self.sample_rate, hz, FILTER_Q);
            }
        }
        // The filter always runs so its state is warm when the knob leaves the centre.
        let (l_lp, l_hp) = self.svf[0].process(l, &self.filter_coefficients);
        let (r_lp, r_hp) = self.svf[1].process(r, &self.filter_coefficients);
        match self.filter_shape {
            FilterShape::Flat => {}
            FilterShape::LowPass { wet, .. } => {
                l += (l_lp - l) * wet;
                r += (r_lp - r) * wet;
            }
            FilterShape::HighPass { wet, .. } => {
                l += (l_hp - l) * wet;
                r += (r_hp - r) * wet;
            }
        }

        self.peak = self.peak.max(l.abs()).max(r.abs());
        let fader = self.fader.tick();
        let pfl = self.pfl.tick();
        ((l, r), (l * fader, r * fader), (l * pfl, r * pfl))
    }

    fn snapshot(&self) -> ChannelSnapshot {
        ChannelSnapshot {
            peak: self.peak,
            ..self.controls
        }
    }
}

/// Main and headphone output of one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixFrame {
    pub main: (f32, f32),
    pub phones: (f32, f32),
}

pub struct Mixer {
    channels: [Channel; 2],
    fx: FxUnit,
    master: MasterSnapshot,
    main_level: Smoothed,
    phones_level: Smoothed,
    cue_mix: Smoothed,
}

impl Mixer {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        let master = MasterSnapshot::default();
        Self {
            channels: [Channel::new(sr), Channel::new(sr)],
            fx: FxUnit::new(sample_rate),
            master,
            main_level: Smoothed::new(sr, fader_gain(master.main_level)),
            phones_level: Smoothed::new(sr, fader_gain(master.phones_level)),
            cue_mix: Smoothed::new(sr, master.cue_mix),
        }
    }

    pub fn apply_channel(&mut self, channel: usize, control: ChannelControl) {
        if let Some(channel) = self.channels.get_mut(channel) {
            channel.apply(control);
        }
    }

    pub fn apply_master(&mut self, control: MasterControl) {
        match control {
            MasterControl::MainLevel(v) => {
                self.master.main_level = v.clamp(0.0, 1.0);
                self.main_level.set(fader_gain(v));
            }
            MasterControl::PhonesLevel(v) => {
                self.master.phones_level = v.clamp(0.0, 1.0);
                self.phones_level.set(fader_gain(v));
            }
            MasterControl::CueMix(v) => {
                self.master.cue_mix = v.clamp(0.0, 1.0);
                self.cue_mix.set(self.master.cue_mix);
            }
            MasterControl::SplitCue(on) => self.master.split_cue = on,
        }
    }

    pub fn apply_fx(&mut self, control: FxControl) {
        self.fx.apply(control);
    }

    /// The tempo and beat for the FX, once per block (see [`FxUnit::set_clock`]).
    pub fn set_fx_clock(&mut self, clock: Option<FxClock>) {
        self.fx.set_clock(clock);
    }

    pub fn fx_snapshot(&self) -> FxSnapshot {
        self.fx.snapshot()
    }

    /// Reset meters; call once per callback before processing.
    pub fn begin_block(&mut self) {
        for channel in &mut self.channels {
            channel.peak = 0.0;
        }
        self.master.phones_peak = 0.0;
    }

    /// Mix one frame from each deck.
    #[inline]
    pub fn process(&mut self, decks: [(f32, f32); 2]) -> MixFrame {
        let mut main = (0.0, 0.0);
        let mut cue = (0.0, 0.0);
        let fx_target = self.fx.target();
        for (i, (channel, input)) in self.channels.iter_mut().zip(decks).enumerate() {
            let (_pre, mut post, send) = channel.process(input);
            if fx_target == FxTarget::Deck(DeckId::ALL[i]) {
                post = self.fx.process(post);
            }
            main.0 += post.0;
            main.1 += post.1;
            cue.0 += send.0;
            cue.1 += send.1;
        }
        if fx_target == FxTarget::Main {
            main = self.fx.process(main);
        }
        let main_level = self.main_level.tick();
        let main = (main.0 * main_level, main.1 * main_level);

        let phones_level = self.phones_level.tick();
        let mix = self.cue_mix.tick();
        let phones = match self.master.split_cue {
            true => ((cue.0 + cue.1) * 0.5, (main.0 + main.1) * 0.5),
            false => (
                cue.0 + (main.0 - cue.0) * mix,
                cue.1 + (main.1 - cue.1) * mix,
            ),
        };
        let phones = (phones.0 * phones_level, phones.1 * phones_level);
        self.master.phones_peak = self
            .master
            .phones_peak
            .max(phones.0.abs())
            .max(phones.1.abs());
        MixFrame { main, phones }
    }

    pub fn channel_snapshot(&self, channel: usize) -> ChannelSnapshot {
        self.channels[channel].snapshot()
    }

    pub fn master_snapshot(&self) -> MasterSnapshot {
        self.master
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn run(mixer: &mut Mixer, frames: usize, decks: [(f32, f32); 2]) -> MixFrame {
        mixer.begin_block();
        let mut last = mixer.process(decks);
        for _ in 1..frames {
            last = mixer.process(decks);
        }
        last
    }

    #[test]
    fn curves() {
        assert!((knob_gain(0.5, 6.0) - 1.0).abs() < 1e-6);
        assert!((knob_gain(1.0, 6.0) - db_to_gain(6.0)).abs() < 1e-6);
        assert_eq!(knob_gain(0.0, 6.0), 0.0);
        assert_eq!(fader_gain(0.0), 0.0);
        assert_eq!(fader_gain(1.0), 1.0);
        assert_eq!(filter_shape(0.0), FilterShape::Flat);
        assert_eq!(filter_shape(0.015), FilterShape::Flat);
        assert!(
            matches!(filter_shape(-1.0), FilterShape::LowPass { hz, wet } if (hz - 60.0).abs() < 0.1 && wet == 1.0)
        );
        assert!(
            matches!(filter_shape(1.0), FilterShape::HighPass { hz, wet } if (hz - 8000.0).abs() < 1.0 && wet == 1.0)
        );
    }

    #[test]
    fn defaults_pass_decks_to_main_and_keep_phones_silent() {
        let mut mixer = Mixer::new(SR);
        // DC passes the crossover at unity (the high bands block it, the low band
        // passes it), so a constant input settles to the same constant.
        let out = run(&mut mixer, SR as usize, [(0.25, 0.25), (0.1, 0.1)]);
        assert!((out.main.0 - 0.35).abs() < 1e-3, "{:?}", out.main);
        assert_eq!(out.phones, (0.0, 0.0));
    }

    #[test]
    fn fader_and_pfl_route_signal() {
        let mut mixer = Mixer::new(SR);
        mixer.apply_channel(0, ChannelControl::Fader(0.0));
        mixer.apply_channel(0, ChannelControl::Pfl(true));
        run(&mut mixer, SR as usize, [(0.5, 0.5), (0.0, 0.0)]);
        // Second block: the filters' start-up transient is over.
        let out = run(&mut mixer, SR as usize, [(0.5, 0.5), (0.0, 0.0)]);
        assert!(out.main.0.abs() < 1e-4, "fader down: {:?}", out.main);
        assert!(
            (out.phones.0 - 0.5).abs() < 1e-3,
            "pre-fader cue: {:?}",
            out.phones
        );
        assert!((mixer.channel_snapshot(0).peak - 0.5).abs() < 1e-3);

        mixer.apply_master(MasterControl::CueMix(1.0));
        let out = run(&mut mixer, SR as usize, [(0.5, 0.5), (0.0, 0.0)]);
        assert!(
            out.phones.0.abs() < 1e-3,
            "phones follow main: {:?}",
            out.phones
        );
    }

    #[test]
    fn split_cue_puts_cue_left_and_main_right() {
        let mut mixer = Mixer::new(SR);
        mixer.apply_channel(1, ChannelControl::Pfl(true));
        mixer.apply_channel(1, ChannelControl::Fader(0.0));
        mixer.apply_master(MasterControl::SplitCue(true));
        let out = run(&mut mixer, SR as usize, [(0.2, 0.2), (0.4, 0.4)]);
        assert!((out.phones.0 - 0.4).abs() < 1e-3, "{:?}", out.phones);
        assert!((out.phones.1 - 0.2).abs() < 1e-3, "{:?}", out.phones);
    }

    #[test]
    fn low_kill_and_lowpass_filter_remove_bass_and_treble() {
        let sine = |hz: f32, i: usize| (std::f32::consts::TAU * hz * i as f32 / SR as f32).sin();
        let peak_out = |mixer: &mut Mixer, hz: f32| {
            mixer.begin_block();
            (0..SR as usize)
                .map(|i| {
                    mixer
                        .process([(sine(hz, i), sine(hz, i)), (0.0, 0.0)])
                        .main
                        .0
                })
                .skip(SR as usize / 2)
                .fold(0.0f32, |p, s| p.max(s.abs()))
        };
        let mut mixer = Mixer::new(SR);
        mixer.apply_channel(0, ChannelControl::EqLow(0.0));
        assert!(peak_out(&mut mixer, 50.0) < 0.05);
        assert!(peak_out(&mut mixer, 5000.0) > 0.9);

        let mut mixer = Mixer::new(SR);
        mixer.apply_channel(0, ChannelControl::Filter(-1.0));
        assert!(peak_out(&mut mixer, 5000.0) < 0.01);
        assert!(peak_out(&mut mixer, 30.0) > 0.9);
    }
}
