//! The settings screen: a panel over everything, one row per setting, each changed with
//! ◂ ▸ (or by clicking its value). Changes apply and save at once.

use dj_ui::{Align, Rect};

use super::layout::{Area, Layout};
use super::painter::{Icon, Painter};
use super::theme::*;
use super::{Control, UiAction};

/// One line on the settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingRow {
    Device,
    Buffer,
    Output,
    TempoRange,
    KeyLock,
    Quantize,
    KeyNotation,
    Waveform,
    Theme,
    Midi,
}

impl SettingRow {
    pub const ALL: [SettingRow; 10] = [
        SettingRow::Device,
        SettingRow::Buffer,
        SettingRow::Output,
        SettingRow::TempoRange,
        SettingRow::KeyLock,
        SettingRow::Quantize,
        SettingRow::KeyNotation,
        SettingRow::Waveform,
        SettingRow::Theme,
        SettingRow::Midi,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingRow::Device => "Output device",
            SettingRow::Buffer => "Buffer size",
            SettingRow::Output => "Outputs",
            SettingRow::TempoRange => "Tempo range",
            SettingRow::KeyLock => "Key lock at start",
            SettingRow::Quantize => "Quantize at start",
            SettingRow::KeyNotation => "Key notation",
            SettingRow::Waveform => "Waveform colours",
            SettingRow::Theme => "Theme",
            SettingRow::Midi => "MIDI controller",
        }
    }

    /// The heading this row sits under.
    fn section(self) -> &'static str {
        match self {
            SettingRow::Device | SettingRow::Buffer | SettingRow::Output => "AUDIO",
            SettingRow::TempoRange | SettingRow::KeyLock | SettingRow::Quantize => "DECKS",
            SettingRow::KeyNotation | SettingRow::Waveform | SettingRow::Theme => "DISPLAY",
            SettingRow::Midi => "CONTROLLER",
        }
    }

    /// The row `by` places away, clamped to the list.
    pub fn moved(self, by: i32) -> Self {
        let i = Self::ALL.iter().position(|&r| r == self).unwrap_or(0) as i32;
        Self::ALL[(i + by).clamp(0, Self::ALL.len() as i32 - 1) as usize]
    }
}

/// What the settings screen's controls do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingsAction {
    Open,
    Close,
    Select(SettingRow),
    /// Choose the previous (-1) or next (+1) value.
    Step(SettingRow, i32),
    /// A click on the panel that hits nothing: keeps it open.
    Nothing,
}

/// One row as shown: the current value and a line of detail under it.
pub struct RowView {
    pub row: SettingRow,
    pub value: String,
    pub detail: String,
    /// Detail drawn as a warning (e.g. the device fell back to another setup).
    pub warn: bool,
}

pub struct SettingsView<'a> {
    pub rows: &'a [RowView],
    pub selected: SettingRow,
    /// Where the settings are saved.
    pub path: &'a str,
}

impl Painter {
    pub fn settings(&mut self, layout: &Layout, v: SettingsView<'_>) {
        self.popup(|p| {
            let screen = Area::new(0.0, 0.0, layout.width, layout.height);
            p.fill(screen, th().scrim);
            p.hit(
                screen,
                Control::Button(UiAction::Settings(SettingsAction::Close)),
            );

            let row_h = p.px(46.0);
            let heading_h = p.px(24.0);
            let sections = 4.0;
            let w = p.px(640.0).min(layout.width - p.px(32.0));
            let h = (row_h * v.rows.len() as f32 + heading_h * sections + p.px(118.0))
                .min(layout.height - p.px(16.0));
            let panel = Area::new((layout.width - w) / 2.0, (layout.height - h) / 2.0, w, h);
            p.push(
                Rect::new(panel.x, panel.y, panel.width, panel.height, th().popup)
                    .gradient(th().popup_bottom)
                    .rounded(p.px(8.0)),
            );
            p.push(
                Rect::new(panel.x, panel.y, panel.width, panel.height, th().line)
                    .rounded(p.px(8.0))
                    .stroke(p.px(1.0)),
            );
            p.hit(
                panel,
                Control::Button(UiAction::Settings(SettingsAction::Nothing)),
            );

            let inner = panel.inset(p.px(20.0));
            let (title, rest) = inner.split_top(p.px(28.0), p.px(8.0));
            p.bold(title, "Settings", 16.0, th().text, Align::Left);
            let (close, hints) = title.split_right(p.px(72.0), p.px(12.0));
            p.label(
                hints,
                "↑↓ choose   ◂ ▸ change   Esc close",
                10.0,
                th().soft_text,
                Align::Right,
            );
            p.button(
                close,
                "CLOSE",
                false,
                th().text,
                UiAction::Settings(SettingsAction::Close),
            );

            let (list, footer) = rest.split_top(rest.height - p.px(28.0), p.px(4.0));
            let mut y = list.y;
            let mut section = "";
            for row in v.rows {
                if row.row.section() != section {
                    section = row.row.section();
                    let heading = Area::new(list.x, y, list.width, heading_h);
                    p.bold(heading, section, 10.0, th().soft_text, Align::Left);
                    y += heading_h;
                }
                let area = Area::new(list.x, y, list.width, row_h);
                p.setting_row(area, row, row.row == v.selected);
                y += row_h;
            }

            p.label(
                footer,
                format!("Changes apply at once and are saved to {}", v.path),
                10.0,
                th().soft_text,
                Align::Left,
            );
        });
    }

