//! Render a track through the engine with each beat FX, to check them by ear, and
//! report what the FX costs per callback.
//!
//!     cargo run --release -p dj-loader --example fx_render -- TRACK OUT_DIR
//!
//! Writes `OUT_DIR/<effect>.wav`: 16 s from the middle of the track, with the effect
//! on main from 4 s to 10 s (so echo and reverb tails ring out after), at level 0.6
//! and one beat.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use dj_engine::{
    BeatGrid, Command, DeckAction, DeckId, Engine, EngineConfig, FxControl, FxKind, TrackId,
};

const SR: u32 = 48_000;
const PERIOD: usize = 128;

#[allow(clippy::print_stdout)]
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let track_path = PathBuf::from(args.next().expect("usage: fx_render TRACK OUT_DIR"));
    let out_dir = PathBuf::from(args.next().expect("usage: fx_render TRACK OUT_DIR"));
    std::fs::create_dir_all(&out_dir)?;
    let never = AtomicBool::new(false);
    let mut decoded = None;
    dj_loader::decode_file(&track_path, TrackId(1), SR, &never, &mut |t, _| {
        decoded = Some(t)
    })?;
    let track = decoded.expect("no audio");
    let analysis = dj_analysis::analyze(&track);
    let grid = analysis.grid.map(|g| g.grid);
    println!("grid: {grid:?}");

    for kind in FxKind::ALL {
        let mut parts = Engine::new(EngineConfig::new(SR, 2));
        parts.tracks.send(DeckId::A, track.clone()).ok();
        let mut out = vec![0.0f32; PERIOD * 2];
        parts.engine.process(&mut out, PERIOD);
        let send = |parts: &mut dj_engine::EngineParts, c: Command| {
            parts.ui.send(c).expect("queue");
        };
        send(
            &mut parts,
            Command::deck(
                DeckId::A,
                DeckAction::SetBeatGrid {
                    track: track.id(),
                    grid: grid.or(Some(BeatGrid::from_bpm(120.0, 0.0, SR))),
                },
            ),
        );
        send(
            &mut parts,
            Command::deck(
                DeckId::A,
                DeckAction::JumpTo {
                    frame: track.len_frames() as f64 / 2.0,
                },
            ),
        );
        send(&mut parts, Command::deck(DeckId::A, DeckAction::PlayPause));
        send(&mut parts, Command::Fx(FxControl::SetKind(kind)));
        send(&mut parts, Command::Fx(FxControl::SetBeats(1.0)));
        send(&mut parts, Command::Fx(FxControl::SetLevel(0.6)));
        let total = SR as usize * 16;
        let mut samples = Vec::with_capacity(total * 2);
        let mut busy = Duration::ZERO;
        let mut worst = Duration::ZERO;
        let mut rendered = 0;
        while rendered < total {
            if rendered == SR as usize * 4 {
                send(&mut parts, Command::Fx(FxControl::SetOn(true)));
            }
            if rendered == SR as usize * 10 {
                send(&mut parts, Command::Fx(FxControl::SetOn(false)));
            }
            let started = Instant::now();
            parts.engine.process(&mut out, PERIOD);
            let took = started.elapsed();
            if (SR as usize * 4..SR as usize * 10).contains(&rendered) {
                busy += took;
                worst = worst.max(took);
            }
            samples.extend(out.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
            rendered += PERIOD;
        }
        let callbacks = (SR as usize * 6 / PERIOD) as u32;
        let budget = Duration::from_secs_f64(PERIOD as f64 / SR as f64);
        println!(
            "{:8} mean {:5.1} µs ({:.1}% of the period), worst {:5.1} µs",
            kind.label(),
            (busy / callbacks).as_secs_f64() * 1e6,
            100.0 * (busy / callbacks).as_secs_f64() / budget.as_secs_f64(),
            worst.as_secs_f64() * 1e6,
        );
        write_wav(
            &out_dir.join(format!("{}.wav", kind.label().to_lowercase())),
            &samples,
        )?;
    }
    Ok(())
}

fn write_wav(path: &Path, samples: &[i16]) -> std::io::Result<()> {
    let data = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&SR.to_le_bytes());
    bytes.extend_from_slice(&(SR * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes)
}
