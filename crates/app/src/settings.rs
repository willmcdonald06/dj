//! User preferences: loaded from `settings.toml`, edited on the settings screen and
//! saved straight away. Command-line options override the file for one run without
//! being saved into it.

use std::path::{Path, PathBuf};

use dj_analysis::KeyNotation;
use dj_audio_io::AudioSettings;
use dj_engine::{OutputMode, SAMPLER_DEFAULT_LEVEL, SAMPLER_SLOTS, SampleMode, TempoRange};
use dj_ui::WaveformStyle;
use serde::Deserialize;

/// Buffer sizes offered on the settings screen, in frames.
pub const BUFFER_CHOICES: [u32; 6] = [64, 128, 256, 512, 1024, 2048];

/// The sampler bank, kept so it comes back after a restart.
#[derive(Debug, Clone, PartialEq)]
pub struct SamplerBank {
    pub paths: [Option<PathBuf>; SAMPLER_SLOTS],
    pub modes: [SampleMode; SAMPLER_SLOTS],
    pub level: f32,
    pub sync: bool,
}

impl Default for SamplerBank {
    fn default() -> Self {
        Self {
            paths: Default::default(),
            modes: [SampleMode::OneShot; SAMPLER_SLOTS],
            level: SAMPLER_DEFAULT_LEVEL,
            sync: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// How keys are written: Open Key (default), Camelot or musical names.
    pub key_notation: KeyNotation,
    /// How waveforms are coloured.
    pub waveform: WaveformStyle,
    /// A built-in theme or a theme file's name.
    pub theme: String,
    /// Substring of the output device's name; `None` for the system default.
    pub audio_device: Option<String>,
    /// Period size in frames; `None` for the device's default.
    pub buffer_frames: Option<u32>,
    pub sample_rate: u32,
    pub output_mode: OutputMode,
    /// Tempo fader range the decks start with.
    pub tempo_range: TempoRange,
    /// Whether key lock is on when the app starts.
    pub key_lock: bool,
    /// Whether quantize is on when the app starts.
    pub quantize: bool,
    pub sampler: SamplerBank,
}

impl Default for Settings {
    fn default() -> Self {
        let audio = AudioSettings::default();
        Self {
            key_notation: KeyNotation::default(),
            waveform: WaveformStyle::default(),
            theme: "dark".to_string(),
            audio_device: audio.device,
            buffer_frames: audio.buffer_frames,
            sample_rate: audio.sample_rate,
            output_mode: OutputMode::InternalMix,
            tempo_range: TempoRange::default(),
            key_lock: true,
            quantize: true,
            sampler: SamplerBank::default(),
        }
    }
}

/// Settings given on the command line, which win over the file for this run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub key_notation: Option<KeyNotation>,
    pub waveform: Option<WaveformStyle>,
    pub theme: Option<String>,
    pub audio_device: Option<String>,
    /// `Some(None)`: the device default (`--buffer default`).
    pub buffer_frames: Option<Option<u32>>,
    pub sample_rate: Option<u32>,
}

impl Settings {
    /// These settings with the command line's overrides applied.
    pub fn with(mut self, overrides: &Overrides) -> Self {
        if let Some(notation) = overrides.key_notation {
            self.key_notation = notation;
        }
        if let Some(style) = overrides.waveform {
            self.waveform = style;
        }
        if let Some(theme) = &overrides.theme {
            self.theme = theme.clone();
        }
        if let Some(device) = &overrides.audio_device {
            self.audio_device = Some(device.clone());
        }
        if let Some(frames) = overrides.buffer_frames {
            self.buffer_frames = frames;
        }
        if let Some(rate) = overrides.sample_rate {
            self.sample_rate = rate;
        }
        self
    }

    /// What to ask the audio backend for. `channels` comes from the command line only.
    pub fn audio(&self, channels: Option<u16>) -> AudioSettings {
        AudioSettings {
            device: self.audio_device.clone(),
            sample_rate: self.sample_rate,
            buffer_frames: self.buffer_frames,
            channels,
        }
    }

    /// The default location: `~/.config/dj/settings.toml` (macOS:
    /// `~/Library/Application Support/dj/settings.toml`).
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let base = match cfg!(target_os = "macos") {
            true => home.join("Library/Application Support"),
            false => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config")),
        };
        base.join("dj").join("settings.toml")
    }

