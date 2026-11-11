//! MIDI learn: mappings made by clicking a control on screen and then moving one on the
//! controller, so any MIDI controller works without code.
//!
//! A [`Learned`] mapping belongs to one controller (by port name) and is saved as TOML in
//! the mappings folder, one file per controller, readable and editable:
//!
//! ```toml
//! port = "DDJ-400"
//! bindings = [
//!   { midi = "note 1 0x0B", action = "deck A play" },
//!   { midi = "cc 1 0x13", action = "deck A fader" },
//! ]
//! ```
//!
//! Learned bindings are checked before a built-in mapping, so they can also change or
//! extend one. Not RT-safe; runs on the control thread.

use std::path::{Path, PathBuf};

use dj_engine::{
    ChannelControl, Command, DeckAction, DeckId, EngineSnapshot, FxControl, HotCue, MasterControl,
    SamplerControl, Transport,
};
use serde::{Deserialize, Serialize};

use crate::{MidiMessage, UiEvent};

/// Which MIDI control a binding listens to. Channels are 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MidiInput {
    Note { channel: u8, note: u8 },
    Cc { channel: u8, control: u8 },
    PitchBend { channel: u8 },
}

impl MidiInput {
    /// The control a message comes from, and its value: 0..1 for notes (pressed is 1),
    /// CCs and pitch bend; `None` for other messages.
    pub fn from_message(message: &MidiMessage<'_>) -> Option<(Self, f32, u8)> {
        match *message {
            MidiMessage::NoteOn { channel, note, .. } => {
                Some((MidiInput::Note { channel, note }, 1.0, 127))
            }
            MidiMessage::NoteOff { channel, note } => {
                Some((MidiInput::Note { channel, note }, 0.0, 0))
            }
            MidiMessage::ControlChange {
                channel,
                control,
                value,
            } => Some((
                MidiInput::Cc { channel, control },
                value as f32 / 127.0,
                value,
            )),
            MidiMessage::PitchBend { channel, value } => Some((
                MidiInput::PitchBend { channel },
                value as f32 / 16_383.0,
                (value >> 7) as u8,
            )),
            _ => None,
        }
    }
}

impl std::fmt::Display for MidiInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            MidiInput::Note { channel, note } => write!(f, "note {} 0x{note:02X}", channel + 1),
            MidiInput::Cc { channel, control } => write!(f, "cc {} 0x{control:02X}", channel + 1),
            MidiInput::PitchBend { channel } => write!(f, "pitch {}", channel + 1),
        }
    }
}

impl std::str::FromStr for MidiInput {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let words: Vec<&str> = text.split_whitespace().collect();
        let number = |word: &str| -> Result<u8, String> {
            let parsed = match word.strip_prefix("0x").or(word.strip_prefix("0X")) {
                Some(hex) => u8::from_str_radix(hex, 16),
                None => word.parse(),
            };
            parsed.map_err(|_| format!("{word:?} isn't a number"))
        };
        let channel = |word: &str| -> Result<u8, String> {
            match number(word)? {
                c @ 1..=16 => Ok(c - 1),
                c => Err(format!("channel {c} isn't 1-16")),
            }
        };
        match words.as_slice() {
            ["note", ch, n] => Ok(MidiInput::Note {
                channel: channel(ch)?,
                note: number(n)?,
            }),
            ["cc", ch, n] => Ok(MidiInput::Cc {
                channel: channel(ch)?,
                control: number(n)?,
            }),
            ["pitch", ch] => Ok(MidiInput::PitchBend {
                channel: channel(ch)?,
            }),
            _ => Err(format!(
                "{text:?} isn't \"note CH NUM\", \"cc CH NUM\" or \"pitch CH\""
            )),
        }
    }
}

/// A function on one deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckTarget {
    Play,
    Cue,
    Sync,
    KeyLock,
    /// 0-based.
    HotCue(u8),
    Loop,
    LoopHalve,
    LoopDouble,
    LoopIn,
    LoopOut,
    Reloop,
    Slip,
    Reverse,
    HeadphoneCue,
    Load,
    Tempo,
    Fader,
    Trim,
    EqLow,
    EqMid,
    EqHigh,
    Filter,
    /// A jog wheel or encoder: bends the tempo.
    Jog,
}

