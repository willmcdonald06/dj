//! Colours. Dark, low-contrast surfaces; colour is kept for what matters: the deck
//! accents, lit buttons, hot cues, keys and meters.

use dj_analysis::{Key, Mode};
use dj_engine::HOT_CUES;
use dj_ui::Color;

/// Window background, between panels.
pub const BACKGROUND: Color = Color::rgb(0.004, 0.004, 0.005);
/// Panels (deck, mixer, browser) and their raised top edge.
pub const PANEL: Color = Color::rgb(0.011, 0.011, 0.013);
pub const PANEL_TOP: Color = Color::rgb(0.016, 0.016, 0.019);
/// Insets inside panels: overview strips, slots, list headers.
pub const WELL: Color = Color::rgb(0.005, 0.005, 0.0065);
/// Hairlines and unlit outlines.
pub const LINE: Color = Color::rgb(0.04, 0.04, 0.048);
/// Waveform lane background.
pub const LANE: Color = Color::rgb(0.006, 0.006, 0.008);

/// Unlit buttons: a faint top-to-bottom gradient.
pub const BUTTON_TOP: Color = Color::rgb(0.034, 0.034, 0.04);
pub const BUTTON_BOTTOM: Color = Color::rgb(0.022, 0.022, 0.026);
pub const BUTTON_HELD: Color = Color::rgb(0.09, 0.09, 0.105);

pub const TEXT: Color = Color::rgb(0.82, 0.82, 0.85);
/// Secondary text: artists, labels.
pub const SOFT_TEXT: Color = Color::rgb(0.3, 0.3, 0.33);
/// Hints and empty states.
pub const DIM_TEXT: Color = Color::rgb(0.13, 0.13, 0.15);

pub const DECK_COLORS: [Color; 2] = [Color::rgb(0.1, 0.42, 0.95), Color::rgb(0.95, 0.42, 0.06)];
pub const PLAY: Color = Color::rgb(0.12, 0.8, 0.32);
pub const CUE: Color = Color::rgb(0.98, 0.62, 0.05);
pub const LOOP: Color = Color::rgb(0.22, 0.82, 0.48);
pub const DANGER: Color = Color::rgb(0.92, 0.2, 0.2);
pub const WARNING: Color = Color::rgb(0.98, 0.8, 0.2);
pub const PLAYHEAD: Color = Color::rgb(0.95, 0.95, 0.95);
pub const SELECTED_ROW: Color = Color::rgb(0.035, 0.06, 0.13);
/// Key lock, lit.
pub const KEY_LOCK: Color = Color::rgb(0.45, 0.55, 0.75);
pub const FILTER: Color = Color::rgb(0.6, 0.35, 0.95);

/// One colour per hot cue slot, as on most controllers.
pub const HOT_CUE_COLORS: [Color; HOT_CUES] = [
    Color::rgb(0.95, 0.2, 0.2),
    Color::rgb(0.98, 0.55, 0.1),
    Color::rgb(0.95, 0.85, 0.15),
    Color::rgb(0.25, 0.85, 0.3),
    Color::rgb(0.15, 0.8, 0.85),
    Color::rgb(0.2, 0.45, 0.95),
    Color::rgb(0.6, 0.3, 0.95),
    Color::rgb(0.95, 0.35, 0.75),
];

/// Meter colours, bottom to top.
pub const METER_LOW: Color = Color::rgb(0.1, 0.7, 0.3);
pub const METER_HIGH: Color = Color::rgb(0.95, 0.25, 0.1);

/// A key's colour on the circle of fifths, so neighbours (which mix well) look alike:
/// the convention of most DJ software. Minor keys are a little deeper than major.
pub fn key_color(key: Key) -> Color {
    let hue = (f32::from(key.wheel()) - 1.0) / 12.0;
    let (saturation, value) = match key.mode {
        Mode::Minor => (0.75, 0.95),
        Mode::Major => (0.5, 1.0),
    };
    hsv(hue, saturation, value)
}

/// HSV (all 0..1) to linear RGB.
pub fn hsv(hue: f32, saturation: f32, value: f32) -> Color {
    let h = (hue.rem_euclid(1.0)) * 6.0;
    let c = value * saturation;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = value - c;
    // The colours above are picked in sRGB; the scene is linear.
    Color::rgb(linear(r + m), linear(g + m), linear(b + m))
}

fn linear(srgb: f32) -> f32 {
    srgb.powf(2.2)
}

/// `color` scaled towards black.
pub fn darker(color: Color, factor: f32) -> Color {
    Color::rgba(
        color.r * factor,
        color.g * factor,
        color.b * factor,
        color.a,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbouring_keys_have_close_colours() {
        let c = key_color(Key::new(0, Mode::Major));
        let g = key_color(Key::new(7, Mode::Major));
        let f_sharp = key_color(Key::new(6, Mode::Major));
        let distance =
            |a: Color, b: Color| (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs();
        assert!(distance(c, g) < distance(c, f_sharp));
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv(0.0, 1.0, 1.0), Color::rgb(1.0, 0.0, 0.0));
        let green = hsv(1.0 / 3.0, 1.0, 1.0);
        assert!(green.g > 0.99 && green.r < 0.01);
    }
}
