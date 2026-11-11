//! Around the decks: the top bar, notices and the keyboard help.

use dj_audio_io::{HISTOGRAM_BUCKETS, StatsSnapshot};
use dj_engine::{DeckId, FxControl, FxSnapshot, FxTarget};
use dj_ui::{Align, Rect};

use super::deck_view::beats_label;
use super::layout::{Area, Layout};
use super::painter::Painter;
use super::theme::*;
use super::{Control, Slider, UiAction};

/// Keyboard shortcuts, as shown by HELP: (keys, what they do).
const SHORTCUTS: [(&str, &str); 33] = [
    ("Space", "play / pause the selected deck"),
    ("C (hold)", "cue"),
    ("Tab", "select the other deck"),
    ("1 – 8", "hot cues (stem pads in STEMS mode)"),
    ("M", "pads: STEMS / HOT CUE"),
    ("N", "pads: SAMPLER / HOT CUE"),
    ("I / O", "loop in / out"),
    ("L", "4-beat loop"),
    ("[ / ]", "halve / double the loop"),
    ("← / →", "seek (whole bars with quantize)"),
    ("↑ / ↓", "tempo, fine (hold to glide)"),
    ("Page Up / Down", "tempo, 1% steps"),
    (", / .", "bend (nudge) slower / faster"),
    ("R", "tempo range"),
    ("S", "sync on / off"),
    ("K", "key lock"),
    ("Q", "quantize"),
    ("J", "slip mode"),
    ("D", "reverse"),
    ("P", "headphone cue"),
    ("E", "eject"),
    ("+ / −", "zoom the waveforms"),
    ("F", "beat FX on / off"),
    ("G", "next effect"),
    ("Z / X", "FX length shorter / longer"),
    ("V", "tap the FX tempo"),
    ("B", "browser keys: ↑/↓ select, ←/→ views, Enter load"),
    ("/", "search the browser"),
    ("Esc", "leave the browser or search"),
    ("W", "record the main output on / off"),
    ("U", "settings (also the button at the top right)"),
    ("T", "test tone"),
    ("F11", "fullscreen on / off"),
];

pub struct TopBar<'a> {
    pub quantize: bool,
    pub zoom_seconds: f64,
    pub stats: &'a StatsSnapshot,
    pub help: bool,
    /// Kick offset between the decks while both play.
    pub phase_ms: Option<f64>,
    pub fx: FxSnapshot,
    /// Seconds recorded so far, while recording.
    pub recording: Option<f64>,
}

impl Painter {
    pub fn top_bar(&mut self, area: Area, v: TopBar<'_>) {
        self.push(
            Rect::new(area.x, area.y, area.width, area.height, th().panel_top).gradient(th().panel),
        );
        self.fill(
            Area::new(
                area.x,
                area.bottom() - self.px(1.0),
                area.width,
                self.px(1.0),
            ),
            th().line,
        );
        let inner = Area::new(
            area.x + self.px(12.0),
            area.y + self.px(5.0),
            area.width - self.px(24.0),
            area.height - self.px(10.0),
        );

        // Left: the name, then audio health.
        let (name, rest) = inner.split_left(self.px(30.0), self.px(12.0));
        self.bold(name, "dj", 15.0, th().text, Align::Left);
        let (health, rest) = rest.split_left(self.px(118.0), self.px(10.0));
        self.audio_health(health, v.stats);
        // REC lights up and shows how long it has been recording.
        let (rec, rest) = rest.split_left(self.px(60.0), self.px(8.0));
        let label = match v.recording {
            Some(seconds) => crate::recorder::clock(seconds),
            None => "REC".to_string(),
        };
        self.button(
            rec,
            &label,
            v.recording.is_some(),
            th().danger,
            UiAction::Record,
        );

        // Right: settings, help, zoom, quantize.
        let gap = self.px(6.0);
        let (settings, rest) = rest.split_right(self.px(30.0), gap);
        self.icon_button(
            settings,
            super::painter::Icon::Settings,
            false,
            th().text,
            UiAction::Settings(super::settings_view::SettingsAction::Open),
        );
        let (help, rest) = rest.split_right(self.px(26.0), gap * 2.0);
        self.button(help, "?", v.help, th().text, UiAction::Help);
        let (zoom_in, rest) = rest.split_right(self.px(28.0), gap);
        self.button(zoom_in, "+", false, th().text, UiAction::Zoom(1.0 / 1.25));
        let (zoom_label, rest) = rest.split_right(self.px(40.0), gap);
        self.label(
            zoom_label,
            format!("{:.1} s", v.zoom_seconds),
            11.0,
            th().soft_text,
            Align::Center,
        );
        let (zoom_out, rest) = rest.split_right(self.px(28.0), gap * 2.0);
        self.button(zoom_out, "−", false, th().text, UiAction::Zoom(1.25));
        let (quantize, rest) = rest.split_right(self.px(86.0), gap);
        self.button(
            quantize,
            "QUANTIZE",
            v.quantize,
            th().cue,
            UiAction::Quantize,
        );

        // Centre: the phase meter, with the beat FX either side.
        let w = self.px(240.0);
        let centre = Area::new(area.centre_x() - w / 2.0, inner.y, w, inner.height);
        if centre.x > rest.x && centre.right() < rest.right() {
            self.phase_meter(centre, v.phase_ms);
            let left = Area::new(rest.x, rest.y, centre.x - rest.x - gap * 2.0, rest.height);
            let right_x = centre.right() + gap * 2.0;
            let right = Area::new(right_x, rest.y, rest.right() - right_x, rest.height);
            self.fx_controls(left, right, &v.fx);
        }
    }