/// A function not tied to a deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalTarget {
    Quantize,
    FxOn,
    FxNext,
    FxTap,
    FxLevel,
    MainLevel,
    PhonesLevel,
    CueMix,
    /// 0-based.
    SamplerPad(u8),
    SamplerLevel,
    /// An encoder that moves the browser selection.
    Browse,
    BrowsePress,
}

/// What a learned control does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Deck(DeckId, DeckTarget),
    Global(GlobalTarget),
}

/// How a target reads its control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Acts on press (and some on release).
    Button,
    /// An absolute position, 0..1.
    Level,
    /// An endless encoder: steps up or down.
    Relative,
}

const DECK_TARGETS: [(&str, DeckTarget); 22] = [
    ("play", DeckTarget::Play),
    ("cue", DeckTarget::Cue),
    ("sync", DeckTarget::Sync),
    ("key lock", DeckTarget::KeyLock),
    ("loop", DeckTarget::Loop),
    ("loop halve", DeckTarget::LoopHalve),
    ("loop double", DeckTarget::LoopDouble),
    ("loop in", DeckTarget::LoopIn),
    ("loop out", DeckTarget::LoopOut),
    ("reloop", DeckTarget::Reloop),
    ("slip", DeckTarget::Slip),
    ("reverse", DeckTarget::Reverse),
    ("headphone cue", DeckTarget::HeadphoneCue),
    ("load", DeckTarget::Load),
    ("tempo", DeckTarget::Tempo),
    ("fader", DeckTarget::Fader),
    ("trim", DeckTarget::Trim),
    ("eq low", DeckTarget::EqLow),
    ("eq mid", DeckTarget::EqMid),
    ("eq high", DeckTarget::EqHigh),
    ("filter", DeckTarget::Filter),
    ("jog", DeckTarget::Jog),
];

const GLOBAL_TARGETS: [(&str, GlobalTarget); 11] = [
    ("quantize", GlobalTarget::Quantize),
    ("fx on", GlobalTarget::FxOn),
    ("fx next", GlobalTarget::FxNext),
    ("fx tap", GlobalTarget::FxTap),
    ("fx level", GlobalTarget::FxLevel),
    ("main level", GlobalTarget::MainLevel),
    ("phones level", GlobalTarget::PhonesLevel),
    ("cue mix", GlobalTarget::CueMix),
    ("sampler level", GlobalTarget::SamplerLevel),
    ("browse", GlobalTarget::Browse),
    ("browse press", GlobalTarget::BrowsePress),
];

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Target::Deck(deck, DeckTarget::HotCue(n)) => {
                write!(f, "deck {deck:?} hot cue {}", n + 1)
            }
            Target::Deck(deck, target) => {
                let name = DECK_TARGETS
                    .iter()
                    .find(|(_, t)| *t == target)
                    .map_or("?", |(name, _)| *name);
                write!(f, "deck {deck:?} {name}")
            }
            Target::Global(GlobalTarget::SamplerPad(n)) => write!(f, "sampler {}", n + 1),
            Target::Global(target) => {
                let name = GLOBAL_TARGETS
                    .iter()
                    .find(|(_, t)| *t == target)
                    .map_or("?", |(name, _)| *name);
                write!(f, "{name}")
            }
        }
    }
}

impl std::str::FromStr for Target {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim().to_ascii_lowercase();
        let pad = |rest: &str| -> Option<u8> {
            rest.trim()
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=8).contains(n))
                .map(|n| n - 1)
        };
        if let Some(rest) = text.strip_prefix("deck ") {
            let (deck, name) = match rest.split_once(' ') {
                Some(("a", name)) => (DeckId::A, name),
                Some(("b", name)) => (DeckId::B, name),
                _ => return Err(format!("{text:?}: decks are A or B")),
            };
            if let Some(n) = name.strip_prefix("hot cue ").and_then(pad) {
                return Ok(Target::Deck(deck, DeckTarget::HotCue(n)));
            }
            return DECK_TARGETS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, t)| Target::Deck(deck, *t))
                .ok_or_else(|| format!("{text:?} isn't a deck control"));
        }
        if let Some(n) = text.strip_prefix("sampler ").and_then(pad) {
            return Ok(Target::Global(GlobalTarget::SamplerPad(n)));
        }
        GLOBAL_TARGETS
            .iter()
            .find(|(n, _)| *n == text)
            .map(|(_, t)| Target::Global(*t))
            .ok_or_else(|| format!("{text:?} isn't a known control"))
    }
}

