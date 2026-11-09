//! Builds the scene: primitives and the widgets everything else is drawn with. Sizes
//! are given in logical pixels and scaled with [`Painter::px`].

use std::f32::consts::PI;

use dj_ui::{Align, Color, Rect, Scene, Shape, Text};

use super::layout::Area;
use super::theme::*;
use super::{Control, Drag, Hit, Slider, UiAction};

/// Knobs turn through 270°, from seven o'clock to five o'clock.
const KNOB_SWEEP: f32 = 1.5 * PI;

pub struct Painter {
    pub scene: Scene,
    scale: f32,
    /// Clickable areas, topmost last.
    pub hits: Vec<Hit>,
    /// The button being held, drawn pressed.
    pub pressed: Option<UiAction>,
    /// Drawing into the popup layer, over everything else.
    popup: bool,
}

/// Something drawn on a button instead of a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Play,
    Pause,
    /// Pointing left or right, for steps and nudges.
    Left,
    Right,
}

impl Painter {
    pub fn new(scale: f32, pressed: Option<UiAction>) -> Self {
        Self {
            scene: Scene {
                background: BACKGROUND,
                ..Scene::default()
            },
            scale,
            hits: Vec::new(),
            pressed,
            popup: false,
        }
    }

    pub fn px(&self, logical: f32) -> f32 {
        logical * self.scale
    }

    pub fn push(&mut self, rect: Rect) {
        match self.popup {
            true => self.scene.popup_rects.push(rect),
            false => self.scene.rects.push(rect),
        }
    }

    /// Draw what `draw` draws over everything else (popups, toasts, drag feedback).
    pub fn popup(&mut self, draw: impl FnOnce(&mut Self)) {
        let was = std::mem::replace(&mut self.popup, true);
        draw(self);
        self.popup = was;
    }

    pub fn fill(&mut self, area: Area, color: Color) {
        self.push(Rect::new(area.x, area.y, area.width, area.height, color));
    }

    pub fn rounded(&mut self, area: Area, radius: f32, color: Color) {
        self.push(Rect::new(area.x, area.y, area.width, area.height, color).rounded(radius));
    }

    /// Drawn over the waveforms (markers, playheads, overlays).
    pub fn overlay(&mut self, rect: Rect) {
        match self.popup {
            true => self.scene.popup_rects.push(rect),
            false => self.scene.overlay.push(rect),
        }
    }

    pub fn overlay_fill(&mut self, area: Area, color: Color) {
        self.overlay(Rect::new(area.x, area.y, area.width, area.height, color));
    }

    pub fn text(&mut self, text: Text) {
        match self.popup {
            true => self.scene.popup_texts.push(text),
            false => self.scene.texts.push(text),
        }
    }

    /// Top of a line of `size` text centred vertically in `area`.
    pub fn text_y(area: Area, size: f32) -> f32 {
        area.y + (area.height - size * 1.3) / 2.0
    }

    /// A line of text centred vertically in `area`, aligned within it, clipped to it.
    pub fn label(
        &mut self,
        area: Area,
        content: impl Into<String>,
        size: f32,
        color: Color,
        align: Align,
    ) {
        let size = self.px(size);
        let x = match align {
            Align::Left => area.x,
            Align::Center => area.centre_x(),
            Align::Right => area.right(),
        };
        self.text(
            Text::new(content, x, Self::text_y(area, size), size, color)
                .align(align)
                .max_width(area.width.max(1.0)),
        );
    }

    /// Like [`Painter::label`], bold.
    pub fn bold(
        &mut self,
        area: Area,
        content: impl Into<String>,
        size: f32,
        color: Color,
        align: Align,
    ) {
        let size = self.px(size);
        let x = match align {
            Align::Left => area.x,
            Align::Center => area.centre_x(),
            Align::Right => area.right(),
        };
        self.text(
            Text::new(content, x, Self::text_y(area, size), size, color)
                .bold()
                .align(align)
                .max_width(area.width.max(1.0)),
        );
    }

    /// A raised panel: rounded, a touch lighter at the top.
    pub fn panel(&mut self, area: Area) {
        let r = self.px(6.0);
        self.push(
            Rect::new(area.x, area.y, area.width, area.height, PANEL_TOP)
                .gradient(PANEL)
                .rounded(r),
        );
    }

    /// A sunken well inside a panel.
    pub fn well(&mut self, area: Area) {
        let r = self.px(3.0);
        self.rounded(area, r, WELL);
    }

    pub fn hit(&mut self, area: Area, control: Control) {
        self.hits.push(Hit { area, control });
    }

