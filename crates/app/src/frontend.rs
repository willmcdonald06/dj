//! The app's UI delegate: keyboard, pointer and dropped files to commands; engine state
//! to the screen. See [`layout`] for where things go; the views live in the submodules.

mod browser_view;
mod chrome;
mod deck_view;
mod layout;
mod mixer_view;
mod painter;
mod theme;
mod waves;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dj_analysis::Key as MusicalKey;
use dj_analysis::{TrackAnalysis, Transient};
use dj_audio_io::{AudioOutput, BufferGrown, CallbackStats};
use dj_engine::{
    ChannelControl, Command, DeckAction, DeckId, DeckSnapshot, EngineHandle, EngineSnapshot,
    HOT_CUES, HotCue, MasterControl,
};
use dj_library::{AnalysisSource, CollectionKind, CueKind, Library, StoredCue};

use crate::browser::{Browser, Column, Filter, track_key};
use crate::library_worker::{LibraryWorker, WorkerEvent, to_library, trusted_grid};
use crate::settings::Settings;
use dj_control::UiEvent;
use dj_engine::BeatGrid;
use dj_loader::{LoadEvent, Loader, TrackInfo};
use dj_ui::{Delegate, Flow, FrameSize, Input, Key, Scene};
use std::sync::mpsc;

use browser_view::BrowserView;
use chrome::TopBar;
use deck_view::{DeckView, KeyDisplay};
use layout::{Area, Layout};
use mixer_view::MixerView;
use painter::Painter;

const SEEK_SECONDS: f32 = 4.0;
/// Tempo change per Up/Down press: 0.02%, a CDJ's finest step. Holding the key glides.
const TEMPO_FINE: f32 = 0.0002;
/// Tempo change per Page Up/Down press.
const TEMPO_COARSE: f32 = 0.01;
/// Nudge per ,/. key event; key repeat sends ~30 a second.
const KEY_NUDGE: f32 = 0.01;
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Seconds shown across the scrolling waveforms at 1:1 tempo, and zoom limits.
const DEFAULT_ZOOM_SECONDS: f64 = 8.0;
const ZOOM_RANGE: (f64, f64) = (2.0, 32.0);
/// Phase offsets beyond this are drawn as a full bar.
const PHASE_BAR_MS: f64 = 50.0;
/// How long a notice stays on screen.
const NOTICE_SECONDS: Duration = Duration::from_secs(5);

/// Pointer travel (pixels) before pressing a browser row becomes dragging it.
const DRAG_START: f32 = 8.0;
/// Two clicks on the same browser row within this load it.
const ROW_DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Reload the browser's list from the library at most this often while it changes.
const BROWSER_RELOAD: Duration = Duration::from_millis(700);
/// Held tempo, nudge and zoom buttons start repeating after this, then fire at this rate.
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(35);
/// Zoom factor per mouse-wheel notch over a waveform.
const WHEEL_ZOOM: f64 = 1.15;
/// Pitch bend per pixel of drag on a playing waveform.
const WAVEFORM_BEND_PER_PIXEL: f32 = 0.004;
/// Two presses on the same slider within this reset it.
const DOUBLE_CLICK: Duration = Duration::from_millis(350);
/// Centred sliders snap to the centre within this fraction of their travel.
const CENTRE_SNAP: f32 = 0.02;
/// Pixels of vertical drag that turn a knob from one end to the other.
const KNOB_TRAVEL: f32 = 160.0;
/// Meters fall by this fraction of full scale per frame.
const METER_FALL: f32 = 0.025;
/// Auto-loop lengths, in beats.
const LOOP_BEATS: (f32, f32) = (1.0 / 32.0, 32.0);
/// Roll pads: label and length in beats.
const ROLL_PADS: [(&str, f32); 8] = [
    ("1/16", 0.0625),
    ("1/8", 0.125),
    ("1/4", 0.25),
    ("1/2", 0.5),
    ("1", 1.0),
    ("2", 2.0),
    ("4", 4.0),
    ("8", 8.0),
];
/// Beat jump pads: back on the top row, forward on the bottom.
const JUMP_PADS: [(&str, f32); 8] = [
    ("‹ 1", -1.0),
    ("‹ 2", -2.0),
    ("‹ 4", -4.0),
    ("‹ 8", -8.0),
    ("1 ›", 1.0),
    ("2 ›", 2.0),
    ("4 ›", 4.0),
    ("8 ›", 8.0),
];

/// What a deck's eight pads do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PadMode {
    #[default]
    HotCue,
    Roll,
    Jump,
}

/// A deck's pad mode, and whether the next hot cue pad press deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PadState {
    mode: PadMode,
    delete_armed: bool,
}

/// What the app knows about the track on one deck, beyond the engine snapshot.
#[derive(Default)]
struct DeckInfo {
    track: Option<TrackInfo>,
    analysis: Option<Arc<TrackAnalysis>>,
    /// Attacks found in the analysis's waveform, for the spike markers.
    transients: Vec<Transient>,
    /// The file, as the library knows it.
    path: Option<PathBuf>,
    /// The key from the offline analyzer's sidecar, which beats the app's own.
    sidecar_key: Option<MusicalKey>,
    /// Hot cues as last saved (or restored), to spot changes worth saving.
    saved_cues: [HotCue; HOT_CUES],
    /// The engine has the restored cues; until then its (empty) cues aren't saved.
    cues_restored: bool,
    pads: PadState,
    /// Length of the next auto loop.
    loop_beats: LoopBeats,
}

/// Auto-loop length in beats; 4 to start with.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LoopBeats(f32);

impl Default for LoopBeats {
    fn default() -> Self {
        Self(4.0)
    }
}

/// Everything the frontend runs with.
pub struct Parts {
    pub engine: EngineHandle,
    pub loader: Loader,
    pub audio: AudioOutput,
    /// Decks to start as soon as their first track is loaded (`--play`).
    pub play_on_load: [bool; 2],
    pub library: Option<Library>,
    pub settings: Settings,
    pub worker: Option<LibraryWorker>,
    /// Browsing and loading from the controller.
    pub controller_events: mpsc::Receiver<UiEvent>,
}

pub struct Frontend {
    engine: EngineHandle,
    loader: Loader,
    audio: AudioOutput,
    stats: Arc<CallbackStats>,
    /// Deck that keys and dropped files act on.
    selected: DeckId,
    decks: [DeckInfo; 2],
    /// Deck the held CUE key went to, so its release goes to the same deck.
    cue_held: Option<DeckId>,
    zoom_seconds: f64,
    /// Decks to start as soon as their first track is loaded (`--play`).
    play_on_load: [bool; 2],
    /// A message for the user (e.g. why a load failed) and when it was posted.
    notice: Option<(String, Instant)>,
    /// Clickable areas from the last frame.
    hits: Vec<Hit>,
    /// The last frame's UI scale (physical pixels per logical pixel).
    scale: f32,
    /// What the mouse or finger is holding.
    grab: Option<Grab>,
    /// The last slider pressed and when, for double-click resets.
    last_slider_press: Option<(Slider, Instant)>,
    /// Where hot cues are kept; `None` if it couldn't be opened.
    library: Option<Library>,
    settings: Settings,
    worker: Option<LibraryWorker>,
    controller_events: mpsc::Receiver<UiEvent>,
    browser: Browser,
    /// The library changed; reload the browser's list soon.
    browser_dirty: bool,
    last_browser_reload: Instant,
    /// Rows the browser showed last frame (for paging and keeping the selection visible).
    browser_visible: usize,
    /// The browser has the arrow keys (it was clicked, or B pressed).
    browser_focus: bool,
    /// The keyboard help is open.
    show_help: bool,
    /// Meter levels with a falling release: deck A, deck B, main.
    meters: [f32; 3],
    /// Last browser row clicked and when, for double-click loading.
    last_row_click: Option<(usize, Instant)>,
    /// A crate or playlist whose name is being typed: its id and the text so far.
    naming: Option<(i64, String)>,
    /// DELETE was pressed once on the crate or playlist being viewed.
    delete_armed: bool,
    /// Where the pointer is, for drag feedback.
    pointer: (f32, f32),
    last_stats_log: Instant,
    last_xruns: u64,
}

