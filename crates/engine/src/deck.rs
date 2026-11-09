//! One deck: a loaded track, transport (play/pause, CDJ-style CUE), tempo, key lock,
//! beatgrid and sync state, and the playhead.
//!
//! # RT safety
//!
//! Everything here except [`Deck::new`] runs on the audio thread and is **RT-safe**.
//! Replacing or ejecting a track drops an `Arc<Track>` here, which only decrements the
//! count: the loader keeps its own reference and frees tracks on its own thread.
//!
//! # Rendering
//!
//! The deck renders in sub-blocks of [`SUB_BLOCK`] frames. With key lock on, audio goes
//! through the [`TimeStretcher`], so tempo changes leave the pitch alone; with it off,
//! the playhead moves `rate` frames per output frame with cubic interpolation, so pitch
//! follows tempo like a turntable.
//!
//! The playhead (`position`) is always the frame you are *hearing*. Under key lock the
//! stretcher reads ahead of it by its latency; on every start or jump the stretcher is
//! reset and primed so its first output is exactly the playhead's frame.
//!
//! # De-clicking
//!
//! Starting, stopping, jumping and switching key lock never cut the signal abruptly.
//! The deck fades its output out over [`RAMP_SECONDS`], applies the change once
//! silent, then fades back in if it is still playing.

use std::sync::Arc;

use dj_stretch::TimeStretcher;

use crate::cues::{HOT_CUES, HotCue, LoopRegion, PendingWraps};
use crate::grid::{BeatGrid, wrap_half};
use crate::track::{Track, TrackId};

/// Fade time for starts, stops and jumps.
pub const RAMP_SECONDS: f32 = 0.004;
/// Time constant of the nudge (pitch bend) decay after the jog stops moving.
pub const NUDGE_DECAY_SECONDS: f32 = 0.1;
/// Largest rate offset a nudge can build up.
pub const NUDGE_MAX: f32 = 0.3;
/// Frames rendered between checks for pending jumps and rate changes.
pub const SUB_BLOCK: usize = 64;
/// Fastest playback the deck supports (tempo +50% plus a full nudge, with headroom).
const MAX_RATE: f64 = 2.0;
const MAX_INPUT: usize = (SUB_BLOCK as f64 * MAX_RATE) as usize + 2;

/// Which deck a command or snapshot refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckId {
    A,
    B,
}

impl DeckId {
    pub const ALL: [DeckId; 2] = [DeckId::A, DeckId::B];

    pub const fn index(self) -> usize {
        match self {
            DeckId::A => 0,
            DeckId::B => 1,
        }
    }

    pub const fn other(self) -> DeckId {
        match self {
            DeckId::A => DeckId::B,
            DeckId::B => DeckId::A,
        }
    }
}

/// Range of the tempo fader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempoRange {
    #[default]
    Percent8,
    Percent16,
    Percent50,
}

impl TempoRange {
    /// Rate change at the end of the fader, e.g. 0.08 for ±8%.
    pub fn fraction(self) -> f32 {
        match self {
            TempoRange::Percent8 => 0.08,
            TempoRange::Percent16 => 0.16,
            TempoRange::Percent50 => 0.5,
        }
    }

    pub fn next(self) -> Self {
        match self {
            TempoRange::Percent8 => TempoRange::Percent16,
            TempoRange::Percent16 => TempoRange::Percent50,
            TempoRange::Percent50 => TempoRange::Percent8,
        }
    }
}

/// Transport, tempo and sync actions on one deck.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeckAction {
    /// Play if stopped, pause if playing. While the CUE button is held, latches play
    /// so that releasing CUE keeps playing.
    PlayPause,
    /// CUE button down. Playing: return to the cue point and stop. Stopped: set the
    /// cue point here (on the nearest beat with quantize on) and preview from it while
    /// held.
    CuePress,
    /// CUE button up. Ends a preview: return to the cue point and stop.
    CueRelease,
    /// Move the playhead by this many seconds (negative is backwards).
    SeekBy {
        seconds: f32,
    },
    /// Move the playhead to `frame` (clamped to the track), e.g. from a click on the
    /// overview strip.
    JumpTo {
        frame: f64,
    },
    /// Set the cue point (and move the playhead) to `frame` for `track`, but only if
    /// that track is loaded and untouched: stopped, with playhead and cue still at the
    /// start. Sent after analysis to skip leading silence, like Rekordbox's auto cue.
    AutoCue {
        track: TrackId,
        frame: f64,
    },
    /// Unload the track.
    Eject,
    /// Hot cue pad down. Empty slot: store the playhead (or the active loop) there.
    /// Set slot, playing: jump to it (keeping the beat phase with quantize on).
    /// Set slot, stopped: play from it while held, like CUE.
    HotCuePress(usize),
    /// Hot cue pad up: ends a preview started by [`DeckAction::HotCuePress`].
    HotCueRelease(usize),
    HotCueClear(usize),
    /// Restore a saved hot cue for `track` (ignored if another track is loaded). One
    /// slot per command keeps commands small.
    SetHotCue {
        track: TrackId,
        slot: usize,
        cue: HotCue,
    },
    /// Mark the loop's start at the playhead.
    LoopIn,
    /// Mark the loop's end at the playhead and start looping.
    LoopOut,
    /// Loop `beats` from the playhead; if a loop is playing, leave it (4 BEAT / EXIT).
    AutoLoop {
        beats: f32,
    },
    LoopHalve,
    LoopDouble,
    /// Leave the loop, or go back into the last one.
    ReloopExit,
    /// Loop roll down: loop `beats` while the track carries on silently underneath.
    RollPress {
        beats: f32,
    },
    /// Loop roll up: play on from where the track would have been.
    RollRelease,
    /// Move by `beats` (negative is back), and the loop with it.
    BeatJump {
        beats: f32,
    },
    /// Tempo fader position, -1..1; positive is faster. Scaled by the tempo range.
    SetTempo {
        fader: f32,
    },
    SetTempoRange(TempoRange),
    CycleTempoRange,
    /// Pitch bend impulse from a jog wheel or button: adds to a temporary rate offset
    /// that decays over [`NUDGE_DECAY_SECONDS`]. Faster turning sends more impulses, so
    /// the bend follows the jog's speed.
    Nudge {
        amount: f32,
    },
    SetKeyLock(bool),
    ToggleKeyLock,
    /// The beatgrid for `track`; ignored if another track has been loaded since.
    SetBeatGrid {
        track: TrackId,
        grid: Option<BeatGrid>,
    },
    /// SYNC on or off. A synced deck follows the master deck's tempo and beat phase
    /// (or is the master). See the engine's `sync` module for the rules.
    SetSyncLock(bool),
    ToggleSyncLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    #[default]
    Stopped,
    Playing,
    /// Playing only while the CUE button is held.
    CuePreview,
}

impl Transport {
    pub fn is_moving(self) -> bool {
        match self {
            Transport::Stopped => false,
            Transport::Playing | Transport::CuePreview => true,
        }
    }
}

/// Deck state published to the UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeckSnapshot {
    pub track: Option<TrackId>,
    pub transport: Transport,
    /// Playhead, in frames at the engine sample rate.
    pub position: u64,
    pub cue: u64,
    /// Expected length while loading, exact once `loaded_fully`.
    pub length: u64,
    /// Frames decoded so far.
    pub ready: u64,
    pub loaded_fully: bool,
    /// Peak absolute output sample in the last callback.
    pub peak: f32,
    /// Tempo fader, -1..1.
    pub tempo_fader: f32,
    pub tempo_range: TempoRange,
    /// Playback rate before nudging (1.0 = original speed): from the tempo fader, or
    /// from the master deck while sync-locked.
    pub tempo_rate: f32,
    /// Current rate offset from nudging and sync phase correction.
    pub nudge: f32,
    pub key_lock: bool,
    /// SYNC is on.
    pub sync_lock: bool,
    /// This deck leads the sync (only meaningful with `sync_lock`).
    pub sync_master: bool,
    pub grid: Option<BeatGrid>,
    pub hot_cues: [HotCue; HOT_CUES],
    /// The current (or last) loop, and whether it is playing.
    pub loop_region: Option<LoopRegion>,
    pub loop_active: bool,
    /// A loop start waiting for LOOP OUT.
    pub loop_in: Option<f64>,
    /// While rolling: where the track would be without the roll.
    pub slip_position: Option<f64>,
}

