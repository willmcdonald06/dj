//! Roland DJ-505 mapping.
//!
//! Message numbers come from the MIDI implementation as documented by Mixxx's DJ-505
//! mapping (used as a reference only; no code is copied). Deck 1 sends on MIDI channel
//! 1, deck 2 on channel 2; decks 3/4 (channels 3/4) are ignored since the app has two
//! decks.
//!
//! The controller starts in standalone mode. Two SysEx messages switch it to "Serato
//! mode", in which it reports the position of every fader and knob and leaves all LEDs
//! to the software. It falls back to standalone mode unless it receives a keep-alive
//! about every 500 ms.
//!
//! The DJ-505's master, booth, phones, mic, sampler level and cue/master mix knobs work
//! on its own hardware mixer and are not mapped.
//!
//! Not RT-safe.

use std::time::{Duration, Instant};

use dj_engine::{
    ChannelControl, Command, DeckAction, DeckId, DeckSnapshot, EngineSnapshot, HOT_CUES, HotCue,
    STEM_COUNT, Stem, Transport,
};

use crate::{Mapping, MidiMessage, RawMessage, UiEvent};

/// Requests Serato mode and the current position of every control.
pub const SYSEX_INIT: [&[u8]; 2] = [
    &[0xF0, 0x00, 0x20, 0x7F, 0x00, 0xF7],
    &[0xF0, 0x00, 0x20, 0x7F, 0x01, 0xF7],
];
pub const KEEP_ALIVE: [u8; 3] = [0xBF, 0x64, 0x00];
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_millis(400);
const BLINK_PERIOD: Duration = Duration::from_millis(500);

// Notes on the deck channels.
const NOTE_PLAY: u8 = 0x00;
const NOTE_CUE: u8 = 0x01;
const NOTE_SYNC: u8 = 0x02;
/// SHIFT + SYNC.
const NOTE_SYNC_OFF: u8 = 0x03;
const NOTE_KEY_LOCK: u8 = 0x0D;
const NOTE_JOG_TOUCH: u8 = 0x06;
/// SHIFT + KEY LOCK.
const NOTE_TEMPO_RANGE: u8 = 0x0E;
const NOTE_PFL: u8 = 0x1B;

// Pad section: MIDI channel 5 for deck 1, 6 for deck 2.
const PAD_CHANNEL: u8 = 4;
const NOTE_MODE_HOT_CUE: u8 = 0x00;
const NOTE_MODE_ROLL: u8 = 0x08;
/// The LOOP pad mode button; we use it for beat jump.
const NOTE_MODE_LOOP: u8 = 0x0D;
/// The SAMPLER pad mode button; we use it for stems (the app has no sampler yet).
const NOTE_MODE_SAMPLER: u8 = 0x0B;
const NOTE_PAD_FIRST: u8 = 0x14;
/// SHIFT + pad.
const NOTE_PAD_SHIFTED_FIRST: u8 = 0x1C;
const NOTE_RELOOP_EXIT: u8 = 0x33;
const NOTE_LOOP_HALVE: u8 = 0x34;
const NOTE_LOOP_DOUBLE: u8 = 0x35;
const NOTE_LOOP_IN: u8 = 0x38;
const NOTE_LOOP_OUT: u8 = 0x39;
const NOTE_AUTO_LOOP: u8 = 0x40;

