//! DJ application entry point.
//!
//! Milestone 2: two decks, the internal mixer and the Roland DJ-505. Tracks load from
//! the command line or by dropping a file on the window. The DJ-505 is picked up
//! whenever it is plugged in; everything also works from the keyboard. Run `dj --help`
//! for options and keys.

mod browser;
mod cli;
mod frontend;
mod library_worker;
mod settings;

use std::time::Duration;

use anyhow::Context as _;
use dj_audio_io::{AudioOutput, OpenDevice};
use dj_control::Controller;
use dj_engine::{Command, DeckAction, DeckId, Engine, EngineHandle, EngineParts, EngineSnapshot};
use dj_loader::{LoadEvent, Loader};

use crate::cli::{Cli, Mode};

// Backs the audio-callback allocation check: in debug builds, allocating inside
// `assert_no_alloc` aborts the process. In release builds the check compiles away and the
// system allocator is used directly.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn".into()),
        )
        .init();

    let cli = Cli::parse(std::env::args().skip(1))?;

    match cli.mode {
        Mode::Help => {
            cli::print_help();
            Ok(())
        }
        Mode::ListDevices => {
            for name in dj_audio_io::output_device_names()? {
                tracing::info!("output device: {name}");
            }
            Ok(())
        }
        Mode::ListMidi => {
            let (inputs, outputs) = dj_control::port_names()?;
            for name in inputs {
                tracing::info!("MIDI input: {name}");
            }
            for name in outputs {
                tracing::info!("MIDI output: {name}");
            }
            Ok(())
        }
        Mode::MidiMonitor { all } => {
            #[allow(clippy::print_stdout)]
            let mut print = |line: &str| println!("{line}");
            dj_control::monitor::run(cli.control.port.as_deref(), None, all, &mut print)?;
            Ok(())
        }
        Mode::Screenshot {
            ref path,
            millis,
            ref keys,
            size: (width, height, scale),
        } => {
            let (mut frontend, controller) = build_frontend(&cli)?;
            let size = dj_ui::FrameSize {
                width,
                height,
                scale: scale as f32 / 100.0,
            };
            screenshot(
                &mut frontend,
                path,
                Duration::from_millis(millis),
                keys,
                size,
            )?;
            drop(controller);
            Ok(())
        }
        Mode::Headless { seconds } => {
            let mut app = start(&cli)?;
            run_headless(&mut app, !cli.files.is_empty(), seconds)
        }
        Mode::Window => {
            // The frontend owns the audio output (its watchdog can reopen the stream),
            // so audio stops when the UI exits.
            let (frontend, controller) = build_frontend(&cli)?;
            let window = dj_ui::WindowConfig {
                fullscreen: cli.fullscreen,
                prefer_wayland: cli.wayland,
                ..Default::default()
            };
            let result = dj_ui::run(window, frontend);
            // Switch the controller's LEDs off.
            drop(controller);
            result
        }
    }
}

/// Drive the frontend at 60 fps without a window for `duration`, then render its scene
/// offscreen at the target panel size and save it as a PNG.
fn screenshot(
    frontend: &mut frontend::Frontend,
    path: &std::path::Path,
    duration: Duration,
    keys: &str,
    size: dj_ui::FrameSize,
) -> anyhow::Result<()> {
    use dj_ui::{Delegate as _, Input, Key};
    let mut offscreen = dj_ui::Offscreen::new().context("starting offscreen rendering")?;
    let started = std::time::Instant::now();
    let deadline = started + duration;
    let mut typed = keys.is_empty();
    // Render every frame, as the window would, so caches behave the same.
    let pixels = loop {
        if !typed && started.elapsed() >= duration / 2 {
            typed = true;
            for c in keys.chars() {
                let key = match c {
                    '\t' => Key::Tab,
                    ' ' => Key::Space,
                    c => Key::Char(c),
                };
                frontend.on_input(Input::KeyDown { key, repeat: false });
                frontend.on_input(Input::KeyUp { key });
            }
        }
        let scene = frontend.scene(size);
        let pixels = offscreen.render(&scene, size.width, size.height)?;
        if std::time::Instant::now() >= deadline {
            break pixels;
        }
        std::thread::sleep(Duration::from_millis(16));
    };
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size.width, size.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&pixels))
        .context("writing PNG")?;
    tracing::info!("saved screenshot to {}", path.display());
    Ok(())
}

/// Start everything the windowed (or screenshot) app needs.
fn build_frontend(cli: &Cli) -> anyhow::Result<(frontend::Frontend, Option<Controller>)> {
    let app = start(cli)?;
    let library_path = cli
        .library
        .clone()
        .unwrap_or_else(dj_library::Library::default_path);
    let library = open_library(&library_path);
    let worker = match library.is_some() {
        true => match library_worker::LibraryWorker::spawn(library_path, music_folders(cli)) {
            Ok(worker) => Some(worker),
            Err(err) => {
                tracing::error!("can't start the library scanner: {err}");
                None
            }
        },
        false => None,
    };
    let parts = frontend::Parts {
        engine: app.handle,
        loader: app.loader,
        audio: app.audio,
        play_on_load: [0, 1].map(|i| cli.play && cli.files.len() > i),
        library,
        settings: cli.settings,
        worker,
        controller_events: app.controller_events,
    };
    Ok((frontend::Frontend::new(parts), app.controller))
}

