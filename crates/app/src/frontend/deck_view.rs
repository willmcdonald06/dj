//! A deck's panel under the waveforms: track header, overview, transport, loops, pads
//! and the tempo fader.

use dj_analysis::{Key as MusicalKey, KeyNotation, TrackAnalysis};
use dj_engine::{DeckAction, DeckId, DeckSnapshot, HotCue, TempoRange, Transport};
use dj_loader::TrackInfo;
use dj_ui::{Align, Rect, WaveformStyle};

use super::layout::Area;
use super::painter::{Icon, Painter, ink_on};
use super::theme::*;
use super::waves::deck_letter;
use super::{
    Control, JUMP_PADS, KEY_NUDGE, PadMode, PadState, ROLL_PADS, Slider, TEMPO_FINE, UiAction,
    clock,
};

/// A deck's key as shown in its header.
#[derive(Debug, Clone, Copy)]
pub struct KeyDisplay {
    /// The key you hear (moved by the tempo when key lock is off).
    pub key: MusicalKey,
    /// Semitones the tempo moved it.
    pub shift: i32,
    /// Mixes harmonically with the other deck's key.
    pub compatible: bool,
}

/// What a deck panel draws.
pub struct DeckView<'a> {
    pub deck: DeckId,
    pub area: Area,
    pub d: &'a DeckSnapshot,
    pub info: Option<&'a TrackInfo>,
    pub analysis: Option<&'a TrackAnalysis>,
    pub selected: bool,
    pub sample_rate: u32,
    pub key: Option<KeyDisplay>,
    pub notation: KeyNotation,
    pub pads: PadState,
    /// Length of the next auto loop, in beats.
    pub loop_beats: f32,
    pub style: WaveformStyle,
}

impl Painter {
    pub fn deck_panel(&mut self, v: DeckView<'_>) {
        self.panel(v.area);
        let pad = self.px(8.0);
        let gap = self.px(6.0);
        let inner = v.area.inset(pad);
        let (header, rest) = inner.split_top(self.px(40.0), gap);
        self.deck_header(&v, header);
        let (overview, rest) = rest.split_top(self.px(34.0), gap);
        self.overview(v.deck, overview, v.d, v.analysis, v.style);

        // Tempo fader on the outer edge: deck A's left, deck B's right.
        let tempo_w = self.px(40.0);
        let (tempo, controls) = match v.deck {
            DeckId::A => rest.split_left(tempo_w, gap * 1.5),
            DeckId::B => rest.split_right(tempo_w, gap * 1.5),
        };
        self.tempo_column(&v, tempo);
        let (transport, rest) = controls.split_top(self.px(40.0), gap);
        self.transport_row(&v, transport);
        let (loops, rest) = rest.split_top(self.px(28.0), gap);
        self.loop_row(&v, loops);
        let (tabs, pads) = rest.split_top(self.px(18.0), self.px(4.0));
        self.pad_tabs(&v, tabs);
        self.pads(&v, pads);
    }