/// Pad LED colours (velocity values).
const PAD_OFF: u8 = 0x00;
const PAD_WHITE: u8 = 0x0F;
const PAD_GREEN: u8 = 0x0E;
const PAD_DIM: u8 = 0x10;
/// Hot cue colours by slot, matching the on-screen pads.
const HOT_CUE_PAD_COLORS: [u8; HOT_CUES] = [0x01, 0x02, 0x04, 0x05, 0x0C, 0x03, 0x08, 0x06];
const ROLL_BEATS: [f32; 8] = [0.0625, 0.125, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
const JUMP_BEATS: [f32; 8] = [-1.0, -2.0, -4.0, -8.0, 1.0, 2.0, 4.0, 8.0];
/// Stem pad colours: vocals, melody, bass, drums (matching the screen).
const STEM_PAD_COLORS: [u8; STEM_COUNT] = [0x05, 0x04, 0x01, 0x03];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PadMode {
    #[default]
    HotCue,
    Roll,
    Jump,
    /// Pads 1–4 mute and unmute the stems; 5–8 play one stem alone.
    Stems,
}

// Browser section, on MIDI channel 16.
const GLOBAL_CHANNEL: u8 = 15;
const CC_BROWSE: u8 = 0x00;
const NOTE_BROWSE_PRESS: u8 = 0x06;
const NOTE_LOAD_A: u8 = 0x02;
const NOTE_LOAD_B: u8 = 0x03;
const NOTE_BACK: u8 = 0x07;

// Controllers on the deck channels.
const CC_JOG: u8 = 0x06;
const CC_FILTER: u8 = 0x1A;

/// 14-bit controls: (MSB controller, LSB controller, target).
const FOURTEEN_BIT: [(u8, u8, Target); 6] = [
    (0x09, 0x3B, Target::Tempo),
    (0x16, 0x34, Target::Trim),
    (0x17, 0x35, Target::EqHigh),
    (0x18, 0x36, Target::EqMid),
    (0x19, 0x37, Target::EqLow),
    (0x1C, 0x3A, Target::Fader),
];

/// Rate offset added per jog tick when nudging. Ticks come faster the faster the jog
/// turns, so the bend follows the jog's speed.
pub const NUDGE_PER_TICK: f32 = 0.002;
/// Track movement per jog tick while the top is touched (scratching).
pub const SCRATCH_SECONDS_PER_TICK: f32 = JOG_SECONDS_PER_REV / JOG_TICKS_PER_REV;
/// Jog ticks per revolution of the platter.
const JOG_TICKS_PER_REV: f32 = 512.0;
/// Track time per platter revolution: a record at 45 rpm (as Mixxx maps the 505).
const JOG_SECONDS_PER_REV: f32 = 60.0 / 45.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Tempo,
    Trim,
    EqHigh,
    EqMid,
    EqLow,
    Fader,
}

/// LED states we last sent for one deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct DeckLeds {
    play: bool,
    cue: bool,
    pfl: bool,
    sync: bool,
    key_lock: bool,
}

pub struct Dj505 {
    /// Last MSB per deck per 14-bit control, waiting for its LSB.
    msb: [[u8; FOURTEEN_BIT.len()]; 2],
    jog_touched: [bool; 2],
    pad_mode: [PadMode; 2],
    /// Pad LED colours last sent, `None` until first sent.
    pad_leds: [[Option<u8>; 8]; 2],
    /// Transport of each deck as of the last feedback update, for the jog.
    transport: [Transport; 2],
    /// `None` until first sent, so everything is sent once after connecting.
    leds: [Option<DeckLeds>; 2],
    last_keep_alive: Option<Instant>,
    blink_epoch: Instant,
}

impl Default for Dj505 {
    fn default() -> Self {
        Self::new()
    }
}

impl Dj505 {
    pub fn new() -> Self {
        Self {
            msb: [[0; FOURTEEN_BIT.len()]; 2],
            jog_touched: [false; 2],
            pad_mode: [PadMode::HotCue; 2],
            pad_leds: [[None; 8]; 2],
            transport: [Transport::Stopped; 2],
            leds: [None; 2],
            last_keep_alive: None,
            blink_epoch: Instant::now(),
        }
    }

    pub fn matches_port(port_name: &str) -> bool {
        port_name.to_ascii_uppercase().contains("DJ-505")
    }

    fn deck(channel: u8) -> Option<DeckId> {
        match channel {
            0 => Some(DeckId::A),
            1 => Some(DeckId::B),
            _ => None,
        }
    }

