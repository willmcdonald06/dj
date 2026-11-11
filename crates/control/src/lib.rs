//! Controller input and output.
//!
//! A [`Mapping`] turns raw controller messages into engine [`Command`]s and engine state
//! into LED feedback. [`Controller`] runs the control thread: it finds a controller with
//! a known mapping, connects to it (and reconnects when it is plugged back in), feeds
//! its messages through the mapping and keeps its LEDs in sync. [`monitor`] is the MIDI
//! monitor used to build mappings.
//!
//! Not RT-safe; runs on the control thread.

pub mod dj505;
pub mod learn;
pub mod monitor;
mod ports;
mod thread;

pub use learn::{DeckTarget, GlobalTarget, MidiInput, Target};
pub use ports::{MidiError, MidiPorts, port_names};
pub use thread::{ControlRequest, ControlSettings, Controller};

use std::time::Instant;

use dj_engine::{Command, DeckId, EngineSnapshot};

/// Something for the UI rather than the engine: browsing and loading tracks, and what
/// the controller connection and MIDI learn are doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// Move the browser selection by this many rows (negative is up).
    Browse(i32),
    /// The browse knob was pressed.
    BrowsePress,
    /// Load the selected track onto a deck.
    Load(DeckId),
    /// The BACK button.
    Back,
    /// A controller is connected: its port name (without the ALSA numbers), whether a
    /// built-in mapping knows it, and how many controls have been learned for it.
    Connected {
        name: String,
        built_in: bool,
        learned: usize,
    },
    Disconnected,
    /// MIDI learn bound a control.
    Learned {
        input: MidiInput,
        target: Target,
    },
    /// MIDI learn got a control that can't do the job (a button for a fader, say).
    LearnRefused {
        input: MidiInput,
        target: Target,
    },
    /// The learned controls were cleared.
    LearnCleared,
}

/// A raw message from a controller (a MIDI message, or later a custom-board packet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMessage<'a> {
    /// Microseconds since an arbitrary epoch, as reported by the input driver.
    pub timestamp_us: u64,
    pub bytes: &'a [u8],
}

/// Translates one controller's messages to commands, and engine state to feedback.
pub trait Mapping: Send {
    /// Human-readable controller name, e.g. "Roland DJ-505".
    fn name(&self) -> &str;

    /// Called once after connecting, to put the controller into the right mode.
    fn on_connect(&mut self, send: &mut dyn FnMut(&[u8]));

    /// Translate an incoming message into zero or more engine commands (`emit`) and
    /// UI events such as browsing (`ui`).
    fn on_message(
        &mut self,
        message: RawMessage<'_>,
        emit: &mut dyn FnMut(Command),
        ui: &mut dyn FnMut(UiEvent),
    );

    /// Produce output messages (LEDs, keep-alives) reflecting the current engine state.
    /// Called every few milliseconds; implementations should only send changes.
    fn update_feedback(
        &mut self,
        state: &EngineSnapshot,
        now: Instant,
        send: &mut dyn FnMut(&[u8]),
    );

    /// Called before disconnecting, e.g. to switch LEDs off.
    fn on_disconnect(&mut self, send: &mut dyn FnMut(&[u8]));
}

/// The mapping for a MIDI port, picked by its name.
pub fn mapping_for_port(port_name: &str) -> Option<Box<dyn Mapping>> {
    if dj505::Dj505::matches_port(port_name) {
        return Some(Box::new(dj505::Dj505::new()));
    }
    None
}