    fn deck_header(&mut self, v: &DeckView<'_>, area: Area) {
        let color = DECK_COLORS[v.deck.index()];
        let d = v.d;
        // Deck letter: filled when this deck takes the keyboard.
        let (tile, rest) = area.split_left(area.height, self.px(10.0));
        let r = self.px(5.0);
        let letter_color = match v.selected {
            true => {
                self.push(
                    Rect::new(tile.x, tile.y, tile.width, tile.height, color)
                        .gradient(darker(color, 0.7))
                        .rounded(r),
                );
                ink_on(color)
            }
            false => {
                self.push(
                    Rect::new(tile.x, tile.y, tile.width, tile.height, color)
                        .rounded(r)
                        .stroke(self.px(1.5)),
                );
                color
            }
        };
        self.bold(tile, deck_letter(v.deck), 20.0, letter_color, Align::Center);
        self.hit(tile, Control::Button(UiAction::Select(v.deck)));

        // Right: key, BPM and time, each a big figure over a small one.
        let (time, rest) = rest.split_right(self.px(82.0), self.px(10.0));
        let (bpm, rest) = rest.split_right(self.px(76.0), self.px(10.0));
        let (key, title) = rest.split_right(self.px(46.0), self.px(10.0));
        let (big, small) = (18.0, 9.5);
        let (top, bottom) = time.split_top(time.height * 0.62, 0.0);
        if d.track.is_some() {
            let remaining = clock(d.length.saturating_sub(d.position), v.sample_rate);
            self.bold(top, format!("-{remaining}"), big, TEXT, Align::Right);
            self.label(
                bottom,
                clock(d.position, v.sample_rate),
                small,
                SOFT_TEXT,
                Align::Right,
            );
        }

        let (top, bottom) = bpm.split_top(bpm.height * 0.62, 0.0);
        let bpm_text = match (
            d.bpm(v.sample_rate),
            v.analysis.and_then(TrackAnalysis::kick_bpm),
        ) {
            (Some(bpm), _) => format!("{bpm:.2}"),
            // No trusted grid: a rough figure from the kicks.
            (None, Some(bpm)) => format!("~{:.1}", bpm * d.tempo_rate as f64),
            (None, None) => "—".to_string(),
        };
        let bpm_color = match d.sync_master {
            true => color,
            false => TEXT,
        };
        if d.track.is_some() {
            self.bold(top, bpm_text, big, bpm_color, Align::Right);
        }
        let range = match d.tempo_range {
            TempoRange::Percent8 => "8",
            TempoRange::Percent16 => "16",
            TempoRange::Percent50 => "50",
        };
        let tempo = (d.tempo_rate - 1.0) * 100.0;
        self.label(
            bottom,
            format!("{tempo:+.2}%  ±{range}"),
            small,
            SOFT_TEXT,
            Align::Right,
        );

        // Key chip in the key's colour; outlined in green when it mixes with the other
        // deck.
        if let Some(k) = v.key {
            let chip_h = self.px(20.0);
            let chip = Area::new(key.x, key.y + self.px(2.0), key.width, chip_h);
            let fill = key_color(k.key);
            self.rounded(chip, self.px(4.0), fill);
            if k.compatible {
                self.push(
                    Rect::new(
                        chip.x - self.px(2.0),
                        chip.y - self.px(2.0),
                        chip.width + self.px(4.0),
                        chip.height + self.px(4.0),
                        PLAY,
                    )
                    .rounded(self.px(5.0))
                    .stroke(self.px(1.5)),
                );
            }
            self.bold(
                chip,
                k.key.name(v.notation),
                11.0,
                ink_on(fill),
                Align::Center,
            );
            let below = Area::new(
                key.x,
                chip.bottom() + self.px(2.0),
                key.width,
                key.bottom() - chip.bottom() - self.px(2.0),
            );
            let note = match (k.shift, d.key_lock) {
                (_, true) => "LOCK".to_string(),
                (0, false) => String::new(),
                (shift, false) => format!("{shift:+}"),
            };
            self.label(below, note, 8.5, SOFT_TEXT, Align::Center);
        }

        // Title over artist.
        let (top, bottom) = title.split_top(title.height * 0.55, 0.0);
        match (d.track, v.info) {
            (None, _) => self.label(top, "No track", 14.0, DIM_TEXT, Align::Left),
            (Some(_), None) => self.label(top, "Loading…", 14.0, SOFT_TEXT, Align::Left),
            (Some(_), Some(info)) => {
                self.bold(top, info.title.clone(), 14.0, TEXT, Align::Left);
                if let Some(artist) = &info.artist {
                    self.label(bottom, artist.clone(), 11.0, SOFT_TEXT, Align::Left);
                }
            }
        }
    }