    /// Read `path`. A missing file gives the defaults. An unreadable one is set aside
    /// as `settings.toml.bad` (so saving doesn't destroy it) and also gives the
    /// defaults; a bad value only resets that setting. Problems are logged.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                tracing::error!(
                    "can't read {}: {err}; using default settings",
                    path.display()
                );
                return Self::default();
            }
        };
        match toml::from_str::<File>(&text) {
            Ok(file) => {
                let (settings, problems) = file.into_settings();
                for problem in problems {
                    tracing::warn!("{}: {problem}", path.display());
                }
                settings
            }
            Err(err) => {
                let mut bad = path.as_os_str().to_owned();
                bad.push(".bad");
                tracing::error!(
                    "{} isn't valid ({err}); using default settings and keeping it as {}",
                    path.display(),
                    PathBuf::from(&bad).display()
                );
                let _ = std::fs::rename(path, &bad);
                Self::default()
            }
        }
    }

    /// Write to `path` (via a temporary file, so a crash never leaves half a file).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        std::fs::write(&temporary, self.to_toml())?;
        std::fs::rename(&temporary, path)
    }

    fn to_toml(&self) -> String {
        let device = match &self.audio_device {
            Some(name) => format!("device = {}\n", toml_string(name)),
            None => "# device = \"name\"   (part of the name; leave out for the system default)\n"
                .to_string(),
        };
        format!(
            "# dj settings. The settings screen writes this file; command-line options\n\
             # override it for one run.\n\
             \n\
             [audio]\n\
             {device}\
             # Frames per period; 0 for the device's default.\n\
             buffer = {buffer}\n\
             sample_rate = {rate}\n\
             # \"mix\" (main on 1-2, headphones on 3-4) or \"per-deck\" (A on 1-2, B on 3-4).\n\
             output = \"{output}\"\n\
             \n\
             [decks]\n\
             # 8, 16 or 50 (percent).\n\
             tempo_range = {range}\n\
             key_lock = {key_lock}\n\
             quantize = {quantize}\n\
             \n\
             [display]\n\
             # \"open\", \"camelot\" or \"musical\".\n\
             key_notation = \"{notation}\"\n\
             # \"spectrum\" or \"bands\".\n\
             waveform = \"{waveform}\"\n\
             # A built-in theme (dark, contrast, daylight) or a file in themes/.\n\
             theme = {theme}\n\
             \n\
             [sampler]\n\
             # One file per slot (\"\" for empty), and one-shot, gate or loop.\n\
             slots = [{slots}]\n\
             modes = [{modes}]\n\
             level = {level:.2}\n\
             sync = {sync}\n",
            buffer = self.buffer_frames.unwrap_or(0),
            rate = self.sample_rate,
            output = output_mode_name(self.output_mode),
            range = tempo_range_percent(self.tempo_range),
            key_lock = self.key_lock,
            quantize = self.quantize,
            notation = key_notation_name(self.key_notation),
            waveform = waveform_style_name(self.waveform),
            theme = toml_string(&self.theme),
            slots = self
                .sampler
                .paths
                .iter()
                .map(|p| toml_string(
                    &p.as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", "),
            modes = self
                .sampler
                .modes
                .iter()
                .map(|m| format!("\"{}\"", sample_mode_name(*m)))
                .collect::<Vec<_>>()
                .join(", "),
            level = self.sampler.level,
            sync = self.sampler.sync,
        )
    }
}

