//! Command-line arguments. Hand-rolled to keep dependencies small.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use dj_control::ControlSettings;

use crate::settings::{Overrides, parse_key_notation, parse_waveform_style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Window,
    /// Run without a window for `seconds`, logging callback stats. Plays deck A if a
    /// file was given, otherwise the test tone.
    Headless {
        seconds: u64,
    },
    ListDevices,
    /// List MIDI ports.
    ListMidi,
    /// Run without a window for `millis`, then save the screen to `path` as a PNG.
    /// `keys` are typed halfway through (to show states for checking the UI).
    Screenshot {
        path: PathBuf,
        millis: u64,
        keys: String,
        /// Width and height in pixels, and the UI scale in percent.
        size: (u32, u32, u32),
        /// A mouse drag from one point to another (pixels), done halfway through
        /// after the keys and released three quarters of the way in.
        drag: Option<((i32, i32), (i32, i32))>,
    },
    /// Write a theme file to edit, `themes/NAME.toml`, and exit.
    WriteTheme {
        name: String,
    },
    /// Log every incoming MIDI message (and what it maps to) until Ctrl-C.
    /// `all` also shows clock and active-sensing messages.
    MidiMonitor {
        all: bool,
    },
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub mode: Mode,
    /// Settings given as options; they win over the settings file for this run.
    pub overrides: Overrides,
    /// Output channels to ask for (`--channels`); `None` prefers 4, then 2.
    pub channels: Option<u16>,
    /// Settings file; `None` uses the default location.
    pub settings_file: Option<PathBuf>,
    pub fullscreen: bool,
    /// Tracks to load at startup: the first onto deck A, the second onto deck B.
    pub files: Vec<PathBuf>,
    pub control: ControlSettings,
    /// Don't look for MIDI controllers.
    pub no_midi: bool,
    /// Start the startup files playing as soon as they are loaded.
    pub play: bool,
    /// Use native Wayland rather than X11 on Linux.
    pub wayland: bool,
    /// Library database; `None` uses the default location.
    pub library: Option<PathBuf>,
    /// Music folders to scan (empty: ~/Music).
    pub music: Vec<PathBuf>,
    /// Where recordings go; `None`: `Recordings` in the music folder.
    pub recordings: Option<PathBuf>,
}