impl Target {
    fn kind(self) -> Kind {
        match self {
            Target::Deck(_, DeckTarget::Jog) | Target::Global(GlobalTarget::Browse) => {
                Kind::Relative
            }
            Target::Deck(
                _,
                DeckTarget::Tempo
                | DeckTarget::Fader
                | DeckTarget::Trim
                | DeckTarget::EqLow
                | DeckTarget::EqMid
                | DeckTarget::EqHigh
                | DeckTarget::Filter,
            )
            | Target::Global(
                GlobalTarget::FxLevel
                | GlobalTarget::MainLevel
                | GlobalTarget::PhonesLevel
                | GlobalTarget::CueMix
                | GlobalTarget::SamplerLevel,
            ) => Kind::Level,
            _ => Kind::Button,
        }
    }

    /// Whether the control can bind to `input`: levels need a CC or pitch bend, endless
    /// encoders a CC. Buttons take anything.
    pub fn accepts(self, input: MidiInput) -> bool {
        match (self.kind(), input) {
            (Kind::Button, _) => true,
            (Kind::Level, MidiInput::Note { .. }) => false,
            (Kind::Level, _) => true,
            (Kind::Relative, MidiInput::Cc { .. }) => true,
            (Kind::Relative, _) => false,
        }
    }

    /// Act on the control's new value. `value` is 0..1; `raw` the 7-bit value (for
    /// relative encoders).
    fn act(self, value: f32, raw: u8, emit: &mut dyn FnMut(Command), ui: &mut dyn FnMut(UiEvent)) {
        let pressed = value >= 0.5;
        let centred = value * 2.0 - 1.0;
        let deck = |deck, action| Command::deck(deck, action);
        let channel = |deck, control| Command::channel(deck, control);
        match self {
            Target::Deck(d, target) => match target {
                DeckTarget::Play if pressed => emit(deck(d, DeckAction::PlayPause)),
                DeckTarget::Cue => match pressed {
                    true => emit(deck(d, DeckAction::CuePress)),
                    false => emit(deck(d, DeckAction::CueRelease)),
                },
                DeckTarget::Sync if pressed => emit(deck(d, DeckAction::ToggleSyncLock)),
                DeckTarget::KeyLock if pressed => emit(deck(d, DeckAction::ToggleKeyLock)),
                DeckTarget::HotCue(n) => match pressed {
                    true => emit(deck(d, DeckAction::HotCuePress(n as usize))),
                    false => emit(deck(d, DeckAction::HotCueRelease(n as usize))),
                },
                DeckTarget::Loop if pressed => emit(deck(d, DeckAction::AutoLoop { beats: 4.0 })),
                DeckTarget::LoopHalve if pressed => emit(deck(d, DeckAction::LoopHalve)),
                DeckTarget::LoopDouble if pressed => emit(deck(d, DeckAction::LoopDouble)),
                DeckTarget::LoopIn if pressed => emit(deck(d, DeckAction::LoopIn)),
                DeckTarget::LoopOut if pressed => emit(deck(d, DeckAction::LoopOut)),
                DeckTarget::Reloop if pressed => emit(deck(d, DeckAction::ReloopExit)),
                DeckTarget::Slip if pressed => emit(deck(d, DeckAction::ToggleSlipMode)),
                DeckTarget::Reverse if pressed => emit(deck(d, DeckAction::ToggleReverse)),
                DeckTarget::HeadphoneCue if pressed => emit(channel(d, ChannelControl::TogglePfl)),
                DeckTarget::Load if pressed => ui(UiEvent::Load(d)),
                DeckTarget::Tempo => emit(deck(d, DeckAction::SetTempo { fader: centred })),
                DeckTarget::Fader => emit(channel(d, ChannelControl::Fader(value))),
                DeckTarget::Trim => emit(channel(d, ChannelControl::Trim(value))),
                DeckTarget::EqLow => emit(channel(d, ChannelControl::EqLow(value))),
                DeckTarget::EqMid => emit(channel(d, ChannelControl::EqMid(value))),
                DeckTarget::EqHigh => emit(channel(d, ChannelControl::EqHigh(value))),
                DeckTarget::Filter => emit(channel(d, ChannelControl::Filter(centred))),
                DeckTarget::Jog => {
                    let steps = relative_steps(raw);
                    if steps != 0 {
                        emit(deck(
                            d,
                            DeckAction::Nudge {
                                amount: steps as f32 * JOG_NUDGE,
                            },
                        ));
                    }
                }
                _ => {}
            },
            Target::Global(target) => match target {
                GlobalTarget::Quantize if pressed => emit(Command::ToggleQuantize),
                GlobalTarget::FxOn if pressed => emit(Command::Fx(FxControl::ToggleOn)),
                GlobalTarget::FxNext if pressed => emit(Command::Fx(FxControl::StepKind(1))),
                GlobalTarget::FxTap if pressed => emit(Command::Fx(FxControl::Tap)),
                GlobalTarget::FxLevel => emit(Command::Fx(FxControl::SetLevel(value))),
                GlobalTarget::MainLevel => emit(Command::Master(MasterControl::MainLevel(value))),
                GlobalTarget::PhonesLevel => {
                    emit(Command::Master(MasterControl::PhonesLevel(value)))
                }
                GlobalTarget::CueMix => emit(Command::Master(MasterControl::CueMix(value))),
                GlobalTarget::SamplerPad(n) => match pressed {
                    true => emit(Command::Sampler(SamplerControl::Press(n as usize))),
                    false => emit(Command::Sampler(SamplerControl::Release(n as usize))),
                },
                GlobalTarget::SamplerLevel => {
                    emit(Command::Sampler(SamplerControl::SetLevel(value)))
                }
                GlobalTarget::Browse => {
                    let steps = relative_steps(raw);
                    if steps != 0 {
                        ui(UiEvent::Browse(steps));
                    }
                }
                GlobalTarget::BrowsePress if pressed => ui(UiEvent::BrowsePress),
                _ => {}
            },
        }
    }