    fn setting_row(&mut self, area: Area, row: &RowView, selected: bool) {
        let id = row.row;
        if selected {
            self.rounded(area, self.px(4.0), th().selected_row);
        }
        self.hit(
            area,
            Control::Button(UiAction::Settings(SettingsAction::Select(id))),
        );
        let inner = area.inset_x(self.px(10.0));
        let (label, rest) = inner.split_left(self.px(150.0), self.px(10.0));
        self.label(label, id.label(), 12.0, th().text, Align::Left);

        let control_h = self.px(26.0);
        let top = area.y + self.px(4.0);
        let step = self.px(26.0);
        let value_w = (rest.width - step * 2.0 - self.px(8.0)).max(self.px(60.0));
        let prev = Area::new(rest.x, top, step, control_h);
        let value = Area::new(prev.right() + self.px(4.0), top, value_w, control_h);
        let next = Area::new(value.right() + self.px(4.0), top, step, control_h);
        self.icon_button(
            prev,
            Icon::Left,
            false,
            th().text,
            UiAction::Settings(SettingsAction::Step(id, -1)),
        );
        self.well(value);
        let ink = match selected {
            true => th().text,
            false => th().text,
        };
        self.bold(
            value.inset_x(self.px(8.0)),
            &row.value,
            11.0,
            ink,
            Align::Left,
        );
        self.hit(
            value,
            Control::Button(UiAction::Settings(SettingsAction::Step(id, 1))),
        );
        self.icon_button(
            next,
            Icon::Right,
            false,
            th().text,
            UiAction::Settings(SettingsAction::Step(id, 1)),
        );
        let detail = Area::new(
            value.x + self.px(8.0),
            value.bottom(),
            value.width,
            area.bottom() - value.bottom(),
        );
        let ink = match row.warn {
            true => th().warning,
            false => th().soft_text,
        };
        self.label(detail, &row.detail, 10.0, ink, Align::Left);
    }
}

/// The MIDI learn bar's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnAction {
    Done,
    /// Forget every learned control.
    Clear,
}

/// What the MIDI learn bar shows.
pub struct LearnBar<'a> {
    /// The connected controller's name.
    pub controller: Option<&'a str>,
    /// The control picked on screen, waiting for the controller, and where it is.
    pub waiting: Option<(&'a str, Area)>,
    /// The last thing that happened (learned, refused…).
    pub status: &'a str,
}

impl Painter {
    /// MIDI learn: a bar along the bottom (over the browser, never the waveforms) and an
    /// outline round the control being learned.
    pub fn learn_bar(&mut self, layout: &Layout, v: LearnBar<'_>) {
        self.popup(|p| {
            if let Some((_, area)) = v.waiting {
                p.push(
                    Rect::new(
                        area.x - p.px(3.0),
                        area.y - p.px(3.0),
                        area.width + p.px(6.0),
                        area.height + p.px(6.0),
                        th().warning,
                    )
                    .rounded(p.px(5.0))
                    .stroke(p.px(2.0)),
                );
            }
            let h = p.px(56.0);
            let bar = Area::new(0.0, layout.height - h, layout.width, h);
            p.push(
                Rect::new(bar.x, bar.y, bar.width, bar.height, th().learn_bar)
                    .gradient(th().learn_bar_bottom),
            );
            p.fill(Area::new(bar.x, bar.y, bar.width, p.px(2.0)), th().warning);
            p.hit(bar, Control::Button(UiAction::Settings(SettingsAction::Nothing)));
            let inner = Area::new(
                bar.x + p.px(16.0),
                bar.y + p.px(10.0),
                bar.width - p.px(32.0),
                bar.height - p.px(18.0),
            );
            let (done, rest) = inner.split_right(p.px(72.0), p.px(8.0));
            p.button(done, "DONE", true, th().warning, UiAction::Learn(LearnAction::Done));
            let (clear, rest) = rest.split_right(p.px(92.0), p.px(16.0));
            p.tinted_button(
                clear,
                "CLEAR ALL",
                false,
                th().danger,
                UiAction::Learn(LearnAction::Clear),
            );
            let (title, rest) = rest.split_left(p.px(104.0), p.px(12.0));
            p.bold(title, "MIDI LEARN", 13.0, th().warning, Align::Left);
            let (top, bottom) = rest.split_top(rest.height / 2.0, 0.0);
            let headline = match (v.controller, v.waiting) {
                (None, _) => "No MIDI controller found: plug one in (it connects within a \
                              couple of seconds)."
                    .to_string(),
                (Some(name), Some((target, _))) => {
                    format!("Now move or press the control for {target} on {name}")
                }
                (Some(name), None) => {
                    format!("Click a button, knob or fader on screen, then use the one on {name} you want for it")
                }
            };
            p.label(top, headline, 12.0, th().text, Align::Left);
            p.label(bottom, v.status, 10.5, th().soft_text, Align::Left);
        });
    }
}