impl Cli {
    pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut cli = Cli {
            mode: Mode::Window,
            overrides: Overrides::default(),
            channels: None,
            settings_file: None,
            fullscreen: false,
            files: Vec::new(),
            control: ControlSettings::default(),
            no_midi: false,
            play: false,
            wayland: false,
            library: None,
            music: Vec::new(),
            recordings: None,
        };
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => cli.mode = Mode::Help,
                "--list-devices" => cli.mode = Mode::ListDevices,
                "--list-midi" => cli.mode = Mode::ListMidi,
                "--midi-monitor" => cli.mode = Mode::MidiMonitor { all: false },
                "--midi-monitor-all" => cli.mode = Mode::MidiMonitor { all: true },
                "--screenshot" => {
                    let path = PathBuf::from(value(&mut args, &arg)?);
                    let (millis, keys, size, drag) = match &cli.mode {
                        Mode::Screenshot {
                            millis,
                            keys,
                            size,
                            drag,
                            ..
                        } => (*millis, keys.clone(), *size, *drag),
                        _ => (3000, String::new(), (1280, 800, 100), None),
                    };
                    cli.mode = Mode::Screenshot {
                        path,
                        millis,
                        keys,
                        size,
                        drag,
                    };
                }
                "--screenshot-after" => {
                    let after: f32 = value(&mut args, &arg)?
                        .parse()
                        .context("--screenshot-after takes seconds")?;
                    match &mut cli.mode {
                        Mode::Screenshot { millis, .. } => *millis = (after * 1000.0) as u64,
                        _ => bail!("--screenshot-after needs --screenshot first"),
                    }
                }
                "--screenshot-drag" => {
                    let text = value(&mut args, &arg)?;
                    let parsed = parse_drag(&text).with_context(|| {
                        format!("--screenshot-drag takes X1,Y1:X2,Y2 in pixels, not {text:?}")
                    })?;
                    match &mut cli.mode {
                        Mode::Screenshot { drag, .. } => *drag = Some(parsed),
                        _ => bail!("--screenshot-drag needs --screenshot first"),
                    }
                }
                "--screenshot-size" => {
                    let text = value(&mut args, &arg)?;
                    let parsed = parse_size(&text).with_context(|| {
                        format!("--screenshot-size takes WxH or WxH@SCALE, not {text:?}")
                    })?;
                    match &mut cli.mode {
                        Mode::Screenshot { size, .. } => *size = parsed,
                        _ => bail!("--screenshot-size needs --screenshot first"),
                    }
                }
                "--screenshot-keys" => {
                    let typed = value(&mut args, &arg)?;
                    match &mut cli.mode {
                        Mode::Screenshot { keys, .. } => *keys = typed,
                        _ => bail!("--screenshot-keys needs --screenshot first"),
                    }
                }
                "--midi" => cli.control.port = Some(value(&mut args, &arg)?),
                "--no-midi" => cli.no_midi = true,
                "--play" => cli.play = true,
                "--wayland" => cli.wayland = true,
                "--library" => cli.library = Some(PathBuf::from(value(&mut args, &arg)?)),
                "--music" => cli.music.push(PathBuf::from(value(&mut args, &arg)?)),
                "--recordings" => cli.recordings = Some(PathBuf::from(value(&mut args, &arg)?)),
                "--key-notation" => {
                    let value = value(&mut args, &arg)?;
                    let notation = parse_key_notation(&value).with_context(|| {
                        format!("--key-notation takes open, camelot or musical, not {value:?}")
                    })?;
                    cli.overrides.key_notation = Some(notation);
                }
                "--waveform" => {
                    let value = value(&mut args, &arg)?;
                    let style = parse_waveform_style(&value).with_context(|| {
                        format!("--waveform takes spectrum or bands, not {value:?}")
                    })?;
                    cli.overrides.waveform = Some(style);
                }
                "--headless" => {
                    let seconds = value(&mut args, &arg)?;
                    cli.mode = Mode::Headless {
                        seconds: seconds.parse().context("--headless takes seconds")?,
                    };
                }
                "--theme" => cli.overrides.theme = Some(value(&mut args, &arg)?),
                "--write-theme" => {
                    cli.mode = Mode::WriteTheme {
                        name: value(&mut args, &arg)?,
                    }
                }
                "--settings" => cli.settings_file = Some(PathBuf::from(value(&mut args, &arg)?)),
                "--device" => cli.overrides.audio_device = Some(value(&mut args, &arg)?),
                "--rate" => {
                    let rate = value(&mut args, &arg)?
                        .parse()
                        .context("--rate takes a sample rate in Hz")?;
                    cli.overrides.sample_rate = Some(rate);
                }
                "--buffer" => {
                    let frames = value(&mut args, &arg)?;
                    cli.overrides.buffer_frames = match frames.as_str() {
                        "default" => Some(None),
                        n => Some(Some(
                            n.parse().context("--buffer takes frames or 'default'")?,
                        )),
                    };
                }
                "--channels" => {
                    cli.channels = Some(
                        value(&mut args, &arg)?
                            .parse()
                            .context("--channels takes a number")?,
                    );
                }
                "--fullscreen" => cli.fullscreen = true,
                other if other.starts_with('-') => {
                    bail!("unknown argument {other:?} (try --help)")
                }
                _ if cli.files.len() >= 2 => bail!("at most two files (deck A and deck B)"),
                file => cli.files.push(PathBuf::from(file)),
            }
        }
        Ok(cli)
    }
}

/// `X1,Y1:X2,Y2`.
fn parse_drag(text: &str) -> Option<((i32, i32), (i32, i32))> {
    let point = |p: &str| {
        let (x, y) = p.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    };
    let (from, to) = text.split_once(':')?;
    Some((point(from)?, point(to)?))
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next().with_context(|| format!("{flag} needs a value"))
}