impl Frontend {
    pub fn new(parts: Parts) -> Self {
        let Parts {
            engine,
            loader,
            audio,
            play_on_load,
            library,
            settings,
            worker,
            controller_events,
        } = parts;
        let mut frontend = Self {
            engine,
            loader,
            stats: audio.stats(),
            audio,
            selected: DeckId::A,
            decks: Default::default(),
            cue_held: None,
            zoom_seconds: DEFAULT_ZOOM_SECONDS,
            play_on_load,
            notice: None,
            hits: Vec::new(),
            scale: 1.0,
            grab: None,
            last_slider_press: None,
            library,
            settings,
            worker,
            controller_events,
            browser: Browser::new(),
            browser_dirty: true,
            last_browser_reload: Instant::now(),
            browser_visible: 10,
            browser_focus: false,
            show_help: false,
            meters: [0.0; 3],
            last_row_click: None,
            naming: None,
            delete_armed: false,
            pointer: (0.0, 0.0),
            last_stats_log: Instant::now(),
            last_xruns: 0,
        };
        frontend.reload_browser();
        frontend
    }

    fn send(&mut self, command: Command) {
        if let Err(err) = self.engine.send(command) {
            tracing::warn!("{err}");
        }
    }

    fn deck_action(&mut self, action: DeckAction) {
        self.send(Command::deck(self.selected, action));
    }

    /// Change the selected deck's tempo by `step` (a rate change, e.g. 0.0002 = 0.02%).
    fn adjust_tempo(&mut self, step: f32) {
        self.adjust_tempo_of(self.selected, step);
    }

    fn adjust_tempo_of(&mut self, deck: DeckId, step: f32) {
        let state = *self.engine.snapshot().deck(deck);
        let range = state.tempo_range.fraction();
        let fader = (state.tempo_fader + step / range).clamp(-1.0, 1.0);
        self.send(Command::deck(deck, DeckAction::SetTempo { fader }));
    }

    /// Do what a button press asks.
    fn press(&mut self, action: UiAction) {
        match action {
            UiAction::Deck(deck, action) => self.send(Command::deck(deck, action)),
            UiAction::Cue(deck) => {
                self.cue_held = Some(deck);
                self.send(Command::deck(deck, DeckAction::CuePress));
            }
            UiAction::Tempo(deck, step) => self.adjust_tempo_of(deck, step),
            UiAction::Nudge(deck, amount) => {
                self.send(Command::deck(deck, DeckAction::Nudge { amount }))
            }
            UiAction::Sync(deck) => self.toggle_sync(deck),
            UiAction::Quantize => self.toggle_quantize(),
            UiAction::Pfl(deck) => self.send(Command::channel(deck, ChannelControl::TogglePfl)),
            UiAction::Zoom(factor) => self.zoom(factor),
            UiAction::Select(deck) => self.selected = deck,
            UiAction::HotCue(deck, slot) => {
                let pads = &mut self.decks[deck.index()].pads;
                match std::mem::take(&mut pads.delete_armed) {
                    true => self.send(Command::deck(deck, DeckAction::HotCueClear(slot))),
                    false => self.send(Command::deck(deck, DeckAction::HotCuePress(slot))),
                }
            }
            UiAction::Roll(deck, beats) => {
                self.send(Command::deck(deck, DeckAction::RollPress { beats }))
            }
            UiAction::PadMode(deck, mode) => {
                let pads = &mut self.decks[deck.index()].pads;
                pads.mode = mode;
                pads.delete_armed = false;
            }
            UiAction::LoadSelected(deck) => self.load_selected(deck),
            UiAction::Rescan => {
                if let Some(worker) = &self.worker {
                    worker.rescan();
                    self.notify("Rescanning the library…".to_string());
                }
            }
            UiAction::Help => self.show_help = !self.show_help,
            UiAction::SplitCue => {
                let on = !self.engine.snapshot().master.split_cue;
                self.send(Command::Master(MasterControl::SplitCue(on)));
            }
            UiAction::NewCollection(kind) => self.new_collection(kind),
            UiAction::LoopSize(deck, factor) => {
                let d = *self.engine.snapshot().deck(deck);
                let beats = &mut self.decks[deck.index()].loop_beats;
                beats.0 = (beats.0 * factor).clamp(LOOP_BEATS.0, LOOP_BEATS.1);
                if d.loop_active {
                    let action = match factor < 1.0 {
                        true => DeckAction::LoopHalve,
                        false => DeckAction::LoopDouble,
                    };
                    self.send(Command::deck(deck, action));
                }
            }
            UiAction::AutoLoop(deck) => {
                let beats = self.decks[deck.index()].loop_beats.0;
                self.send(Command::deck(deck, DeckAction::AutoLoop { beats }));
            }
            UiAction::RenameCollection
            | UiAction::DeleteCollection
            | UiAction::RemoveFromCollection
            | UiAction::MoveInPlaylist(_) => self.collection_action(action),
            UiAction::DeleteArm(deck) => {
                let pads = &mut self.decks[deck.index()].pads;
                pads.delete_armed = !pads.delete_armed;
            }
        }
    }

    /// The button being held, to draw it pressed.
    fn pressed_action(&self) -> Option<UiAction> {
        match self.grab {
            Some(Grab::Button { action, .. }) => Some(action),
            _ => None,
        }
    }

    /// Fire a held repeating button (tempo, nudge, zoom) at the repeat rate.
    fn repeat_held_button(&mut self) {
        if let Some(Grab::Button {
            action,
            next_repeat,
        }) = self.grab
        {
            if action.repeats() && Instant::now() >= next_repeat {
                self.press(action);
                self.grab = Some(Grab::Button {
                    action,
                    next_repeat: Instant::now() + REPEAT_INTERVAL,
                });
            }
        }
    }