/// `settings.toml` as written. Everything optional: a missing value keeps its default.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct File {
    audio: AudioFile,
    decks: DecksFile,
    display: DisplayFile,
    sampler: SamplerFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SamplerFile {
    slots: Vec<String>,
    modes: Vec<String>,
    level: Option<f32>,
    sync: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AudioFile {
    device: Option<String>,
    buffer: Option<u32>,
    sample_rate: Option<u32>,
    output: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DecksFile {
    tempo_range: Option<u32>,
    key_lock: Option<bool>,
    quantize: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DisplayFile {
    key_notation: Option<String>,
    waveform: Option<String>,
    theme: Option<String>,
}

impl File {
    /// The settings, and a message for each value that couldn't be used.
    fn into_settings(self) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut problems = Vec::new();
        s.audio_device = self.audio.device.filter(|name| !name.trim().is_empty());
        match self.audio.buffer {
            Some(0) => s.buffer_frames = None,
            Some(frames) if (16..=8192).contains(&frames) => s.buffer_frames = Some(frames),
            Some(frames) => problems.push(format!("buffer = {frames} is out of range")),
            None => {}
        }
        match self.audio.sample_rate {
            Some(rate) if (8_000..=384_000).contains(&rate) => s.sample_rate = rate,
            Some(rate) => problems.push(format!("sample_rate = {rate} is out of range")),
            None => {}
        }
        if let Some(name) = self.audio.output {
            match parse_output_mode(&name) {
                Some(mode) => s.output_mode = mode,
                None => problems.push(format!("output = {name:?} isn't mix or per-deck")),
            }
        }
        if let Some(percent) = self.decks.tempo_range {
            match parse_tempo_range(percent) {
                Some(range) => s.tempo_range = range,
                None => problems.push(format!("tempo_range = {percent} isn't 8, 16 or 50")),
            }
        }
        if let Some(on) = self.decks.key_lock {
            s.key_lock = on;
        }
        if let Some(on) = self.decks.quantize {
            s.quantize = on;
        }
        if let Some(name) = self.display.key_notation {
            match parse_key_notation(&name) {
                Some(notation) => s.key_notation = notation,
                None => problems.push(format!("key_notation = {name:?} isn't known")),
            }
        }
        if let Some(name) = self.display.theme.filter(|n| !n.trim().is_empty()) {
            s.theme = name;
        }
        if let Some(name) = self.display.waveform {
            match parse_waveform_style(&name) {
                Some(style) => s.waveform = style,
                None => problems.push(format!("waveform = {name:?} isn't known")),
            }
        }
        for (slot, path) in s.sampler.paths.iter_mut().zip(&self.sampler.slots) {
            if !path.is_empty() {
                *slot = Some(PathBuf::from(path));
            }
        }
        for (slot, name) in s.sampler.modes.iter_mut().zip(&self.sampler.modes) {
            match parse_sample_mode(name) {
                Some(mode) => *slot = mode,
                None => problems.push(format!("sampler mode {name:?} isn't known")),
            }
        }
        if let Some(level) = self.sampler.level {
            s.sampler.level = level.clamp(0.0, 1.0);
        }
        if let Some(sync) = self.sampler.sync {
            s.sampler.sync = sync;
        }
        (s, problems)
    }
}

fn parse_sample_mode(value: &str) -> Option<SampleMode> {
    match value.to_ascii_lowercase().as_str() {
        "one-shot" | "oneshot" => Some(SampleMode::OneShot),
        "gate" | "gated" => Some(SampleMode::Gated),
        "loop" => Some(SampleMode::Loop),
        _ => None,
    }
}

fn sample_mode_name(mode: SampleMode) -> &'static str {
    match mode {
        SampleMode::OneShot => "one-shot",
        SampleMode::Gated => "gate",
        SampleMode::Loop => "loop",
    }
}

/// A TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parse a `--waveform` value.
pub fn parse_waveform_style(value: &str) -> Option<WaveformStyle> {
    match value.to_ascii_lowercase().as_str() {
        "spectrum" | "rgb" => Some(WaveformStyle::Spectrum),
        "bands" | "3band" | "three-band" => Some(WaveformStyle::ThreeBand),
        _ => None,
    }
}

fn waveform_style_name(style: WaveformStyle) -> &'static str {
    match style {
        WaveformStyle::Spectrum => "spectrum",
        WaveformStyle::ThreeBand => "bands",
    }
}

/// Parse a `--key-notation` value.
pub fn parse_key_notation(value: &str) -> Option<KeyNotation> {
    match value.to_ascii_lowercase().as_str() {
        "open" | "openkey" | "open-key" => Some(KeyNotation::OpenKey),
        "camelot" => Some(KeyNotation::Camelot),
        "musical" | "music" | "classic" => Some(KeyNotation::Musical),
        _ => None,
    }
}

