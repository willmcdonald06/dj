//! The mixer between the deck panels: a channel strip per deck (trim, EQ and filter
//! knobs; fader, meter and headphone cue), and the master section in the middle.

use dj_engine::{DeckId, EngineSnapshot};
use dj_ui::Align;

use super::layout::Area;
use super::painter::Painter;
use super::theme::*;
use super::waves::deck_letter;
use super::{Slider, UiAction};

pub struct MixerView<'a> {
    pub area: Area,
    pub state: &'a EngineSnapshot,
    /// Smoothed meter levels: deck A, deck B, main.
    pub meters: [f32; 3],
}

impl Painter {
    pub fn mixer(&mut self, v: MixerView<'_>) {
        self.panel(v.area);
        let inner = v.area.inset(self.px(8.0));
        let gap = self.px(6.0);
        let strip_w = self.px(102.0);
        let (a, rest) = inner.split_left(strip_w, gap);
        let (b, centre) = rest.split_right(strip_w, gap);
        self.channel_strip(DeckId::A, a, &v);
        self.channel_strip(DeckId::B, b, &v);
        self.master_section(centre, &v);
    }

    fn channel_strip(&mut self, deck: DeckId, area: Area, v: &MixerView<'_>) {
        let ch = v.state.channel(deck);
        let color = th().decks[deck.index()];
        let gap = self.px(6.0);
        let knob_w = self.px(48.0);
        // Knobs on the outside, the fader towards the middle, like the deck panels.
        let (knobs, rest) = match deck {
            DeckId::A => area.split_left(knob_w, gap),
            DeckId::B => area.split_right(knob_w, gap),
        };
        let knob_h = knobs.height / 5.0;
        let rows: [(&str, f32, bool, dj_ui::Color, Slider); 5] = [
            ("TRIM", ch.trim, true, th().text, Slider::Trim(deck)),
            ("HI", ch.eq[2], true, color, Slider::Eq(deck, 2)),
            ("MID", ch.eq[1], true, color, Slider::Eq(deck, 1)),
            ("LOW", ch.eq[0], true, color, Slider::Eq(deck, 0)),
            (
                "FILTER",
                (ch.filter + 1.0) / 2.0,
                true,
                th().filter,
                Slider::Filter(deck),
            ),
        ];
        for (i, (label, value, centred, c, slider)) in rows.into_iter().enumerate() {
            let cell = Area::new(
                knobs.x,
                knobs.y + i as f32 * knob_h,
                knobs.width,
                knob_h - self.px(2.0),
            );
            self.knob(cell, label, value, centred, c, slider);
        }

        // Deck letter, fader and meter, headphone cue.
        let (letter, rest) = rest.split_top(self.px(14.0), self.px(4.0));
        self.bold(letter, deck_letter(deck), 11.0, color, Align::Center);
        let (column, cue) = rest.split_top(rest.height - self.px(30.0), self.px(6.0));
        let meter_w = self.px(6.0);
        let (meter, fader) = match deck {
            DeckId::A => column.split_right(meter_w, self.px(4.0)),
            DeckId::B => column.split_left(meter_w, self.px(4.0)),
        };
        self.fader(
            fader,
            ch.fader,
            false,
            false,
            th().text,
            Slider::Fader(deck),
        );
        self.meter(
            Area::new(
                meter.x,
                meter.y + self.px(8.0),
                meter.width,
                meter.height - self.px(16.0),
            ),
            v.meters[deck.index()],
        );
        self.button(cue, "CUE", ch.pfl, th().cue, UiAction::Pfl(deck));
    }

    /// Main and headphone levels, the headphone cue/main blend, split cue and the main
    /// meter.
    fn master_section(&mut self, area: Area, v: &MixerView<'_>) {
        let m = &v.state.master;
        let (label, rest) = area.split_top(self.px(14.0), self.px(4.0));
        self.bold(label, "MAIN", 9.0, th().soft_text, Align::Center);
        let knob_h = self.px(46.0);
        let (main, rest) = rest.split_top(knob_h, self.px(4.0));
        self.knob(main, "LEVEL", m.main_level, false, th().text, Slider::Main);
        let meter_h = (rest.height - 3.0 * knob_h - self.px(56.0)).max(self.px(20.0));
        let (meter, rest) = rest.split_top(meter_h, self.px(6.0));
        let w = self.px(6.0);
        self.meter(
            Area::new(meter.centre_x() - w / 2.0, meter.y, w, meter.height),
            v.meters[2],
        );
        let (heading, rest) = rest.split_top(self.px(12.0), self.px(2.0));
        self.bold(heading, "PHONES", 9.0, th().soft_text, Align::Center);
        let (phones, rest) = rest.split_top(knob_h, self.px(4.0));
        self.knob(
            phones,
            "LEVEL",
            m.phones_level,
            false,
            th().text,
            Slider::Phones,
        );
        let (mix, rest) = rest.split_top(knob_h, self.px(6.0));
        self.knob(
            mix,
            "CUE · MAIN",
            m.cue_mix,
            false,
            th().cue,
            Slider::CueMix,
        );
        let (split, _) = rest.split_top(self.px(22.0), 0.0);
        self.button(split, "SPLIT", m.split_cue, th().cue, UiAction::SplitCue);
    }
}