    fn pointer_down(&mut self, x: f32, y: f32) {
        // Clicking the browser gives it the arrow keys; clicking elsewhere takes them
        // back.
        if !self.show_help {
            self.browser_focus = self
                .hits
                .iter()
                .any(|h| h.control == Control::BrowserArea && h.area.contains(x, y));
            if !self.browser_focus {
                self.browser.search_focused = false;
            }
        }
        let Some(hit) = self
            .hits
            .iter()
            .rev()
            .find(|h| h.area.contains(x, y))
            .copied()
        else {
            return;
        };
        match hit.control {
            Control::Button(action) => {
                self.press(action);
                self.grab = Some(Grab::Button {
                    action,
                    next_repeat: Instant::now() + REPEAT_DELAY,
                });
            }
            Control::Slider(slider, drag) => {
                let double = self
                    .last_slider_press
                    .is_some_and(|(last, at)| last == slider && at.elapsed() < DOUBLE_CLICK);
                self.last_slider_press = Some((slider, Instant::now()));
                let from_unit = slider.unit(&self.engine.snapshot());
                match double {
                    true => self.send(slider.command(slider.default_unit())),
                    false => self.drag_slider(slider, drag, from_unit, y, y),
                }
                self.grab = Some(Grab::Slider {
                    slider,
                    drag,
                    from_y: y,
                    from_unit,
                });
            }
            Control::Overview(deck, length) => {
                self.scrub(deck, hit.area, length, x);
                self.grab = Some(Grab::Overview {
                    deck,
                    area: hit.area,
                    length,
                });
            }
            Control::SearchBox => self.browser.search_focused = true,
            Control::BrowserList | Control::BrowserArea => {}
            Control::BrowserSort(column) => self.browser.sort_by(column),
            Control::BrowserSource(index) => {
                let filter = match index {
                    0 => Filter::All,
                    i => match self.browser.sources.get(i - 1) {
                        Some(source) => Filter::Folder(source.path.clone()),
                        None => Filter::All,
                    },
                };
                self.finish_naming();
                self.delete_armed = false;
                self.browser.set_filter(filter);
            }
            Control::Collection(index) => {
                if let Some(collection) = self.browser.collections.get(index).cloned() {
                    if self
                        .naming
                        .as_ref()
                        .is_some_and(|(id, _)| *id == collection.id)
                    {
                        return;
                    }
                    self.finish_naming();
                    self.delete_armed = false;
                    self.show_collection(collection.id, collection.kind);
                }
            }
            Control::BrowserRow(row) => {
                self.browser.search_focused = false;
                let double = self
                    .last_row_click
                    .is_some_and(|(last, at)| last == row && at.elapsed() < ROW_DOUBLE_CLICK);
                self.browser.select_row(row);
                self.last_row_click = Some((row, Instant::now()));
                if double {
                    self.load_selected(self.selected);
                    self.last_row_click = None;
                }
                if let Some(track) = self.browser.selected_id() {
                    self.grab = Some(Grab::Row {
                        track,
                        from: (x, y),
                        dragging: false,
                    });
                }
            }
            Control::Waveform(deck, frames_per_pixel) => {
                let snapshot = self.engine.snapshot();
                self.grab = Some(Grab::Waveform {
                    deck,
                    frames_per_pixel,
                    last_x: x,
                    position: snapshot.deck(deck).position as f64,
                    moved: false,
                });
            }
        }
    }

    /// Mouse wheel: over a scrolling waveform, zoom (up zooms in).
    fn scroll(&mut self, x: f32, y: f32, lines: f32) {
        let over_list = self
            .hits
            .iter()
            .any(|h| matches!(h.control, Control::BrowserList) && h.area.contains(x, y));
        if over_list {
            self.browser
                .scroll_by((-lines * 3.0).round() as i32, self.browser_visible);
            return;
        }
        let over_waveform = self
            .hits
            .iter()
            .any(|h| matches!(h.control, Control::Waveform(..)) && h.area.contains(x, y));
        if over_waveform {
            self.zoom(WHEEL_ZOOM.powf(-lines as f64));
        }
    }

    fn pointer_move(&mut self, x: f32, y: f32) {
        self.pointer = (x, y);
        match self.grab {
            Some(Grab::Row {
                track,
                from,
                dragging: false,
            }) if (x - from.0).abs() + (y - from.1).abs() > DRAG_START => {
                self.grab = Some(Grab::Row {
                    track,
                    from,
                    dragging: true,
                });
            }
            Some(Grab::Slider {
                slider,
                drag,
                from_y,
                from_unit,
            }) => self.drag_slider(slider, drag, from_unit, from_y, y),
            Some(Grab::Overview { deck, area, length }) => self.scrub(deck, area, length, x),
            Some(Grab::Waveform {
                deck,
                frames_per_pixel,
                last_x,
                position,
                moved,
            }) => {
                let dx = x - last_x;
                if dx == 0.0 {
                    return;
                }
                let playing = self.engine.snapshot().deck(deck).transport.is_moving();
                // Like grabbing the record: dragging left moves the track forward.
                let position = match playing {
                    true => {
                        let amount = (-dx * WAVEFORM_BEND_PER_PIXEL).clamp(-0.3, 0.3);
                        self.send(Command::deck(deck, DeckAction::Nudge { amount }));
                        position
                    }
                    false => {
                        let frame = (position - dx as f64 * frames_per_pixel).max(0.0);
                        self.send(Command::deck(deck, DeckAction::JumpTo { frame }));
                        frame
                    }
                };
                self.grab = Some(Grab::Waveform {
                    deck,
                    frames_per_pixel,
                    last_x: x,
                    position,
                    moved: moved || dx.abs() > 0.0,
                });
            }
            _ => {}
        }
    }

    fn pointer_up(&mut self) {
        match self.grab {
            Some(Grab::Button {
                action: UiAction::Cue(deck),
                ..
            }) => {
                self.cue_held = None;
                self.send(Command::deck(deck, DeckAction::CueRelease));
            }
            Some(Grab::Button {
                action: UiAction::HotCue(deck, slot),
                ..
            }) => self.send(Command::deck(deck, DeckAction::HotCueRelease(slot))),
            Some(Grab::Button {
                action: UiAction::Roll(deck, _),
                ..
            }) => self.send(Command::deck(deck, DeckAction::RollRelease)),
            // A track dropped on a crate or playlist goes into it.
            Some(Grab::Row {
                track,
                dragging: true,
                ..
            }) => {
                let (x, y) = self.pointer;
                let target = self.hits.iter().rev().find_map(|h| match h.control {
                    Control::Collection(index) if h.area.contains(x, y) => Some(index),
                    _ => None,
                });
                if let Some(index) = target {
                    self.add_to_collection(index, track);
                }
            }
            // A click without dragging selects the deck.
            Some(Grab::Waveform {
                deck, moved: false, ..
            }) => self.selected = deck,
            _ => {}
        }
        self.grab = None;
    }

    fn scrub(&mut self, deck: DeckId, area: Area, length: u64, x: f32) {
        let fraction = ((x - area.x) / area.width).clamp(0.0, 1.0) as f64;
        let frame = fraction * length as f64;
        self.send(Command::deck(deck, DeckAction::JumpTo { frame }));
    }

    /// Move a slider for the pointer at height `y`: faders follow it, knobs turn by
    /// how far it has moved since `from_y`, when the knob was at `from_unit`.
    fn drag_slider(&mut self, slider: Slider, drag: Drag, from_unit: f32, from_y: f32, y: f32) {
        let mut unit = match drag {
            Drag::Vertical {
                inverted,
                top,
                travel,
            } => {
                let down = ((y - top) / travel.max(1.0)).clamp(0.0, 1.0);
                match inverted {
                    true => down,
                    false => 1.0 - down,
                }
            }
            Drag::Knob => from_unit + (from_y - y) / (KNOB_TRAVEL * self.scale),
        };
        unit = unit.clamp(0.0, 1.0);
        // Centred controls snap to their detent.
        if slider.centred() && (unit - 0.5).abs() < CENTRE_SNAP {
            unit = 0.5;
        }
        self.send(slider.command(unit));
    }

    /// Keys go to the browser first: it has focus, or text is being typed.
    fn browser_wants_keys(&self) -> bool {
        self.browser_focus || self.browser.search_focused || self.naming.is_some()
    }

    fn zoom(&mut self, factor: f64) {
        self.zoom_seconds = (self.zoom_seconds * factor).clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
    }