    fn on_note(&mut self, deck: DeckId, note: u8, down: bool, emit: &mut dyn FnMut(Command)) {
        let d = deck.index();
        match (note, down) {
            (NOTE_PLAY, true) => emit(Command::deck(deck, DeckAction::PlayPause)),
            (NOTE_CUE, true) => emit(Command::deck(deck, DeckAction::CuePress)),
            (NOTE_CUE, false) => emit(Command::deck(deck, DeckAction::CueRelease)),
            (NOTE_JOG_TOUCH, _) => {
                // Hand on the platter: scratch, like a record (playing or not).
                if down != self.jog_touched[d] {
                    self.jog_touched[d] = down;
                    let action = match down {
                        true => DeckAction::ScratchStart,
                        false => DeckAction::ScratchEnd,
                    };
                    emit(Command::deck(deck, action));
                }
            }
            (NOTE_TEMPO_RANGE, true) => emit(Command::deck(deck, DeckAction::CycleTempoRange)),
            (NOTE_KEY_LOCK, true) => emit(Command::deck(deck, DeckAction::ToggleKeyLock)),
            (NOTE_SYNC, true) => emit(Command::deck(deck, DeckAction::ToggleSyncLock)),
            (NOTE_SYNC_OFF, true) => emit(Command::deck(deck, DeckAction::SetSyncLock(false))),
            (NOTE_PFL, true) => emit(Command::channel(deck, ChannelControl::TogglePfl)),
            _ => {}
        }
    }

    /// A note on the pad section's channel.
    fn on_pad_note(&mut self, deck: DeckId, note: u8, down: bool, emit: &mut dyn FnMut(Command)) {
        let d = deck.index();
        let act = |action| Command::deck(deck, action);
        match (note, down) {
            (NOTE_MODE_HOT_CUE, true) => self.set_pad_mode(d, PadMode::HotCue),
            (NOTE_MODE_ROLL, true) => self.set_pad_mode(d, PadMode::Roll),
            (NOTE_MODE_LOOP, true) => self.set_pad_mode(d, PadMode::Jump),
            (NOTE_MODE_SAMPLER, true) => self.set_pad_mode(d, PadMode::Stems),
            (NOTE_LOOP_IN, true) => emit(act(DeckAction::LoopIn)),
            (NOTE_LOOP_OUT, true) => emit(act(DeckAction::LoopOut)),
            (NOTE_AUTO_LOOP, true) => emit(act(DeckAction::AutoLoop { beats: 4.0 })),
            (NOTE_LOOP_HALVE, true) => emit(act(DeckAction::LoopHalve)),
            (NOTE_LOOP_DOUBLE, true) => emit(act(DeckAction::LoopDouble)),
            (NOTE_RELOOP_EXIT, true) => emit(act(DeckAction::ReloopExit)),
            (n, _) if (NOTE_PAD_FIRST..NOTE_PAD_FIRST + 8).contains(&n) => {
                let pad = (n - NOTE_PAD_FIRST) as usize;
                let action = match (self.pad_mode[d], down) {
                    (PadMode::HotCue, true) => Some(DeckAction::HotCuePress(pad)),
                    (PadMode::HotCue, false) => Some(DeckAction::HotCueRelease(pad)),
                    (PadMode::Roll, true) => Some(DeckAction::RollPress {
                        beats: ROLL_BEATS[pad],
                    }),
                    (PadMode::Roll, false) => Some(DeckAction::RollRelease),
                    (PadMode::Jump, true) => Some(DeckAction::BeatJump {
                        beats: JUMP_BEATS[pad],
                    }),
                    (PadMode::Jump, false) => None,
                    (PadMode::Stems, true) => Some(match pad < STEM_COUNT {
                        true => DeckAction::ToggleStem(Stem::ALL[pad]),
                        false => DeckAction::SoloStem(Stem::ALL[pad - STEM_COUNT]),
                    }),
                    (PadMode::Stems, false) => None,
                };
                if let Some(action) = action {
                    emit(act(action));
                }
            }
            (n, true)
                if (NOTE_PAD_SHIFTED_FIRST..NOTE_PAD_SHIFTED_FIRST + 8).contains(&n)
                    && self.pad_mode[d] == PadMode::HotCue =>
            {
                let pad = (n - NOTE_PAD_SHIFTED_FIRST) as usize;
                emit(act(DeckAction::HotCueClear(pad)));
            }
            _ => {}
        }
    }