    /// Whether the target's LED should be lit, for targets that have a state.
    fn lit(self, state: &EngineSnapshot) -> Option<bool> {
        match self {
            Target::Deck(deck, target) => {
                let d = state.deck(deck);
                match target {
                    DeckTarget::Play => Some(d.transport == Transport::Playing),
                    DeckTarget::Sync => Some(d.sync_lock),
                    DeckTarget::KeyLock => Some(d.key_lock),
                    DeckTarget::HotCue(n) => {
                        Some(d.hot_cues.get(n as usize).is_some_and(HotCue::is_set))
                    }
                    DeckTarget::Loop | DeckTarget::Reloop => Some(d.loop_active),
                    DeckTarget::Slip => Some(d.slip_mode),
                    DeckTarget::Reverse => Some(d.reverse),
                    DeckTarget::HeadphoneCue => Some(state.channel(deck).pfl),
                    _ => None,
                }
            }
            Target::Global(target) => match target {
                GlobalTarget::Quantize => Some(state.quantize),
                GlobalTarget::FxOn => Some(state.fx.on),
                GlobalTarget::SamplerPad(n) => Some(
                    state
                        .sampler
                        .slots
                        .get(n as usize)
                        .is_some_and(|s| s.playing),
                ),
                _ => None,
            },
        }
    }
}

/// Tempo bend per jog step.
const JOG_NUDGE: f32 = 0.002;