    /// CUE, PLAY, SYNC, KEY LOCK; then nudge and fine tempo steps.
    fn transport_row(&mut self, v: &DeckView<'_>, area: Area) {
        let d = v.d;
        let deck = v.deck;
        let color = DECK_COLORS[deck.index()];
        let gap = self.px(6.0);
        let playing = d.transport.is_moving();
        let cue_lit = d.track.is_some()
            && (d.transport == Transport::CuePreview || (!playing && d.position == d.cue));
        let big = self.px(60.0);
        let (cue, rest) = area.split_left(big, gap);
        self.button(cue, "CUE", cue_lit, CUE, UiAction::Cue(deck));
        let (play, rest) = rest.split_left(big, gap * 2.0);
        let icon = match playing {
            true => Icon::Pause,
            false => Icon::Play,
        };
        self.icon_button(
            play,
            icon,
            playing,
            PLAY,
            UiAction::Deck(deck, DeckAction::PlayPause),
        );

        let small = self.px(30.0);
        let (right, rest) = rest.split_right(small * 4.0 + gap * 2.0 + self.px(10.0), gap);
        let sync_label = match d.sync_master && d.sync_lock {
            true => "MASTER",
            false => "SYNC",
        };
        let (sync, rest) = rest.split_left(self.px(64.0), gap);
        self.button(sync, sync_label, d.sync_lock, color, UiAction::Sync(deck));
        let (key, _) = rest.split_left(self.px(52.0), gap);
        self.button(
            key,
            "KEY",
            d.key_lock,
            KEY_LOCK,
            UiAction::Deck(deck, DeckAction::ToggleKeyLock),
        );

        // Bend (arrows) and fine tempo (−/+), both repeat while held.
        let (bend, tempo) = right.split_left(small * 2.0 + gap, self.px(10.0));
        let mut bends = bend.columns(2, gap);
        if let (Some(slow), Some(fast)) = (bends.next(), bends.next()) {
            self.icon_button(
                slow,
                Icon::Left,
                false,
                TEXT,
                UiAction::Nudge(deck, -KEY_NUDGE),
            );
            self.icon_button(
                fast,
                Icon::Right,
                false,
                TEXT,
                UiAction::Nudge(deck, KEY_NUDGE),
            );
        }
        let mut steps = tempo.columns(2, gap);
        if let (Some(down), Some(up)) = (steps.next(), steps.next()) {
            self.button(down, "−", false, TEXT, UiAction::Tempo(deck, -TEMPO_FINE));
            self.button(up, "+", false, TEXT, UiAction::Tempo(deck, TEMPO_FINE));
        }
    }

    /// Loop in/out, the auto-loop size and its halve/double steps, reloop.
    fn loop_row(&mut self, v: &DeckView<'_>, area: Area) {
        let d = v.d;
        let deck = v.deck;
        let gap = self.px(6.0);
        let (in_out, rest) = area.split_left(self.px(96.0), gap);
        let mut cells = in_out.columns(2, gap);
        if let (Some(loop_in), Some(loop_out)) = (cells.next(), cells.next()) {
            self.button(
                loop_in,
                "IN",
                d.loop_in.is_some(),
                LOOP,
                UiAction::Deck(deck, DeckAction::LoopIn),
            );
            self.button(
                loop_out,
                "OUT",
                false,
                LOOP,
                UiAction::Deck(deck, DeckAction::LoopOut),
            );
        }
        let (reloop, rest) = rest.split_right(self.px(70.0), gap);
        self.button(
            reloop,
            "RELOOP",
            false,
            LOOP,
            UiAction::Deck(deck, DeckAction::ReloopExit),
        );
        let step = self.px(30.0);
        let (halve, rest) = rest.split_left(step, gap);
        let (double, size) = rest.split_right(step, gap);
        self.icon_button(
            halve,
            Icon::Left,
            false,
            TEXT,
            UiAction::LoopSize(deck, 0.5),
        );
        self.icon_button(
            double,
            Icon::Right,
            false,
            TEXT,
            UiAction::LoopSize(deck, 2.0),
        );
        let beats = match d.loop_active {
            true => d
                .loop_region
                .zip(d.grid)
                .map(|(r, g)| (r.len() / g.period) as f32)
                .unwrap_or(v.loop_beats),
            false => v.loop_beats,
        };
        let label = format!("LOOP  {}", beats_label(beats));
        self.button(size, &label, d.loop_active, LOOP, UiAction::AutoLoop(deck));
    }