impl Default for DeckSnapshot {
    fn default() -> Self {
        Self {
            track: None,
            transport: Transport::Stopped,
            position: 0,
            cue: 0,
            length: 0,
            ready: 0,
            loaded_fully: false,
            peak: 0.0,
            tempo_fader: 0.0,
            tempo_range: TempoRange::default(),
            tempo_rate: 1.0,
            nudge: 0.0,
            key_lock: true,
            sync_lock: false,
            sync_master: false,
            grid: None,
            hot_cues: [HotCue::Empty; HOT_CUES],
            loop_region: None,
            loop_active: false,
            loop_in: None,
            slip_position: None,
        }
    }
}

impl DeckSnapshot {
    /// Current BPM (grid tempo times playback rate), if the track has a grid.
    pub fn bpm(&self, sample_rate: u32) -> Option<f64> {
        self.grid
            .map(|grid| grid.bpm(sample_rate) * self.tempo_rate as f64)
    }
}

pub struct Deck {
    sample_rate: u32,
    track: Option<Arc<Track>>,
    transport: Transport,
    /// The frame being heard; fractional because of varispeed.
    position: f64,
    cue: u64,
    /// A jump waiting for the fade-out to finish.
    pending_seek: Option<f64>,
    /// A key lock switch waiting for the fade-out to finish.
    pending_key_lock: Option<bool>,
    gain: f32,
    ramp_step: f32,
    peak: f32,
    tempo_fader: f32,
    tempo_range: TempoRange,
    nudge: f32,
    nudge_decay: f32,
    /// Rate from the master deck while sync-locked.
    sync_rate: Option<f32>,
    /// Rate offset steering the beat phase onto the master's while sync-locked.
    phase_correction: f32,
    sync_lock: bool,
    grid: Option<BeatGrid>,
    quantize: bool,
    key_lock: bool,
    stretcher: Box<dyn TimeStretcher>,
    /// The stretcher must be reset and primed at `position` before its next use.
    restart_stretcher: bool,
    /// Next frame the stretcher reads (ahead of `position` by its latency).
    read_position: i64,
    /// Fractional input frames carried between sub-blocks.
    input_carry: f64,
    input: [Vec<f32>; 2],
    preroll: [Vec<f32>; 2],
    hot_cues: [HotCue; HOT_CUES],
    /// Hot cue being previewed (held on a stopped deck).
    hot_cue_preview: Option<usize>,
    loop_region: Option<LoopRegion>,
    loop_active: bool,
    loop_in: Option<f64>,
    slip: Option<Slip>,
    wraps: PendingWraps,
    // Track progress, loaded once per callback so the per-frame loop does fewer atomic
    // loads.
    block_ready: u64,
    block_len: u64,
    block_complete: bool,
}

impl Deck {
    /// Not RT-safe: allocates scratch buffers sized for `stretcher`.
    pub fn new(sample_rate: u32, stretcher: Box<dyn TimeStretcher>, key_lock: bool) -> Self {
        let sr = sample_rate as f32;
        let preroll = stretcher.max_preroll();
        Self {
            sample_rate,
            track: None,
            transport: Transport::Stopped,
            position: 0.0,
            cue: 0,
            pending_seek: None,
            pending_key_lock: None,
            gain: 0.0,
            ramp_step: 1.0 / (RAMP_SECONDS * sr).max(1.0),
            peak: 0.0,
            tempo_fader: 0.0,
            tempo_range: TempoRange::default(),
            nudge: 0.0,
            nudge_decay: (-1.0 / (NUDGE_DECAY_SECONDS * sr)).exp(),
            sync_rate: None,
            phase_correction: 0.0,
            sync_lock: false,
            grid: None,
            quantize: false,
            key_lock,
            stretcher,
            restart_stretcher: true,
            read_position: 0,
            input_carry: 0.0,
            input: [vec![0.0; MAX_INPUT], vec![0.0; MAX_INPUT]],
            preroll: [vec![0.0; preroll], vec![0.0; preroll]],
            hot_cues: [HotCue::Empty; HOT_CUES],
            hot_cue_preview: None,
            loop_region: None,
            loop_active: false,
            loop_in: None,
            slip: None,
            wraps: PendingWraps::default(),
            block_ready: 0,
            block_len: 0,
            block_complete: false,
        }
    }

    /// Replace the loaded track (or eject with `None`). Stops the deck and resets the
    /// playhead, cue point and beatgrid. Tempo, key lock and sync are kept, as on a CDJ.
    pub fn load(&mut self, track: Option<Arc<Track>>) {
        self.track = track;
        self.transport = Transport::Stopped;
        self.position = 0.0;
        self.cue = 0;
        self.pending_seek = None;
        self.gain = 0.0;
        self.nudge = 0.0;
        self.phase_correction = 0.0;
        self.grid = None;
        self.restart_stretcher = true;
        self.hot_cues = [HotCue::Empty; HOT_CUES];
        self.hot_cue_preview = None;
        self.loop_region = None;
        self.loop_active = false;
        self.loop_in = None;
        self.slip = None;
        self.wraps.clear();
    }