    fn toggle_quantize(&mut self) {
        let on = !self.engine.snapshot().quantize;
        self.send(Command::SetQuantize(on));
        let message = match on {
            true => {
                "Quantize on: cue points snap to the nearest beat and ←/→ jump by whole \
                     bars, keeping you on the beat."
            }
            false => "Quantize off.",
        };
        self.notify(message.to_string());
    }

    /// Toggle SYNC on the selected deck, and say what happened, especially when it
    /// can't do anything.
    fn toggle_sync(&mut self, deck: DeckId) {
        let state = self.engine.snapshot();
        let other = deck.other();
        let (d, o) = (state.deck(deck), state.deck(other));
        if d.sync_lock {
            self.send(Command::deck(deck, DeckAction::SetSyncLock(false)));
            self.notify(format!(
                "Deck {deck:?} sync off; it keeps its current tempo."
            ));
            return;
        }
        let message = match (d.track, d.grid, o.track, o.grid) {
            (None, ..) => format!("Deck {deck:?} is empty: load a track to sync it."),
            (_, None, ..) => format!(
                "Deck {deck:?}'s tempo is unclear (no beatgrid from the analysis), so it \
                 can't sync."
            ),
            (_, _, None, _) => format!(
                "Deck {deck:?} sync on as master. Load a track on deck {other:?} to sync it \
                 to this one."
            ),
            (_, _, _, None) => format!(
                "Deck {other:?}'s tempo is unclear (no beatgrid), so deck {deck:?} can't \
                 follow it. Deck {deck:?} is master."
            ),
            _ if o.sync_lock => format!("Deck {deck:?} now follows deck {other:?}."),
            _ => format!(
                "Deck {deck:?} now follows deck {other:?} (master): tempo matched to {:.2} BPM.",
                o.bpm(state.sample_rate).unwrap_or_default()
            ),
        };
        if d.track.is_some() && d.grid.is_some() {
            self.send(Command::deck(deck, DeckAction::SetSyncLock(true)));
        }
        self.notify(message);
    }

    /// Read `path`'s saved hot cues and send them to `deck` (for track `id`).
    fn restore_cues(
        &mut self,
        deck: DeckId,
        id: dj_engine::TrackId,
        path: &std::path::Path,
    ) -> [HotCue; HOT_CUES] {
        let mut cues = [HotCue::Empty; HOT_CUES];
        let Some(library) = &self.library else {
            return cues;
        };
        let rate = self.engine.snapshot().sample_rate as f64;
        match library.hot_cues(path) {
            Ok(stored) => {
                for cue in stored {
                    let slot = cue.slot as usize;
                    if slot >= HOT_CUES {
                        continue;
                    }
                    cues[slot] = match cue.kind {
                        CueKind::Point { seconds } => HotCue::Point(seconds * rate),
                        CueKind::Loop { start, end } => {
                            match dj_engine::LoopRegion::new(start * rate, end * rate) {
                                Some(region) => HotCue::Loop(region),
                                None => HotCue::Empty,
                            }
                        }
                    };
                }
            }
            Err(err) => tracing::warn!("reading hot cues: {err}"),
        }
        for (slot, &cue) in cues.iter().enumerate() {
            if cue.is_set() {
                self.send(Command::deck(
                    deck,
                    DeckAction::SetHotCue {
                        track: id,
                        slot,
                        cue,
                    },
                ));
            }
        }
        cues
    }

    /// Save any hot cue changes to the library straight away.
    fn persist_cues(&mut self, state: &EngineSnapshot) {
        let rate = state.sample_rate as f64;
        for deck in DeckId::ALL {
            let snapshot = state.deck(deck);
            let info = &mut self.decks[deck.index()];
            let (Some(track), Some(path)) = (&info.track, &info.path) else {
                continue;
            };
            if snapshot.track != Some(track.id) {
                continue;
            }
            if !info.cues_restored {
                info.cues_restored = snapshot.hot_cues == info.saved_cues;
                continue;
            }
            if snapshot.hot_cues == info.saved_cues {
                continue;
            }
            info.saved_cues = snapshot.hot_cues;
            let stored: Vec<StoredCue> = snapshot
                .hot_cues
                .iter()
                .enumerate()
                .filter_map(|(slot, cue)| {
                    let kind = match cue {
                        HotCue::Empty => return None,
                        HotCue::Point(frame) => CueKind::Point {
                            seconds: frame / rate,
                        },
                        HotCue::Loop(region) => CueKind::Loop {
                            start: region.start / rate,
                            end: region.end / rate,
                        },
                    };
                    Some(StoredCue {
                        slot: slot as u8,
                        kind,
                    })
                })
                .collect();
            if let Some(library) = &mut self.library {
                if let Err(err) = library.save_hot_cues(path, &stored) {
                    tracing::error!("saving hot cues: {err}");
                }
            }
        }
    }

    fn notify(&mut self, message: String) {
        self.notice = Some((message, Instant::now()));
    }

    fn load(&mut self, path: PathBuf) {
        self.load_to(self.selected, path);
    }

    fn load_selected(&mut self, deck: DeckId) {
        match self.browser.selected() {
            Some(track) => {
                let path = track.path.clone();
                self.load_to(deck, path);
            }
            None => self.notify("Pick a track in the browser first.".to_string()),
        }
    }

    fn reload_browser(&mut self) {
        self.browser_dirty = false;
        self.last_browser_reload = Instant::now();
        let Some(library) = &self.library else {
            return;
        };
        match library.collections() {
            Ok(collections) => self.browser.collections = collections,
            Err(err) => tracing::warn!("reading crates and playlists: {err}"),
        }
        // Refresh the crate or playlist being viewed (or leave it if it has gone).
        if let Filter::Collection { id, kind, .. } = self.browser.filter.clone() {
            let exists = self.browser.collections.iter().any(|c| c.id == id);
            let filter = match (exists, library.collection_tracks(id)) {
                (true, Ok(order)) => Filter::Collection { id, kind, order },
                _ => Filter::All,
            };
            self.browser.filter = filter;
        }
        match library.tracks() {
            Ok(tracks) => self.browser.set_tracks(tracks),
            Err(err) => tracing::warn!("reading the library: {err}"),
        }
    }

    /// Library worker news and controller browsing.
    fn poll_library(&mut self) {
        if let Some(worker) = &self.worker {
            while let Some(event) = worker.try_event() {
                match event {
                    WorkerEvent::Changed => self.browser_dirty = true,
                    WorkerEvent::Sources(sources) => self.browser.sources = sources,
                    WorkerEvent::Status(status) => self.browser.status = status,
                }
            }
        }
        if self.browser_dirty && self.last_browser_reload.elapsed() >= BROWSER_RELOAD {
            self.reload_browser();
        }
        while let Ok(event) = self.controller_events.try_recv() {
            match event {
                UiEvent::Browse(steps) => {
                    self.browser_focus = true;
                    self.browser.move_selection(steps, self.browser_visible);
                }
                UiEvent::BrowsePress => self.browser_focus = !self.browser_focus,
                UiEvent::Load(deck) => self.load_selected(deck),
                UiEvent::Back => self.browser_focus = false,
            }
        }
    }

    /// Typed text goes to the name being edited or the search box.
    fn type_text(&mut self, text: &str) {
        if let Some((_, name)) = &mut self.naming {
            name.push_str(text);
        } else if self.browser.search_focused {
            let search = format!("{}{text}", self.browser.search);
            self.browser.set_search(search);
        }
    }

    fn show_collection(&mut self, id: i64, kind: CollectionKind) {
        let order = match &self.library {
            Some(library) => library.collection_tracks(id).unwrap_or_default(),
            None => Vec::new(),
        };
        self.browser
            .set_filter(Filter::Collection { id, kind, order });
    }