    fn pad_tabs(&mut self, v: &DeckView<'_>, area: Area) {
        let color = DECK_COLORS[v.deck.index()];
        let modes = [
            ("HOT CUE", PadMode::HotCue),
            ("ROLL", PadMode::Roll),
            ("JUMP", PadMode::Jump),
        ];
        let tab_w = self.px(64.0);
        let mut x = area.x;
        for (label, mode) in modes {
            let tab = Area::new(x, area.y, tab_w, area.height);
            let active = v.pads.mode == mode;
            let ink = match active {
                true => TEXT,
                false => SOFT_TEXT,
            };
            self.bold(tab, label, 9.0, ink, Align::Left);
            if active {
                let w = self.px(label.len() as f32 * 6.2);
                self.fill(
                    Area::new(tab.x, tab.bottom() - self.px(2.0), w, self.px(2.0)),
                    color,
                );
            }
            self.hit(tab, Control::Button(UiAction::PadMode(v.deck, mode)));
            x += tab_w;
        }
        if v.pads.mode == PadMode::HotCue {
            let (delete, _) = area.split_right(self.px(64.0), 0.0);
            let label = match v.pads.delete_armed {
                true => "TAP A CUE",
                false => "DELETE",
            };
            self.tinted_button(
                delete,
                label,
                v.pads.delete_armed,
                DANGER,
                UiAction::DeleteArm(v.deck),
            );
        }
    }

    fn pads(&mut self, v: &DeckView<'_>, area: Area) {
        let d = v.d;
        let deck = v.deck;
        for (i, pad) in area.columns(8, self.px(5.0)).enumerate() {
            match v.pads.mode {
                PadMode::HotCue => {
                    let cue = d.hot_cues[i];
                    let label = match cue {
                        HotCue::Empty | HotCue::Point(_) => format!("{}", i + 1),
                        HotCue::Loop(_) => format!("{} LOOP", i + 1),
                    };
                    self.tinted_button(
                        pad,
                        &label,
                        cue.is_set(),
                        HOT_CUE_COLORS[i],
                        UiAction::HotCue(deck, i),
                    );
                }
                PadMode::Roll => {
                    let (label, beats) = ROLL_PADS[i];
                    let on = d.slip_position.is_some()
                        && d.loop_region.is_some_and(|r| {
                            d.grid.is_some_and(|g| {
                                ((r.len() / g.period) as f32 - beats).abs() < 0.001
                            })
                        });
                    self.button(pad, label, on, LOOP, UiAction::Roll(deck, beats));
                }
                PadMode::Jump => {
                    let (label, beats) = JUMP_PADS[i];
                    self.button(
                        pad,
                        label,
                        false,
                        TEXT,
                        UiAction::Deck(deck, DeckAction::BeatJump { beats }),
                    );
                }
            }
        }
    }

    /// The tempo range (click to cycle) over a vertical tempo fader: up is slower, as
    /// on a CDJ.
    fn tempo_column(&mut self, v: &DeckView<'_>, area: Area) {
        let deck = v.deck;
        let range = match v.d.tempo_range {
            TempoRange::Percent8 => "±8%",
            TempoRange::Percent16 => "±16%",
            TempoRange::Percent50 => "±50%",
        };
        let (button, fader) = area.split_top(self.px(22.0), self.px(6.0));
        self.button(
            button,
            range,
            false,
            TEXT,
            UiAction::Deck(deck, DeckAction::CycleTempoRange),
        );
        let value = (v.d.tempo_fader + 1.0) / 2.0;
        self.fader(
            fader,
            value,
            true,
            true,
            DECK_COLORS[deck.index()],
            Slider::Tempo(deck),
        );
    }
}

/// `1/4`, `1`, `16`…
pub fn beats_label(beats: f32) -> String {
    match beats >= 1.0 {
        true => format!("{}", beats.round() as u32),
        false => format!("1/{}", (1.0 / beats).round() as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_lengths_read_as_fractions_or_whole_beats() {
        assert_eq!(beats_label(0.25), "1/4");
        assert_eq!(beats_label(0.03125), "1/32");
        assert_eq!(beats_label(1.0), "1");
        assert_eq!(beats_label(16.0), "16");
    }
}