    pub fn apply(&mut self, action: DeckAction) {
        // These work on an empty deck too: the controls are physical.
        match action {
            DeckAction::SetTempo { fader } => {
                self.tempo_fader = fader.clamp(-1.0, 1.0);
                return;
            }
            DeckAction::SetTempoRange(range) => {
                self.tempo_range = range;
                return;
            }
            DeckAction::CycleTempoRange => {
                self.tempo_range = self.tempo_range.next();
                return;
            }
            DeckAction::SetKeyLock(on) => {
                self.set_key_lock(on);
                return;
            }
            DeckAction::ToggleKeyLock => {
                let on = !self.pending_key_lock.unwrap_or(self.key_lock);
                self.set_key_lock(on);
                return;
            }
            // Sync is coordinated by the engine, which sees both decks.
            DeckAction::SetSyncLock(_) | DeckAction::ToggleSyncLock => {
                return;
            }
            _ => {}
        }
        let Some(track) = &self.track else {
            return;
        };
        match action {
            DeckAction::PlayPause => match self.transport {
                Transport::Stopped => self.transport = Transport::Playing,
                Transport::Playing => {
                    let here = self.target_position();
                    self.stop_at(here);
                }
                Transport::CuePreview => {
                    self.transport = Transport::Playing;
                    self.hot_cue_preview = None;
                }
            },
            DeckAction::CuePress => match self.transport {
                Transport::Playing => self.stop_at(self.cue as f64),
                Transport::Stopped => {
                    let mut here = self.target_position();
                    if let (true, Some(grid)) = (self.quantize, self.grid) {
                        here = grid.snap(here).max(0.0);
                    }
                    self.cue = here.round() as u64;
                    self.jump(self.cue as f64);
                    self.transport = Transport::CuePreview;
                }
                Transport::CuePreview => {}
            },
            DeckAction::CueRelease => {
                if self.transport == Transport::CuePreview {
                    self.stop_at(self.cue as f64);
                }
            }
            DeckAction::SeekBy { seconds } => {
                let mut delta = (seconds as f64 * self.sample_rate as f64).round();
                // Quantized: move by whole bars (4 beats) so the beat phase is kept.
                if let (true, Some(grid)) = (self.quantize, self.grid) {
                    let bars = (delta / grid.period / 4.0).round();
                    let bars = match bars == 0.0 {
                        true => delta.signum(),
                        false => bars,
                    };
                    delta = bars * 4.0 * grid.period;
                }
                let max = self.length().saturating_sub(1) as f64;
                let to = (self.target_position() + delta).clamp(0.0, max);
                self.jump(to);
            }
            DeckAction::JumpTo { frame } => {
                let max = self.length().saturating_sub(1) as f64;
                self.jump(frame.clamp(0.0, max));
            }
            DeckAction::AutoCue { track: id, frame } => {
                let untouched = self.transport == Transport::Stopped
                    && self.cue == 0
                    && self.target_position() == 0.0;
                if track.id() == id && untouched {
                    let max = self.length().saturating_sub(1) as f64;
                    let frame = frame.clamp(0.0, max);
                    self.cue = frame.round() as u64;
                    self.jump(self.cue as f64);
                }
            }
            DeckAction::Eject => self.load(None),
            DeckAction::HotCuePress(slot) => self.hot_cue_press(slot),
            DeckAction::HotCueRelease(slot) => {
                if self.hot_cue_preview == Some(slot) {
                    self.hot_cue_preview = None;
                    if self.transport == Transport::CuePreview {
                        let back = self.hot_cues[slot].position().unwrap_or(self.position);
                        self.stop_at(back);
                    }
                }
            }
            DeckAction::HotCueClear(slot) => {
                if let Some(cue) = self.hot_cues.get_mut(slot) {
                    *cue = HotCue::Empty;
                }
            }
            DeckAction::SetHotCue {
                track: id,
                slot,
                cue,
            } => {
                if track.id() == id && slot < HOT_CUES {
                    self.hot_cues[slot] = cue;
                }
            }
            DeckAction::LoopIn => self.loop_in = Some(self.quantized(self.target_position())),
            DeckAction::LoopOut => self.loop_out(),
            DeckAction::AutoLoop { beats } => match self.loop_active {
                true => self.loop_active = false,
                false => self.auto_loop(beats as f64),
            },
            DeckAction::LoopHalve => self.resize_loop(0.5),
            DeckAction::LoopDouble => self.resize_loop(2.0),
            DeckAction::ReloopExit => self.reloop_exit(),
            DeckAction::RollPress { beats } => self.roll_press(beats as f64),
            DeckAction::RollRelease => self.roll_release(),
            DeckAction::BeatJump { beats } => {
                let delta = beats as f64 * self.beat_period();
                if let (true, Some(region)) = (self.loop_active, self.loop_region) {
                    self.loop_region = LoopRegion::new(region.start + delta, region.end + delta);
                }
                self.shift_by(delta);
            }
            DeckAction::Nudge { amount } => {
                self.nudge = (self.nudge + amount).clamp(-NUDGE_MAX, NUDGE_MAX);
            }
            DeckAction::SetBeatGrid { track: id, grid } => {
                if track.id() == id {
                    self.grid = grid;
                }
            }
            DeckAction::SetTempo { .. }
            | DeckAction::SetTempoRange(_)
            | DeckAction::CycleTempoRange
            | DeckAction::SetKeyLock(_)
            | DeckAction::ToggleKeyLock
            | DeckAction::SetSyncLock(_)
            | DeckAction::ToggleSyncLock => {}
        }
    }

    /// Playback rate set by the tempo fader.
    pub fn fader_rate(&self) -> f32 {
        1.0 + self.tempo_fader * self.tempo_range.fraction()
    }

    /// Playback rate before nudging: the master's while sync-locked, else the fader's.
    pub fn tempo_rate(&self) -> f32 {
        match (self.sync_lock, self.sync_rate) {
            (true, Some(rate)) => rate,
            _ => self.fader_rate(),
        }
    }

    pub fn grid(&self) -> Option<BeatGrid> {
        self.grid
    }

    pub fn is_moving(&self) -> bool {
        self.transport.is_moving()
    }

    pub fn has_track(&self) -> bool {
        self.track.is_some()
    }

    pub fn sync_lock(&self) -> bool {
        self.sync_lock
    }

    /// The frame being heard, or about to be once a pending jump lands.
    pub fn target_position(&self) -> f64 {
        self.pending_seek.unwrap_or(self.position)
    }

    pub fn set_quantize(&mut self, on: bool) {
        self.quantize = on;
    }

    /// Turn sync lock on or off (the engine makes sure only one deck follows). Turning
    /// it off moves the tempo fader to the synced rate, so the tempo doesn't jump.
    pub fn set_sync_lock(&mut self, on: bool) {
        if self.sync_lock && !on {
            let range = self.tempo_range.fraction();
            self.tempo_fader = ((self.tempo_rate() - 1.0) / range).clamp(-1.0, 1.0);
            self.sync_rate = None;
        }
        self.sync_lock = on;
        self.phase_correction = 0.0;
    }

    /// While following: the rate to follow, and the phase-steering rate offset. The
    /// tempo fader follows the rate too, so tempo steps start from where the deck is.
    pub fn set_sync_rates(&mut self, rate: Option<f32>, phase_correction: f32) {
        self.sync_rate = rate;
        self.phase_correction = phase_correction;
        if let Some(rate) = rate {
            self.set_rate_with_fader(rate);
        }
    }

    /// Stop following and lead at the current tempo (SYNC stays on).
    pub fn become_master(&mut self) {
        let rate = self.tempo_rate();
        self.sync_rate = None;
        self.phase_correction = 0.0;
        self.set_rate_with_fader(rate);
    }

    /// Move the tempo fader so the rate is `rate`, within the current range.
    pub fn set_rate_with_fader(&mut self, rate: f32) {
        let range = self.tempo_range.fraction();
        self.tempo_fader = ((rate - 1.0) / range).clamp(-1.0, 1.0);
    }

    /// Output frames until a jump requested now takes effect: the fade-out, rounded up
    /// to whole sub-blocks (pending jumps land at sub-block starts).
    pub fn jump_delay(&self) -> f64 {
        match self.gain == 0.0 {
            true => 0.0,
            false => {
                let fade = (self.gain / self.ramp_step).ceil() as usize;
                fade.div_ceil(SUB_BLOCK) as f64 * SUB_BLOCK as f64
            }
        }
    }

    /// Jump so that, once the jump lands, the playhead is `delta` frames ahead of where
    /// it would have been without jumping. Use it to line up with another deck that
    /// keeps playing during the fade.
    pub fn shift_in_time(&mut self, delta: f64) {
        let rate = (self.tempo_rate() + self.nudge + self.phase_correction) as f64;
        self.shift_by(delta + self.jump_delay() * rate);
    }

    /// Jump (with the usual fade) so that the playhead is `delta` frames further on.
    pub fn shift_by(&mut self, delta: f64) {
        let max = self.length().saturating_sub(1) as f64;
        let to = (self.target_position() + delta).clamp(0.0, max);
        self.jump(to);
    }

    /// Peak output of the last [`render`](Self::render) call.
    pub fn snapshot_peak(&self) -> f32 {
        self.peak
    }

    pub fn snapshot(&self) -> DeckSnapshot {
        let common = DeckSnapshot {
            tempo_fader: self.tempo_fader,
            tempo_range: self.tempo_range,
            tempo_rate: self.tempo_rate(),
            nudge: self.nudge + self.phase_correction,
            key_lock: self.pending_key_lock.unwrap_or(self.key_lock),
            sync_lock: self.sync_lock,
            grid: self.grid,
            hot_cues: self.hot_cues,
            loop_region: self.loop_region,
            loop_active: self.loop_active,
            loop_in: self.loop_in,
            slip_position: self.slip.map(|s| s.position),
            ..DeckSnapshot::default()
        };
        match &self.track {
            None => common,
            Some(track) => DeckSnapshot {
                track: Some(track.id()),
                transport: self.transport,
                position: self.target_position().max(0.0) as u64,
                cue: self.cue,
                length: track.len_frames(),
                ready: track.ready_frames(),
                loaded_fully: track.is_complete(),
                peak: self.peak,
                ..common
            },
        }
    }