/// Folders to scan: those given with --music, else ~/Music if it exists.
fn music_folders(cli: &Cli) -> Vec<std::path::PathBuf> {
    if !cli.music.is_empty() {
        return cli.music.clone();
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    home.map(|h| h.join("Music"))
        .filter(|p| p.is_dir())
        .into_iter()
        .collect()
}

/// Open the library, or carry on without one (hot cues then aren't saved).
fn open_library(path: &std::path::Path) -> Option<dj_library::Library> {
    match dj_library::Library::open(path) {
        Ok(library) => {
            tracing::info!("library: {}", path.display());
            Some(library)
        }
        Err(err) => {
            tracing::error!("can't open the library, so hot cues won't be saved: {err}");
            None
        }
    }
}

struct Running {
    audio: AudioOutput,
    handle: EngineHandle,
    loader: Loader,
    controller: Option<Controller>,
    /// Browsing and loading from the controller, for the frontend.
    controller_events: std::sync::mpsc::Receiver<dj_control::UiEvent>,
}

/// Open the audio device, start the engine and loader, and queue the startup files.
fn start(cli: &Cli) -> anyhow::Result<Running> {
    let device = OpenDevice::open(&cli.audio).context("opening audio device")?;
    tracing::info!(
        "audio: {} — {} ch, {} Hz, {:?}, buffer {:?}",
        device.device_name(),
        device.channels(),
        device.sample_rate(),
        device.sample_format(),
        device.buffer_size(),
    );
    let EngineParts {
        engine,
        ui: handle,
        control,
        tracks,
    } = Engine::new(device.engine_config());
    let loader = Loader::spawn(device.sample_rate(), tracks).context("starting loader thread")?;
    let audio = device.start(engine).context("starting audio stream")?;
    let (controller_tx, controller_events) = std::sync::mpsc::channel();
    let controller = match cli.no_midi {
        true => None,
        false => Some(
            Controller::spawn(control, cli.control.clone(), controller_tx)
                .context("starting control thread")?,
        ),
    };
    for (deck, file) in DeckId::ALL.into_iter().zip(&cli.files) {
        loader.load(deck, file.clone());
    }
    Ok(Running {
        audio,
        handle,
        loader,
        controller,
        controller_events,
    })
}

fn run_headless(app: &mut Running, play_deck: bool, seconds: u64) -> anyhow::Result<()> {
    if play_deck {
        wait_for_deck_a(&app.loader)?;
        app.handle
            .send(Command::deck(DeckId::A, DeckAction::PlayPause))
            .context("starting deck A")?;
    } else {
        app.handle
            .send(Command::SetToneEnabled(true))
            .context("enabling tone")?;
    }
    let stats = app.audio.stats();
    for tick in 1..=seconds * 10 {
        std::thread::sleep(Duration::from_millis(100));
        if let Some(Err(err)) = app.audio.watchdog() {
            tracing::error!("{err}");
        }
        while let Some(event) = app.loader.try_event() {
            log_load_event(&event);
        }
        if tick % 10 == 0 {
            log_stats(&stats, &mut app.handle);
        }
    }
    Ok(())
}

/// Block until deck A's startup load is playable, logging load events on the way.
fn wait_for_deck_a(loader: &Loader) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let Some(event) = loader.wait_event(Duration::from_millis(100)) else {
            continue;
        };
        log_load_event(&event);
        match event {
            LoadEvent::Loaded {
                deck: DeckId::A, ..
            } => return Ok(()),
            LoadEvent::Failed {
                deck: DeckId::A,
                path,
                error,
            } => anyhow::bail!("cannot load {}: {error}", path.display()),
            _ => {}
        }
    }
    anyhow::bail!("timed out loading deck A")
}

pub(crate) fn log_load_event(event: &LoadEvent) {
    match event {
        LoadEvent::Loaded {
            deck,
            info,
            path,
            latency,
        } => tracing::info!(
            "deck {deck:?}: loaded {:?} ({} Hz, {} ch) from {}, playable after {latency:.1?}",
            info.title,
            info.source_sample_rate,
            info.source_channels,
            path.display(),
        ),
        LoadEvent::Finished { deck, stats, .. } => tracing::info!(
            "deck {deck:?}: decoded {} frames in {:.1?}{}",
            stats.frames,
            stats.total_time,
            match stats.skipped_packets {
                0 => String::new(),
                n => format!(" ({n} damaged packets skipped)"),
            },
        ),
        LoadEvent::Analyzed {
            deck,
            analysis,
            elapsed,
            ..
        } => tracing::info!(
            "deck {deck:?}: analysed in {elapsed:.1?}: {} kicks{}",
            analysis.kicks.len(),
            match analysis.kick_bpm() {
                Some(bpm) => format!(", about {bpm:.1} BPM between kicks"),
                None => String::new(),
            },
        ),
        LoadEvent::Failed { deck, path, error } => {
            tracing::error!("deck {deck:?}: cannot load {}: {error}", path.display())
        }
    }
}

pub(crate) fn log_stats(stats: &dj_audio_io::CallbackStats, handle: &mut EngineHandle) {
    let s = stats.snapshot();
    let snapshot = handle.snapshot();
    tracing::info!(
        "callbacks {} (period {} frames), xruns {}, errors {}, last {:?}, max {:?}, peak {:.3}{}",
        s.callbacks,
        s.last_frames,
        s.xruns,
        s.other_errors,
        s.last_duration,
        s.max_duration,
        snapshot.main_peak,
        deck_summary(&snapshot, DeckId::A),
    );
}

fn deck_summary(snapshot: &EngineSnapshot, deck: DeckId) -> String {
    let d = snapshot.deck(deck);
    if d.track.is_none() {
        return String::new();
    }
    let rate = snapshot.sample_rate as f64;
    format!(
        ", deck {deck:?} {:?} {:.1}/{:.1} s, tempo {:+.2}%",
        d.transport,
        d.position as f64 / rate,
        d.length as f64 / rate,
        (d.tempo_rate - 1.0) * 100.0,
    )
}
