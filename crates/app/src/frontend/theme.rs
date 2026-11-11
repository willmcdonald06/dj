//! Colours, as a theme that can be switched at run time and written by users.
//!
//! The UI thread draws with [`th`], the current theme. Built-in themes are
//! [`Theme::dark`] (the default: dark, low-contrast surfaces, colour kept for what
//! matters), [`Theme::contrast`] and [`Theme::daylight`] (a light UI for bright rooms,
//! with dark waveform lanes so waveforms stay readable). Users add their own as TOML in
//! the themes folder (see [`Theme::from_toml`]).

use std::cell::Cell;

use dj_analysis::{Key, Mode};
use dj_engine::{HOT_CUES, STEM_COUNT};
use dj_ui::Color;

/// Declares the theme's single colours once: the struct fields, their names in theme
/// files, and what each is for (written into theme files as comments).
macro_rules! theme_colors {
    ($($field:ident: $doc:literal,)*) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct Theme {
            pub name: String,
            $(#[doc = $doc] pub $field: Color,)*
            /// Deck A and deck B.
            pub decks: [Color; 2],
            /// One per hot cue slot.
            pub hot_cues: [Color; HOT_CUES],
            /// Vocals, melody, bass, drums.
            pub stems: [Color; STEM_COUNT],
        }

        impl Theme {
            /// (name, what it's for, colour) for every single colour.
            fn singles(&self) -> Vec<(&'static str, &'static str, Color)> {
                vec![$((stringify!($field), $doc, self.$field),)*]
            }

            fn singles_mut(&mut self) -> Vec<(&'static str, &mut Color)> {
                vec![$((stringify!($field), &mut self.$field),)*]
            }
        }
    };
}

theme_colors! {
    background: "Window background, between panels",
    panel: "Panels (decks, mixer, browser)",
    panel_top: "The raised top edge of panels",
    well: "Insets inside panels: overview strips, slots, list headers",
    line: "Hairlines and unlit outlines",
    lane: "Waveform lanes",
    button_top: "Unlit buttons, top of their gradient",
    button_bottom: "Unlit buttons, bottom of their gradient",
    button_held: "A button being pressed",
    text: "Main text",
    strong_text: "Text that must stand out: the selected row, active items",
    soft_text: "Secondary text: artists, labels",
    dim_text: "Hints and empty states",
    play: "Play",
    cue: "Cue, quantize and other orange highlights",
    loops: "Loops and things that fit (Matching, played marks)",
    danger: "Delete, record, dropouts",
    warning: "Warnings and MIDI learn",
    playhead: "The playhead",
    selected_row: "The selected browser row",
    row_stripe: "Every other browser row",
    key_lock: "Key lock, lit",
    filter: "Filter knobs, gated samples, slip",
    echo: "Echo and reverb (effects with tails)",
    meter_low: "Level meters, bottom",
    meter_high: "Level meters, top",
    popup: "Popups (settings, help, notices), top of their gradient",
    popup_bottom: "Popups, bottom of their gradient",
    scrim: "The shade behind popups",
    learn_bar: "The MIDI learn bar, top of its gradient",
    learn_bar_bottom: "The MIDI learn bar, bottom of its gradient",
    knob_top: "Knob caps, top",
    knob_bottom: "Knob caps, bottom",
    fader_top: "Fader caps, top",
    fader_bottom: "Fader caps, bottom",
    bar_line: "Bar lines on the waveforms",
    beat_tick: "Beat ticks on the waveforms",
    bar_number: "Bar numbers on the waveforms",
    slip_marker: "Where the track really is, while slipping",
    played_shade: "Shade over the played part of the overview",
    histogram: "The audio health histogram",
    ink_dark: "Text on light backgrounds (lit buttons)",
    ink_light: "Text on dark backgrounds (lit buttons)",
}