/// Steps from a relative encoder. Controllers send either two's complement (1 = +1,
/// 127 = −1) or centred on 64 (65 = +1, 63 = −1); the reading with the smaller step is
/// almost always the right one, as encoders send small steps.
pub fn relative_steps(raw: u8) -> i32 {
    let raw = raw as i32;
    let twos = match raw < 64 {
        true => raw,
        false => raw - 128,
    };
    let centred = raw - 64;
    match twos.abs() <= centred.abs() {
        true => twos,
        false => centred,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub input: MidiInput,
    pub target: Target,
}

/// One controller's learned bindings, and where they are saved.
#[derive(Debug, Clone, Default)]
pub struct Learned {
    pub port: String,
    pub bindings: Vec<Binding>,
    path: Option<PathBuf>,
    /// LED state last sent per binding (by index), to send only changes.
    lit: Vec<Option<bool>>,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    port: String,
    #[serde(default)]
    bindings: Vec<FileBinding>,
}

#[derive(Serialize, Deserialize)]
struct FileBinding {
    midi: String,
    action: String,
}

impl Learned {
    /// The learned mapping for `port` in `folder`, or an empty one that will be saved
    /// there. Problems with the file are logged and its bad lines skipped.
    pub fn load(folder: &Path, port: &str) -> Self {
        let path = folder.join(file_name(port));
        let mut learned = Learned {
            port: port_key(port).to_string(),
            path: Some(path.clone()),
            ..Learned::default()
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return learned,
            Err(err) => {
                tracing::warn!("can't read {}: {err}", path.display());
                return learned;
            }
        };
        let file: File = match toml::from_str(&text) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!("{} isn't valid: {err}", path.display());
                return learned;
            }
        };
        for binding in file.bindings {
            match (binding.midi.parse(), binding.action.parse()) {
                (Ok(input), Ok(target)) => learned.bind(input, target),
                (Err(err), _) | (_, Err(err)) => {
                    tracing::warn!("{}: skipping a binding: {err}", path.display())
                }
            }
        }
        learned
    }

    /// Bind `input` to `target`, replacing what either was bound to before (one
    /// control per function and one function per control).
    pub fn bind(&mut self, input: MidiInput, target: Target) {
        self.bindings
            .retain(|b| b.input != input && b.target != target);
        self.bindings.push(Binding { input, target });
        self.lit.clear();
    }

    pub fn clear(&mut self) {
        self.bindings.clear();
        self.lit.clear();
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let file = File {
            port: self.port.clone(),
            bindings: self
                .bindings
                .iter()
                .map(|b| FileBinding {
                    midi: b.input.to_string(),
                    action: b.target.to_string(),
                })
                .collect(),
        };
        let body = toml::to_string(&file).map_err(std::io::Error::other)?;
        let text = format!(
            "# Learned MIDI mapping for {:?}, written by MIDI learn. Edit freely:\n\
             # midi is \"note CH NUM\", \"cc CH NUM\" or \"pitch CH\"; see the README for\n\
             # the action names.\n\n{body}",
            self.port
        );
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)
    }

    /// Handle a message if a learned binding wants it. Returns whether one did.
    pub fn on_message(
        &mut self,
        message: &MidiMessage<'_>,
        emit: &mut dyn FnMut(Command),
        ui: &mut dyn FnMut(UiEvent),
    ) -> bool {
        let Some((input, value, raw)) = MidiInput::from_message(message) else {
            return false;
        };
        let Some(binding) = self.bindings.iter().find(|b| b.input == input) else {
            return false;
        };
        binding.target.act(value, raw, emit, ui);
        true
    }

    /// Light the LEDs of learned buttons that have a state, by sending their note back
    /// (127 on, 0 off), which most controllers understand. Only changes are sent.
    pub fn update_feedback(&mut self, state: &EngineSnapshot, send: &mut dyn FnMut(&[u8])) {
        self.lit.resize(self.bindings.len(), None);
        for (binding, last) in self.bindings.iter().zip(self.lit.iter_mut()) {
            let MidiInput::Note { channel, note } = binding.input else {
                continue;
            };
            let Some(on) = binding.target.lit(state) else {
                continue;
            };
            if *last != Some(on) {
                *last = Some(on);
                let velocity = match on {
                    true => 0x7F,
                    false => 0x00,
                };
                send(&[0x90 | (channel & 0x0F), note & 0x7F, velocity]);
            }
        }
    }
}

/// A port's name without the ALSA client:port numbers that change between plug-ins,
/// e.g. "DDJ-400:DDJ-400 MIDI 1 24:0" → "DDJ-400:DDJ-400 MIDI 1".
pub fn port_key(port: &str) -> &str {
    let trimmed = port.trim();
    match trimmed.rsplit_once(' ') {
        Some((rest, last))
            if last.split(':').count() == 2
                && last
                    .split(':')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) =>
        {
            rest.trim_end()
        }
        _ => trimmed,
    }
}