fn key_notation_name(notation: KeyNotation) -> &'static str {
    match notation {
        KeyNotation::OpenKey => "open",
        KeyNotation::Camelot => "camelot",
        KeyNotation::Musical => "musical",
    }
}

fn parse_output_mode(value: &str) -> Option<OutputMode> {
    match value.to_ascii_lowercase().as_str() {
        "mix" | "internal" => Some(OutputMode::InternalMix),
        "per-deck" | "perdeck" | "decks" => Some(OutputMode::PerDeck),
        _ => None,
    }
}

fn output_mode_name(mode: OutputMode) -> &'static str {
    match mode {
        OutputMode::InternalMix => "mix",
        OutputMode::PerDeck => "per-deck",
    }
}

fn parse_tempo_range(percent: u32) -> Option<TempoRange> {
    match percent {
        8 => Some(TempoRange::Percent8),
        16 => Some(TempoRange::Percent16),
        50 => Some(TempoRange::Percent50),
        _ => None,
    }
}

pub fn tempo_range_percent(range: TempoRange) -> u32 {
    match range {
        TempoRange::Percent8 => 8,
        TempoRange::Percent16 => 16,
        TempoRange::Percent50 => 50,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dj-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.toml")
    }

    #[test]
    fn key_notation_names() {
        assert_eq!(parse_key_notation("open"), Some(KeyNotation::OpenKey));
        assert_eq!(parse_key_notation("Camelot"), Some(KeyNotation::Camelot));
        assert_eq!(parse_key_notation("musical"), Some(KeyNotation::Musical));
        assert_eq!(parse_key_notation("nope"), None);
        assert_eq!(Settings::default().key_notation, KeyNotation::OpenKey);
    }

    #[test]
    fn waveform_style_names() {
        assert_eq!(
            parse_waveform_style("bands"),
            Some(WaveformStyle::ThreeBand)
        );
        assert_eq!(parse_waveform_style("RGB"), Some(WaveformStyle::Spectrum));
        assert_eq!(parse_waveform_style("nope"), None);
        assert_eq!(Settings::default().waveform, WaveformStyle::Spectrum);
    }

    #[test]
    fn saves_and_loads_every_setting() {
        let path = scratch("roundtrip");
        let settings = Settings {
            key_notation: KeyNotation::Camelot,
            waveform: WaveformStyle::ThreeBand,
            theme: "my \"own\"".to_string(),
            audio_device: Some("DJ-505 \"USB\"".to_string()),
            buffer_frames: None,
            sample_rate: 44_100,
            output_mode: OutputMode::PerDeck,
            tempo_range: TempoRange::Percent16,
            key_lock: false,
            quantize: false,
            sampler: SamplerBank {
                paths: std::array::from_fn(|i| match i {
                    2 => Some(PathBuf::from("/music/air horn.wav")),
                    _ => None,
                }),
                modes: std::array::from_fn(|i| match i {
                    2 => SampleMode::Loop,
                    5 => SampleMode::Gated,
                    _ => SampleMode::OneShot,
                }),
                level: 0.5,
                sync: false,
            },
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        let defaults = Settings::default();
        defaults.save(&path).unwrap();
        assert_eq!(Settings::load(&path), defaults);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_or_bad_files_give_defaults() {
        let path = scratch("bad");
        assert_eq!(Settings::load(&path), Settings::default());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[decks]\ntempo_range = 12\nkey_lock = false\n").unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(
            loaded.tempo_range,
            TempoRange::Percent8,
            "bad value ignored"
        );
        assert!(!loaded.key_lock, "good value kept");

        std::fs::write(&path, "this is not toml [").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        assert!(!path.exists());
        assert!(
            path.with_extension("toml.bad").exists(),
            "kept for the user"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn command_line_wins_without_touching_the_rest() {
        let file = Settings {
            key_lock: false,
            buffer_frames: Some(512),
            ..Settings::default()
        };
        let overrides = Overrides {
            buffer_frames: Some(None),
            key_notation: Some(KeyNotation::Musical),
            ..Overrides::default()
        };
        let run = file.clone().with(&overrides);
        assert_eq!(run.buffer_frames, None);
        assert_eq!(run.key_notation, KeyNotation::Musical);
        assert!(!run.key_lock);
    }
}
