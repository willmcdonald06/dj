//! The real-time audio engine.
//!
//! This crate has no hardware dependencies (no `cpal`, `midir`, `winit` or `wgpu`), so
//! it can be unit-tested and benchmarked by rendering buffers offline.
//!
//! # RT safety
//!
//! [`Engine`] lives on the audio thread. [`Engine::process`] and everything it calls is
//! **RT-safe**: no allocation, no locks, no I/O, no logging, and only loops bounded by
//! the buffer size or a fixed queue capacity.
//!
//! [`Engine::new`] returns the engine and the handles other threads use to talk to it
//! ([`EngineParts`]): one [`EngineHandle`] for the UI thread and one for the control
//! (MIDI) thread, each with its own command queue and snapshot reader, and the
//! [`TrackSender`] for the loader thread. Creating them is **not** RT-safe; sending a
//! command ([`EngineHandle::send`]) or a track is wait-free.

mod command;
mod cues;
mod deck;
pub mod dsp;
mod fx;
mod grid;
mod mixer;
mod osc;
mod record;
mod routing;
mod snapshot;
pub mod stem_codec;
mod stems;
mod sync;
mod track;

pub use command::Command;
pub use cues::{HOT_CUES, HotCue, LoopRegion, MIN_LOOP_FRAMES};
pub use deck::{
    DeckAction, DeckId, DeckSnapshot, NUDGE_DECAY_SECONDS, NUDGE_MAX, RAMP_SECONDS, SUB_BLOCK,
    TempoRange, Transport,
};
pub use dj_stretch::Quality as StretchQuality;
pub use fx::{DEFAULT_FX_BPM, FX_BEATS, FxClock, FxControl, FxKind, FxSnapshot, FxTarget};
pub use grid::{BeatGrid, wrap_half};
pub use mixer::{
    ChannelControl, ChannelSnapshot, EQ_MAX_DB, FILTER_DEAD_ZONE, MasterControl, MasterSnapshot,
    TRIM_MAX_DB, db_to_gain, fader_gain, knob_gain,
};
pub use record::{RECORD_CHANNELS, RECORD_QUEUE_SECONDS, RecordSource};
pub use routing::{OutputMode, OutputRouting, StereoPair};
pub use snapshot::EngineSnapshot;
pub use stems::{STEM_COUNT, STEM_RAMP_SECONDS, Stem, StemCache, StemSet, StemsSnapshot};
pub use track::{TRACK_CHANNELS, Track, TrackId, TrackWriter};

use std::sync::Arc;

use deck::Deck;
use dj_stretch::Signalsmith;
use mixer::Mixer;
use osc::SineOsc;
use record::RecordTap;

/// Capacity of the command queue into the audio thread. Also bounds how many commands
/// a single callback will process.
pub const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Capacity of the queue of loaded tracks into the audio thread.
pub const TRACK_QUEUE_CAPACITY: usize = 16;

/// Static engine configuration, fixed for the lifetime of an [`Engine`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// Engine sample rate in Hz. Tracks are resampled to this on load.
    pub sample_rate: u32,
    /// Where the main and headphone buses go in the interleaved output buffer.
    pub routing: OutputRouting,
    /// Key lock state of the decks at startup (on, per the spec).
    pub key_lock: bool,
    pub stretch_quality: StretchQuality,
}

impl EngineConfig {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            sample_rate,
            routing: OutputRouting::internal_mix(channels),
            key_lock: true,
            stretch_quality: StretchQuality::Default,
        }
    }
}

/// Frames the decks render in one go before mixing.
const RENDER_BLOCK: usize = 512;

/// Error returned when the command queue is full. Carries the rejected command back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueFull(pub Command);

impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "engine command queue is full (dropped {:?})", self.0)
    }
}

impl std::error::Error for QueueFull {}

/// A track, or a track's stems, ready to go onto a deck.
#[derive(Debug)]
pub enum TrackLoad {
    Track { deck: DeckId, track: Arc<Track> },
    Stems { deck: DeckId, stems: Arc<StemSet> },
}

/// Loader-side sender of decoded tracks to the audio thread. Not RT-safe to create;
/// [`send`](Self::send) is wait-free.
pub struct TrackSender {
    loads: rtrb::Producer<TrackLoad>,
}

