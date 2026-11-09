//! Around the decks: the top bar, notices and the keyboard help.

use dj_audio_io::{HISTOGRAM_BUCKETS, StatsSnapshot};
use dj_ui::{Align, Color, Rect};

use super::layout::{Area, Layout};
use super::painter::Painter;
use super::theme::*;
use super::{Control, UiAction};

/// Keyboard shortcuts, as shown by HELP: (keys, what they do).
const SHORTCUTS: [(&str, &str); 23] = [
    ("Space", "play / pause the selected deck"),
    ("C (hold)", "cue"),
    ("Tab", "select the other deck"),
    ("1 – 8", "hot cues"),
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
    ("P", "headphone cue"),
    ("E", "eject"),
    ("+ / −", "zoom the waveforms"),
    ("B", "browser keys: ↑/↓ select, Enter load"),
    ("/", "search the browser"),
    ("Esc", "leave the browser or search"),
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
}

impl Painter {
    pub fn top_bar(&mut self, area: Area, v: TopBar<'_>) {
        self.push(Rect::new(area.x, area.y, area.width, area.height, PANEL_TOP).gradient(PANEL));
        self.fill(
            Area::new(
                area.x,
                area.bottom() - self.px(1.0),
                area.width,
                self.px(1.0),
            ),
            LINE,
        );
        let inner = Area::new(
            area.x + self.px(12.0),
            area.y + self.px(5.0),
            area.width - self.px(24.0),
            area.height - self.px(10.0),
        );

        // Left: the name, then audio health.
        let (name, rest) = inner.split_left(self.px(30.0), self.px(12.0));
        self.bold(name, "dj", 15.0, TEXT, Align::Left);
        let (health, rest) = rest.split_left(self.px(200.0), self.px(12.0));
        self.audio_health(health, v.stats);

        // Right: help, zoom, quantize.
        let gap = self.px(6.0);
        let (help, rest) = rest.split_right(self.px(52.0), gap * 2.0);
        self.button(help, "HELP", v.help, TEXT, UiAction::Help);
        let (zoom_in, rest) = rest.split_right(self.px(28.0), gap);
        self.button(zoom_in, "+", false, TEXT, UiAction::Zoom(1.0 / 1.25));
        let (zoom_label, rest) = rest.split_right(self.px(54.0), gap);
        self.label(
            zoom_label,
            format!("{:.1} s", v.zoom_seconds),
            11.0,
            SOFT_TEXT,
            Align::Center,
        );
        let (zoom_out, rest) = rest.split_right(self.px(28.0), gap * 2.0);
        self.button(zoom_out, "−", false, TEXT, UiAction::Zoom(1.25));
        let (quantize, rest) = rest.split_right(self.px(86.0), gap);
        self.button(quantize, "QUANTIZE", v.quantize, CUE, UiAction::Quantize);

        // Centre: the phase meter.
        let w = self.px(300.0);
        let centre = Area::new(area.centre_x() - w / 2.0, inner.y, w, inner.height);
        if centre.x > rest.x && centre.right() < rest.right() {
            self.phase_meter(centre, v.phase_ms);
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
                10 => DANGER,
                7..=9 => WARNING,
                _ => Color::rgb(0.25, 0.32, 0.45),
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
            0 => ("audio ok".to_string(), SOFT_TEXT),
            1 => ("1 dropout".to_string(), DANGER),
            n => (format!("{n} dropouts"), DANGER),
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
                Rect::new(
                    area.x,
                    area.y,
                    area.width,
                    area.height,
                    Color::rgb(0.07, 0.07, 0.085),
                )
                .gradient(Color::rgb(0.045, 0.045, 0.055))
                .rounded(p.px(6.0)),
            );
            p.push(
                Rect::new(area.x, area.y, area.width, area.height, LINE)
                    .rounded(p.px(6.0))
                    .stroke(p.px(1.0)),
            );
            p.rounded(
                Area::new(area.x, area.y, p.px(4.0), area.height),
                p.px(2.0),
                WARNING,
            );
            p.label(area.inset_x(p.px(16.0)), message, 12.0, TEXT, Align::Left);
        });
    }

    /// The keyboard shortcuts, over everything; a click anywhere closes it.
    pub fn help(&mut self, layout: &Layout) {
        self.popup(|p| {
            let screen = Area::new(0.0, 0.0, layout.width, layout.height);
            p.fill(screen, Color::rgba(0.0, 0.0, 0.0, 0.7));
            let row_h = p.px(22.0);
            let rows = SHORTCUTS.len().div_ceil(2);
            let w = p.px(760.0).min(layout.width - p.px(32.0));
            let h = row_h * rows as f32 + p.px(80.0);
            let panel = Area::new((layout.width - w) / 2.0, (layout.height - h) / 2.0, w, h);
            p.push(
                Rect::new(
                    panel.x,
                    panel.y,
                    panel.width,
                    panel.height,
                    Color::rgb(0.05, 0.05, 0.06),
                )
                .gradient(Color::rgb(0.03, 0.03, 0.037))
                .rounded(p.px(8.0)),
            );
            let inner = panel.inset(p.px(20.0));
            let (title, list) = inner.split_top(p.px(24.0), p.px(12.0));
            p.bold(title, "Keyboard", 16.0, TEXT, Align::Left);
            p.label(
                title,
                "Everything here is also a button. Click anywhere to close.",
                11.0,
                SOFT_TEXT,
                Align::Right,
            );
            for (column, area) in list.columns(2, p.px(24.0)).enumerate() {
                for (i, (keys, what)) in SHORTCUTS.iter().skip(column * rows).take(rows).enumerate()
                {
                    let row = Area::new(area.x, area.y + i as f32 * row_h, area.width, row_h);
                    let (key_area, text) = row.split_left(p.px(116.0), p.px(10.0));
                    p.bold(key_area, *keys, 11.0, CUE, Align::Right);
                    p.label(text, *what, 12.0, TEXT, Align::Left);
                }
            }
            p.hit(screen, Control::Button(UiAction::Help));
        });
    }
}