impl Theme {
    /// The built-in themes' names, in the order the settings offer them.
    pub const BUILT_IN: [&'static str; 3] = ["dark", "contrast", "daylight"];

    pub fn built_in(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::dark()),
            "contrast" => Some(Self::contrast()),
            "daylight" => Some(Self::daylight()),
            _ => None,
        }
    }

    /// The default: dark, low-contrast surfaces; colour is kept for what matters.
    pub fn dark() -> Self {
        Self {
            name: "dark".into(),
            background: Color::rgb(0.004, 0.004, 0.005),
            panel: Color::rgb(0.011, 0.011, 0.013),
            panel_top: Color::rgb(0.016, 0.016, 0.019),
            well: Color::rgb(0.005, 0.005, 0.0065),
            line: Color::rgb(0.04, 0.04, 0.048),
            lane: Color::rgb(0.006, 0.006, 0.008),
            button_top: Color::rgb(0.034, 0.034, 0.04),
            button_bottom: Color::rgb(0.022, 0.022, 0.026),
            button_held: Color::rgb(0.09, 0.09, 0.105),
            text: Color::rgb(0.82, 0.82, 0.85),
            strong_text: Color::rgb(0.95, 0.95, 0.97),
            soft_text: Color::rgb(0.3, 0.3, 0.33),
            dim_text: Color::rgb(0.13, 0.13, 0.15),
            play: Color::rgb(0.12, 0.8, 0.32),
            cue: Color::rgb(0.98, 0.62, 0.05),
            loops: Color::rgb(0.22, 0.82, 0.48),
            danger: Color::rgb(0.92, 0.2, 0.2),
            warning: Color::rgb(0.98, 0.8, 0.2),
            playhead: Color::rgb(0.95, 0.95, 0.95),
            selected_row: Color::rgb(0.035, 0.06, 0.13),
            row_stripe: Color::rgba(1.0, 1.0, 1.0, 0.012),
            key_lock: Color::rgb(0.45, 0.55, 0.75),
            filter: Color::rgb(0.6, 0.35, 0.95),
            echo: Color::rgb(0.3, 0.6, 1.0),
            meter_low: Color::rgb(0.1, 0.7, 0.3),
            meter_high: Color::rgb(0.95, 0.25, 0.1),
            popup: Color::rgb(0.05, 0.05, 0.06),
            popup_bottom: Color::rgb(0.03, 0.03, 0.037),
            scrim: Color::rgba(0.0, 0.0, 0.0, 0.7),
            learn_bar: Color::rgb(0.06, 0.05, 0.02),
            learn_bar_bottom: Color::rgb(0.035, 0.03, 0.012),
            knob_top: Color::rgb(0.09, 0.09, 0.105),
            knob_bottom: Color::rgb(0.035, 0.035, 0.042),
            fader_top: Color::rgb(0.32, 0.32, 0.36),
            fader_bottom: Color::rgb(0.12, 0.12, 0.14),
            bar_line: Color::rgba(1.0, 1.0, 1.0, 0.6),
            beat_tick: Color::rgba(0.6, 0.6, 0.65, 0.8),
            bar_number: Color::rgb(0.75, 0.75, 0.78),
            slip_marker: Color::rgba(1.0, 1.0, 1.0, 0.45),
            played_shade: Color::rgba(0.0, 0.0, 0.0, 0.55),
            histogram: Color::rgb(0.25, 0.32, 0.45),
            ink_dark: Color::rgb(0.02, 0.02, 0.025),
            ink_light: Color::rgb(0.95, 0.95, 0.97),
            decks: [Color::rgb(0.1, 0.42, 0.95), Color::rgb(0.95, 0.42, 0.06)],
            hot_cues: [
                Color::rgb(0.95, 0.2, 0.2),
                Color::rgb(0.98, 0.55, 0.1),
                Color::rgb(0.95, 0.85, 0.15),
                Color::rgb(0.25, 0.85, 0.3),
                Color::rgb(0.15, 0.8, 0.85),
                Color::rgb(0.2, 0.45, 0.95),
                Color::rgb(0.6, 0.3, 0.95),
                Color::rgb(0.95, 0.35, 0.75),
            ],
            stems: [
                Color::rgb(0.25, 0.85, 0.3),
                Color::rgb(0.95, 0.85, 0.15),
                Color::rgb(0.95, 0.2, 0.2),
                Color::rgb(0.15, 0.8, 0.85),
            ],
        }
    }

    /// Dark, with brighter text, edges and buttons: easier to read in a dark booth or
    /// on a dim screen.
    pub fn contrast() -> Self {
        Self {
            name: "contrast".into(),
            background: Color::rgb(0.0, 0.0, 0.0),
            panel: Color::rgb(0.02, 0.02, 0.024),
            panel_top: Color::rgb(0.03, 0.03, 0.036),
            well: Color::rgb(0.0, 0.0, 0.0),
            line: Color::rgb(0.12, 0.12, 0.14),
            button_top: Color::rgb(0.07, 0.07, 0.08),
            button_bottom: Color::rgb(0.045, 0.045, 0.052),
            button_held: Color::rgb(0.18, 0.18, 0.2),
            text: Color::rgb(1.0, 1.0, 1.0),
            strong_text: Color::rgb(1.0, 1.0, 1.0),
            soft_text: Color::rgb(0.6, 0.6, 0.64),
            dim_text: Color::rgb(0.3, 0.3, 0.33),
            selected_row: Color::rgb(0.06, 0.12, 0.3),
            row_stripe: Color::rgba(1.0, 1.0, 1.0, 0.03),
            beat_tick: Color::rgba(0.8, 0.8, 0.85, 0.9),
            bar_number: Color::rgb(1.0, 1.0, 1.0),
            ..Self::dark()
        }
    }

    /// Light panels and dark text for bright rooms and daylight. The waveform lanes stay
    /// dark so the waveforms keep their contrast.
    pub fn daylight() -> Self {
        Self {
            name: "daylight".into(),
            background: Color::rgb(0.45, 0.46, 0.48),
            panel: Color::rgb(0.78, 0.79, 0.81),
            panel_top: Color::rgb(0.86, 0.87, 0.89),
            well: Color::rgb(0.6, 0.61, 0.63),
            line: Color::rgb(0.42, 0.43, 0.45),
            lane: Color::rgb(0.012, 0.012, 0.016),
            button_top: Color::rgb(0.95, 0.95, 0.96),
            button_bottom: Color::rgb(0.82, 0.83, 0.85),
            button_held: Color::rgb(0.6, 0.61, 0.64),
            text: Color::rgb(0.01, 0.01, 0.015),
            strong_text: Color::rgb(0.0, 0.0, 0.0),
            soft_text: Color::rgb(0.13, 0.13, 0.15),
            dim_text: Color::rgb(0.3, 0.3, 0.33),
            play: Color::rgb(0.03, 0.5, 0.15),
            cue: Color::rgb(0.9, 0.42, 0.0),
            loops: Color::rgb(0.05, 0.5, 0.25),
            danger: Color::rgb(0.75, 0.05, 0.05),
            warning: Color::rgb(0.8, 0.5, 0.0),
            selected_row: Color::rgb(0.45, 0.6, 0.9),
            row_stripe: Color::rgba(0.0, 0.0, 0.0, 0.04),
            key_lock: Color::rgb(0.15, 0.25, 0.55),
            filter: Color::rgb(0.35, 0.12, 0.75),
            echo: Color::rgb(0.05, 0.3, 0.85),
            popup: Color::rgb(0.9, 0.9, 0.92),
            popup_bottom: Color::rgb(0.8, 0.8, 0.83),
            scrim: Color::rgba(0.2, 0.2, 0.22, 0.6),
            learn_bar: Color::rgb(0.95, 0.85, 0.5),
            learn_bar_bottom: Color::rgb(0.85, 0.72, 0.35),
            knob_top: Color::rgb(0.98, 0.98, 0.99),
            knob_bottom: Color::rgb(0.7, 0.71, 0.73),
            fader_top: Color::rgb(0.98, 0.98, 0.99),
            fader_bottom: Color::rgb(0.6, 0.61, 0.63),
            histogram: Color::rgb(0.15, 0.2, 0.35),
            ..Self::dark()
        }
    }

    /// Text colour for a lit background: dark on light, light on dark.
    pub fn ink_on(&self, background: Color) -> Color {
        let luminance = 0.2126 * background.r + 0.7152 * background.g + 0.0722 * background.b;
        match luminance > 0.25 {
            true => self.ink_dark,
            false => self.ink_light,
        }
    }

    /// A theme from a theme file. Colours are `"#rrggbb"` or `"#rrggbbaa"` (sRGB); any
    /// left out come from `base` (a built-in theme, "dark" if not given). Returns the
    /// theme and a message for each value that couldn't be used.
    pub fn from_toml(name: &str, text: &str) -> Result<(Self, Vec<String>), String> {
        let table: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut problems = Vec::new();
        let base = table.get("base").and_then(|v| v.as_str()).unwrap_or("dark");
        let mut theme = match Self::built_in(base) {
            Some(theme) => theme,
            None => {
                problems.push(format!(
                    "base = {base:?} isn't a built-in theme; using dark"
                ));
                Self::dark()
            }
        };
        theme.name = name.to_string();
        let empty = toml::Table::new();
        let colors = table
            .get("colors")
            .and_then(|v| v.as_table())
            .unwrap_or(&empty);
        let mut known: Vec<&str> = Vec::new();
        for (key, slot) in theme.singles_mut() {
            known.push(key);
            if let Some(value) = colors.get(key) {
                match value.as_str().and_then(parse_hex) {
                    Some(color) => *slot = color,
                    None => problems.push(format!("{key}: {value} isn't a #rrggbb colour")),
                }
            }
        }
        let mut list = |key: &'static str, slots: &mut [Color]| {
            known.push(key);
            let Some(value) = colors.get(key) else {
                return;
            };
            let Some(items) = value.as_array() else {
                problems.push(format!("{key} should be a list of colours"));
                return;
            };
            for (slot, item) in slots.iter_mut().zip(items) {
                match item.as_str().and_then(parse_hex) {
                    Some(color) => *slot = color,
                    None => problems.push(format!("{key}: {item} isn't a #rrggbb colour")),
                }
            }
        };
        list("decks", &mut theme.decks);
        list("hot_cues", &mut theme.hot_cues);
        list("stems", &mut theme.stems);
        for key in colors.keys() {
            if !known.contains(&key.as_str()) {
                problems.push(format!("{key} isn't a theme colour"));
            }
        }
        Ok((theme, problems))
    }

    /// The theme as a complete theme file, every colour listed with what it's for.
    pub fn to_toml(&self, base: &str) -> String {
        let mut out = format!(
            "# dj theme \"{}\". Colours are \"#rrggbb\" or \"#rrggbbaa\" (sRGB). Any colour\n\
             # left out comes from the base theme ({}). Saved changes show at once.\n\n\
             base = \"{base}\"\n\n[colors]\n",
            self.name,
            Self::BUILT_IN.join(", ")
        );
        for (key, doc, color) in self.singles() {
            out.push_str(&format!("# {doc}\n{key} = \"{}\"\n", to_hex(color)));
        }
        let list = |colors: &[Color]| {
            colors
                .iter()
                .map(|c| format!("\"{}\"", to_hex(*c)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "# Deck A and deck B\ndecks = [{}]\n",
            list(&self.decks)
        ));
        out.push_str(&format!(
            "# Hot cues 1 to 8\nhot_cues = [{}]\n",
            list(&self.hot_cues)
        ));
        out.push_str(&format!(
            "# Stems: vocals, melody, bass, drums\nstems = [{}]\n",
            list(&self.stems)
        ));
        out
    }
}