    fn new_collection(&mut self, kind: CollectionKind) {
        self.finish_naming();
        let name = match kind {
            CollectionKind::Crate => "New crate",
            CollectionKind::Playlist => "New playlist",
        };
        let created = self
            .library
            .as_mut()
            .map(|library| library.create_collection(name, kind, unix_now()));
        match created {
            Some(Ok(id)) => {
                // Stay on the current list, so tracks can be dragged into the new one;
                // type its name straight away.
                self.reload_browser();
                self.naming = Some((id, String::new()));
            }
            Some(Err(err)) => tracing::error!("creating a crate or playlist: {err}"),
            None => self.notify("No library, so crates and playlists can't be saved.".into()),
        }
    }

    /// Save the name being typed (an empty name keeps the old one).
    fn finish_naming(&mut self) {
        let Some((id, name)) = self.naming.take() else {
            return;
        };
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(library) = &mut self.library {
            if let Err(err) = library.rename_collection(id, name) {
                tracing::error!("renaming: {err}");
            }
        }
        self.reload_browser();
    }

    fn add_to_collection(&mut self, index: usize, track: i64) {
        let Some(collection) = self.browser.collections.get(index).cloned() else {
            return;
        };
        let Some(library) = &mut self.library else {
            return;
        };
        match library.add_to_collection(collection.id, track) {
            Ok(true) => self.notify(format!("Added to {}", collection.name)),
            Ok(false) => self.notify(format!("Already in {}", collection.name)),
            Err(err) => tracing::error!("adding to {}: {err}", collection.name),
        }
        self.reload_browser();
    }