#[allow(clippy::print_stdout)]
pub fn print_help() {
    println!(
        "dj — two-deck DJ software

USAGE: dj [OPTIONS] [FILE_A] [FILE_B]

Files given on the command line load onto deck A and deck B. In the window, drop a
file onto it to load it onto the selected deck.

OPTIONS:
  --settings FILE      Settings file. Default: ~/.config/dj/settings.toml (macOS:
                       ~/Library/Application Support/dj/settings.toml). The options
                       below override it for this run only
  --device NAME        Output device (substring match). Default: system default
  --rate HZ            Engine sample rate. Default: 48000
  --buffer FRAMES      Period size in frames, or 'default'. Default: 128
  --channels N         Output channels. Default: 4 if available, else 2
  --fullscreen         Borderless fullscreen window
  --wayland            Linux: use native Wayland instead of X11/XWayland (drag and
                       drop doesn't work there yet)
  --screenshot PNG     No window: run for a few seconds, then save the screen
                       (1280x800) to PNG. --screenshot-after SECONDS sets the
                       delay (default 3); --screenshot-keys KEYS types KEYS
                       halfway through (\t is Tab, a space is Space,
                       \n is Enter, ←→↑↓ are the arrow keys);
                       --screenshot-size WxH[@SCALE] sets the size;
                       --screenshot-drag X1,Y1:X2,Y2 drags the mouse halfway
                       through and lets go at three quarters
  --music DIR          Music folder to scan into the library (repeatable). Default:
                       ~/Music. USB drives are added when plugged in
  --theme NAME         Theme for this run: dark, contrast, daylight or a file in
                       themes/ next to the settings file
  --write-theme NAME   Write themes/NAME.toml (every colour, from the current
                       built-in theme) to edit, then exit. Saved edits show at once
  --recordings DIR     Where REC saves the main output (24-bit WAV). Default:
                       Recordings in the music folder (~/Music/Recordings)
  --library FILE       Library database (hot cues, loops). Default:
                       ~/.local/share/dj/library.db (macOS: ~/Library/Application
                       Support/dj/library.db)
  --key-notation N     How keys are shown: open (Open Key, default), camelot, or
                       musical (e.g. F♯m)
  --waveform STYLE     Waveform colours: spectrum (default; red bass, green mids,
                       blue highs) or bands (blue low, orange mid, white high)
  --play               Start the files given on the command line playing once loaded
  --headless SECONDS   No window: play FILE_A (or the test tone) and log stats
  --list-devices       List output devices
  --midi NAME          Use the MIDI controller whose port name contains NAME.
                       Default: the first port with a mapping (DJ-505), connected
                       whenever it is plugged in
  --no-midi            Don't use MIDI controllers
  --list-midi          List MIDI ports
    --midi-monitor       Log incoming MIDI messages and what they map to (filter
                       ports with --midi). Clock ticks are hidden;
                       --midi-monitor-all shows everything
  -h, --help           Show this help

KEYS (act on the selected deck):
  Tab          Select deck A / B
  Space        Play / pause
  C            CUE (hold): at a stopped position, sets the cue and previews it;
               while playing, returns to the cue point and stops
  Left / Right Seek back / forward 4 s
  Up / Down    Tempo faster / slower (1/16 of the range per press)
  , / .        Nudge slower / faster (hold to keep bending)
  R            Cycle tempo range (±8%, ±16%, ±50%)
  P            Headphone cue (PFL)
  E            Eject
  T            Toggle test tone
  Esc          Quit"
    );
}

/// `1920x1080` or `2560x1440@2`: width, height and scale in percent.
fn parse_size(text: &str) -> Option<(u32, u32, u32)> {
    let (dims, scale) = match text.split_once('@') {
        Some((dims, scale)) => (dims, (scale.parse::<f32>().ok()? * 100.0).round() as u32),
        None => (text, 100),
    };
    let (w, h) = dims.split_once('x')?;
    let (w, h): (u32, u32) = (w.parse().ok()?, h.parse().ok()?);
    match w > 0 && h > 0 && scale > 0 {
        true => Some((w, h, scale)),
        false => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<Cli> {
        Cli::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.mode, Mode::Window);
        assert_eq!(cli.overrides, Overrides::default());
    }

    #[test]
    fn parses_audio_options() {
        let cli = parse(&["--device", "DJ-505", "--buffer", "256", "--channels", "4"]).unwrap();
        assert_eq!(cli.overrides.audio_device.as_deref(), Some("DJ-505"));
        assert_eq!(cli.overrides.buffer_frames, Some(Some(256)));
        assert_eq!(cli.channels, Some(4));
    }

    #[test]
    fn buffer_default_keyword() {
        let cli = parse(&["--buffer", "default"]).unwrap();
        assert_eq!(cli.overrides.buffer_frames, Some(None));
    }

    #[test]
    fn files_load_onto_decks() {
        let cli = parse(&["a.flac", "--buffer", "256", "b.mp3"]).unwrap();
        assert_eq!(
            cli.files,
            vec![PathBuf::from("a.flac"), PathBuf::from("b.mp3")]
        );
        assert!(parse(&["a", "b", "c"]).is_err());
    }

    #[test]
    fn midi_options() {
        let cli = parse(&["--midi", "505", "--midi-monitor"]).unwrap();
        assert_eq!(cli.control.port.as_deref(), Some("505"));
        assert_eq!(cli.mode, Mode::MidiMonitor { all: false });
        assert!(parse(&["--no-midi"]).unwrap().no_midi);
    }

    #[test]
    fn rejects_unknown_and_missing_values() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--device"]).is_err());
    }

    #[test]
    fn screenshot_sizes() {
        assert_eq!(parse_size("1920x1080"), Some((1920, 1080, 100)));
        assert_eq!(parse_size("2560x1440@1.5"), Some((2560, 1440, 150)));
        assert_eq!(parse_size("0x10"), None);
        assert_eq!(parse_size("big"), None);
    }
}