impl TrackSender {
    /// Queue `track` to be loaded onto `deck`. The caller must keep its own reference
    /// to the track so the audio thread never frees it. On a full queue the load is
    /// handed back.
    pub fn send(&mut self, deck: DeckId, track: Arc<Track>) -> Result<(), TrackLoad> {
        self.push(TrackLoad::Track { deck, track })
    }

    /// Queue stems for the track on `deck`; the deck ignores them if another track has
    /// been loaded since. As with tracks, the caller keeps a reference.
    pub fn send_stems(&mut self, deck: DeckId, stems: Arc<StemSet>) -> Result<(), TrackLoad> {
        self.push(TrackLoad::Stems { deck, stems })
    }

    fn push(&mut self, load: TrackLoad) -> Result<(), TrackLoad> {
        match self.loads.push(load) {
            Ok(()) => Ok(()),
            Err(rtrb::PushError::Full(load)) => Err(load),
        }
    }
}

/// A thread's link to the engine: sends commands in, reads state snapshots out.
///
/// Not RT-safe to create. [`send`](Self::send) is wait-free.
pub struct EngineHandle {
    commands: rtrb::Producer<Command>,
    snapshots: triple_buffer::Output<EngineSnapshot>,
}

impl EngineHandle {
    /// Queue a command for the audio thread. Never blocks.
    pub fn send(&mut self, command: Command) -> Result<(), QueueFull> {
        match self.commands.push(command) {
            Ok(()) => Ok(()),
            Err(rtrb::PushError::Full(command)) => Err(QueueFull(command)),
        }
    }

    /// The most recent state published by the audio thread.
    pub fn snapshot(&mut self) -> EngineSnapshot {
        *self.snapshots.read()
    }
}

/// The audio-thread half of the engine. See the crate docs for RT-safety rules.
pub struct Engine {
    config: EngineConfig,
    /// One queue per sending thread (UI, control): `rtrb` is single-producer.
    commands: [rtrb::Consumer<Command>; 2],
    /// One snapshot buffer per reading thread: a triple buffer has a single reader.
    snapshots: [triple_buffer::Input<EngineSnapshot>; 2],
    loads: rtrb::Consumer<TrackLoad>,
    decks: [Deck; 2],
    /// Per-deck render buffers, `[deck][channel]`, [`RENDER_BLOCK`] frames each.
    deck_audio: [[Vec<f32>; 2]; 2],
    mixer: Mixer,
    sync: sync::Sync,
    state: EngineSnapshot,
    tone: SineOsc,
    /// Copy of the main bus for the recorder, and its block buffer
    /// ([`RENDER_BLOCK`] stereo frames).
    record: RecordTap,
    record_block: Vec<f32>,
}

/// Everything [`Engine::new`] creates.
pub struct EngineParts {
    /// Moves to the audio thread.
    pub engine: Engine,
    /// For the UI thread.
    pub ui: EngineHandle,
    /// For the control (MIDI) thread.
    pub control: EngineHandle,
    /// For the loader thread.
    pub tracks: TrackSender,
    /// For the recorder: the main output while recording is armed.
    pub recording: RecordSource,
}

impl Engine {
    /// Create the engine and the handles that talk to it. Call this off the audio
    /// thread: it allocates the queues.
    // Returns the engine together with the handles that talk to it; they only make
    // sense created as a set.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(config: EngineConfig) -> EngineParts {
        let initial = EngineSnapshot {
            sample_rate: config.sample_rate,
            output_mode: config.routing.mode,
            ..EngineSnapshot::default()
        };
        let link = || {
            let (producer, consumer) = rtrb::RingBuffer::new(COMMAND_QUEUE_CAPACITY);
            let (input, output) = triple_buffer::triple_buffer(&initial);
            let handle = EngineHandle {
                commands: producer,
                snapshots: output,
            };
            (handle, consumer, input)
        };
        let (ui, ui_commands, ui_snapshots) = link();
        let (control, control_commands, control_snapshots) = link();
        let (load_producer, load_consumer) = rtrb::RingBuffer::new(TRACK_QUEUE_CAPACITY);
        let (record, recording) = record::record_link(config.sample_rate);

        let mut tone = SineOsc::new(config.sample_rate as f32);
        tone.set_frequency(initial.tone_frequency_hz);
        tone.set_gain(initial.tone_gain);