    /// Buttons for the crate or playlist being viewed.
    fn collection_action(&mut self, action: UiAction) {
        let Some(id) = self.browser.filter.collection_id() else {
            return;
        };
        let selected = self.browser.selected_id();
        if action != UiAction::DeleteCollection {
            self.delete_armed = false;
        }
        let Some(library) = &mut self.library else {
            return;
        };
        let result = match (action, selected) {
            (UiAction::RenameCollection, _) => {
                self.naming = Some((id, String::new()));
                Ok(())
            }
            (UiAction::DeleteCollection, _) => match std::mem::take(&mut self.delete_armed) {
                true => {
                    let result = library.delete_collection(id);
                    self.browser.set_filter(Filter::All);
                    result
                }
                false => {
                    self.delete_armed = true;
                    Ok(())
                }
            },
            (UiAction::RemoveFromCollection, Some(track)) => {
                library.remove_from_collection(id, track)
            }
            (UiAction::MoveInPlaylist(delta), Some(track)) => {
                let result = library.move_in_collection(id, track, delta);
                // Keep the moved track in view.
                let visible = self.browser_visible;
                self.reload_browser();
                if let Some(row) = self.browser.selected_row() {
                    self.browser.scroll_to(row, visible);
                }
                result
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            tracing::error!("changing a crate or playlist: {err}");
        }
        self.reload_browser();
    }

    /// Keys while the browser shows. Returns true if the key was used.
    fn browser_key(&mut self, key: Key, repeat: bool) -> bool {
        if self.naming.is_some() {
            match key {
                Key::Enter | Key::Tab => self.finish_naming(),
                Key::Escape => self.naming = None,
                Key::Backspace => {
                    if let Some((_, name)) = &mut self.naming {
                        name.pop();
                    }
                }
                _ => {}
            }
            return true;
        }
        if self.browser.search_focused {
            let mut search = self.browser.search.clone();
            match key {
                Key::Escape | Key::Enter | Key::Tab => self.browser.search_focused = false,
                Key::Backspace => {
                    search.pop();
                }
                // Characters arrive as `Input::Text`, with their case.
                Key::Space | Key::Char(_) => {}
                Key::Up => self.browser.move_selection(-1, self.browser_visible),
                Key::Down => self.browser.move_selection(1, self.browser_visible),
                _ => return false,
            }
            if search != self.browser.search {
                self.browser.set_search(search);
            }
            return true;
        }
        let page = self.browser_visible.max(1) as i32;
        match key {
            Key::Up => self.browser.move_selection(-1, self.browser_visible),
            Key::Down => self.browser.move_selection(1, self.browser_visible),
            Key::PageUp => self.browser.move_selection(-page, self.browser_visible),
            Key::PageDown => self.browser.move_selection(page, self.browser_visible),
            Key::Enter if !repeat => self.load_selected(self.selected),
            Key::Char('/') => self.browser.search_focused = true,
            Key::Escape => self.browser_focus = false,
            _ => return false,
        }
        true
    }

    fn load_to(&mut self, deck: DeckId, path: PathBuf) {
        let state = *self.engine.snapshot().deck(deck);
        let name = file_name(&path);
        if state.transport.is_moving() {
            tracing::warn!("deck {deck:?} is playing; not loading {}", path.display());
            self.notify(format!(
                "Deck {deck:?} is playing, so {name} wasn't loaded. Pause it or press Tab to pick the other deck."
            ));
            return;
        }
        tracing::info!("deck {deck:?}: loading {}", path.display());
        self.notify(format!("Loading {name} onto deck {deck:?}"));
        self.loader.load(deck, path);
    }

    fn poll_loader(&mut self) {
        while let Some(event) = self.loader.try_event() {
            crate::log_load_event(&event);
            match event {
                LoadEvent::Loaded {
                    deck, info, path, ..
                } => {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    let cues = self.restore_cues(deck, info.id, &path);
                    let pads = self.decks[deck.index()].pads;
                    let loop_beats = self.decks[deck.index()].loop_beats;
                    self.decks[deck.index()] = DeckInfo {
                        track: Some(info),
                        analysis: None,
                        transients: Vec::new(),
                        path: Some(path),
                        sidecar_key: None,
                        saved_cues: cues,
                        cues_restored: cues.iter().all(|c| !c.is_set()),
                        pads,
                        loop_beats,
                    };
                    if std::mem::take(&mut self.play_on_load[deck.index()]) {
                        self.send(Command::deck(deck, DeckAction::PlayPause));
                    }
                }
                LoadEvent::Analyzed {
                    deck, id, analysis, ..
                } => {
                    // A sidecar's grid and key (offline analyzer) beat the app's own.
                    let rate = self.engine.snapshot().sample_rate;
                    let stored = self.decks[deck.index()]
                        .path
                        .as_ref()
                        .and_then(|path| self.library.as_ref()?.track(path).ok().flatten());
                    let sidecar = stored
                        .as_ref()
                        .filter(|t| t.analysis_source == Some(AnalysisSource::Sidecar));
                    let grid = match sidecar {
                        Some(track) => track.analysis.bpm.map(|bpm| {
                            let anchor = track.analysis.grid_anchor.unwrap_or(0.0);
                            BeatGrid::from_bpm(bpm, anchor * rate as f64, rate)
                        }),
                        None => trusted_grid(&analysis),
                    };
                    let sidecar_key = sidecar.and_then(track_key);
                    // Keep the library up to date with what the app found.
                    if sidecar.is_none() {
                        if let (Some(library), Some(path)) =
                            (self.library.as_mut(), &self.decks[deck.index()].path)
                        {
                            let result = library.set_analysis(
                                path,
                                &to_library(&analysis),
                                AnalysisSource::App,
                                dj_analysis::ANALYSIS_VERSION,
                                None,
                            );
                            if let Err(err) = result {
                                tracing::warn!("saving analysis: {err}");
                            }
                            self.browser_dirty = true;
                        }
                    }
                    let info = &mut self.decks[deck.index()];
                    if info.track.as_ref().is_some_and(|t| t.id == id) {
                        info.sidecar_key = sidecar_key;
                        let auto_cue = analysis.auto_cue(grid);
                        info.transients = dj_analysis::transients::detect(&analysis.waveform);
                        info.analysis = Some(analysis);
                        self.send(Command::deck(
                            deck,
                            DeckAction::SetBeatGrid { track: id, grid },
                        ));
                        // Skip leading silence: cue the first beat, as long as the deck
                        // hasn't been touched since loading.
                        if let Some(frame) = auto_cue {
                            self.send(Command::deck(
                                deck,
                                DeckAction::AutoCue { track: id, frame },
                            ));
                        }
                    }
                }
                LoadEvent::Failed { deck, path, error } => {
                    let name = file_name(&path);
                    self.notify(format!("Couldn't load {name} onto deck {deck:?}: {error}"));
                }
                LoadEvent::Finished { .. } => {}
            }
        }
    }

    fn maybe_log_stats(&mut self) {
        let xruns = self.stats.snapshot().xruns;
        let xruns_changed = xruns != self.last_xruns;
        if xruns_changed || self.last_stats_log.elapsed() >= STATS_LOG_INTERVAL {
            if xruns_changed {
                tracing::warn!("xrun detected ({xruns} total)");
            }
            crate::log_stats(&self.stats, &mut self.engine);
            self.last_stats_log = Instant::now();
            self.last_xruns = xruns;
        }
    }

    /// The loader's info and analysis for what is on `deck` now, if they match it.
    fn info(
        &self,
        deck: DeckId,
        state: &DeckSnapshot,
    ) -> (Option<&TrackInfo>, Option<&TrackAnalysis>) {
        let info = &self.decks[deck.index()];
        let track = info.track.as_ref().filter(|t| Some(t.id) == state.track);
        let analysis = match track {
            Some(_) => info.analysis.as_deref(),
            None => None,
        };
        (track, analysis)
    }
}

impl Delegate for Frontend {
    fn on_input(&mut self, input: Input) -> Flow {
        match input {
            Input::FileDropped(path) => self.load(path),
            Input::PointerDown { x, y } => self.pointer_down(x, y),
            Input::PointerMove { x, y } => self.pointer_move(x, y),
            Input::Text(text) => self.type_text(&text),
            Input::PointerUp { .. } => self.pointer_up(),
            Input::Scroll { x, y, lines } => self.scroll(x, y, lines),
            Input::KeyDown { repeat: false, .. } if self.show_help => self.show_help = false,
            Input::KeyDown { key, repeat }
                if self.browser_wants_keys() && self.browser_key(key, repeat) => {}
            Input::KeyDown { key, repeat } => match key {
                // Esc never quits: it's what people press to leave fullscreen.
                Key::Escape => {}
                Key::Char('b') if !repeat => self.browser_focus = !self.browser_focus,
                Key::Char('/') => {
                    self.browser_focus = true;
                    self.browser.search_focused = true;
                }
                Key::Char('?') | Key::Char('h') if !repeat => self.show_help = true,
                Key::Tab if !repeat => {
                    self.selected = match self.selected {
                        DeckId::A => DeckId::B,
                        DeckId::B => DeckId::A,
                    };
                }
                Key::Space if !repeat => self.deck_action(DeckAction::PlayPause),
                Key::Char('c') if !repeat => {
                    self.cue_held = Some(self.selected);
                    self.deck_action(DeckAction::CuePress);
                }
                Key::Left => self.deck_action(DeckAction::SeekBy {
                    seconds: -SEEK_SECONDS,
                }),
                Key::Right => self.deck_action(DeckAction::SeekBy {
                    seconds: SEEK_SECONDS,
                }),
                Key::Up => self.adjust_tempo(TEMPO_FINE),
                Key::Down => self.adjust_tempo(-TEMPO_FINE),
                Key::PageUp => self.adjust_tempo(TEMPO_COARSE),
                Key::PageDown => self.adjust_tempo(-TEMPO_COARSE),
                Key::Char('k') if !repeat => self.deck_action(DeckAction::ToggleKeyLock),
                Key::Char('q') if !repeat => self.toggle_quantize(),
                Key::Char('s') if !repeat => self.toggle_sync(self.selected),
                Key::Char(',') => self.deck_action(DeckAction::Nudge { amount: -KEY_NUDGE }),
                Key::Char('.') => self.deck_action(DeckAction::Nudge { amount: KEY_NUDGE }),
                Key::Char('r') if !repeat => self.deck_action(DeckAction::CycleTempoRange),
                Key::Char('p') if !repeat => {
                    self.send(Command::channel(self.selected, ChannelControl::TogglePfl))
                }
                Key::Char('e') if !repeat => self.deck_action(DeckAction::Eject),
                Key::Char('t') if !repeat => self.send(Command::ToggleTone),
                Key::Char(c @ '1'..='8') if !repeat => {
                    let slot = c as usize - '1' as usize;
                    self.press(UiAction::HotCue(self.selected, slot));
                }
                Key::Char('i') if !repeat => self.deck_action(DeckAction::LoopIn),
                Key::Char('o') if !repeat => self.deck_action(DeckAction::LoopOut),
                Key::Char('l') if !repeat => self.deck_action(DeckAction::AutoLoop { beats: 4.0 }),
                Key::Char('[') => self.deck_action(DeckAction::LoopHalve),
                Key::Char(']') => self.deck_action(DeckAction::LoopDouble),
                Key::Char('=') | Key::Char('+') => self.zoom(1.0 / 1.25),
                Key::Char('-') => self.zoom(1.25),
                _ => {}
            },
            Input::KeyUp { key } => match key {
                Key::Char('c') => {
                    if let Some(deck) = self.cue_held.take() {
                        self.send(Command::deck(deck, DeckAction::CueRelease));
                    }
                }
                Key::Char(c @ '1'..='8') => {
                    let slot = c as usize - '1' as usize;
                    self.deck_action(DeckAction::HotCueRelease(slot));
                }
                _ => {}
            },
        }
        Flow::Continue
    }