    /// The beat FX: ON, effect and length at `left`; target, level and tempo (TAP) at
    /// `right`. Parts that don't fit are left out.
    fn fx_controls(&mut self, left: Area, right: Area, fx: &FxSnapshot) {
        let gap = self.px(4.0);
        let step = self.px(20.0);
        let fx_color = match fx.kind.has_tail() {
            true => th().echo,
            false => th().filter,
        };
        let needed = self.px(44.0 + 20.0 * 4.0 + 66.0 + 36.0) + gap * 7.0;
        if left.width >= needed {
            let (on, rest) = left.split_left(self.px(44.0), gap * 2.0);
            self.button(on, "FX", fx.on, fx_color, UiAction::Fx(FxControl::ToggleOn));
            let (prev, rest) = rest.split_left(step, gap);
            self.icon_button(
                prev,
                super::painter::Icon::Left,
                false,
                th().text,
                UiAction::Fx(FxControl::StepKind(-1)),
            );
            let (kind, rest) = rest.split_left(self.px(66.0), gap);
            self.well(kind);
            self.bold(kind, fx.kind.label(), 10.0, fx_color, Align::Center);
            self.hit(kind, Control::Button(UiAction::Fx(FxControl::StepKind(1))));
            let (next, rest) = rest.split_left(step, gap * 2.0);
            self.icon_button(
                next,
                super::painter::Icon::Right,
                false,
                th().text,
                UiAction::Fx(FxControl::StepKind(1)),
            );
            let (shorter, rest) = rest.split_left(step, gap);
            self.icon_button(
                shorter,
                super::painter::Icon::Left,
                false,
                th().text,
                UiAction::Fx(FxControl::StepBeats(-1)),
            );
            let (beats, rest) = rest.split_left(self.px(36.0), gap);
            self.label(beats, beats_label(fx.beats), 11.0, th().text, Align::Center);
            let (longer, _) = rest.split_left(step, gap);
            self.icon_button(
                longer,
                super::painter::Icon::Right,
                false,
                th().text,
                UiAction::Fx(FxControl::StepBeats(1)),
            );
        }

        let needed = self.px(24.0 * 2.0 + 40.0 + 26.0 + 36.0 + 28.0) + gap * 6.0;
        if right.width >= needed {
            let mut rest = right;
            let targets = [
                ("A", FxTarget::Deck(DeckId::A), th().decks[0]),
                ("B", FxTarget::Deck(DeckId::B), th().decks[1]),
                ("MAIN", FxTarget::Main, th().text),
            ];
            for (label, target, color) in targets {
                let w = match target {
                    FxTarget::Main => self.px(40.0),
                    FxTarget::Deck(_) => self.px(24.0),
                };
                let (cell, after) = rest.split_left(w, gap);
                self.button(
                    cell,
                    label,
                    fx.target == target,
                    color,
                    UiAction::Fx(FxControl::SetTarget(target)),
                );
                rest = after;
            }
            let (level, rest) = rest.split_left(self.px(26.0), gap * 2.0);
            self.small_knob(level, fx.level, fx_color, Slider::FxLevel);
            // TAP shows the FX tempo; once tapped, a click on the tempo goes back to
            // following the decks.
            let (tap, rest) = rest.split_left(self.px(36.0), gap);
            self.button(
                tap,
                "TAP",
                fx.tapped,
                th().cue,
                UiAction::Fx(FxControl::Tap),
            );
            let (bpm, _) = rest.split_left(self.px(28.0), 0.0);
            let ink = match fx.tapped {
                true => th().cue,
                false => th().soft_text,
            };
            self.label(bpm, format!("{:.0}", fx.bpm), 10.0, ink, Align::Left);
            if fx.tapped {
                self.hit(bpm, Control::Button(UiAction::Fx(FxControl::AutoTempo)));
            }
        }
    }