        let engine = Engine {
            config,
            commands: [ui_commands, control_commands],
            snapshots: [ui_snapshots, control_snapshots],
            loads: load_consumer,
            decks: [0, 1].map(|_| {
                let stretcher = Signalsmith::new(config.sample_rate, config.stretch_quality);
                Deck::new(config.sample_rate, Box::new(stretcher), config.key_lock)
            }),
            deck_audio: [0, 1].map(|_| [vec![0.0; RENDER_BLOCK], vec![0.0; RENDER_BLOCK]]),
            mixer: Mixer::new(config.sample_rate),
            sync: sync::Sync::default(),
            state: initial,
            tone,
            record,
            record_block: vec![0.0; RENDER_BLOCK * record::RECORD_CHANNELS],
        };
        EngineParts {
            engine,
            ui,
            control,
            tracks: TrackSender {
                loads: load_producer,
            },
            recording,
        }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// Change where the buses go, e.g. for a device with a different channel count or
    /// to switch to per-deck outputs. Only while the engine is off the audio thread (it
    /// takes `&mut self`, so it can't be called while a callback owns it). Not RT-safe
    /// to call from the callback; it doesn't allocate, but the caller is the one moving
    /// the engine between streams.
    pub fn set_routing(&mut self, routing: OutputRouting) {
        self.config.routing = routing;
        self.state.output_mode = routing.mode;
    }

    /// Render `frames` frames into the interleaved buffer `out`, which must hold at
    /// least `frames * routing.channels` samples. Channels not driven by a bus are
    /// written as silence.
    ///
    /// RT-safe.
    pub fn process(&mut self, out: &mut [f32], frames: usize) {
        // Loads first, so a command sent right after a load applies to the new track.
        self.drain_loads();
        self.drain_commands();
        self.sync.update(&mut self.decks, self.config.sample_rate);
        let clock = self.fx_clock();
        self.mixer.set_fx_clock(clock);

        let routing = self.config.routing;
        let channels = routing.channels;
        let needed = frames * channels;
        let out = match out.get_mut(..needed) {
            Some(out) => out,
            None => {
                // Caller gave us a short buffer. Fill what we have with silence rather
                // than panic on the audio thread.
                out.fill(0.0);
                return;
            }
        };
        out.fill(0.0);

        self.mixer.begin_block();
        let mut peak = 0.0f32;
        let mut deck_peaks = [0.0f32; 2];
        let recording = self.record.armed();

        for block in out.chunks_mut(RENDER_BLOCK * channels) {
            let n = block.len() / channels;
            for (deck, [left, right]) in self.decks.iter_mut().zip(&mut self.deck_audio) {
                deck.render(&mut left[..n], &mut right[..n]);
            }
            for (d, deck) in self.decks.iter().enumerate() {
                deck_peaks[d] = deck_peaks[d].max(deck.snapshot_peak());
            }
            let [[a_l, a_r], [b_l, b_r]] = &self.deck_audio;
            for (i, frame) in block.chunks_exact_mut(channels).enumerate() {
                let tone = self.tone.next_sample();
                let decks = [(a_l[i], a_r[i]), (b_l[i], b_r[i])];
                let (main, second) = match routing.mode {
                    OutputMode::InternalMix => {
                        let mix = self.mixer.process(decks);
                        (mix.main, mix.phones)
                    }
                    // Mixing happens in an external analog mixer: each deck goes out raw.
                    OutputMode::PerDeck => (decks[0], decks[1]),
                };
                // Never send the device more than full scale.
                let main = (
                    (main.0 + tone).clamp(-1.0, 1.0),
                    (main.1 + tone).clamp(-1.0, 1.0),
                );
                peak = peak.max(main.0.abs()).max(main.1.abs());
                frame[routing.main.left] = main.0;
                frame[routing.main.right] = main.1;
                if recording {
                    self.record_block[i * 2] = main.0;
                    self.record_block[i * 2 + 1] = main.1;
                }
                if let Some(pair) = routing.cue {
                    frame[pair.left] = (second.0 + tone).clamp(-1.0, 1.0);
                    frame[pair.right] = (second.1 + tone).clamp(-1.0, 1.0);
                }
            }
            if recording {
                self.record.write(&self.record_block[..n * 2]);
            }
        }

        self.state.frames_processed += frames as u64;
        self.state.main_peak = peak;
        for (i, deck) in self.decks.iter().enumerate() {
            self.state.decks[i] = deck.snapshot();
            self.state.decks[i].peak = deck_peaks[i];
            self.state.decks[i].sync_master = deck.sync_lock() && self.sync.master() == Some(i);
            self.state.channels[i] = self.mixer.channel_snapshot(i);
        }
        self.state.master = self.mixer.master_snapshot();
        self.state.fx = self.mixer.fx_snapshot();
        for snapshots in &mut self.snapshots {
            snapshots.write(self.state);
        }
    }