    fn scene(&mut self, size: FrameSize) -> Scene {
        self.scale = size.scale;
        self.repeat_held_button();
        match self.audio.watchdog() {
            Some(Ok(BufferGrown { from, to })) => {
                let rate = self.engine.snapshot().sample_rate as f64;
                self.notify(format!(
                    "Audio was dropping out with a {from}-frame buffer, so it now uses {to} \
                     frames ({:.1} ms latency).",
                    to as f64 / rate * 1000.0
                ));
            }
            Some(Err(err)) => self.notify(format!("Audio problem: {err}")),
            None => {}
        }
        self.poll_loader();
        self.poll_library();
        self.maybe_log_stats();
        let state = self.engine.snapshot();
        self.persist_cues(&state);
        let stats = self.stats.snapshot();
        let peaks = [
            state.channels[0].peak,
            state.channels[1].peak,
            state.main_peak,
        ];
        for (meter, peak) in self.meters.iter_mut().zip(peaks) {
            *meter = peak.min(1.0).max(*meter - METER_FALL);
        }
        let layout = Layout::new(size);
        let mut painter = Painter::new(size.scale, self.pressed_action());
        let style = self.settings.waveform;

        // Shared scale for both scrolling waveforms: frames per physical pixel at 1:1.
        let base_fpp = self.zoom_seconds * state.sample_rate as f64 / layout.width as f64;
        let analyses = DeckId::ALL.map(|deck| self.info(deck, state.deck(deck)).1);

        // The key each deck sounds in now: without key lock, tempo moves it too.
        let keys = DeckId::ALL.map(|deck| {
            let d = state.deck(deck);
            let (info, analysis) = self.info(deck, d);
            info?;
            let key = self.decks[deck.index()]
                .sidecar_key
                .or_else(|| analysis.and_then(|a| a.key).map(|k| k.key))?;
            let shift = match d.key_lock {
                true => 0,
                false => (12.0 * (d.tempo_rate as f64).log2()).round() as i32,
            };
            Some((key.transposed(shift), shift))
        });

        // The phase offset only means something while both decks play.
        let both_playing = DeckId::ALL
            .iter()
            .all(|&deck| state.deck(deck).transport.is_moving());
        let phase_ms = match both_playing {
            true => phase_offset_ms(&state, analyses),
            false => None,
        };
        painter.top_bar(
            layout.top_bar,
            TopBar {
                quantize: state.quantize,
                zoom_seconds: self.zoom_seconds,
                stats: &stats,
                help: self.show_help,
                phase_ms,
            },
        );
        let look = waves::WaveLook {
            base_fpp,
            style,
            grid_to: layout.transients.y + layout.transients.height / 2.0,
        };
        for deck in DeckId::ALL {
            let d = state.deck(deck);
            painter.scrolling(
                deck,
                layout.waveform(deck),
                d,
                analyses[deck.index()],
                &look,
            );
        }
        let transients = DeckId::ALL.map(|deck| match analyses[deck.index()] {
            Some(_) => self.decks[deck.index()].transients.as_slice(),
            None => &[],
        });
        painter.transients(
            layout.transients,
            [
                (state.deck(DeckId::A), transients[0]),
                (state.deck(DeckId::B), transients[1]),
            ],
            base_fpp,
            style,
        );
        painter.playhead(layout.waveforms[0], layout.waveforms[1]);

        for deck in DeckId::ALL {
            let d = state.deck(deck);
            let (info, analysis) = self.info(deck, d);
            let key = keys[deck.index()].map(|(key, shift)| {
                let other = keys[deck.other().index()].map(|(k, _)| k);
                KeyDisplay {
                    key,
                    shift,
                    compatible: other.is_some_and(|other| key.compatible(&other)),
                }
            });
            let deck_info = &self.decks[deck.index()];
            painter.deck_panel(DeckView {
                deck,
                area: layout.deck(deck),
                d,
                info,
                analysis,
                selected: deck == self.selected,
                sample_rate: state.sample_rate,
                key,
                notation: self.settings.key_notation,
                pads: deck_info.pads,
                loop_beats: deck_info.loop_beats.0,
                style,
            });
        }
        painter.mixer(MixerView {
            area: layout.mixer,
            state: &state,
            meters: self.meters,
        });

        let drag = match self.grab {
            Some(Grab::Row {
                track,
                dragging: true,
                ..
            }) => self
                .browser
                .rows()
                .find(|t| t.id == track)
                .map(|t| (t.title.clone(), self.pointer)),
            _ => None,
        };
        let loaded = DeckId::ALL.map(|deck| {
            let path = self.decks[deck.index()].path.as_ref()?;
            self.browser
                .rows()
                .find(|t| &t.path == path)
                .map(|t| t.id)
                .or_else(|| {
                    self.library
                        .as_ref()?
                        .track(path)
                        .ok()
                        .flatten()
                        .map(|t| t.id)
                })
        });
        self.browser_visible = painter.browser(BrowserView {
            area: layout.browser,
            browser: &self.browser,
            focused: self.browser_focus,
            selected_deck: self.selected,
            loaded,
            deck_keys: keys.map(|k| k.map(|(key, _)| key)),
            notation: self.settings.key_notation,
            drag: drag.as_ref().map(|(title, at)| (title.as_str(), *at)),
            naming: self.naming.as_ref().map(|(id, text)| (*id, text.as_str())),
            delete_armed: self.delete_armed,
        });

        if let Some((message, posted)) = &self.notice {
            match posted.elapsed() < NOTICE_SECONDS {
                true => painter.notice(&layout, message),
                false => self.notice = None,
            }
        }
        if self.show_help {
            painter.help(&layout);
        }
        self.hits = painter.hits;
        painter.scene
    }
}

/// Real-time offset between the kicks nearest each deck's playhead, in milliseconds,
/// wrapped to within half a beat. Positive: deck A's kick came first (A is ahead).
fn phase_offset_ms(state: &EngineSnapshot, analyses: [Option<&TrackAnalysis>; 2]) -> Option<f64> {
    let rate = state.sample_rate as f64;
    let mut since = [0.0; 2];
    let mut beat_ms = None;
    for deck in DeckId::ALL {
        let d = state.deck(deck);
        d.track?;
        let analysis = analyses[deck.index()]?;
        let position = d.position as f64;
        let kick = analysis.nearest_kick(position)? as f64;
        let speed = (d.tempo_rate + d.nudge).max(0.01) as f64;
        // Seconds of real time since that kick played (negative: still to come).
        since[deck.index()] = (position - kick) / rate / speed;
        if deck == DeckId::A {
            beat_ms = analysis.kick_bpm().map(|bpm| 60_000.0 / (bpm * speed));
        }
    }
    let offset = (since[0] - since[1]) * 1000.0;
    // Kicks a beat apart are in phase as far as the ear is concerned.
    let offset = match beat_ms {
        Some(beat) => offset - beat * (offset / beat).round(),
        None => offset,
    };
    Some(offset)
}

/// Something a click or tap can do.
#[derive(Debug, Clone, Copy, PartialEq)]
enum UiAction {
    Deck(DeckId, DeckAction),
    /// CUE: pressed while held, released on let go.
    Cue(DeckId),
    /// Tempo step (a rate change); repeats while held.
    Tempo(DeckId, f32),
    /// Pitch bend impulse; repeats while held.
    Nudge(DeckId, f32),
    Sync(DeckId),
    Quantize,
    Pfl(DeckId),
    SplitCue,
    Zoom(f64),
    Select(DeckId),
    /// Hot cue pad: pressed while held (or deleted, when DELETE is armed).
    HotCue(DeckId, usize),
    /// Roll pad, in beats: rolls while held.
    Roll(DeckId, f32),
    PadMode(DeckId, PadMode),
    DeleteArm(DeckId),
    /// Multiply the auto-loop length (and the active loop) by this.
    LoopSize(DeckId, f32),
    /// Start a loop of the chosen length, or leave the active one.
    AutoLoop(DeckId),
    LoadSelected(DeckId),
    Rescan,
    Help,
    NewCollection(CollectionKind),
    RenameCollection,
    /// Press twice to delete the crate or playlist being viewed.
    DeleteCollection,
    RemoveFromCollection,
    MoveInPlaylist(i32),
}

impl UiAction {
    fn repeats(self) -> bool {
        matches!(
            self,
            UiAction::Tempo(..) | UiAction::Nudge(..) | UiAction::Zoom(_)
        )
    }
}

/// A control with a continuous value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slider {
    Tempo(DeckId),
    Trim(DeckId),
    /// Band 0 = low, 1 = mid, 2 = high.
    Eq(DeckId, usize),
    Filter(DeckId),
    Fader(DeckId),
    Main,
    Phones,
    CueMix,
}

