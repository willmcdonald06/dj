//! Mapping of the engine's buses onto interleaved device channels.
//!
//! Output mode is a routing switch, not a different mixer: in [`OutputMode::PerDeck`]
//! the same graph runs with the software mixer bypassed and each deck sent to its own
//! pair (implemented once decks exist, Milestone 10).

/// Two channel indices (0-based) in the interleaved output buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StereoPair {
    pub left: usize,
    pub right: usize,
}

impl StereoPair {
    pub const fn new(left: usize, right: usize) -> Self {
        Self { left, right }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Engine mixes both decks: main on outputs 1–2, headphone cue on 3–4.
    #[default]
    InternalMix,
    /// Deck A on 1–2, deck B on 3–4; mixing happens in an external analog mixer.
    PerDeck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRouting {
    pub mode: OutputMode,
    /// Total interleaved channels in the device buffer.
    pub channels: usize,
    pub main: StereoPair,
    /// Headphone cue bus. `None` when the device has fewer than four outputs.
    pub cue: Option<StereoPair>,
}

impl OutputRouting {
    /// Default routing: main on 1–2 (mono devices get main on channel 1 only), cue on
    /// 3–4 when the device has at least four channels.
    pub fn internal_mix(channels: usize) -> Self {
        let channels = channels.max(1);
        let main = match channels {
            1 => StereoPair::new(0, 0),
            _ => StereoPair::new(0, 1),
        };
        let cue = match channels {
            0..=3 => None,
            _ => Some(StereoPair::new(2, 3)),
        };
        Self {
            mode: OutputMode::InternalMix,
            channels,
            main,
            cue,
        }
    }

    /// Deck A on 1–2 and deck B on 3–4, for an external mixer. `None` for devices with
    /// fewer than four channels, where deck B would have nowhere to go.
    pub fn per_deck(channels: usize) -> Option<Self> {
        match channels {
            0..=3 => None,
            _ => Some(Self {
                mode: OutputMode::PerDeck,
                ..Self::internal_mix(channels)
            }),
        }
    }

    /// Routing for `mode` on a `channels`-channel device; `None` if it can't be done.
    pub fn for_mode(mode: OutputMode, channels: usize) -> Option<Self> {
        match mode {
            OutputMode::InternalMix => Some(Self::internal_mix(channels)),
            OutputMode::PerDeck => Self::per_deck(channels),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_device_has_no_cue() {
        let r = OutputRouting::internal_mix(2);
        assert_eq!(r.main, StereoPair::new(0, 1));
        assert_eq!(r.cue, None);
    }

    #[test]
    fn four_channel_device_has_cue_on_3_4() {
        let r = OutputRouting::internal_mix(4);
        assert_eq!(r.cue, Some(StereoPair::new(2, 3)));
    }

    #[test]
    fn per_deck_needs_four_channels() {
        assert_eq!(OutputRouting::per_deck(2), None);
        let r = OutputRouting::for_mode(OutputMode::PerDeck, 4).unwrap();
        assert_eq!(r.mode, OutputMode::PerDeck);
        assert_eq!(r.main, StereoPair::new(0, 1));
        assert_eq!(r.cue, Some(StereoPair::new(2, 3)));
    }

    #[test]
    fn mono_device_routes_main_to_channel_one() {
        let r = OutputRouting::internal_mix(1);
        assert_eq!(r.main, StereoPair::new(0, 0));
    }
}
