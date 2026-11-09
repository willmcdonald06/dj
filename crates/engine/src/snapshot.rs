//! State published by the audio thread for the UI to read.

use crate::deck::{DeckId, DeckSnapshot};
use crate::mixer::{ChannelSnapshot, MasterSnapshot};

/// A copy of engine state, written by the audio thread once per callback into a triple
/// buffer. The UI reads the latest one; it never blocks the audio thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineSnapshot {
    pub sample_rate: u32,
    pub decks: [DeckSnapshot; 2],
    /// Mixer channel of each deck.
    pub channels: [ChannelSnapshot; 2],
    pub master: MasterSnapshot,
    pub quantize: bool,
    pub tone_enabled: bool,
    pub tone_frequency_hz: f32,
    pub tone_gain: f32,
    /// Total frames rendered since the engine started.
    pub frames_processed: u64,
    /// Peak absolute sample on the main bus in the last callback.
    pub main_peak: f32,
}

impl EngineSnapshot {
    pub fn deck(&self, deck: DeckId) -> &DeckSnapshot {
        &self.decks[deck.index()]
    }

    pub fn channel(&self, deck: DeckId) -> &ChannelSnapshot {
        &self.channels[deck.index()]
    }
}

impl Default for EngineSnapshot {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            decks: [DeckSnapshot::default(); 2],
            channels: [ChannelSnapshot::default(); 2],
            master: MasterSnapshot::default(),
            quantize: false,
            tone_enabled: false,
            tone_frequency_hz: 440.0,
            tone_gain: 0.25,
            frames_processed: 0,
            main_peak: 0.0,
        }
    }
}