/// The mapping file for a port: its key, lowercased, with anything but letters and
/// digits as dashes.
fn file_name(port: &str) -> String {
    let mut name = String::new();
    for c in port_key(port).chars() {
        match c.is_ascii_alphanumeric() {
            true => name.push(c.to_ascii_lowercase()),
            false if !name.ends_with('-') => name.push('-'),
            false => {}
        }
    }
    format!("{}.toml", name.trim_matches('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_and_targets_round_trip_through_text() {
        let inputs = [
            MidiInput::Note {
                channel: 0,
                note: 0x0B,
            },
            MidiInput::Cc {
                channel: 15,
                control: 0x13,
            },
            MidiInput::PitchBend { channel: 3 },
        ];
        for input in inputs {
            assert_eq!(input.to_string().parse::<MidiInput>(), Ok(input));
        }
        let mut targets: Vec<Target> = DECK_TARGETS
            .iter()
            .flat_map(|(_, t)| [Target::Deck(DeckId::A, *t), Target::Deck(DeckId::B, *t)])
            .chain(GLOBAL_TARGETS.iter().map(|(_, t)| Target::Global(*t)))
            .collect();
        targets.push(Target::Deck(DeckId::B, DeckTarget::HotCue(7)));
        targets.push(Target::Global(GlobalTarget::SamplerPad(0)));
        for target in targets {
            assert_eq!(target.to_string().parse::<Target>(), Ok(target), "{target}");
        }
        assert!("deck C play".parse::<Target>().is_err());
        assert!("cc 17 1".parse::<MidiInput>().is_err());
    }

    #[test]
    fn relative_encoders_both_ways() {
        assert_eq!(relative_steps(1), 1);
        assert_eq!(relative_steps(127), -1);
        assert_eq!(relative_steps(65), 1);
        assert_eq!(relative_steps(63), -1);
        assert_eq!(relative_steps(3), 3);
        assert_eq!(relative_steps(64), 0);
    }

    #[test]
    fn learned_bindings_drive_the_engine_and_save() {
        let folder = std::env::temp_dir().join(format!("dj-learn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        let port = "DDJ-400:DDJ-400 MIDI 1 24:0";
        let mut learned = Learned::load(&folder, port);
        assert!(learned.bindings.is_empty());
        let fader = MidiInput::Cc {
            channel: 0,
            control: 0x13,
        };
        let play = MidiInput::Note {
            channel: 0,
            note: 0x0B,
        };
        learned.bind(fader, Target::Deck(DeckId::A, DeckTarget::Fader));
        learned.bind(play, Target::Deck(DeckId::A, DeckTarget::Play));
        // Re-learning the fader onto another control moves it.
        learned.bind(
            MidiInput::Cc {
                channel: 0,
                control: 0x14,
            },
            Target::Deck(DeckId::A, DeckTarget::Fader),
        );
        assert_eq!(learned.bindings.len(), 2);
        learned.save().unwrap();

        // Plugged in again, on another ALSA port number.
        let mut again = Learned::load(&folder, "DDJ-400:DDJ-400 MIDI 1 28:0");
        assert_eq!(again.bindings, learned.bindings);
        let mut commands = Vec::new();
        let handled = again.on_message(
            &MidiMessage::parse(&[0xB0, 0x14, 127]),
            &mut |c| commands.push(c),
            &mut |_| {},
        );
        assert!(handled);
        assert_eq!(
            commands,
            [Command::channel(DeckId::A, ChannelControl::Fader(1.0))]
        );
        assert!(!again.on_message(
            &MidiMessage::parse(&[0xB0, 0x13, 0]),
            &mut |_| {},
            &mut |_| {}
        ));
        // The play LED follows the transport.
        let mut sent = Vec::new();
        let mut state = EngineSnapshot::default();
        again.update_feedback(&state, &mut |b| sent.push(b.to_vec()));
        state.decks[0].transport = Transport::Playing;
        again.update_feedback(&state, &mut |b| sent.push(b.to_vec()));
        again.update_feedback(&state, &mut |b| sent.push(b.to_vec()));
        assert_eq!(sent, [vec![0x90, 0x0B, 0], vec![0x90, 0x0B, 0x7F]]);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn port_names_lose_their_numbers() {
        assert_eq!(
            port_key("DDJ-400:DDJ-400 MIDI 1 24:0"),
            "DDJ-400:DDJ-400 MIDI 1"
        );
        assert_eq!(port_key("Launchpad"), "Launchpad");
        assert_eq!(
            file_name("DDJ-400:DDJ-400 MIDI 1 24:0"),
            "ddj-400-ddj-400-midi-1.toml"
        );
    }

    #[test]
    fn levels_only_bind_to_knobs_and_faders() {
        let fader = Target::Deck(DeckId::A, DeckTarget::Fader);
        assert!(!fader.accepts(MidiInput::Note {
            channel: 0,
            note: 1
        }));
        assert!(fader.accepts(MidiInput::PitchBend { channel: 0 }));
        let jog = Target::Deck(DeckId::A, DeckTarget::Jog);
        assert!(!jog.accepts(MidiInput::PitchBend { channel: 0 }));
        assert!(
            Target::Deck(DeckId::A, DeckTarget::Play).accepts(MidiInput::Cc {
                channel: 0,
                control: 1
            })
        );
    }
}