    /// Callback-duration histogram (log-scaled bars) and the xrun count.
    fn audio_health(&mut self, area: Area, stats: &StatsSnapshot) {
        let bar_w = self.px(3.0);
        let gap = self.px(1.0);
        let max_count = stats.histogram.iter().copied().max().unwrap_or(0).max(1);
        for (i, &count) in stats.histogram.iter().enumerate() {
            let fraction = ((count as f32 + 1.0).ln() / (max_count as f32 + 1.0).ln()).min(1.0);
            let h = (area.height - self.px(4.0)) * fraction;
            let color = match i {
                10 => th().danger,
                7..=9 => th().warning,
                _ => th().histogram,
            };
            self.fill(
                Area::new(
                    area.x + i as f32 * (bar_w + gap),
                    area.bottom() - self.px(2.0) - h,
                    bar_w,
                    h,
                ),
                color,
            );
        }
        let x = area.x + HISTOGRAM_BUCKETS as f32 * (bar_w + gap) + self.px(8.0);
        let (text, color) = match stats.xruns {
            0 => ("audio ok".to_string(), th().soft_text),
            1 => ("1 dropout".to_string(), th().danger),
            n => (format!("{n} dropouts"), th().danger),
        };
        self.label(
            Area::new(x, area.y, area.right() - x, area.height),
            text,
            10.0,
            color,
            Align::Left,
        );
    }

    /// A message at the bottom of the window, over the browser.
    pub fn notice(&mut self, layout: &Layout, message: &str) {
        self.popup(|p| {
            let w = (layout.width * 0.6).min(p.px(720.0));
            let h = p.px(34.0);
            let area = Area::new(
                (layout.width - w) / 2.0,
                layout.height - h - p.px(18.0),
                w,
                h,
            );
            p.push(
                Rect::new(area.x, area.y, area.width, area.height, th().popup)
                    .gradient(th().popup_bottom)
                    .rounded(p.px(6.0)),
            );
            p.push(
                Rect::new(area.x, area.y, area.width, area.height, th().line)
                    .rounded(p.px(6.0))
                    .stroke(p.px(1.0)),
            );
            p.rounded(
                Area::new(area.x, area.y, p.px(4.0), area.height),
                p.px(2.0),
                th().warning,
            );
            p.label(
                area.inset_x(p.px(16.0)),
                message,
                12.0,
                th().text,
                Align::Left,
            );
        });
    }

    /// The keyboard shortcuts, over everything; a click anywhere closes it.
    pub fn help(&mut self, layout: &Layout) {
        self.popup(|p| {
            let screen = Area::new(0.0, 0.0, layout.width, layout.height);
            p.fill(screen, th().scrim);
            let row_h = p.px(22.0);
            let rows = SHORTCUTS.len().div_ceil(2);
            let w = p.px(760.0).min(layout.width - p.px(32.0));
            let h = row_h * rows as f32 + p.px(80.0);
            let panel = Area::new((layout.width - w) / 2.0, (layout.height - h) / 2.0, w, h);
            p.push(
                Rect::new(panel.x, panel.y, panel.width, panel.height, th().popup)
                    .gradient(th().popup_bottom)
                    .rounded(p.px(8.0)),
            );
            let inner = panel.inset(p.px(20.0));
            let (title, list) = inner.split_top(p.px(24.0), p.px(12.0));
            p.bold(title, "Keyboard", 16.0, th().text, Align::Left);
            p.label(
                title,
                "Everything here is also a button. Click anywhere to close.",
                11.0,
                th().soft_text,
                Align::Right,
            );
            for (column, area) in list.columns(2, p.px(24.0)).enumerate() {
                for (i, (keys, what)) in SHORTCUTS.iter().skip(column * rows).take(rows).enumerate()
                {
                    let row = Area::new(area.x, area.y + i as f32 * row_h, area.width, row_h);
                    let (key_area, text) = row.split_left(p.px(116.0), p.px(10.0));
                    p.bold(key_area, *keys, 11.0, th().cue, Align::Right);
                    p.label(text, *what, 12.0, th().text, Align::Left);
                }
            }
            p.hit(screen, Control::Button(UiAction::Help));
        });
    }
}