    /// Render into `left` and `right` (equal lengths) and advance the playhead.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        self.peak = 0.0;
        if let Some(track) = &self.track {
            // Read `complete` before `ready`: once complete, `ready` is final.
            self.block_complete = track.is_complete();
            self.block_ready = track.ready_frames();
            self.block_len = track.len_frames();
        }
        for (l, r) in left.chunks_mut(SUB_BLOCK).zip(right.chunks_mut(SUB_BLOCK)) {
            self.render_sub_block(l, r);
        }
    }

    fn render_sub_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        self.nudge *= self.nudge_decay.powi(n as i32);
        let moving = self.transport.is_moving();
        if self.track.is_none() {
            left.fill(0.0);
            right.fill(0.0);
            return;
        }

        if self.gain == 0.0 {
            if let Some(to) = self.pending_seek.take() {
                self.position = to;
            }
            if let Some(on) = self.pending_key_lock.take() {
                self.key_lock = on;
            }
            if !moving {
                left.fill(0.0);
                right.fill(0.0);
                return;
            }
            // Starting from silence: whatever the stretcher held is stale.
            self.restart_stretcher = true;
        }

        let rate = (self.tempo_rate() + self.nudge + self.phase_correction)
            .clamp(0.0, MAX_RATE as f32 - 0.01) as f64;
        let advance = match self.key_lock {
            true => self.read_stretched(&mut left[..n], &mut right[..n], rate),
            false => self.read_varispeed(&mut left[..n], &mut right[..n], rate),
        };
        // While rolling, the track carries on underneath.
        if let Some(slip) = &mut self.slip {
            slip.position += advance;
        }

        let target = match moving && self.pending_seek.is_none() && self.pending_key_lock.is_none()
        {
            true => 1.0,
            false => 0.0,
        };
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let delta = target - self.gain;
            if delta.abs() <= self.ramp_step {
                self.gain = target;
            } else {
                self.gain += self.ramp_step * delta.signum();
            }
            *l *= self.gain;
            *r *= self.gain;
            self.peak = self.peak.max(l.abs()).max(r.abs());
        }

        if self.block_complete && self.position >= self.block_len as f64 {
            self.transport = Transport::Stopped;
            self.position = self.block_len as f64;
            self.pending_seek = None;
            self.gain = 0.0;
        }
    }

    /// The loop that playback wraps around, if one is playing.
    fn active_loop(&self) -> Option<LoopRegion> {
        match self.loop_active {
            true => self.loop_region,
            false => None,
        }
    }

    /// Returns how far the track advanced (before loop wraps), in frames.
    fn read_varispeed(&mut self, left: &mut [f32], right: &mut [f32], rate: f64) -> f64 {
        let Some(track) = &self.track else {
            return 0.0;
        };
        let active = self.active_loop();
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            (*l, *r) = read_interpolated(track, self.position, self.block_ready);
            let before = self.position;
            self.position += rate;
            if let Some(region) = active {
                if before < region.end && self.position >= region.end {
                    self.position -= region.len();
                }
            }
        }
        left.len() as f64 * rate
    }

    /// Returns how far the track advanced (before loop wraps), in frames.
    fn read_stretched(&mut self, left: &mut [f32], right: &mut [f32], rate: f64) -> f64 {
        if self.restart_stretcher {
            self.restart(rate);
        }
        let Some(track) = &self.track else {
            return 0.0;
        };
        let active = self.active_loop();
        let wanted = left.len() as f64 * rate + self.input_carry;
        let count = (wanted.floor() as usize).min(MAX_INPUT);
        self.input_carry = wanted - count as f64;
        let [in_l, in_r] = &mut self.input;
        fill_looped(
            track,
            self.read_position,
            self.block_ready,
            active,
            &mut in_l[..count],
            &mut in_r[..count],
        );
        self.stretcher
            .process((&in_l[..count], &in_r[..count]), (left, right));
        // The input wraps now; what you hear wraps at the same point once the
        // stretcher's latency has passed.
        let before = self.read_position;
        self.read_position += count as i64;
        if let Some(region) = active {
            let (end, len) = (region.end.round() as i64, region.len().round() as i64);
            if before < end && self.read_position >= end {
                self.read_position -= len;
                self.wraps.push(region.end, len as f64);
            }
        }
        let before = self.position;
        self.position += count as f64;
        if let Some((end, len)) = self.wraps.front() {
            if before < end && self.position >= end {
                self.position -= len;
                self.wraps.pop();
            }
        }
        count as f64
    }

    /// Reset the stretcher and prime it so its next output is the playhead's frame.
    fn restart(&mut self, rate: f64) {
        self.restart_stretcher = false;
        self.input_carry = 0.0;
        self.wraps.clear();
        self.stretcher.reset();
        let Some(track) = &self.track else {
            return;
        };
        // After priming with input ending at frame E, the stretcher's processing point
        // is E - input_latency, and output lags that by output_latency output frames
        // (output_latency * rate input frames). Choose E so the first output frame is
        // `position`.
        let input_latency = self.stretcher.input_latency() as f64;
        let output_latency = self.stretcher.output_latency() as f64 * rate;
        let end = (self.position + output_latency + input_latency).round() as i64;
        let [pre_l, pre_r] = &mut self.preroll;
        let length = pre_l.len();
        fill_from_track(track, end - length as i64, self.block_ready, pre_l, pre_r);
        self.stretcher
            .seek(&pre_l[..length], &pre_r[..length], rate);
        self.read_position = end;
        // Starting just before a loop's end: the input is already past it.
        if let Some(region) = self.active_loop() {
            let (loop_end, len) = (region.end.round() as i64, region.len().round() as i64);
            if self.position < region.end && end >= loop_end {
                self.read_position -= len;
                self.wraps.push(region.end, len as f64);
            }
        }
    }

    /// Frames per beat: from the grid, or 120 BPM without one.
    fn beat_period(&self) -> f64 {
        match self.grid {
            Some(grid) => grid.period,
            None => self.sample_rate as f64 / 2.0,
        }
    }

    /// `frame` on the nearest beat with quantize on and a grid, else unchanged.
    fn quantized(&self, frame: f64) -> f64 {
        match (self.quantize, self.grid) {
            (true, Some(grid)) => grid.snap(frame).max(0.0),
            _ => frame,
        }
    }

    /// Jump to `target`; with quantize on, keep the beat phase the playhead has now
    /// (so a playing deck stays on the beat), allowing for the fade before the jump.
    fn jump_in_phase(&mut self, target: f64) {
        let (true, Some(grid), true) = (self.quantize, self.grid, self.transport.is_moving())
        else {
            self.jump(target);
            return;
        };
        let rate = (self.tempo_rate() + self.nudge + self.phase_correction) as f64;
        let landing = self.target_position() + self.jump_delay() * rate;
        let offset = wrap_half(grid.phase(landing) - grid.phase(target)) * grid.period;
        self.jump((target + offset).max(0.0));
    }

    fn hot_cue_press(&mut self, slot: usize) {
        let Some(cue) = self.hot_cues.get(slot).copied() else {
            return;
        };
        match cue {
            HotCue::Empty => {
                self.hot_cues[slot] = match self.active_loop() {
                    Some(region) => HotCue::Loop(region),
                    None => HotCue::Point(self.quantized(self.target_position())),
                };
            }
            HotCue::Point(_) | HotCue::Loop(_) => {
                if let HotCue::Loop(region) = cue {
                    self.loop_region = Some(region);
                    self.loop_active = true;
                }
                let target = cue.position().unwrap_or(0.0);
                match self.transport {
                    Transport::Playing => self.jump_in_phase(target),
                    Transport::Stopped | Transport::CuePreview => {
                        self.jump(target);
                        self.transport = Transport::CuePreview;
                        self.hot_cue_preview = Some(slot);
                    }
                }
            }
        }
    }

    fn loop_out(&mut self) {
        let here = self.quantized(self.target_position());
        let start = self
            .loop_in
            .or(self.active_loop().map(|region| region.start));
        let Some(region) = start.and_then(|start| LoopRegion::new(start, here)) else {
            return;
        };
        self.loop_region = Some(region);
        self.loop_active = true;
        self.loop_in = None;
        // A snapped end can be just behind the playhead: wrap by hand then.
        let position = self.target_position();
        if position >= region.end {
            self.jump(region.start + (position - region.end));
        }
    }

    fn auto_loop(&mut self, beats: f64) {
        let start = self.quantized(self.target_position());
        if let Some(region) = LoopRegion::new(start, start + beats * self.beat_period()) {
            self.loop_region = Some(region);
            self.loop_active = true;
            self.loop_in = None;
        }
    }

    fn resize_loop(&mut self, factor: f64) {
        let Some(region) = self.loop_region else {
            return;
        };
        let Some(resized) = LoopRegion::new(region.start, region.start + region.len() * factor)
        else {
            return;
        };
        self.loop_region = Some(resized);
        // Halving can leave the playhead past the new end: wrap it into the loop.
        let position = self.target_position();
        if self.loop_active && position >= resized.end {
            let into = (position - resized.start).rem_euclid(resized.len());
            self.jump(resized.start + into);
        }
    }

    fn reloop_exit(&mut self) {
        if self.loop_active {
            self.loop_active = false;
            return;
        }
        let Some(region) = self.loop_region else {
            return;
        };
        self.loop_active = true;
        if !region.contains(self.target_position()) {
            self.jump_in_phase(region.start);
        }
    }

    fn roll_press(&mut self, beats: f64) {
        let position = self.target_position();
        let len = beats * self.beat_period();
        // Start on the current division of the beat, so the roll stays on the grid.
        let start = match self.grid {
            Some(grid) => grid.anchor + ((position - grid.anchor) / len).floor() * len,
            None => position,
        };
        let Some(region) = LoopRegion::new(start.max(0.0), start.max(0.0) + len) else {
            return;
        };
        if self.slip.is_none() {
            self.slip = Some(Slip {
                position,
                saved_loop: self.loop_region,
                saved_active: self.loop_active,
            });
        }
        self.loop_region = Some(region);
        self.loop_active = true;
    }

    fn roll_release(&mut self) {
        let Some(slip) = self.slip.take() else {
            return;
        };
        self.loop_region = slip.saved_loop;
        self.loop_active = slip.saved_active;
        // Rejoin the track where it would be once the jump lands.
        let rate = (self.tempo_rate() + self.nudge + self.phase_correction) as f64;
        let target = slip.position + self.jump_delay() * rate;
        if (target - self.target_position()).abs() > 1.0 {
            self.jump(target);
        }
    }

    fn set_key_lock(&mut self, on: bool) {
        if on == self.pending_key_lock.unwrap_or(self.key_lock) {
            return;
        }
        if self.gain == 0.0 {
            self.key_lock = on;
            self.pending_key_lock = None;
            self.restart_stretcher = true;
        } else {
            self.pending_key_lock = Some(on);
        }
    }

    fn length(&self) -> u64 {
        match &self.track {
            Some(track) => track.len_frames(),
            None => 0,
        }
    }

    fn stop_at(&mut self, position: f64) {
        self.transport = Transport::Stopped;
        self.jump(position);
    }

    fn jump(&mut self, to: f64) {
        if self.gain == 0.0 {
            self.position = to;
            self.pending_seek = None;
            self.restart_stretcher = true;
        } else {
            self.pending_seek = Some(to);
        }
    }
}