/// A decoded MIDI message. Channels are 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMessage<'a> {
    /// Note on with velocity > 0.
    NoteOn {
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Note off, or note on with velocity 0.
    NoteOff {
        channel: u8,
        note: u8,
    },
    ControlChange {
        channel: u8,
        control: u8,
        value: u8,
    },
    PitchBend {
        channel: u8,
        value: u16,
    },
    SysEx(&'a [u8]),
    Other(&'a [u8]),
}

impl<'a> MidiMessage<'a> {
    pub fn parse(bytes: &'a [u8]) -> Self {
        match *bytes {
            [status, note, velocity, ..] if status & 0xF0 == 0x90 && velocity > 0 => {
                MidiMessage::NoteOn {
                    channel: status & 0x0F,
                    note,
                    velocity,
                }
            }
            [status, note, ..] if status & 0xF0 == 0x90 || status & 0xF0 == 0x80 => {
                MidiMessage::NoteOff {
                    channel: status & 0x0F,
                    note,
                }
            }
            [status, control, value, ..] if status & 0xF0 == 0xB0 => MidiMessage::ControlChange {
                channel: status & 0x0F,
                control,
                value,
            },
            [status, lsb, msb, ..] if status & 0xF0 == 0xE0 => MidiMessage::PitchBend {
                channel: status & 0x0F,
                value: (msb as u16) << 7 | lsb as u16,
            },
            [0xF0, ..] => MidiMessage::SysEx(bytes),
            _ => MidiMessage::Other(bytes),
        }
    }
}

impl std::fmt::Display for MidiMessage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } => write!(
                f,
                "note on   ch {:2}  note 0x{note:02X}  vel {velocity}",
                channel + 1
            ),
            MidiMessage::NoteOff { channel, note } => {
                write!(f, "note off  ch {:2}  note 0x{note:02X}", channel + 1)
            }
            MidiMessage::ControlChange {
                channel,
                control,
                value,
            } => write!(
                f,
                "CC        ch {:2}  cc   0x{control:02X}  val {value}",
                channel + 1
            ),
            MidiMessage::PitchBend { channel, value } => {
                write!(f, "pitch     ch {:2}  {value}", channel + 1)
            }
            MidiMessage::SysEx(bytes) => write!(f, "sysex     {} bytes", bytes.len()),
            MidiMessage::Other(bytes) => {
                let name = match bytes.first() {
                    Some(0xF8) => "clock",
                    Some(0xFA) => "start",
                    Some(0xFB) => "continue",
                    Some(0xFC) => "stop",
                    Some(0xFE) => "active sensing",
                    Some(0xFF) => "reset",
                    Some(status) if status & 0xF0 == 0xA0 => "aftertouch",
                    Some(status) if status & 0xF0 == 0xC0 => "program change",
                    Some(status) if status & 0xF0 == 0xD0 => "channel pressure",
                    _ => "other",
                };
                write!(f, "{name}")
            }
        }
    }
}

/// Whether a message is MIDI timing housekeeping (clock ticks or active sensing), which
/// some controllers send nonstop: the DJ-505's TR-S sends 24 clock ticks per beat.
pub fn is_timing(bytes: &[u8]) -> bool {
    matches!(bytes, [0xF8] | [0xFE])
}

/// Hex dump of a message, e.g. `90 00 7F`.
pub fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_messages() {
        assert_eq!(
            MidiMessage::parse(&[0x91, 0x01, 0x7F]),
            MidiMessage::NoteOn {
                channel: 1,
                note: 1,
                velocity: 127
            }
        );
        assert_eq!(
            MidiMessage::parse(&[0x90, 0x01, 0x00]),
            MidiMessage::NoteOff {
                channel: 0,
                note: 1
            }
        );
        assert_eq!(
            MidiMessage::parse(&[0x80, 0x05, 0x40]),
            MidiMessage::NoteOff {
                channel: 0,
                note: 5
            }
        );
        assert_eq!(
            MidiMessage::parse(&[0xBF, 0x64, 0x00]),
            MidiMessage::ControlChange {
                channel: 15,
                control: 0x64,
                value: 0
            }
        );
        assert_eq!(
            MidiMessage::parse(&[0xE0, 0x00, 0x40]),
            MidiMessage::PitchBend {
                channel: 0,
                value: 8192
            }
        );
        assert!(matches!(
            MidiMessage::parse(&[0xF0, 0x00, 0xF7]),
            MidiMessage::SysEx(_)
        ));
        assert!(matches!(MidiMessage::parse(&[0xFA]), MidiMessage::Other(_)));
    }

    #[test]
    fn clock_and_active_sensing_are_timing() {
        assert!(is_timing(&[0xF8]) && is_timing(&[0xFE]));
        assert!(!is_timing(&[0xFA]) && !is_timing(&[0x90, 0x00, 0x7F]));
        assert_eq!(MidiMessage::parse(&[0xF8]).to_string(), "clock");
    }

    #[test]
    fn finds_mapping_by_port_name() {
        assert!(mapping_for_port("DJ-505:DJ-505 MIDI 1 24:0").is_some());
        assert!(mapping_for_port("Midi Through Port-0").is_none());
    }
}
