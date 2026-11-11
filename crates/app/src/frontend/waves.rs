//! The full-width scrolling waveforms, the phase meter between them and the overview
//! strips.

use std::sync::Arc;

use dj_analysis::TrackAnalysis;
use dj_analysis::transients::{self, Transient};
use dj_engine::{DeckId, DeckSnapshot};
use dj_ui::{Align, Color, Rect, Shape, WaveformStyle, WaveformView};

use super::layout::Area;
use super::painter::{Painter, ink_on};
use super::theme::*;
use super::{Control, PHASE_BAR_MS};

/// How the scrolling waveforms are drawn, shared by both decks.
#[derive(Debug, Clone, Copy)]
pub struct WaveLook {
    /// Frames per pixel at 1:1 tempo.
    pub base_fpp: f64,
    pub style: WaveformStyle,
    /// Where bar lines run on to: the middle of the transient strip.
    pub grid_to: f32,
}

/// Played audio is drawn at this brightness, so what's coming stands out.
const PLAYED_BRIGHTNESS: f32 = 0.55;

impl Painter {
    /// One deck's scrolling waveform, centred on the playhead. `base_fpp` is frames per
    /// pixel at 1:1 tempo; it is scaled by the deck's tempo so both decks scroll at the
    /// same real-time rate and matched beats line up. Bar lines run on to `grid_to`
    /// (the middle of the transient strip), joining the deck's spikes.
    pub fn scrolling(
        &mut self,
        deck: DeckId,
        area: Area,
        d: &DeckSnapshot,
        analysis: Option<&TrackAnalysis>,
        look: &WaveLook,
    ) {
        let WaveLook {
            base_fpp,
            style,
            grid_to,
        } = *look;
        self.fill(area, LANE);
        self.fill(
            Area::new(area.x, area.y + area.height / 2.0, area.width, self.px(1.0)),
            LINE,
        );
        // Deck letter at the left edge.
        let color = DECK_COLORS[deck.index()];
        let tab = Area::new(area.x, area.y, self.px(3.0), area.height);
        self.overlay_fill(tab, color);
        let chip = Area::new(
            area.x + self.px(8.0),
            area.y + self.px(6.0),
            self.px(18.0),
            self.px(16.0),
        );
        self.overlay(
            Rect::new(
                chip.x,
                chip.y,
                chip.width,
                chip.height,
                color.with_alpha(0.9),
            )
            .rounded(self.px(3.0)),
        );
        self.bold(chip, deck_letter(deck), 10.0, ink_on(color), Align::Center);

        let Some(analysis) = analysis else {
            let message = match d.track {
                Some(_) => "Analysing…",
                None => "Empty: load a track from the browser, or drop a file here",
            };
            self.label(area, message, 12.0, DIM_TEXT, Align::Center);
            return;
        };
        let (start, fpp) = view_window(d, area.width, base_fpp);
        let end = start + area.width as f64 * fpp;
        // Played audio (left of the playhead) dimmed.
        let half = (area.width / 2.0).round();
        for (x, width, brightness) in [
            (area.x, half, PLAYED_BRIGHTNESS),
            (area.x + half, area.width - half, 1.0),
        ] {
            self.scene.waveforms.push(WaveformView {
                data: Arc::clone(&analysis.waveform),
                x,
                y: area.y,
                width,
                height: area.height,
                start_frame: start + (x - area.x) as f64 * fpp,
                frames_per_pixel: fpp,
                brightness,
                style,
            });
        }
        self.hit(area, Control::Waveform(deck, fpp));

        let x_of = |frame: f64| area.x + ((frame - start) / fpp) as f32;
        // Beatgrid, Serato style: short grey ticks on every beat at the top and bottom
        // edges, a white line with the bar number every four beats (the grid's anchor
        // is a downbeat). Without a grid trusted enough to sync, the analysis's own
        // estimate is shown, so there is always something to line up by eye.
        if let Some(grid) = d.grid.or(analysis.grid.map(|g| g.grid)) {
            let first = grid.beat_at(start.max(0.0)).ceil() as i64;
            let last = grid.beat_at(end).floor() as i64;
            let tick = (area.height * 0.12).max(self.px(5.0));
            for beat in first..=last {
                let x = x_of(grid.frame_of(beat as f64));
                match beat.rem_euclid(4) == 0 {
                    true => {
                        let w = self.px(1.5);
                        let (top, bottom) = (area.y.min(grid_to), area.bottom().max(grid_to));
                        self.overlay(Rect::new(
                            x - w / 2.0,
                            top,
                            w,
                            bottom - top,
                            Color::rgba(1.0, 1.0, 1.0, 0.6),
                        ));
                        if beat >= 0 {
                            let label = Area::new(
                                x + self.px(3.0),
                                area.y + self.px(1.0),
                                self.px(40.0),
                                self.px(13.0),
                            );
                            self.label(
                                label,
                                format!("{}", beat / 4 + 1),
                                9.5,
                                Color::rgb(0.75, 0.75, 0.78),
                                Align::Left,
                            );
                        }
                    }
                    false => {
                        let w = self.px(1.0);
                        for y in [area.y, area.bottom() - tick] {
                            self.overlay(Rect::new(
                                x - w / 2.0,
                                y,
                                w,
                                tick,
                                Color::rgba(0.6, 0.6, 0.65, 0.8),
                            ));
                        }
                    }
                }
            }
        }
        // Loop region and the pending loop-in point.
        if let Some(region) = d.loop_region {
            let (x0, x1) = (x_of(region.start.max(start)), x_of(region.end.min(end)));
            if x1 > x0 {
                let alpha = match d.loop_active {
                    true => 0.2,
                    false => 0.07,
                };
                self.overlay(Rect::new(
                    x0,
                    area.y,
                    x1 - x0,
                    area.height,
                    LOOP.with_alpha(alpha),
                ));
                for edge in [region.start, region.end] {
                    if (start..end).contains(&edge) {
                        self.overlay(Rect::new(
                            x_of(edge) - self.px(1.0),
                            area.y,
                            self.px(2.0),
                            area.height,
                            LOOP,
                        ));
                    }
                }
            }
        }
        if let Some(loop_in) = d.loop_in.filter(|at| (start..end).contains(at)) {
            self.overlay(Rect::new(
                x_of(loop_in) - self.px(1.0),
                area.y,
                self.px(2.0),
                area.height,
                LOOP,
            ));
        }
        // Hot cues: a line with a numbered flag at the top.
        for (slot, cue) in d.hot_cues.iter().enumerate() {
            let Some(at) = cue.position().filter(|at| (start..end).contains(at)) else {
                continue;
            };
            let x = x_of(at);
            let color = HOT_CUE_COLORS[slot];
            self.overlay(Rect::new(
                x - self.px(1.0),
                area.y,
                self.px(2.0),
                area.height,
                color,
            ));
            let flag = Area::new(x, area.y, self.px(16.0), self.px(15.0));
            self.overlay(
                Rect::new(flag.x, flag.y, flag.width, flag.height, color).rounded(self.px(2.0)),
            );
            self.bold(
                flag,
                format!("{}", slot + 1),
                10.0,
                ink_on(color),
                Align::Center,
            );
        }
        // While rolling or slipping: where the track really is.
        if let Some(slip) = d.slip_position.filter(|at| (start..end).contains(at)) {
            self.overlay(Rect::new(
                x_of(slip) - self.px(1.0),
                area.y,
                self.px(2.0),
                area.height,
                Color::rgba(1.0, 1.0, 1.0, 0.45),
            ));
        }
        // The cue point: triangles on the edges.
        let cue = d.cue as f64;
        if (start..end).contains(&cue) {
            self.edge_markers(area, x_of(cue), self.px(10.0), CUE);
        }
    }