impl Slider {
    /// Whether the control has a centre detent (and snaps to it).
    fn centred(self) -> bool {
        matches!(
            self,
            Slider::Tempo(_) | Slider::Trim(_) | Slider::Eq(..) | Slider::Filter(_)
        )
    }

    /// Where the control is now, 0..1.
    fn unit(self, state: &EngineSnapshot) -> f32 {
        match self {
            Slider::Tempo(deck) => (state.deck(deck).tempo_fader + 1.0) / 2.0,
            Slider::Trim(deck) => state.channel(deck).trim,
            Slider::Eq(deck, band) => state.channel(deck).eq[band.min(2)],
            Slider::Filter(deck) => (state.channel(deck).filter + 1.0) / 2.0,
            Slider::Fader(deck) => state.channel(deck).fader,
            Slider::Main => state.master.main_level,
            Slider::Phones => state.master.phones_level,
            Slider::CueMix => state.master.cue_mix,
        }
    }

    /// The command that moves the control to `unit` (0..1).
    fn command(self, unit: f32) -> Command {
        let unit = unit.clamp(0.0, 1.0);
        let centred = unit * 2.0 - 1.0;
        match self {
            Slider::Tempo(deck) => Command::deck(deck, DeckAction::SetTempo { fader: centred }),
            Slider::Trim(deck) => Command::channel(deck, ChannelControl::Trim(unit)),
            Slider::Eq(deck, 0) => Command::channel(deck, ChannelControl::EqLow(unit)),
            Slider::Eq(deck, 1) => Command::channel(deck, ChannelControl::EqMid(unit)),
            Slider::Eq(deck, _) => Command::channel(deck, ChannelControl::EqHigh(unit)),
            Slider::Filter(deck) => Command::channel(deck, ChannelControl::Filter(centred)),
            Slider::Fader(deck) => Command::channel(deck, ChannelControl::Fader(unit)),
            Slider::Main => Command::Master(MasterControl::MainLevel(unit)),
            Slider::Phones => Command::Master(MasterControl::PhonesLevel(unit)),
            Slider::CueMix => Command::Master(MasterControl::CueMix(unit)),
        }
    }

    /// Where a double-click puts it: the centre, or full for levels, or cue only for
    /// the headphone mix.
    fn default_unit(self) -> f32 {
        match self {
            _ if self.centred() => 0.5,
            Slider::CueMix => 0.0,
            _ => 1.0,
        }
    }
}

/// How dragging a slider maps to its value.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    /// Absolute: the pointer's height on the fader's travel, which starts `top`
    /// pixels down and runs `travel` pixels. Inverted faders are highest at the top.
    Vertical {
        inverted: bool,
        top: f32,
        travel: f32,
    },
    /// Relative: dragging up turns it up, [`KNOB_TRAVEL`] pixels end to end.
    Knob,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Control {
    Button(UiAction),
    Slider(Slider, Drag),
    /// The overview strip: click or drag to move the playhead.
    Overview(DeckId, u64),
    /// A scrolling waveform, with its frames per pixel: drag to scrub or bend, wheel to
    /// zoom, click to select the deck.
    Waveform(DeckId, f64),
    /// Anywhere on the browser: clicking it gives it the arrow keys.
    BrowserArea,
    SearchBox,
    /// The browser's track list (for the mouse wheel).
    BrowserList,
    BrowserRow(usize),
    BrowserSort(Column),
    /// 0 is all tracks, then the sources in order.
    BrowserSource(usize),
    /// A crate or playlist, by index into the browser's list.
    Collection(usize),
}

/// A clickable area recorded while painting.
#[derive(Debug, Clone, Copy)]
struct Hit {
    area: Area,
    control: Control,
}

/// What the pointer is holding.
#[derive(Debug, Clone, Copy)]
enum Grab {
    Button {
        action: UiAction,
        next_repeat: Instant,
    },
    Slider {
        slider: Slider,
        drag: Drag,
        /// Pointer height and the control's value when grabbed (for knobs).
        from_y: f32,
        from_unit: f32,
    },
    Overview {
        deck: DeckId,
        area: Area,
        length: u64,
    },
    /// A browser row pressed; dragging it onto a crate or playlist adds it there.
    Row {
        track: i64,
        from: (f32, f32),
        dragging: bool,
    },
    Waveform {
        deck: DeckId,
        frames_per_pixel: f64,
        last_x: f32,
        /// Where the dragged (paused) playhead is, ahead of the engine's snapshot.
        position: f64,
        moved: bool,
    },
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `m:ss.t` for a frame count.
fn clock(frames: u64, sample_rate: u32) -> String {
    let tenths = frames * 10 / sample_rate.max(1) as u64;
    let seconds = tenths / 10;
    format!("{}:{:02}.{}", seconds / 60, seconds % 60, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dj_analysis::analyze_frames;
    use dj_engine::TrackId;

    #[test]
    fn slider_commands_round_trip_through_the_snapshot() {
        let mut state = EngineSnapshot::default();
        state.decks[0].tempo_fader = -0.5;
        state.channels[1].filter = 0.5;
        state.master.cue_mix = 0.25;
        assert_eq!(Slider::Tempo(DeckId::A).unit(&state), 0.25);
        assert_eq!(Slider::Filter(DeckId::B).unit(&state), 0.75);
        assert_eq!(
            Slider::Tempo(DeckId::A).command(0.25),
            Command::deck(DeckId::A, DeckAction::SetTempo { fader: -0.5 })
        );
        assert_eq!(
            Slider::CueMix.command(state.master.cue_mix),
            Command::Master(MasterControl::CueMix(0.25))
        );
        // Out-of-range drags are clamped.
        assert_eq!(
            Slider::Fader(DeckId::B).command(1.7),
            Command::channel(DeckId::B, ChannelControl::Fader(1.0))
        );
        assert_eq!(Slider::Eq(DeckId::A, 1).default_unit(), 0.5);
        assert_eq!(Slider::Main.default_unit(), 1.0);
        assert_eq!(Slider::CueMix.default_unit(), 0.0);
    }

    #[test]
    fn clock_formats_minutes_seconds_and_tenths() {
        assert_eq!(clock(0, 48_000), "0:00.0");
        assert_eq!(clock(48_000 * 65 + 24_000, 48_000), "1:05.5");
        assert_eq!(clock(48_000 * 600 + 47_999, 48_000), "10:00.9");
    }

    fn with_kicks(kicks: Vec<u64>) -> TrackAnalysis {
        let mut a = analyze_frames(std::iter::empty(), 48_000, 0);
        a.kicks = kicks;
        a
    }

    #[test]
    fn phase_offset_compares_time_since_nearest_kicks() {
        let mut state = EngineSnapshot::default();
        for d in &mut state.decks {
            d.track = Some(TrackId(1));
        }
        // A is 480 frames (10 ms) past its kick, B is exactly on its kick.
        state.decks[0].position = 48_480;
        state.decks[1].position = 96_000;
        let a = with_kicks(vec![48_000, 72_000]);
        let b = with_kicks(vec![96_000]);
        let offset = phase_offset_ms(&state, [Some(&a), Some(&b)]).unwrap();
        assert!((offset - 10.0).abs() < 1e-9, "{offset}");

        // Twice the speed: half the real time since the kick.
        state.decks[0].tempo_rate = 2.0;
        let offset = phase_offset_ms(&state, [Some(&a), Some(&b)]).unwrap();
        assert!((offset - 5.0).abs() < 1e-9, "{offset}");

        assert_eq!(phase_offset_ms(&state, [Some(&a), None]), None);
    }
}