    fn set_pad_mode(&mut self, deck: usize, mode: PadMode) {
        self.pad_mode[deck] = mode;
        // Repaint every pad for the new mode.
        self.pad_leds[deck] = [None; 8];
    }

    fn pad_colors(&self, deck: usize, state: &DeckSnapshot) -> [u8; 8] {
        let mut colors = [PAD_OFF; 8];
        for (pad, color) in colors.iter_mut().enumerate() {
            *color = match self.pad_mode[deck] {
                PadMode::HotCue => match state.hot_cues[pad] {
                    HotCue::Empty => PAD_OFF,
                    _ => HOT_CUE_PAD_COLORS[pad],
                },
                PadMode::Roll => match state.rolling {
                    true => PAD_GREEN,
                    false => PAD_GREEN | PAD_DIM,
                },
                PadMode::Jump => PAD_WHITE | PAD_DIM,
                PadMode::Stems => {
                    let stem = pad % STEM_COUNT;
                    let playing = !state.stems.muted[stem];
                    match (state.stems.available, pad < STEM_COUNT, playing) {
                        (false, _, _) => PAD_OFF,
                        // Toggles: bright while the stem plays, dim while muted.
                        (true, true, true) => STEM_PAD_COLORS[stem],
                        (true, true, false) => STEM_PAD_COLORS[stem] | PAD_DIM,
                        // Solo pads: bright while that stem plays alone.
                        (true, false, _) => {
                            let alone =
                                (0..STEM_COUNT).all(|s| state.stems.muted[s] == (s != stem));
                            match alone {
                                true => STEM_PAD_COLORS[stem],
                                false => STEM_PAD_COLORS[stem] | PAD_DIM,
                            }
                        }
                    }
                }
            };
        }
        colors
    }

    fn on_cc(&mut self, deck: DeckId, control: u8, value: u8, emit: &mut dyn FnMut(Command)) {
        let d = deck.index();
        if control == CC_JOG {
            let ticks = value as f32 - 64.0;
            let action = match self.jog_touched[d] {
                true => DeckAction::ScratchMove {
                    seconds: ticks * SCRATCH_SECONDS_PER_TICK,
                },
                false => DeckAction::Nudge {
                    amount: ticks * NUDGE_PER_TICK,
                },
            };
            emit(Command::deck(deck, action));
            return;
        }
        if control == CC_FILTER {
            // 7-bit, centre 64.
            let filter = ((value as f32 - 64.0) / 63.0).clamp(-1.0, 1.0);
            emit(Command::channel(deck, ChannelControl::Filter(filter)));
            return;
        }
        for (i, &(msb_cc, lsb_cc, target)) in FOURTEEN_BIT.iter().enumerate() {
            if control == msb_cc {
                // Wait for the LSB, which the controller sends right after.
                self.msb[d][i] = value;
                return;
            }
            if control == lsb_cc {
                let raw = (self.msb[d][i] as u16) << 7 | value as u16;
                emit(fourteen_bit_command(deck, target, raw));
                return;
            }
        }
    }

    fn deck_leds(state: &DeckSnapshot, pfl: bool, blink_on: bool) -> DeckLeds {
        let sync = state.sync_lock;
        let key_lock = state.key_lock;
        let loaded = state.track.is_some();
        let at_cue = state.position == state.cue;
        let (play, cue) = match state.transport {
            Transport::Playing => (true, false),
            Transport::CuePreview => (true, true),
            // Paused: play blinks; cue is lit on the cue point and blinks elsewhere,
            // like a CDJ.
            Transport::Stopped if loaded => (blink_on, at_cue || blink_on),
            Transport::Stopped => (false, false),
        };
        DeckLeds {
            play,
            cue,
            pfl,
            sync,
            key_lock,
        }
    }
}