    /// Triangles pointing in from the top and bottom edges of `area` at `x`.
    pub fn edge_markers(&mut self, area: Area, x: f32, size: f32, color: Color) {
        let h = size * 0.7;
        self.overlay(
            Rect::new(x - size / 2.0, area.y, size, h, color)
                .shape(Shape::Triangle { quarter_turns: 1 }),
        );
        self.overlay(
            Rect::new(x - size / 2.0, area.bottom() - h, size, h, color)
                .shape(Shape::Triangle { quarter_turns: 3 }),
        );
    }

    /// The shared centre playhead across both lanes and the phase meter.
    pub fn playhead(&mut self, top: Area, bottom: Area) {
        let w = self.px(2.0);
        let x = top.centre_x() - w / 2.0;
        self.overlay(Rect::new(x, top.y, w, bottom.bottom() - top.y, PLAYHEAD));
        let size = self.px(12.0);
        self.edge_markers(
            Area::new(top.x, top.y, top.width, bottom.bottom() - top.y),
            top.centre_x(),
            size,
            PLAYHEAD,
        );
    }

    /// Between the lanes: each deck's transients as spikes coloured like the waveform
    /// there, standing on the centre line: deck A's pointing up, deck B's down, so two
    /// decks in phase have spikes whose bases line up.
    pub fn transients(
        &mut self,
        area: Area,
        decks: [(&DeckSnapshot, &[Transient]); 2],
        base_fpp: f64,
        style: WaveformStyle,
    ) {
        self.fill(area, WELL);
        let mid = area.y + area.height / 2.0;
        self.fill(
            Area::new(area.x, mid - self.px(0.5), area.width, self.px(1.0)),
            LINE,
        );
        let half = area.height / 2.0 - self.px(1.0);
        let w = self.px(4.5);
        for (deck, (d, transients)) in DeckId::ALL.into_iter().zip(decks) {
            let (start, fpp) = view_window(d, area.width, base_fpp);
            let end = start + area.width as f64 * fpp;
            for t in transients::between(transients, start, end) {
                let x = area.x + ((t.frame as f64 - start) / fpp) as f32;
                let strength = (f32::from(t.bins[3]) / 255.0).powf(0.7);
                let h = half * (0.6 + 0.4 * strength);
                let color = waveform_color(t.bins, style);
                // A spike: a narrow triangle pointing into the strip from its deck's
                // edge, tip towards the middle.
                let faded = darker(color, 0.45);
                // Flat base on the centre line, tip out towards the deck's waveform,
                // so the two decks' bases meet when they are in phase.
                let (y, turns, top, bottom) = match deck {
                    DeckId::A => (mid - h, 3, faded, color),
                    DeckId::B => (mid, 1, color, faded),
                };
                self.overlay(Rect::new(x - w / 2.0, y, w, h, top).gradient(bottom).shape(
                    Shape::Spike {
                        quarter_turns: turns,
                    },
                ));
            }
        }
    }