/// While rolling: the position the track would have without the roll, and the loop to
/// restore afterwards.
#[derive(Debug, Clone, Copy)]
struct Slip {
    position: f64,
    saved_loop: Option<LoopRegion>,
    saved_active: bool,
}

/// Like [`fill_from_track`], but frames at or past the end of `loop_region` come from
/// its start again (only when the read starts before the end).
fn fill_looped(
    track: &Track,
    start: i64,
    ready: u64,
    loop_region: Option<LoopRegion>,
    left: &mut [f32],
    right: &mut [f32],
) {
    let Some(region) = loop_region else {
        fill_from_track(track, start, ready, left, right);
        return;
    };
    let (end, len) = (region.end.round() as i64, region.len().round() as i64);
    for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
        let mut frame = start + i as i64;
        if start < end && frame >= end {
            frame -= len;
        }
        (*l, *r) = match frame >= 0 && (frame as u64) < ready {
            true => track.frame_unchecked_ready(frame as u64),
            false => (0.0, 0.0),
        };
    }
}

/// Copy frames `start..start + out.len()` of `track` into `left`/`right`; frames before
/// the start, past `ready` or past the end are silence.
fn fill_from_track(track: &Track, start: i64, ready: u64, left: &mut [f32], right: &mut [f32]) {
    for (i, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
        let frame = start + i as i64;
        (*l, *r) = match frame >= 0 && (frame as u64) < ready {
            true => track.frame_unchecked_ready(frame as u64),
            false => (0.0, 0.0),
        };
    }
}

/// Read the frame at fractional `position` with 4-point cubic (Catmull-Rom)
/// interpolation. Frames not decoded yet (at or past `ready`) read as silence.
#[inline]
fn read_interpolated(track: &Track, position: f64, ready: u64) -> (f32, f32) {
    let index = position.floor();
    let frac = (position - index) as f32;
    let index = index as i64;
    let frame = |i: i64| -> (f32, f32) {
        match i >= 0 && (i as u64) < ready {
            true => track.frame_unchecked_ready(i as u64),
            false => (0.0, 0.0),
        }
    };
    let y1 = frame(index);
    if frac == 0.0 {
        return y1;
    }
    let y0 = frame(index - 1);
    let y2 = frame(index + 1);
    let y3 = frame(index + 2);
    (
        catmull_rom(y0.0, y1.0, y2.0, y3.0, frac),
        catmull_rom(y0.1, y1.1, y2.1, y3.1, frac),
    )
}