    /// The body of a button: lit buttons fill with `color`; held ones darken.
    fn button_body(&mut self, area: Area, on: bool, color: Color, held: bool) -> Color {
        let r = self.px(4.0);
        let rect = Rect::new(area.x, area.y, area.width, area.height, BUTTON_TOP).rounded(r);
        match (on, held) {
            (true, false) => {
                self.push(
                    rect.shape(Shape::Fill)
                        .gradient(darker(color, 0.62))
                        .with_color(color),
                );
                ink_on(color)
            }
            (true, true) => {
                self.push(
                    rect.with_color(darker(color, 0.55))
                        .gradient(darker(color, 0.4)),
                );
                ink_on(darker(color, 0.5))
            }
            (false, true) => {
                self.push(rect.with_color(BUTTON_HELD));
                TEXT
            }
            (false, false) => {
                self.push(rect.gradient(BUTTON_BOTTOM));
                self.push(rect.with_color(LINE).stroke(self.px(1.0)));
                SOFT_TEXT
            }
        }
    }

    /// A clickable button with a label. `on` lights it in `color`.
    pub fn button(&mut self, area: Area, label: &str, on: bool, color: Color, action: UiAction) {
        let held = self.pressed == Some(action);
        let ink = self.button_body(area, on, color, held);
        let size = (area.height / self.scale * 0.36).clamp(9.0, 14.0);
        self.bold(area, label, size, ink, Align::Center);
        self.hit(area, Control::Button(action));
    }

    /// A button whose label, when off, is drawn in `color` (a mode or colour hint).
    pub fn tinted_button(
        &mut self,
        area: Area,
        label: &str,
        on: bool,
        color: Color,
        action: UiAction,
    ) {
        let held = self.pressed == Some(action);
        let ink = self.button_body(area, on, color, held);
        let ink = match on || held {
            true => ink,
            false => color,
        };
        let size = (area.height / self.scale * 0.36).clamp(9.0, 14.0);
        self.bold(area, label, size, ink, Align::Center);
        self.hit(area, Control::Button(action));
    }

    /// A clickable button with an icon.
    pub fn icon_button(
        &mut self,
        area: Area,
        icon: Icon,
        on: bool,
        color: Color,
        action: UiAction,
    ) {
        let held = self.pressed == Some(action);
        let ink = self.button_body(area, on, color, held);
        let size = (area.height.min(area.width) * 0.42).min(self.px(22.0));
        self.icon(area, icon, size, ink);
        self.hit(area, Control::Button(action));
    }

    /// An icon `size` pixels tall, centred in `area`.
    pub fn icon(&mut self, area: Area, icon: Icon, size: f32, color: Color) {
        let cx = area.centre_x();
        let cy = area.y + area.height / 2.0;
        let triangle = |turns: u8, w: f32| {
            Rect::new(cx - w / 2.0, cy - size / 2.0, w, size, color).shape(Shape::Triangle {
                quarter_turns: turns,
            })
        };
        match icon {
            Icon::Play => self.push(triangle(0, size)),
            Icon::Right => self.push(triangle(0, size * 0.7)),
            Icon::Left => self.push(triangle(2, size * 0.7)),
            Icon::Pause => {
                let bar = size * 0.3;
                for x in [cx - size * 0.35, cx + size * 0.05] {
                    self.push(
                        Rect::new(x, cy - size / 2.0, bar, size, color).rounded(self.px(1.0)),
                    );
                }
            }
        }
    }

    /// A rotary knob in `area` (a square at its top, label below). `value` is 0..1;
    /// `centred` knobs light from twelve o'clock. Drag up or down to turn; double-click
    /// resets.
    pub fn knob(
        &mut self,
        area: Area,
        label: &str,
        value: f32,
        centred: bool,
        color: Color,
        slider: Slider,
    ) {
        let label_h = self.px(12.0);
        let d = (area.height - label_h).min(area.width).max(4.0);
        let dial = Area::new(area.centre_x() - d / 2.0, area.y, d, d);
        let start = -KNOB_SWEEP / 2.0;
        let angle = start + value.clamp(0.0, 1.0) * KNOB_SWEEP;
        let track = self.px(3.0);
        let ring = |from: f32, to: f32, c: Color| {
            Rect::new(dial.x, dial.y, d, d, c).shape(Shape::Arc {
                thickness: track,
                start: from,
                end: to,
            })
        };
        self.push(ring(start, -start, LINE));
        let (from, to) = match centred {
            true => (angle.min(0.0), angle.max(0.0)),
            false => (start, angle),
        };
        if to - from > 0.01 {
            self.push(ring(from, to, color));
        }
        // Cap: a disc lit from above, with a pointer dot.
        let cap = d - track * 2.0 - self.px(4.0);
        let cap_area = Area::new(
            dial.centre_x() - cap / 2.0,
            dial.y + (d - cap) / 2.0,
            cap,
            cap,
        );
        let disc = Shape::Arc {
            thickness: cap,
            start: 0.0,
            end: 2.0 * PI,
        };
        self.push(
            Rect::new(
                cap_area.x,
                cap_area.y,
                cap,
                cap,
                Color::rgb(0.09, 0.09, 0.105),
            )
            .gradient(Color::rgb(0.035, 0.035, 0.042))
            .shape(disc),
        );
        let dot = self.px(4.0);
        let reach = cap / 2.0 - dot;
        let (sin, cos) = angle.sin_cos();
        let (px, py) = (
            dial.centre_x() + sin * reach,
            dial.y + d / 2.0 - cos * reach,
        );
        self.push(
            Rect::new(px - dot / 2.0, py - dot / 2.0, dot, dot, TEXT).shape(Shape::Arc {
                thickness: dot,
                start: 0.0,
                end: 2.0 * PI,
            }),
        );
        let label_area = Area::new(area.x, dial.bottom(), area.width, label_h);
        self.bold(label_area, label, 8.5, SOFT_TEXT, Align::Center);
        self.hit(
            Area::new(area.x, area.y, area.width, d),
            Control::Slider(slider, Drag::Knob),
        );
    }