    /// How far apart the decks' kicks are: a bar from the centre towards the deck that
    /// is ahead, and the offset in ms.
    pub fn phase_meter(&mut self, area: Area, offset_ms: Option<f64>) {
        let (text, bar_area) = area.split_right(self.px(84.0), self.px(8.0));
        let h = self.px(6.0);
        let y = bar_area.y + (bar_area.height - h) / 2.0;
        let cx = bar_area.centre_x();
        let max = bar_area.width / 2.0;
        self.rounded(Area::new(bar_area.x, y, bar_area.width, h), h / 2.0, WELL);
        self.fill(
            Area::new(
                cx - self.px(0.5),
                y - self.px(2.0),
                self.px(1.0),
                h + self.px(4.0),
            ),
            SOFT_TEXT,
        );
        let Some(offset) = offset_ms else {
            self.label(text, "PHASE", 9.0, DIM_TEXT, Align::Left);
            return;
        };
        let fraction = (offset / PHASE_BAR_MS).clamp(-1.0, 1.0) as f32;
        let w = (max * fraction.abs()).max(self.px(2.0));
        let (x, color) = match fraction >= 0.0 {
            true => (cx - w, DECK_COLORS[0]),
            false => (cx, DECK_COLORS[1]),
        };
        let color = match offset.abs() < 1.0 {
            true => PLAY,
            false => color,
        };
        self.rounded(Area::new(x, y, w, h), h / 2.0, color);
        let label = match offset {
            o if o.abs() < 1.0 => "IN PHASE".to_string(),
            o if o > 0.0 => format!("A +{o:.1} ms"),
            o => format!("B +{:.1} ms", -o),
        };
        self.bold(text, label, 9.5, SOFT_TEXT, Align::Left);
    }