    /// Put newly loaded tracks onto their decks. Bounded by the queue capacity. The
    /// replaced track's `Arc` is dropped here, which never frees it: the loader holds
    /// another reference.
    fn drain_loads(&mut self) {
        for _ in 0..TRACK_QUEUE_CAPACITY {
            let load = match self.loads.pop() {
                Ok(load) => load,
                Err(_) => break,
            };
            match load {
                TrackLoad::Track { deck, track } => self.decks[deck.index()].load(Some(track)),
                TrackLoad::Stems { deck, stems } => self.decks[deck.index()].set_stems(stems),
            }
        }
    }

    /// Apply queued commands. Bounded by the queue capacities, so at most
    /// 2 × [`COMMAND_QUEUE_CAPACITY`] iterations.
    fn drain_commands(&mut self) {
        for queue in 0..self.commands.len() {
            for _ in 0..COMMAND_QUEUE_CAPACITY {
                let command = match self.commands[queue].pop() {
                    Ok(command) => command,
                    Err(_) => break,
                };
                self.apply(command);
            }
        }
    }

    /// The tempo and beat the FX follows: its target deck's, or for the main mix the
    /// sync master's, else the playing deck with the higher fader.
    fn fx_clock(&self) -> Option<FxClock> {
        let deck = match self.mixer.fx_snapshot().target {
            FxTarget::Deck(deck) => deck.index(),
            FxTarget::Main => {
                let master = self
                    .sync
                    .master()
                    .filter(|&i| self.decks[i].sync_lock() && self.decks[i].grid().is_some());
                let playing =
                    |i: usize| self.decks[i].is_moving() && self.decks[i].grid().is_some();
                let fader = |i: usize| self.mixer.channel_snapshot(i).fader;
                match (master, playing(0), playing(1)) {
                    (Some(i), _, _) => i,
                    (None, true, true) if fader(1) > fader(0) => 1,
                    (None, true, _) => 0,
                    (None, false, true) => 1,
                    (None, false, false) => return None,
                }
            }
        };
        let deck = &self.decks[deck];
        let grid = deck.grid()?;
        let bpm = (grid.bpm(self.config.sample_rate) * deck.tempo_rate() as f64) as f32;
        let beat = match deck.is_moving() {
            true => Some((deck.target_position() - grid.anchor) / grid.period),
            false => None,
        };
        Some(FxClock { bpm, beat })
    }

    fn set_quantize(&mut self, on: bool) {
        self.state.quantize = on;
        for deck in &mut self.decks {
            deck.set_quantize(on);
        }
    }

    fn apply_deck(&mut self, index: usize, action: DeckAction) {
        match action {
            DeckAction::SetSyncLock(on) => self.sync.set(&mut self.decks, index, on),
            DeckAction::ToggleSyncLock => {
                let on = !self.decks[index].sync_lock();
                self.sync.set(&mut self.decks, index, on);
            }
            DeckAction::SetTempo { .. } => {
                self.decks[index].apply(action);
                // On a follower, the fader sets the tempo of both decks.
                self.sync.tempo_changed(&mut self.decks, index);
            }
            DeckAction::PlayPause => {
                let was_moving = self.decks[index].is_moving();
                self.decks[index].apply(action);
                // A following deck starts on the master's beat.
                if !was_moving
                    && self.decks[index].is_moving()
                    && self.sync.is_follower(&self.decks, index)
                {
                    sync::align_phase(&mut self.decks, index);
                }
            }
            _ => self.decks[index].apply(action),
        }
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Deck { deck, action } => self.apply_deck(deck.index(), action),
            Command::SetQuantize(on) => self.set_quantize(on),
            Command::ToggleQuantize => self.set_quantize(!self.state.quantize),
            Command::Channel { deck, control } => self.mixer.apply_channel(deck.index(), control),
            Command::Master(control) => self.mixer.apply_master(control),
            Command::Fx(control) => self.mixer.apply_fx(control),
            Command::SetToneEnabled(enabled) => {
                self.state.tone_enabled = enabled;
                self.tone.set_enabled(enabled);
            }
            Command::ToggleTone => {
                let enabled = !self.state.tone_enabled;
                self.state.tone_enabled = enabled;
                self.tone.set_enabled(enabled);
            }
            Command::SetToneFrequency(hz) => {
                let nyquist = self.config.sample_rate as f32 / 2.0;
                let hz = hz.clamp(20.0, nyquist * 0.9);
                self.state.tone_frequency_hz = hz;
                self.tone.set_frequency(hz);
            }
            Command::SetToneGain(gain) => {
                let gain = gain.clamp(0.0, 1.0);
                self.state.tone_gain = gain;
                self.tone.set_gain(gain);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn render(engine: &mut Engine, frames: usize) -> Vec<f32> {
        let channels = engine.config().routing.channels;
        let mut buf = vec![1.0; frames * channels];
        engine.process(&mut buf, frames);
        buf
    }

    #[test]
    fn silent_by_default() {
        let mut engine = Engine::new(EngineConfig::new(SR, 2)).engine;
        let buf = render(&mut engine, 512);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn tone_reaches_requested_gain_on_main_and_cue() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 4));
        handle.send(Command::SetToneGain(0.5)).unwrap();
        handle.send(Command::SetToneEnabled(true)).unwrap();

        // Let the de-click ramp finish, then measure a full second.
        render(&mut engine, SR as usize / 10);
        let buf = render(&mut engine, SR as usize);

        for ch in 0..4 {
            let peak = buf
                .iter()
                .skip(ch)
                .step_by(4)
                .fold(0.0f32, |p, s| p.max(s.abs()));
            assert!((peak - 0.5).abs() < 1e-3, "channel {ch} peak {peak}");
        }
        assert!((handle.snapshot().main_peak - 0.5).abs() < 1e-3);
    }