/// `#rrggbb` or `#rrggbbaa` in sRGB, as a linear colour.
fn parse_hex(text: &str) -> Option<Color> {
    let hex = text.trim().strip_prefix('#')?;
    if !matches!(hex.len(), 6 | 8) || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let channel = |i: usize| byte(i).map(|b| linear(b as f32 / 255.0));
    let alpha = match hex.len() {
        8 => byte(6)? as f32 / 255.0,
        _ => 1.0,
    };
    Some(Color::rgba(channel(0)?, channel(2)?, channel(4)?, alpha))
}

fn to_hex(color: Color) -> String {
    let byte = |linear: f32| (linear.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
    let mut hex = format!(
        "#{:02x}{:02x}{:02x}",
        byte(color.r),
        byte(color.g),
        byte(color.b)
    );
    if color.a < 1.0 {
        hex.push_str(&format!(
            "{:02x}",
            (color.a.clamp(0.0, 1.0) * 255.0).round() as u8
        ));
    }
    hex
}

/// The theme called `name`: a built-in one, or `<folder>/<name>.toml`. Returns it,
/// the problems found in its file, and the file (to watch for changes).
pub fn load(
    name: &str,
    folder: &std::path::Path,
) -> Result<(Theme, Vec<String>, Option<std::path::PathBuf>), String> {
    if let Some(theme) = Theme::built_in(name) {
        return Ok((theme, Vec::new(), None));
    }
    let path = folder.join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("can't read {}: {err}", path.display()))?;
    let (theme, problems) =
        Theme::from_toml(name, &text).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok((theme, problems, Some(path)))
}