    /// The whole track, with what has played dimmed, cue points and the playhead.
    /// Click or drag to move the playhead.
    pub fn overview(
        &mut self,
        deck: DeckId,
        area: Area,
        d: &DeckSnapshot,
        analysis: Option<&TrackAnalysis>,
        style: WaveformStyle,
    ) {
        self.well(area);
        if d.track.is_none() || d.length == 0 {
            return;
        }
        let at =
            |frames: u64| area.x + area.width * (frames.min(d.length) as f32 / d.length as f32);
        match analysis {
            Some(analysis) => self.scene.waveforms.push(WaveformView {
                data: Arc::clone(&analysis.waveform),
                x: area.x,
                y: area.y,
                width: area.width,
                height: area.height,
                start_frame: 0.0,
                frames_per_pixel: d.length as f64 / area.width as f64,
                brightness: 0.9,
                style,
            }),
            // Until the analysis lands, show decoding progress.
            None => self.fill(
                Area::new(area.x, area.y, at(d.ready) - area.x, area.height),
                LINE,
            ),
        }
        self.overlay(Rect::new(
            area.x,
            area.y,
            at(d.position) - area.x,
            area.height,
            Color::rgba(0.0, 0.0, 0.0, 0.55),
        ));
        if let Some(region) = d.loop_region {
            let (x0, x1) = (at(region.start as u64), at(region.end as u64));
            self.overlay(Rect::new(
                x0,
                area.y,
                (x1 - x0).max(self.px(2.0)),
                area.height,
                LOOP.with_alpha(0.45),
            ));
        }
        for (slot, cue) in d.hot_cues.iter().enumerate() {
            if let Some(frame) = cue.position() {
                let x = at(frame as u64);
                let color = HOT_CUE_COLORS[slot];
                self.overlay(Rect::new(
                    x - self.px(1.0),
                    area.y,
                    self.px(2.0),
                    area.height,
                    color,
                ));
                let flag = Area::new(x - self.px(1.0), area.y, self.px(11.0), self.px(11.0));
                self.overlay(
                    Rect::new(flag.x, flag.y, flag.width, flag.height, color).rounded(self.px(1.5)),
                );
                self.bold(
                    flag,
                    format!("{}", slot + 1),
                    7.5,
                    ink_on(color),
                    Align::Center,
                );
            }
        }
        let cue_size = self.px(9.0);
        self.overlay(
            Rect::new(
                at(d.cue) - cue_size / 2.0,
                area.bottom() - cue_size * 0.7,
                cue_size,
                cue_size * 0.7,
                CUE,
            )
            .shape(Shape::Triangle { quarter_turns: 3 }),
        );
        self.overlay(Rect::new(
            at(d.position) - self.px(1.0),
            area.y,
            self.px(2.0),
            area.height,
            PLAYHEAD,
        ));
        self.hit(area, Control::Overview(deck, d.length));
    }
}

/// The track frame at the left edge of a scrolling view `width` pixels wide, and its
/// frames per pixel: centred on the playhead, scaled by the deck's tempo. Not by a
/// nudge: that should be seen sliding the waveform along (so its beats can be walked
/// onto the other deck's), not stretching it.
fn view_window(d: &DeckSnapshot, width: f32, base_fpp: f64) -> (f64, f64) {
    let speed = d.tempo_rate.max(0.01) as f64;
    let fpp = base_fpp * speed;
    (d.position as f64 - (width as f64 / 2.0) * fpp, fpp)
}

/// The colour the waveform shader gives these bins (see `waveform.wgsl`).
fn waveform_color(bins: [u8; 4], style: WaveformStyle) -> Color {
    let band = |i: usize| (f32::from(bins[i]) / 255.0).powf(0.8);
    let (low, mid, high) = (band(0), band(1), band(2));
    match style {
        WaveformStyle::Spectrum => {
            let (wl, wm, wh) = (low.powf(2.5), mid.powf(2.5), high.powf(2.5));
            let total = wl + wm + wh + 0.0001;
            let mix = |l: f32, m: f32, h: f32| (wl * l + wm * m + wh * h) / total;
            let (r, g, b) = (
                mix(1.0, 0.25, 0.15),
                mix(0.1, 0.95, 0.45),
                mix(0.03, 0.12, 1.0),
            );
            let peak = r.max(g).max(b).max(0.001);
            Color::rgb(r / peak, g / peak, b / peak)
        }
        WaveformStyle::ThreeBand => match (low >= mid && low >= high, mid >= high) {
            (true, _) => Color::rgb(0.06, 0.25, 0.85),
            (false, true) => Color::rgb(0.9, 0.42, 0.08),
            (false, false) => Color::rgb(0.85, 0.85, 0.9),
        },
    }
}

pub fn deck_letter(deck: DeckId) -> &'static str {
    match deck {
        DeckId::A => "A",
        DeckId::B => "B",
    }
}