/// Engine command for a 14-bit control at `raw` (0..=16383).
fn fourteen_bit_command(deck: DeckId, target: Target, raw: u16) -> Command {
    let unit = raw as f32 / 16383.0;
    match target {
        // The reference mapping inverts the fader for pitch "as per controller
        // labelling", i.e. a value of 0 is the "+" end.
        Target::Tempo => {
            let mut fader = 1.0 - 2.0 * unit;
            // The centre detent doesn't land exactly on 8192.
            if fader.abs() < 0.004 {
                fader = 0.0;
            }
            Command::deck(deck, DeckAction::SetTempo { fader })
        }
        Target::Trim => Command::channel(deck, ChannelControl::Trim(unit)),
        Target::EqHigh => Command::channel(deck, ChannelControl::EqHigh(unit)),
        Target::EqMid => Command::channel(deck, ChannelControl::EqMid(unit)),
        Target::EqLow => Command::channel(deck, ChannelControl::EqLow(unit)),
        Target::Fader => Command::channel(deck, ChannelControl::Fader(unit)),
    }
}

fn led(channel: u8, note: u8, on: bool) -> [u8; 3] {
    let value = match on {
        true => 0x7F,
        false => 0x00,
    };
    [0x90 | channel, note, value]
}

impl Mapping for Dj505 {
    fn name(&self) -> &str {
        "Roland DJ-505"
    }

    fn on_connect(&mut self, send: &mut dyn FnMut(&[u8])) {
        for message in SYSEX_INIT {
            send(message);
        }
        send(&KEEP_ALIVE);
        self.last_keep_alive = Some(Instant::now());
        self.leds = [None; 2];
    }

    fn on_message(
        &mut self,
        message: RawMessage<'_>,
        emit: &mut dyn FnMut(Command),
        ui: &mut dyn FnMut(UiEvent),
    ) {
        match MidiMessage::parse(message.bytes) {
            MidiMessage::NoteOn {
                channel: GLOBAL_CHANNEL,
                note,
                ..
            } => match note {
                NOTE_BROWSE_PRESS => ui(UiEvent::BrowsePress),
                NOTE_LOAD_A => ui(UiEvent::Load(DeckId::A)),
                NOTE_LOAD_B => ui(UiEvent::Load(DeckId::B)),
                NOTE_BACK => ui(UiEvent::Back),
                _ => {}
            },
            MidiMessage::ControlChange {
                channel: GLOBAL_CHANNEL,
                control: CC_BROWSE,
                value,
            } => {
                // Relative: 1 is a step clockwise, 127 a step anticlockwise.
                let steps = match value {
                    1..=63 => value as i32,
                    65..=127 => value as i32 - 128,
                    _ => 0,
                };
                if steps != 0 {
                    ui(UiEvent::Browse(steps));
                }
            }
            MidiMessage::NoteOn { channel, note, .. } => {
                if let Some(deck) = Self::deck(channel) {
                    self.on_note(deck, note, true, emit);
                } else if let Some(deck) = channel.checked_sub(PAD_CHANNEL).and_then(Self::deck) {
                    self.on_pad_note(deck, note, true, emit);
                }
            }
            MidiMessage::NoteOff { channel, note } => {
                if let Some(deck) = Self::deck(channel) {
                    self.on_note(deck, note, false, emit);
                } else if let Some(deck) = channel.checked_sub(PAD_CHANNEL).and_then(Self::deck) {
                    self.on_pad_note(deck, note, false, emit);
                }
            }
            MidiMessage::ControlChange {
                channel,
                control,
                value,
            } => {
                if let Some(deck) = Self::deck(channel) {
                    self.on_cc(deck, control, value, emit);
                }
            }
            _ => {}
        }
    }