    #[test]
    fn tone_has_requested_frequency() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        handle.send(Command::SetToneFrequency(1000.0)).unwrap();
        handle.send(Command::SetToneEnabled(true)).unwrap();
        render(&mut engine, SR as usize / 10);
        let buf = render(&mut engine, SR as usize);

        let left: Vec<f32> = buf.iter().step_by(2).copied().collect();
        let rising = left
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        assert!((999..=1001).contains(&rising), "got {rising} cycles");
    }

    #[test]
    fn unused_channels_are_silent() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 6));
        handle.send(Command::SetToneEnabled(true)).unwrap();
        let buf = render(&mut engine, 4800);
        for frame in buf.chunks_exact(6) {
            assert_eq!(frame[4], 0.0);
            assert_eq!(frame[5], 0.0);
        }
    }

    #[test]
    fn toggling_off_ramps_to_silence_without_a_click() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        handle.send(Command::SetToneEnabled(true)).unwrap();
        render(&mut engine, 4800);
        handle.send(Command::ToggleTone).unwrap();
        let buf = render(&mut engine, 4800);

        let max_step = buf
            .windows(2)
            .step_by(2)
            .fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()));
        // A 440 Hz sine at gain 0.25 never moves more than ~0.015 per sample; an
        // instant cut would jump by up to the full amplitude.
        assert!(max_step < 0.05, "step {max_step}");
        assert!(buf[buf.len() - 200..].iter().all(|&s| s == 0.0));
        assert!(!handle.snapshot().tone_enabled);
    }

    #[test]
    fn recording_copies_the_main_bus_only_while_armed() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            mut recording,
            ..
        } = Engine::new(EngineConfig::new(SR, 4));
        handle.send(Command::SetToneEnabled(true)).unwrap();
        render(&mut engine, 1000);
        let mut recorded = Vec::new();
        assert_eq!(recording.drain_into(&mut recorded), 0, "not armed yet");

        recording.set_armed(true);
        // Longer than one render block, to cover the block boundaries.
        let buf = render(&mut engine, RENDER_BLOCK * 3 + 7);
        recording.set_armed(false);
        render(&mut engine, 1000);

        recording.drain_into(&mut recorded);
        let main: Vec<f32> = buf
            .chunks_exact(4)
            .flat_map(|frame| [frame[0], frame[1]])
            .collect();
        assert_eq!(recorded, main);
        assert!(recorded.iter().any(|&s| s != 0.0));
        assert_eq!(recording.dropped_frames(), 0);
    }

    #[test]
    fn short_buffer_is_silenced_not_panicking() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        handle.send(Command::SetToneEnabled(true)).unwrap();
        let mut buf = vec![1.0; 10];
        engine.process(&mut buf, 100);
        assert!(buf.iter().all(|&s| s == 0.0));
    }

    /// A complete track of constant `level` (DC passes the EQ crossovers at unity).
    fn dc_track(id: u64, level: f32) -> Arc<Track> {
        let (track, mut writer) = Track::new(TrackId(id), SR, SR as usize);
        writer.push_interleaved(&vec![level; SR as usize * 2]);
        writer.finish();
        track
    }

    fn play(handle: &mut EngineHandle, deck: DeckId) {
        handle
            .send(Command::deck(deck, DeckAction::PlayPause))
            .unwrap();
    }

    #[test]
    fn loaded_track_plays_on_main_and_pfl_puts_it_on_cue() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            mut tracks,
            ..
        } = Engine::new(EngineConfig::new(SR, 4));
        let track = dc_track(3, 0.5);
        tracks.send(DeckId::A, Arc::clone(&track)).unwrap();
        play(&mut handle, DeckId::A);

        let buf = render(&mut engine, 9600);
        let snap = handle.snapshot();
        assert_eq!(snap.deck(DeckId::A).track, Some(TrackId(3)));
        assert_eq!(snap.deck(DeckId::A).transport, Transport::Playing);
        assert_eq!(snap.deck(DeckId::B).track, None);
        for frame in buf.chunks_exact(4).skip(4800) {
            assert!((frame[0] - 0.5).abs() < 1e-3, "main {frame:?}");
            assert_eq!(&frame[2..], &[0.0, 0.0], "no PFL, silent cue");
        }

        handle
            .send(Command::Channel {
                deck: DeckId::A,
                control: ChannelControl::Pfl(true),
            })
            .unwrap();
        let buf = render(&mut engine, 4800);
        let last = &buf[buf.len() - 4..];
        assert!((last[2] - 0.5).abs() < 1e-3, "cue {last:?}");

        // Ejecting on the audio thread drops its reference; ours keeps the buffer alive.
        handle
            .send(Command::deck(DeckId::A, DeckAction::Eject))
            .unwrap();
        render(&mut engine, 64);
        assert_eq!(Arc::strong_count(&track), 1);
    }

    #[test]
    fn control_handle_commands_and_snapshots_work_alongside_ui() {
        let EngineParts {
            mut engine,
            mut ui,
            mut control,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        control
            .send(Command::Channel {
                deck: DeckId::B,
                control: ChannelControl::Fader(0.25),
            })
            .unwrap();
        ui.send(Command::Master(MasterControl::SplitCue(true)))
            .unwrap();
        render(&mut engine, 64);
        for snap in [ui.snapshot(), control.snapshot()] {
            assert_eq!(snap.channels[1].fader, 0.25);
            assert!(snap.master.split_cue);
        }
    }

    #[test]
    fn decks_mix_and_output_is_clamped() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            mut tracks,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        tracks.send(DeckId::A, dc_track(1, 0.75)).unwrap();
        tracks.send(DeckId::B, dc_track(2, 0.75)).unwrap();
        play(&mut handle, DeckId::A);
        play(&mut handle, DeckId::B);
        let buf = render(&mut engine, 9600);
        assert_eq!(buf[buf.len() - 1], 1.0);
        assert!(buf.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn per_deck_mode_sends_each_deck_raw_to_its_own_pair() {
        let mut config = EngineConfig::new(SR, 4);
        config.routing.mode = OutputMode::PerDeck;
        // Exact sample values: varispeed passes them through untouched.
        config.key_lock = false;
        let EngineParts {
            mut engine,
            ui: mut handle,
            mut tracks,
            ..
        } = Engine::new(config);
        tracks.send(DeckId::A, dc_track(1, 0.3)).unwrap();
        tracks.send(DeckId::B, dc_track(2, 0.6)).unwrap();
        play(&mut handle, DeckId::A);
        play(&mut handle, DeckId::B);
        // Software faders are bypassed in this mode.
        handle
            .send(Command::Channel {
                deck: DeckId::A,
                control: ChannelControl::Fader(0.0),
            })
            .unwrap();
        let buf = render(&mut engine, 1000);
        assert_eq!(&buf[buf.len() - 4..], &[0.3, 0.3, 0.6, 0.6]);
    }

    /// Two loaded decks with grids: A at 128 BPM, B at 124 BPM, both with a beat at
    /// frame 0. Key lock off (positions only matter here).
    fn synced_pair() -> (Engine, EngineHandle) {
        synced_pair_with(false)
    }

    fn synced_pair_with(key_lock: bool) -> (Engine, EngineHandle) {
        let mut config = EngineConfig::new(SR, 2);
        config.key_lock = key_lock;
        let EngineParts {
            mut engine,
            ui: mut handle,
            mut tracks,
            ..
        } = Engine::new(config);
        for (deck, id, bpm) in [(DeckId::A, 1, 128.0), (DeckId::B, 2, 124.0)] {
            let (track, mut writer) = Track::new(TrackId(id), SR, SR as usize * 120);
            writer.push_interleaved(&vec![0.1; SR as usize * 240]);
            writer.finish();
            tracks.send(deck, track).unwrap();
            render(&mut engine, 64);
            let grid = BeatGrid::from_bpm(bpm, 0.0, SR);
            handle
                .send(Command::deck(
                    deck,
                    DeckAction::SetBeatGrid {
                        track: TrackId(id),
                        grid: Some(grid),
                    },
                ))
                .unwrap();
        }
        render(&mut engine, 64);
        (engine, handle)
    }

    fn bpm(handle: &mut EngineHandle, deck: DeckId) -> f64 {
        handle.snapshot().deck(deck).bpm(SR).unwrap()
    }

    /// B's beat phase minus A's, in beats, wrapped to ±0.5.
    fn phase_gap(handle: &mut EngineHandle) -> f64 {
        let snap = handle.snapshot();
        let phase = |deck: DeckId| {
            let d = snap.deck(deck);
            d.grid.unwrap().phase(d.position as f64)
        };
        wrap_half(phase(DeckId::B) - phase(DeckId::A))
    }

    fn sync(handle: &mut EngineHandle, deck: DeckId, on: bool) {
        handle
            .send(Command::deck(deck, DeckAction::SetSyncLock(on)))
            .unwrap();
    }

    #[test]
    fn syncing_one_deck_makes_the_other_master_and_both_stay_synced() {
        let (mut engine, mut handle) = synced_pair();
        play(&mut handle, DeckId::A);
        play(&mut handle, DeckId::B);
        render(&mut engine, 5_000);
        sync(&mut handle, DeckId::B, true);
        render(&mut engine, 1_000);
        let snap = handle.snapshot();
        let (a, b) = (snap.deck(DeckId::A), snap.deck(DeckId::B));
        assert!(a.sync_lock && a.sync_master, "A synced as master");
        assert!(b.sync_lock && !b.sync_master, "B follows");
        assert!((bpm(&mut handle, DeckId::B) - 128.0).abs() < 0.01);
        assert!(
            phase_gap(&mut handle).abs() < 0.005,
            "{}",
            phase_gap(&mut handle)
        );

        // Syncing A again changes nothing; both stay on.
        sync(&mut handle, DeckId::A, true);
        render(&mut engine, 480);
        assert!(handle.snapshot().deck(DeckId::B).sync_lock);
    }

    #[test]
    fn tempo_moves_both_synced_decks_and_phase_holds() {
        let (mut engine, mut handle) = synced_pair();
        play(&mut handle, DeckId::A);
        play(&mut handle, DeckId::B);
        render(&mut engine, 5_000);
        sync(&mut handle, DeckId::B, true);
        render(&mut engine, 1_000);

        // From the master.
        handle
            .send(Command::deck(
                DeckId::A,
                DeckAction::SetTempo { fader: 0.25 },
            ))
            .unwrap();
        for _ in 0..200 {
            render(&mut engine, 480);
        }
        assert!((bpm(&mut handle, DeckId::A) - 130.56).abs() < 0.01);
        assert!((bpm(&mut handle, DeckId::B) - 130.56).abs() < 0.01);
        assert!(
            phase_gap(&mut handle).abs() < 0.01,
            "{}",
            phase_gap(&mut handle)
        );

        // From the follower: its fader steps from where it is, and the master follows.
        let b = *handle.snapshot().deck(DeckId::B);
        let fader = b.tempo_fader + 0.01 / b.tempo_range.fraction();
        handle
            .send(Command::deck(DeckId::B, DeckAction::SetTempo { fader }))
            .unwrap();
        render(&mut engine, 480);
        let expected = 124.0 * (b.tempo_rate as f64 + 0.01);
        assert!((bpm(&mut handle, DeckId::B) - expected).abs() < 0.05);
        assert!((bpm(&mut handle, DeckId::A) - expected).abs() < 0.05);

        // Pushed off the beat, the follower is pulled back without jumping.
        handle
            .send(Command::deck(DeckId::B, DeckAction::Nudge { amount: 0.1 }))
            .unwrap();
        render(&mut engine, 4_800);
        let pushed = phase_gap(&mut handle).abs();
        for _ in 0..300 {
            render(&mut engine, 480);
        }
        let settled = phase_gap(&mut handle).abs();
        assert!(
            pushed > 0.01 && settled < 0.01,
            "pushed {pushed}, settled {settled}"
        );
    }

    #[test]
    fn master_turning_sync_off_hands_over_and_keeps_tempo() {
        let (mut engine, mut handle) = synced_pair();
        play(&mut handle, DeckId::A);
        play(&mut handle, DeckId::B);
        sync(&mut handle, DeckId::B, true);
        render(&mut engine, 1_000);
        sync(&mut handle, DeckId::A, false);
        render(&mut engine, 480);
        let snap = handle.snapshot();
        assert!(!snap.deck(DeckId::A).sync_lock);
        assert!(snap.deck(DeckId::B).sync_lock && snap.deck(DeckId::B).sync_master);
        assert!(
            (bpm(&mut handle, DeckId::B) - 128.0).abs() < 0.01,
            "no tempo jump"
        );

        sync(&mut handle, DeckId::B, false);
        render(&mut engine, 480);
        assert!(!handle.snapshot().deck(DeckId::B).sync_lock);
        assert!(
            (bpm(&mut handle, DeckId::B) - 128.0).abs() < 0.01,
            "keeps its tempo"
        );
    }

    #[test]
    fn following_deck_starts_on_the_beat() {
        let (mut engine, mut handle) = synced_pair();
        sync(&mut handle, DeckId::B, true);
        play(&mut handle, DeckId::A);
        render(&mut engine, 7_777);
        // B is stopped off the beat; starting it lands on A's phase.
        handle
            .send(Command::deck(
                DeckId::B,
                DeckAction::SeekBy { seconds: 0.12 },
            ))
            .unwrap();
        render(&mut engine, 64);
        play(&mut handle, DeckId::B);
        render(&mut engine, 256);
        assert!(
            phase_gap(&mut handle).abs() < 0.005,
            "{}",
            phase_gap(&mut handle)
        );
    }

    #[test]
    fn sync_with_key_lock_on_changes_bpm() {
        let (mut engine, mut handle) = synced_pair_with(true);
        play(&mut handle, DeckId::A);
        play(&mut handle, DeckId::B);
        render(&mut engine, 4_800);
        handle
            .send(Command::deck(DeckId::A, DeckAction::ToggleSyncLock))
            .unwrap();
        render(&mut engine, 4_800);
        let snap = handle.snapshot();
        let a = snap.deck(DeckId::A);
        assert!(a.sync_lock, "A synced");
        assert!(
            (bpm(&mut handle, DeckId::A) - 124.0).abs() < 0.01,
            "A follows B's 124"
        );
        let before = handle.snapshot().deck(DeckId::A).position;
        render(&mut engine, 48_000);
        let moved = handle.snapshot().deck(DeckId::A).position - before;
        let expected = 48_000.0 * 124.0 / 128.0;
        assert!(
            (moved as f64 - expected).abs() < 200.0,
            "A plays at the synced rate: moved {moved}, expected {expected}"
        );
    }

    #[test]
    fn first_synced_deck_without_a_partner_is_master() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        sync(&mut handle, DeckId::A, true);
        render(&mut engine, 64);
        let a = *handle.snapshot().deck(DeckId::A);
        assert!(a.sync_lock && a.sync_master);
    }

    #[test]
    fn quantize_is_global() {
        let EngineParts {
            mut engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(SR, 2));
        handle.send(Command::ToggleQuantize).unwrap();
        render(&mut engine, 64);
        assert!(handle.snapshot().quantize);
    }

    #[test]
    fn commands_stay_small() {
        // They are copied through the lock-free queue on every send.
        assert!(
            std::mem::size_of::<Command>() <= 64,
            "{}",
            std::mem::size_of::<Command>()
        );
    }

    #[test]
    fn queue_full_returns_command() {
        let mut handle = Engine::new(EngineConfig::new(SR, 2)).ui;
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            handle.send(Command::ToggleTone).unwrap();
        }
        assert_eq!(
            handle.send(Command::SetToneGain(0.1)),
            Err(QueueFull(Command::SetToneGain(0.1)))
        );
    }
}