/// Theme names to offer: the built-in ones, then the theme files in `folder`.
pub fn available(folder: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = Theme::BUILT_IN.iter().map(|n| n.to_string()).collect();
    let mut files: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .filter_map(|p| Some(p.file_stem()?.to_string_lossy().into_owned()))
                .filter(|n| !Theme::BUILT_IN.contains(&n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    names.extend(files);
    names
}

/// Write a complete theme file `<folder>/<name>.toml` starting from the `base` theme,
/// for editing. Refuses to overwrite one.
pub fn write_new(
    name: &str,
    base: &str,
    folder: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let mut theme = Theme::built_in(base).ok_or_else(|| {
        format!(
            "{base:?} isn't a built-in theme ({})",
            Theme::BUILT_IN.join(", ")
        )
    })?;
    if Theme::built_in(name).is_some() {
        return Err(format!("{name:?} is a built-in theme's name; pick another"));
    }
    let path = folder.join(format!("{name}.toml"));
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    std::fs::create_dir_all(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    theme.name = name.to_string();
    std::fs::write(&path, theme.to_toml(base)).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

thread_local! {
    static CURRENT: Cell<&'static Theme> = Cell::new(Box::leak(Box::new(Theme::dark())));
}

/// The theme the UI draws with.
pub fn th() -> &'static Theme {
    CURRENT.with(Cell::get)
}

/// Draw with `theme` from now on. Each switch keeps the old theme alive (a few hundred
/// bytes), since earlier frames' references may still be around; switching is rare.
pub fn set_theme(theme: Theme) {
    CURRENT.with(|current| current.set(Box::leak(Box::new(theme))));
}

/// `color` as text on the theme's panels and buttons: as is on dark themes, darker on
/// light ones (bright colours wash out on white).
pub fn readable(color: Color) -> Color {
    let panel = th().button_bottom;
    let luminance = 0.2126 * panel.r + 0.7152 * panel.g + 0.0722 * panel.b;
    match luminance > 0.25 {
        true => darker(color, 0.45),
        false => color,
    }
}

/// `text` on a lit background in the current theme.
pub fn ink_on(background: Color) -> Color {
    th().ink_on(background)
}

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
    fn themes_round_trip_through_theme_files() {
        for name in Theme::BUILT_IN {
            let theme = Theme::built_in(name).unwrap();
            let (back, problems) = Theme::from_toml(name, &theme.to_toml(name)).unwrap();
            assert!(problems.is_empty(), "{problems:?}");
            for ((key, _, a), (_, _, b)) in theme.singles().into_iter().zip(back.singles()) {
                let close = (a.r - b.r).abs() < 0.01
                    && (a.g - b.g).abs() < 0.01
                    && (a.b - b.b).abs() < 0.01
                    && (a.a - b.a).abs() < 0.01;
                assert!(close, "{name} {key}: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn theme_files_override_their_base_and_report_mistakes() {
        let text = "base = \"daylight\"\n[colors]\ntext = \"#ff0000\"\n\
                    hot_cues = [\"#00ff00\"]\nbogus = \"#000000\"\ncue = \"orange\"\n";
        let (theme, problems) = Theme::from_toml("mine", text).unwrap();
        assert_eq!(theme.name, "mine");
        assert_eq!(theme.text, Color::rgb(1.0, 0.0, 0.0));
        assert_eq!(theme.hot_cues[0], Color::rgb(0.0, 1.0, 0.0));
        assert_eq!(theme.hot_cues[1], Theme::daylight().hot_cues[1]);
        assert_eq!(theme.panel, Theme::daylight().panel);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(Theme::from_toml("x", "not toml [").is_err());
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv(0.0, 1.0, 1.0), Color::rgb(1.0, 0.0, 0.0));
        let green = hsv(1.0 / 3.0, 1.0, 1.0);
        assert!(green.g > 0.99 && green.r < 0.01);
    }
}