    fn update_feedback(
        &mut self,
        state: &EngineSnapshot,
        now: Instant,
        send: &mut dyn FnMut(&[u8]),
    ) {
        let due = self
            .last_keep_alive
            .is_none_or(|last| now.duration_since(last) >= KEEP_ALIVE_INTERVAL);
        if due {
            send(&KEEP_ALIVE);
            self.last_keep_alive = Some(now);
        }

        let phase =
            now.saturating_duration_since(self.blink_epoch).as_millis() / BLINK_PERIOD.as_millis();
        let blink_on = phase % 2 == 0;
        for deck in DeckId::ALL {
            let d = deck.index();
            let deck_state = state.deck(deck);
            self.transport[d] = deck_state.transport;
            let wanted = Self::deck_leds(deck_state, state.channel(deck).pfl, blink_on);
            let channel = d as u8;
            let previous = self.leds[d];
            if previous.map(|p| p.play) != Some(wanted.play) {
                send(&led(channel, NOTE_PLAY, wanted.play));
            }
            if previous.map(|p| p.cue) != Some(wanted.cue) {
                send(&led(channel, NOTE_CUE, wanted.cue));
            }
            if previous.map(|p| p.pfl) != Some(wanted.pfl) {
                send(&led(channel, NOTE_PFL, wanted.pfl));
            }
            if previous.map(|p| p.sync) != Some(wanted.sync) {
                send(&led(channel, NOTE_SYNC, wanted.sync));
            }
            if previous.map(|p| p.key_lock) != Some(wanted.key_lock) {
                send(&led(channel, NOTE_KEY_LOCK, wanted.key_lock));
            }
            self.leds[d] = Some(wanted);

            let colors = self.pad_colors(d, deck_state);
            for (pad, &color) in colors.iter().enumerate() {
                if self.pad_leds[d][pad] != Some(color) {
                    send(&[
                        0x90 | (PAD_CHANNEL + d as u8),
                        NOTE_PAD_FIRST + pad as u8,
                        color,
                    ]);
                    self.pad_leds[d][pad] = Some(color);
                }
            }
        }
    }