    /// A vertical fader: `value` 0..1, bottom to top (or top to bottom when `inverted`,
    /// like a tempo fader, where up is slower). `centre` draws a centre detent line.
    pub fn fader(
        &mut self,
        area: Area,
        value: f32,
        inverted: bool,
        centre: bool,
        color: Color,
        slider: Slider,
    ) {
        let cap_h = self.px(16.0);
        let slot_w = self.px(4.0);
        let travel = (area.height - cap_h).max(1.0);
        let top = area.y + cap_h / 2.0;
        // Scale marks.
        for i in 0..=10 {
            let y = top + travel * i as f32 / 10.0;
            let major = i % 5 == 0;
            let w = match major {
                true => self.px(7.0),
                false => self.px(4.0),
            };
            let mark_color = match centre && i == 5 {
                true => SOFT_TEXT,
                false => LINE,
            };
            for x in [area.x, area.right() - w] {
                self.fill(Area::new(x, y - self.px(0.5), w, self.px(1.0)), mark_color);
            }
        }
        let slot = Area::new(area.centre_x() - slot_w / 2.0, top, slot_w, travel);
        self.rounded(slot, slot_w / 2.0, WELL);
        let position = match inverted {
            true => value.clamp(0.0, 1.0),
            false => 1.0 - value.clamp(0.0, 1.0),
        };
        let cy = top + travel * position;
        // Lit from the detent (or the bottom) to the cap.
        let (from, to) = match centre {
            true => (top + travel / 2.0, cy),
            false => (area.bottom() - cap_h / 2.0, cy),
        };
        let (y0, y1) = (from.min(to), from.max(to));
        self.rounded(Area::new(slot.x, y0, slot_w, y1 - y0), slot_w / 2.0, color);
        let cap_w = (area.width - self.px(6.0)).min(self.px(34.0));
        let cap = Area::new(
            area.centre_x() - cap_w / 2.0,
            cy - cap_h / 2.0,
            cap_w,
            cap_h,
        );
        self.push(
            Rect::new(
                cap.x,
                cap.y,
                cap.width,
                cap.height,
                Color::rgb(0.32, 0.32, 0.36),
            )
            .gradient(Color::rgb(0.12, 0.12, 0.14))
            .rounded(self.px(3.0)),
        );
        self.fill(
            Area::new(
                cap.x + self.px(3.0),
                cy - self.px(1.0),
                cap.width - self.px(6.0),
                self.px(2.0),
            ),
            TEXT,
        );
        let drag = Drag::Vertical {
            inverted,
            top: area.y + cap_h / 2.0,
            travel,
        };
        self.hit(area, Control::Slider(slider, drag));
    }

    /// A vertical LED-style level meter, `level` 0..1 (1 = full scale).
    pub fn meter(&mut self, area: Area, level: f32) {
        self.fill(area, WELL);
        let level = level.clamp(0.0, 1.0);
        if level > 0.0 {
            self.push(
                Rect::new(area.x, area.y, area.width, area.height, METER_HIGH).gradient(METER_LOW),
            );
            self.fill(
                Area::new(area.x, area.y, area.width, area.height * (1.0 - level)),
                WELL,
            );
        }
        // Segments.
        let step = self.px(4.0);
        let mut y = area.bottom() - step;
        while y > area.y {
            self.fill(Area::new(area.x, y, area.width, self.px(1.0)), WELL);
            y -= step;
        }
    }
}

/// Text colour that reads on a `background` fill.
pub fn ink_on(background: Color) -> Color {
    let luminance = 0.2126 * background.r + 0.7152 * background.g + 0.0722 * background.b;
    match luminance > 0.25 {
        true => Color::rgb(0.02, 0.02, 0.025),
        false => Color::rgb(0.95, 0.95, 0.97),
    }
}

trait WithColor {
    fn with_color(self, color: Color) -> Self;
}

impl WithColor for Rect {
    fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }
}
