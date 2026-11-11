//! Commands sent from the control and UI threads into the audio thread.

use crate::deck::{DeckAction, DeckId};
use crate::fx::FxControl;
use crate::mixer::{ChannelControl, MasterControl};

/// A request to change engine state. Must stay `Copy` and small: commands travel
/// through a lock-free queue and are applied on the audio thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    /// A transport action on one deck.
    Deck {
        deck: DeckId,
        action: DeckAction,
    },
    /// A mixer control on a deck's channel.
    Channel {
        deck: DeckId,
        control: ChannelControl,
    },
    /// A main or headphone bus control.
    Master(MasterControl),
    /// The beat FX unit.
    Fx(FxControl),
    /// The sampler bank.
    Sampler(crate::SamplerControl),
    /// Quantize (snap cues and jumps to the beatgrid) on or off, for both decks.
    SetQuantize(bool),
    ToggleQuantize,
    /// Test tone on or off.
    SetToneEnabled(bool),
    /// Flip the test tone on or off.
    ToggleTone,
    /// Test tone frequency in Hz (clamped to 20 Hz .. 0.45 × sample rate).
    SetToneFrequency(f32),
    /// Test tone linear gain, 0.0 ..= 1.0.
    SetToneGain(f32),
}

impl Command {
    pub const fn deck(deck: DeckId, action: DeckAction) -> Self {
        Command::Deck { deck, action }
    }

    pub const fn channel(deck: DeckId, control: ChannelControl) -> Self {
        Command::Channel { deck, control }
    }
}