    fn on_disconnect(&mut self, send: &mut dyn FnMut(&[u8])) {
        for channel in 0..2 {
            for note in [NOTE_PLAY, NOTE_CUE, NOTE_PFL, NOTE_SYNC, NOTE_KEY_LOCK] {
                send(&led(channel, note, false));
            }
        }
        self.leds = [None; 2];
        for channel in 0..2u8 {
            for pad in 0..8u8 {
                send(&[
                    0x90 | (PAD_CHANNEL + channel),
                    NOTE_PAD_FIRST + pad,
                    PAD_OFF,
                ]);
            }
        }
        self.pad_leds = [[None; 8]; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dj_engine::TrackId;

    fn feed(mapping: &mut Dj505, bytes: &[u8]) -> Vec<Command> {
        let mut out = Vec::new();
        mapping.on_message(
            RawMessage {
                timestamp_us: 0,
                bytes,
            },
            &mut |c| out.push(c),
            &mut |_| {},
        );
        out
    }

    fn feed_ui(mapping: &mut Dj505, bytes: &[u8]) -> Vec<UiEvent> {
        let mut out = Vec::new();
        mapping.on_message(
            RawMessage {
                timestamp_us: 0,
                bytes,
            },
            &mut |_| {},
            &mut |e| out.push(e),
        );
        out
    }

    #[test]
    fn browse_knob_and_load_buttons_are_ui_events() {
        let mut m = Dj505::new();
        assert_eq!(feed_ui(&mut m, &[0xBF, 0x00, 0x01]), [UiEvent::Browse(1)]);
        assert_eq!(feed_ui(&mut m, &[0xBF, 0x00, 0x7F]), [UiEvent::Browse(-1)]);
        assert_eq!(
            feed_ui(&mut m, &[0x9F, 0x03, 0x7F]),
            [UiEvent::Load(DeckId::B)]
        );
        assert_eq!(feed_ui(&mut m, &[0x9F, 0x06, 0x7F]), [UiEvent::BrowsePress]);
        assert!(feed_ui(&mut m, &[0x9F, 0x03, 0x00]).is_empty(), "release");
    }

    #[test]
    fn transport_buttons() {
        let mut m = Dj505::new();
        assert_eq!(
            feed(&mut m, &[0x90, 0x00, 0x7F]),
            vec![Command::deck(DeckId::A, DeckAction::PlayPause)]
        );
        assert!(feed(&mut m, &[0x90, 0x00, 0x00]).is_empty(), "play release");
        assert_eq!(
            feed(&mut m, &[0x91, 0x01, 0x7F]),
            vec![Command::deck(DeckId::B, DeckAction::CuePress)]
        );
        assert_eq!(
            feed(&mut m, &[0x81, 0x01, 0x00]),
            vec![Command::deck(DeckId::B, DeckAction::CueRelease)]
        );
        assert_eq!(
            feed(&mut m, &[0x90, 0x1B, 0x7F]),
            vec![Command::channel(DeckId::A, ChannelControl::TogglePfl)]
        );
        // Deck 3 is ignored.
        assert!(feed(&mut m, &[0x92, 0x00, 0x7F]).is_empty());
    }

    #[test]
    fn key_lock_and_sync_buttons() {
        let mut m = Dj505::new();
        assert_eq!(
            feed(&mut m, &[0x90, 0x0D, 0x7F]),
            vec![Command::deck(DeckId::A, DeckAction::ToggleKeyLock)]
        );
        assert_eq!(
            feed(&mut m, &[0x91, 0x02, 0x7F]),
            vec![Command::deck(DeckId::B, DeckAction::ToggleSyncLock)]
        );
        assert!(
            feed(&mut m, &[0x91, 0x02, 0x00]).is_empty(),
            "release does nothing"
        );
        assert_eq!(
            feed(&mut m, &[0x90, 0x03, 0x7F]),
            vec![Command::deck(DeckId::A, DeckAction::SetSyncLock(false))]
        );
    }

    #[test]
    fn pads_follow_their_mode() {
        let mut m = Dj505::new();
        assert_eq!(
            feed(&mut m, &[0x94, 0x16, 0x7F]),
            vec![Command::deck(DeckId::A, DeckAction::HotCuePress(2))]
        );
        assert_eq!(
            feed(&mut m, &[0x94, 0x16, 0x00]),
            vec![Command::deck(DeckId::A, DeckAction::HotCueRelease(2))]
        );
        assert_eq!(
            feed(&mut m, &[0x95, 0x1C, 0x7F]),
            vec![Command::deck(DeckId::B, DeckAction::HotCueClear(0))]
        );
        feed(&mut m, &[0x95, 0x08, 0x7F]); // deck 2 ROLL mode
        assert_eq!(
            feed(&mut m, &[0x95, 0x15, 0x7F]),
            vec![Command::deck(
                DeckId::B,
                DeckAction::RollPress { beats: 0.125 }
            )]
        );
        assert_eq!(
            feed(&mut m, &[0x95, 0x15, 0x00]),
            vec![Command::deck(DeckId::B, DeckAction::RollRelease)]
        );
        feed(&mut m, &[0x94, 0x0D, 0x7F]); // deck 1 LOOP button: beat jump
        assert_eq!(
            feed(&mut m, &[0x94, 0x1B, 0x7F]),
            vec![Command::deck(
                DeckId::A,
                DeckAction::BeatJump { beats: 8.0 }
            )]
        );
        assert_eq!(
            feed(&mut m, &[0x94, 0x40, 0x7F]),
            vec![Command::deck(
                DeckId::A,
                DeckAction::AutoLoop { beats: 4.0 }
            )]
        );
    }

    #[test]
    fn fourteen_bit_controls_combine_msb_and_lsb() {
        let mut m = Dj505::new();
        assert!(feed(&mut m, &[0xB0, 0x1C, 0x7F]).is_empty(), "MSB waits");
        assert_eq!(
            feed(&mut m, &[0xB0, 0x3A, 0x7F]),
            vec![Command::channel(DeckId::A, ChannelControl::Fader(1.0))]
        );
        feed(&mut m, &[0xB1, 0x19, 0x00]);
        assert_eq!(
            feed(&mut m, &[0xB1, 0x37, 0x00]),
            vec![Command::channel(DeckId::B, ChannelControl::EqLow(0.0))]
        );
    }

    #[test]
    fn tempo_fader_zero_is_plus_and_centre_snaps() {
        let tempo = |m: &mut Dj505, raw: u16| {
            feed(m, &[0xB0, 0x09, (raw >> 7) as u8]);
            match feed(m, &[0xB0, 0x3B, (raw & 0x7F) as u8])[..] {
                [
                    Command::Deck {
                        action: DeckAction::SetTempo { fader },
                        ..
                    },
                ] => fader,
                ref other => panic!("{other:?}"),
            }
        };
        let mut m = Dj505::new();
        assert_eq!(tempo(&mut m, 0), 1.0);
        assert_eq!(tempo(&mut m, 16383), -1.0);
        assert_eq!(tempo(&mut m, 8192), 0.0);
        assert_eq!(tempo(&mut m, 8200), 0.0);
    }

    #[test]
    fn filter_is_centred_on_64() {
        let mut m = Dj505::new();
        assert_eq!(
            feed(&mut m, &[0xB1, 0x1A, 64]),
            vec![Command::channel(DeckId::B, ChannelControl::Filter(0.0))]
        );
        assert_eq!(
            feed(&mut m, &[0xB1, 0x1A, 0]),
            vec![Command::channel(DeckId::B, ChannelControl::Filter(-1.0))]
        );
    }

    #[test]
    fn jog_nudges_from_the_side_and_scratches_from_the_top() {
        let mut m = Dj505::new();
        assert_eq!(
            feed(&mut m, &[0xB0, 0x06, 66]),
            vec![Command::deck(
                DeckId::A,
                DeckAction::Nudge {
                    amount: 2.0 * NUDGE_PER_TICK
                }
            )]
        );
        assert_eq!(
            feed(&mut m, &[0x90, 0x06, 0x7F]),
            vec![Command::deck(DeckId::A, DeckAction::ScratchStart)]
        );
        assert_eq!(
            feed(&mut m, &[0xB0, 0x06, 62]),
            vec![Command::deck(
                DeckId::A,
                DeckAction::ScratchMove {
                    seconds: -2.0 * SCRATCH_SECONDS_PER_TICK
                }
            )]
        );
        assert_eq!(
            feed(&mut m, &[0x90, 0x06, 0x00]),
            vec![Command::deck(DeckId::A, DeckAction::ScratchEnd)]
        );
    }

    #[test]
    fn connect_sends_serato_mode_and_keep_alives_follow() {
        let mut m = Dj505::new();
        let mut sent = Vec::new();
        m.on_connect(&mut |b| sent.push(b.to_vec()));
        assert_eq!(sent[0], SYSEX_INIT[0]);
        assert_eq!(sent[1], SYSEX_INIT[1]);

        let state = EngineSnapshot::default();
        let start = Instant::now();
        let mut sent = Vec::new();
        m.update_feedback(&state, start, &mut |b| sent.push(b.to_vec()));
        // All ten button LEDs and sixteen pads are sent once after connecting.
        assert_eq!(sent.len(), 26);
        let mut sent = Vec::new();
        m.update_feedback(&state, start + Duration::from_millis(10), &mut |b| {
            sent.push(b.to_vec())
        });
        assert!(sent.is_empty(), "nothing changed");
        m.update_feedback(&state, start + Duration::from_secs(1), &mut |b| {
            sent.push(b.to_vec())
        });
        assert!(sent.contains(&KEEP_ALIVE.to_vec()));
    }

    #[test]
    fn leds_follow_transport() {
        let mut state = EngineSnapshot::default();
        state.decks[0].track = Some(TrackId(1));
        state.decks[0].transport = Transport::Playing;
        state.decks[0].position = 1000;
        state.channels[1].pfl = true;
        let mut m = Dj505::new();
        let mut sent = Vec::new();
        m.update_feedback(&state, Instant::now(), &mut |b| sent.push(b.to_vec()));
        assert!(sent.contains(&vec![0x90, 0x00, 0x7F]), "deck A play on");
        assert!(sent.contains(&vec![0x90, 0x01, 0x00]), "deck A cue off");
        assert!(sent.contains(&vec![0x91, 0x1B, 0x7F]), "deck B PFL on");
        assert!(
            sent.contains(&vec![0x91, 0x00, 0x00]),
            "empty deck B play off"
        );
    }
}