#[inline]
fn catmull_rom(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

#[cfg(test)]
mod tests {
    use super::*;
    use dj_stretch::{Quality, Signalsmith};

    const SR: u32 = 48_000;
    const RAMP: usize = (RAMP_SECONDS * SR as f32) as usize + 2;

    /// A complete track whose left sample at frame `i` is `i` and right is `-i`, so the
    /// output shows exactly which frame played.
    fn ramp_track(frames: usize) -> Arc<Track> {
        let (track, mut writer) = Track::new(TrackId(7), SR, frames);
        let samples: Vec<f32> = (0..frames).flat_map(|i| [i as f32, -(i as f32)]).collect();
        writer.push_interleaved(&samples);
        writer.finish();
        track
    }

    fn new_deck(key_lock: bool) -> Deck {
        Deck::new(
            SR,
            Box::new(Signalsmith::new(SR, Quality::Default)),
            key_lock,
        )
    }

    /// A varispeed deck (key lock off) with a ramp track, so output samples are exact
    /// frame numbers.
    fn loaded(frames: usize) -> Deck {
        let mut deck = new_deck(false);
        deck.load(Some(ramp_track(frames)));
        deck
    }

    fn run(deck: &mut Deck, frames: usize) -> Vec<(f32, f32)> {
        let mut left = vec![0.0; frames];
        let mut right = vec![0.0; frames];
        // Render in callback-sized pieces, as the engine does.
        for (l, r) in left.chunks_mut(128).zip(right.chunks_mut(128)) {
            deck.render(l, r);
        }
        left.into_iter().zip(right).collect()
    }

    #[test]
    fn empty_deck_is_silent_and_ignores_transport() {
        let mut deck = new_deck(true);
        deck.apply(DeckAction::PlayPause);
        assert!(run(&mut deck, 100).iter().all(|&f| f == (0.0, 0.0)));
        assert_eq!(deck.snapshot(), DeckSnapshot::default());
    }

    #[test]
    fn stopped_deck_is_silent_and_does_not_move() {
        let mut deck = loaded(1000);
        assert!(run(&mut deck, 100).iter().all(|&f| f == (0.0, 0.0)));
        assert_eq!(deck.snapshot().position, 0);
    }

    #[test]
    fn play_fades_in_then_plays_frames_in_order() {
        let mut deck = loaded(10_000);
        deck.apply(DeckAction::PlayPause);
        let out = run(&mut deck, 1000);
        // Fading in: quieter than the source.
        assert!(out[10].0 < 10.0 && out[10].0 > 0.0);
        // After the fade: exact source frames.
        for (i, &(l, r)) in out.iter().enumerate().skip(RAMP) {
            assert_eq!((l, r), (i as f32, -(i as f32)));
        }
        assert_eq!(deck.snapshot().position, 1000);
        assert_eq!(deck.snapshot().transport, Transport::Playing);
    }

    #[test]
    fn pause_fades_out_and_keeps_position_of_the_press() {
        let mut deck = loaded(10_000);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 1000);
        deck.apply(DeckAction::PlayPause);
        assert_eq!(deck.snapshot().position, 1000);
        let out = run(&mut deck, 1000);
        assert!(out[0].0 > 0.0, "fades rather than cutting");
        assert!(out[RAMP..].iter().all(|&f| f == (0.0, 0.0)));
        assert_eq!(deck.snapshot().position, 1000);
        assert_eq!(deck.snapshot().transport, Transport::Stopped);
    }

    #[test]
    fn cue_while_stopped_sets_cue_and_previews_while_held() {
        let mut deck = loaded(10_000);
        deck.apply(DeckAction::SeekBy { seconds: 0.01 }); // frame 480
        deck.apply(DeckAction::CuePress);
        assert_eq!(deck.snapshot().cue, 480);
        assert_eq!(deck.snapshot().transport, Transport::CuePreview);
        let out = run(&mut deck, 500);
        assert_eq!(out[499].0, 979.0);

        deck.apply(DeckAction::CueRelease);
        run(&mut deck, 500);
        let snap = deck.snapshot();
        assert_eq!(snap.transport, Transport::Stopped);
        assert_eq!(snap.position, 480, "returns to the cue point");
    }

    #[test]
    fn cue_while_playing_returns_to_cue_and_stops() {
        let mut deck = loaded(10_000);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 2000);
        deck.apply(DeckAction::CuePress);
        run(&mut deck, 500);
        deck.apply(DeckAction::CueRelease);
        let snap = deck.snapshot();
        assert_eq!(snap.transport, Transport::Stopped);
        assert_eq!(snap.position, 0);
        assert_eq!(snap.cue, 0);
    }

    #[test]
    fn play_during_cue_preview_keeps_playing_after_release() {
        let mut deck = loaded(10_000);
        deck.apply(DeckAction::CuePress);
        run(&mut deck, 300);
        deck.apply(DeckAction::PlayPause);
        deck.apply(DeckAction::CueRelease);
        let out = run(&mut deck, 300);
        assert_eq!(deck.snapshot().transport, Transport::Playing);
        // No fade or jump: playback just continues.
        assert_eq!(out[0].0, 300.0);
        assert_eq!(deck.snapshot().position, 600);
    }

    #[test]
    fn jump_while_playing_fades_out_jumps_and_fades_in() {
        let mut deck = loaded(100_000);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 1000);
        deck.apply(DeckAction::SeekBy { seconds: 1.0 });
        assert_eq!(deck.snapshot().position, 49_000);
        let out = run(&mut deck, 2000);
        let max_step = out
            .windows(2)
            .map(|w| (w[1].0 - w[0].0).abs())
            .fold(0.0f32, f32::max);
        // The source rises by 1.0 per frame. A cut from ~1000 to 49000 would step by
        // ~48000; the fades keep each step to a fraction of the level.
        assert!(max_step < 49_000.0 / RAMP as f32 * 2.0, "step {max_step}");
        let (last, _) = out[1999];
        assert!((49_000.0..51_000.0).contains(&last), "last {last}");
    }

    #[test]
    fn seek_clamps_to_track() {
        let mut deck = loaded(1000);
        deck.apply(DeckAction::SeekBy { seconds: -5.0 });
        assert_eq!(deck.snapshot().position, 0);
        deck.apply(DeckAction::SeekBy { seconds: 5.0 });
        assert_eq!(deck.snapshot().position, 999);
    }

    #[test]
    fn stops_at_end_of_track() {
        let mut deck = loaded(500);
        deck.apply(DeckAction::PlayPause);
        let out = run(&mut deck, 1000);
        assert!(out[500..].iter().all(|&f| f == (0.0, 0.0)));
        let snap = deck.snapshot();
        assert_eq!(snap.transport, Transport::Stopped);
        assert_eq!(snap.position, 500);
    }

    #[test]
    fn plays_silence_past_decoded_frames_while_loading() {
        let (track, mut writer) = Track::new(TrackId(1), SR, 1000);
        writer.push_interleaved(&[0.5; 200]); // 100 frames
        let mut deck = new_deck(false);
        deck.load(Some(Arc::clone(&track)));
        deck.apply(DeckAction::PlayPause);
        let out = run(&mut deck, 300);
        assert_eq!(out[150], (0.0, 0.0));
        assert_eq!(deck.snapshot().transport, Transport::Playing);
        assert_eq!(deck.snapshot().position, 300);
        assert!(!deck.snapshot().loaded_fully);
    }

    #[test]
    fn tempo_fader_changes_speed_and_survives_loads() {
        let mut deck = loaded(100_000);
        deck.apply(DeckAction::SetTempo { fader: 1.0 });
        assert!((deck.snapshot().tempo_rate - 1.08).abs() < 1e-6);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 10_000);
        assert_eq!(deck.snapshot().position, 10_800);
        // The ramp source is linear, so interpolated output stays exactly on it.
        let out = run(&mut deck, 1000);
        let (l, _) = out[500];
        assert!((l - (10_800.0 + 500.0 * 1.08)).abs() < 0.01, "{l}");

        deck.apply(DeckAction::CycleTempoRange);
        assert!((deck.snapshot().tempo_rate - 1.16).abs() < 1e-6);
        deck.load(Some(ramp_track(10)));
        assert_eq!(deck.snapshot().tempo_range, TempoRange::Percent16);
    }

    #[test]
    fn tempo_works_on_an_empty_deck() {
        let mut deck = new_deck(false);
        deck.apply(DeckAction::SetTempo { fader: -0.5 });
        assert!((deck.snapshot().tempo_rate - 0.96).abs() < 1e-6);
    }

    #[test]
    fn nudge_speeds_up_briefly_then_decays() {
        let mut deck = loaded(100_000);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 1000);
        deck.apply(DeckAction::Nudge { amount: 0.1 });
        run(&mut deck, 1000);
        let nudged = deck.snapshot().position;
        assert!(nudged > 2000 + 50, "nudge moved ahead: {nudged}");
        run(&mut deck, SR as usize);
        assert!(deck.snapshot().nudge.abs() < 1e-4);
    }

    #[test]
    fn catmull_rom_hits_samples_and_lines() {
        assert_eq!(catmull_rom(0.0, 1.0, 2.0, 3.0, 0.0), 1.0);
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 0.25) - 1.25).abs() < 1e-6);
    }

    /// Two seconds of silence with a short burst at frame 48 000.
    fn click_track() -> Arc<Track> {
        let frames = SR as usize * 2;
        let (track, mut writer) = Track::new(TrackId(9), SR, frames);
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = match (48_000..48_048).contains(&i) {
                    true => 0.8 * (i as f32 * 1.3).sin(),
                    false => 0.0,
                };
                [s, s]
            })
            .collect();
        writer.push_interleaved(&samples);
        writer.finish();
        track
    }

    fn loudest_frame(out: &[(f32, f32)]) -> usize {
        out.iter()
            .enumerate()
            .max_by(|a, b| a.1.0.abs().total_cmp(&b.1.0.abs()))
            .map_or(0, |(i, _)| i)
    }

    #[test]
    fn key_lock_plays_the_playhead_frame_without_extra_delay() {
        for (fader, rate) in [(0.0f32, 1.0f64), (1.0, 1.08)] {
            let mut deck = new_deck(true);
            deck.load(Some(click_track()));
            deck.apply(DeckAction::SetTempo { fader });
            // Start 0.1 s (4800 frames) before the click.
            deck.apply(DeckAction::SeekBy { seconds: 0.9 });
            deck.apply(DeckAction::PlayPause);
            let out = run(&mut deck, 9600);
            let expected = (4800.0 / rate) as i64;
            let found = loudest_frame(&out) as i64;
            assert!(
                (found - expected).abs() < 120,
                "rate {rate}: click at {found}, expected {expected}"
            );
            // The playhead advanced at the tempo rate.
            let position = deck.snapshot().position as f64;
            assert!(
                (position - (43_200.0 + 9600.0 * rate)).abs() < 2.0,
                "{position}"
            );
        }
    }

    fn sine_track(hz: f32, seconds: f32) -> Arc<Track> {
        let frames = (SR as f32 * seconds) as usize;
        let (track, mut writer) = Track::new(TrackId(5), SR, frames);
        let samples: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let s = (std::f32::consts::TAU * hz * i as f32 / SR as f32).sin() * 0.5;
                [s, s]
            })
            .collect();
        writer.push_interleaved(&samples);
        writer.finish();
        track
    }

    fn frequency(out: &[(f32, f32)]) -> f32 {
        let rising = out
            .windows(2)
            .filter(|w| w[0].0 < 0.0 && w[1].0 >= 0.0)
            .count();
        rising as f32 * SR as f32 / out.len() as f32
    }

    #[test]
    fn key_lock_keeps_pitch_and_varispeed_does_not() {
        for (key_lock, expected) in [(true, 440.0), (false, 440.0 * 1.08)] {
            let mut deck = new_deck(key_lock);
            deck.load(Some(sine_track(440.0, 4.0)));
            deck.apply(DeckAction::SetTempo { fader: 1.0 });
            deck.apply(DeckAction::PlayPause);
            run(&mut deck, SR as usize / 2);
            let out = run(&mut deck, SR as usize);
            let hz = frequency(&out);
            assert!(
                (hz - expected).abs() < 3.0,
                "key lock {key_lock}: {hz} Hz, expected {expected}"
            );
        }
    }

    #[test]
    fn switching_key_lock_while_playing_fades_without_a_click() {
        let mut deck = new_deck(false);
        deck.load(Some(sine_track(220.0, 4.0)));
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 4800);
        deck.apply(DeckAction::ToggleKeyLock);
        assert!(deck.snapshot().key_lock, "reported as switched at once");
        let out = run(&mut deck, 9600);
        let max_step = out
            .windows(2)
            .map(|w| (w[1].0 - w[0].0).abs())
            .fold(0.0f32, f32::max);
        // A 220 Hz sine at 0.5 moves at most ~0.015 per sample; a cut would jump by
        // up to 0.5.
        assert!(max_step < 0.05, "step {max_step}");
        let tail = &out[out.len() - 2400..];
        assert!(
            tail.iter().any(|f| f.0.abs() > 0.4),
            "playing again after the switch"
        );
    }

    #[test]
    fn quantized_cue_snaps_to_the_grid() {
        let mut deck = loaded(100_000);
        deck.apply(DeckAction::SetBeatGrid {
            track: TrackId(7),
            grid: Some(BeatGrid::from_bpm(120.0, 1000.0, SR)),
        });
        deck.apply(DeckAction::SeekBy { seconds: 0.6 }); // frame 28 800
        deck.set_quantize(true);
        deck.apply(DeckAction::CuePress);
        assert_eq!(deck.snapshot().cue, 25_000);
        // A grid for another track is ignored.
        deck.apply(DeckAction::SetBeatGrid {
            track: TrackId(99),
            grid: None,
        });
        assert!(deck.snapshot().grid.is_some());
    }

    #[test]
    fn quantized_seek_moves_whole_bars() {
        let mut deck = loaded(1_000_000);
        deck.apply(DeckAction::SetBeatGrid {
            track: TrackId(7),
            grid: Some(BeatGrid::from_bpm(120.0, 0.0, SR)),
        });
        deck.set_quantize(true);
        // 4 s at 120 BPM is 8 beats = 2 bars; 1 s rounds up to one bar.
        deck.apply(DeckAction::SeekBy { seconds: 4.0 });
        assert_eq!(deck.snapshot().position, 192_000);
        deck.apply(DeckAction::SeekBy { seconds: -1.0 });
        assert_eq!(deck.snapshot().position, 96_000);
    }

    #[test]
    fn jump_to_moves_and_clamps() {
        let mut deck = loaded(10_000);
        deck.apply(DeckAction::JumpTo { frame: 5_000.0 });
        assert_eq!(deck.snapshot().position, 5_000);
        deck.apply(DeckAction::JumpTo { frame: 1e9 });
        assert_eq!(deck.snapshot().position, 9_999);
    }

    #[test]
    fn auto_cue_only_applies_to_an_untouched_deck() {
        let mut deck = loaded(100_000);
        deck.apply(DeckAction::AutoCue {
            track: TrackId(7),
            frame: 4_800.0,
        });
        assert_eq!(
            (deck.snapshot().cue, deck.snapshot().position),
            (4_800, 4_800)
        );
        // Already moved: ignored.
        deck.apply(DeckAction::AutoCue {
            track: TrackId(7),
            frame: 9_600.0,
        });
        assert_eq!(deck.snapshot().cue, 4_800);
        // Another track's: ignored.
        let mut fresh = loaded(100_000);
        fresh.apply(DeckAction::AutoCue {
            track: TrackId(1),
            frame: 4_800.0,
        });
        assert_eq!(fresh.snapshot().cue, 0);
    }

    #[test]
    fn hot_cue_sets_then_jumps_while_playing() {
        let mut deck = loaded(200_000);
        deck.apply(DeckAction::SeekBy { seconds: 0.5 });
        deck.apply(DeckAction::HotCuePress(2));
        deck.apply(DeckAction::HotCueRelease(2));
        assert_eq!(deck.snapshot().hot_cues[2], HotCue::Point(24_000.0));
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 48_000);
        deck.apply(DeckAction::HotCuePress(2));
        deck.apply(DeckAction::HotCueRelease(2));
        run(&mut deck, 1_000);
        let position = deck.snapshot().position;
        assert!((24_000..26_000).contains(&position), "{position}");
        assert_eq!(deck.snapshot().transport, Transport::Playing);
        deck.apply(DeckAction::HotCueClear(2));
        assert_eq!(deck.snapshot().hot_cues[2], HotCue::Empty);
    }

    #[test]
    fn hot_cue_on_a_stopped_deck_previews_while_held() {
        let mut deck = loaded(200_000);
        deck.apply(DeckAction::SetHotCue {
            track: TrackId(7),
            slot: 0,
            cue: HotCue::Point(10_000.0),
        });
        deck.apply(DeckAction::HotCuePress(0));
        assert_eq!(deck.snapshot().transport, Transport::CuePreview);
        run(&mut deck, 4_800);
        deck.apply(DeckAction::HotCueRelease(0));
        run(&mut deck, 1_000);
        let snap = deck.snapshot();
        assert_eq!(snap.transport, Transport::Stopped);
        assert_eq!(snap.position, 10_000);
    }

    /// Positions seen while rendering `frames` in 128-frame pieces.
    fn positions(deck: &mut Deck, frames: usize) -> Vec<u64> {
        (0..frames / 128)
            .map(|_| {
                run(deck, 128);
                deck.snapshot().position
            })
            .collect()
    }

    #[test]
    fn auto_loop_wraps_with_and_without_key_lock() {
        for key_lock in [false, true] {
            let mut deck = new_deck(key_lock);
            deck.load(Some(ramp_track(400_000)));
            deck.apply(DeckAction::SetBeatGrid {
                track: TrackId(7),
                grid: Some(BeatGrid::from_bpm(120.0, 0.0, SR)),
            });
            deck.apply(DeckAction::SeekBy { seconds: 1.0 });
            deck.apply(DeckAction::PlayPause);
            run(&mut deck, 1_000);
            // One beat (24 000 frames) from 49 000.
            deck.apply(DeckAction::AutoLoop { beats: 1.0 });
            let region = deck.snapshot().loop_region.unwrap();
            assert!(deck.snapshot().loop_active);
            let seen = positions(&mut deck, 96_000);
            assert!(
                seen.iter()
                    .all(|&p| (p as f64) >= region.start - 1.0 && (p as f64) < region.end + 1.0),
                "key lock {key_lock}: left the loop {region:?}"
            );
            // 4 BEAT / EXIT again leaves the loop and plays on.
            deck.apply(DeckAction::AutoLoop { beats: 1.0 });
            let seen = positions(&mut deck, 48_000);
            assert!(
                *seen.last().unwrap() as f64 > region.end,
                "key lock {key_lock}: still looping"
            );
        }
    }

    #[test]
    fn loop_in_out_halve_double_and_reloop() {
        let mut deck = loaded(400_000);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 10_000);
        deck.apply(DeckAction::LoopIn);
        run(&mut deck, 20_000);
        deck.apply(DeckAction::LoopOut);
        let region = deck.snapshot().loop_region.unwrap();
        assert_eq!((region.start, region.end), (10_000.0, 30_000.0));
        deck.apply(DeckAction::LoopHalve);
        assert_eq!(deck.snapshot().loop_region.unwrap().end, 20_000.0);
        deck.apply(DeckAction::LoopDouble);
        deck.apply(DeckAction::LoopDouble);
        assert_eq!(deck.snapshot().loop_region.unwrap().end, 50_000.0);
        deck.apply(DeckAction::ReloopExit);
        assert!(!deck.snapshot().loop_active);
        deck.apply(DeckAction::ReloopExit);
        assert!(deck.snapshot().loop_active);
    }

    #[test]
    fn hot_cue_pressed_during_a_loop_saves_the_loop() {
        let mut deck = loaded(400_000);
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 1_000);
        deck.apply(DeckAction::AutoLoop { beats: 2.0 });
        let region = deck.snapshot().loop_region.unwrap();
        deck.apply(DeckAction::HotCuePress(5));
        deck.apply(DeckAction::HotCueRelease(5));
        assert_eq!(deck.snapshot().hot_cues[5], HotCue::Loop(region));
        // Leave it, move on, and the pad brings the loop back.
        deck.apply(DeckAction::ReloopExit);
        deck.apply(DeckAction::SeekBy { seconds: 3.0 });
        run(&mut deck, 1_000);
        deck.apply(DeckAction::HotCuePress(5));
        run(&mut deck, 1_000);
        let snap = deck.snapshot();
        assert!(snap.loop_active && snap.loop_region == Some(region));
        assert!(region.contains(snap.position as f64));
    }

    #[test]
    fn roll_rejoins_the_track_where_it_would_have_been() {
        let mut deck = loaded(400_000);
        deck.apply(DeckAction::SetBeatGrid {
            track: TrackId(7),
            grid: Some(BeatGrid::from_bpm(120.0, 0.0, SR)),
        });
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 30_000);
        // Quarter-beat roll (6 000 frames) on the current division: 30 000..36 000.
        deck.apply(DeckAction::RollPress { beats: 0.25 });
        let roll = deck.snapshot().loop_region.unwrap();
        assert_eq!((roll.start, roll.end), (30_000.0, 36_000.0));
        let seen = positions(&mut deck, 24_064);
        assert!(seen.iter().all(|&p| (30_000..=36_000).contains(&p)));
        let slip = deck.snapshot().slip_position.unwrap();
        assert!((slip - 54_064.0).abs() < 2.0, "slip {slip}");
        deck.apply(DeckAction::RollRelease);
        run(&mut deck, 1_024);
        let snap = deck.snapshot();
        assert!(!snap.loop_active && snap.slip_position.is_none());
        let position = snap.position as f64;
        assert!((position - 55_088.0).abs() < 64.0, "rejoined at {position}");
    }

    #[test]
    fn beat_jump_moves_the_playhead_and_the_loop() {
        let mut deck = loaded(400_000);
        deck.apply(DeckAction::SetBeatGrid {
            track: TrackId(7),
            grid: Some(BeatGrid::from_bpm(120.0, 0.0, SR)),
        });
        deck.apply(DeckAction::SeekBy { seconds: 1.0 });
        deck.apply(DeckAction::AutoLoop { beats: 4.0 });
        deck.apply(DeckAction::BeatJump { beats: 4.0 });
        let snap = deck.snapshot();
        assert_eq!(snap.position, 48_000 + 96_000);
        assert_eq!(snap.loop_region.unwrap().start, 144_000.0);
        deck.apply(DeckAction::BeatJump { beats: -2.0 });
        assert_eq!(deck.snapshot().position, 96_000);
    }

    #[test]
    fn quantized_hot_cue_jump_keeps_the_beat_phase() {
        let mut deck = loaded(400_000);
        deck.apply(DeckAction::SetBeatGrid {
            track: TrackId(7),
            grid: Some(BeatGrid::from_bpm(120.0, 0.0, SR)),
        });
        deck.set_quantize(true);
        deck.apply(DeckAction::HotCuePress(0)); // at 0
        deck.apply(DeckAction::HotCueRelease(0));
        deck.apply(DeckAction::PlayPause);
        run(&mut deck, 100_000);
        deck.apply(DeckAction::HotCuePress(0));
        run(&mut deck, 1_000);
        // Before: 100 000 frames in, a quarter beat (4 000) past beat 4. After the jump
        // the phase is the same, near the cue.
        let grid = BeatGrid::from_bpm(120.0, 0.0, SR);
        let phase = grid.phase(deck.snapshot().position as f64);
        let expected = grid.phase(101_000.0);
        assert!(
            (phase - expected).abs() < 0.01,
            "phase {phase} vs {expected}"
        );
        assert!(deck.snapshot().position < 30_000);
    }

    #[test]
    fn eject_unloads() {
        let mut deck = loaded(1000);
        deck.apply(DeckAction::PlayPause);
        deck.apply(DeckAction::Eject);
        assert_eq!(deck.snapshot().track, None);
    }
}
