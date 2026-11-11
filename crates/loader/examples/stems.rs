//! Load a track and its stems file and report how well they line up.
//!
//!     cargo run --release -p dj-loader --example stems -- track.mp3 [out.wav]
//!
//! With `out.wav`, also writes 20 s from the middle of the track with the vocals
//! subtracted, to check by ear.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use dj_engine::{Stem, StemCache, TrackId};

#[allow(clippy::print_stdout)]
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().expect("usage: stems <track> [out.wav]"));
    let out = args.next().map(PathBuf::from);
    let never = AtomicBool::new(false);
    let mut decoded = None;
    dj_loader::decode_file(&path, TrackId(1), 48_000, &never, &mut |t, _| {
        decoded = Some(t)
    })?;
    let track = decoded.expect("no audio");
    let loaded = dj_loader::load_stems(&dj_loader::stems_path(&path), &track, &never)?;
    let frames = track.len_frames();
    let raw = frames as usize * 4 * 2 * 4;
    println!(
        "offset {} frames, residual {:.1} dB, {:.1} MB compressed ({:.0}% of 16-bit), loaded in {:.2} s",
        loaded.offset,
        loaded.residual_db,
        loaded.stems.compressed_bytes() as f64 / 1e6,
        100.0 * loaded.stems.compressed_bytes() as f64 / (raw as f64 / 2.0),
        loaded.elapsed.as_secs_f64()
    );
    if let Some(out) = out {
        let mut cache = StemCache::new();
        let start = frames / 2;
        let mut samples = Vec::new();
        for f in start..start + 48_000 * 20 {
            let (l, r) = track.frame_unchecked_ready(f);
            let (vl, vr) = cache.frame(loaded.stems.stem(Stem::Vocals), f as i64 + loaded.offset);
            samples.push(((l - vl).clamp(-1.0, 1.0) * 32767.0) as i16);
            samples.push(((r - vr).clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        write_wav(&out, &samples)?;
    }
    Ok(())
}

fn write_wav(path: &std::path::Path, samples: &[i16]) -> std::io::Result<()> {
    let data = (samples.len() * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&48_000u32.to_le_bytes());
    bytes.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes)
}
