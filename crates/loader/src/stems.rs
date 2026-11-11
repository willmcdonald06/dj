//! Loading a track's stems file into a compressed in-memory [`StemSet`].
//!
//! The offline analyzer writes `<track>.stems.flac` next to the track: four stereo
//! pairs (vocals, melody, bass, drums) in one 8-channel 16-bit FLAC, with a comment tag
//! `dj-stems/1 gain=<g> ...` giving the gain that undoes any turning down applied to
//! avoid clipping. Loading decodes it, converts each stem to the engine rate and
//! re-encodes it with the engine's stem codec, then lines the stems up with the
//! decoded track: different decoders disagree by a few hundred to a couple of
//! thousand samples about where an MP3 starts, which would comb-filter the mix when a
//! stem is subtracted.
//!
//! Not RT-safe; runs on loader threads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dj_engine::stem_codec::{EncodedStem, StemEncoder};
use dj_engine::{STEM_COUNT, StemCache, StemSet, Track};
use realfft::RealFftPlanner;

use crate::decode::{LoadError, Source};
use crate::resample::StereoResampler;

/// Appended to a track's file name to name its stems file.
pub const STEMS_SUFFIX: &str = ".stems.flac";
const STEMS_FORMAT: &str = "dj-stems";
const STEMS_VERSION: u32 = 1;
/// Largest misalignment between stems and track looked for, in seconds.
const MAX_OFFSET_SECONDS: f64 = 0.1;
/// Window length for the alignment search (frames, a power of two).
const ALIGN_WINDOW: usize = 1 << 15;
/// Windows spread over the track for the alignment search.
const ALIGN_WINDOWS: usize = 6;
/// The aligned stems must rebuild the mix at least this well (residual energy
/// relative to the mix, in dB) or they are rejected: subtracting misfit stems would do
/// more harm than good.
pub const MAX_RESIDUAL_DB: f32 = -12.0;

/// The stems file for `track`, whether or not it exists.
pub fn stems_path(track: &Path) -> PathBuf {
    let mut name = track.file_name().unwrap_or_default().to_os_string();
    name.push(STEMS_SUFFIX);
    track.with_file_name(name)
}

/// Whether `path` is a stems file (so library scans can skip it).
pub fn is_stems_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(STEMS_SUFFIX))
}

/// A loaded stem set and how well it fits its track.
#[derive(Debug)]
pub struct LoadedStems {
    pub stems: StemSet,
    /// Stem frames ahead of the track (see [`StemSet::offset`]).
    pub offset: i64,
    /// Energy of `mix − Σ stems` relative to the mix, in dB (lower is better).
    pub residual_db: f32,
    pub elapsed: Duration,
}

/// Load `path` (a stems file) for `track`, which must be fully decoded.
pub fn load_stems(
    path: &Path,
    track: &Track,
    cancel: &AtomicBool,
) -> Result<LoadedStems, LoadError> {
    let started = Instant::now();
    let mut source = Source::open(path)?;
    let gain = parse_comment(source.comment.as_deref())?;
    if source.channels != STEM_COUNT * 2 {
        return Err(LoadError::Format(format!(
            "stems file has {} channels, expected {}",
            source.channels,
            STEM_COUNT * 2
        )));
    }
    let engine_rate = track.sample_rate();
    let source_rate = source.sample_rate;

    // Decode here; resample and encode each stem on its own thread.
    let encoded = std::thread::scope(|scope| -> Result<_, LoadError> {
        let mut senders = Vec::with_capacity(STEM_COUNT);
        let mut workers = Vec::with_capacity(STEM_COUNT);
        for _ in 0..STEM_COUNT {
            let (send, receive) = mpsc::sync_channel::<(Vec<f32>, Vec<f32>)>(32);
            senders.push(send);
            workers.push(scope.spawn(move || encode_stem(receive, source_rate, engine_rate, gain)));
        }
        let mut cancelled = false;
        while let Some(planar) = source.next_planar()? {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            if planar.len() < STEM_COUNT * 2 {
                continue;
            }
            for (s, send) in senders.iter().enumerate() {
                let pair = (planar[2 * s].clone(), planar[2 * s + 1].clone());
                // A worker only hangs up on a resampler error, reported below.
                let _ = send.send(pair);
            }
        }
        drop(senders);
        let mut encoded = Vec::with_capacity(STEM_COUNT);
        for worker in workers {
            match worker.join() {
                Ok(result) => encoded.push(result?),
                Err(_) => return Err(LoadError::Decode("stem encoder panicked".into())),
            }
        }
        match cancelled {
            true => Err(LoadError::Cancelled),
            false => Ok(encoded),
        }
    })?;
    let Ok(encoded) = <[EncodedStem; STEM_COUNT]>::try_from(encoded) else {
        return Err(LoadError::Empty);
    };
    if encoded.iter().all(|e| e.frames() == 0) {
        return Err(LoadError::Empty);
    }

    let unaligned = StemSet::new(track.id(), encoded, 0);
    let (offset, residual_db) = align(track, &unaligned);
    Ok(LoadedStems {
        stems: unaligned.with_offset(offset),
        offset,
        residual_db,
        elapsed: started.elapsed(),
    })
}

/// Resample (if needed) and encode one stem's audio as it arrives.
fn encode_stem(
    input: mpsc::Receiver<(Vec<f32>, Vec<f32>)>,
    source_rate: u32,
    engine_rate: u32,
    gain: f32,
) -> Result<EncodedStem, LoadError> {
    let mut encoder = StemEncoder::new(gain);
    let mut resampler = match source_rate == engine_rate {
        true => None,
        false => Some(StereoResampler::new(source_rate, engine_rate)?),
    };
    for (left, right) in input {
        match resampler.as_mut() {
            Some(resampler) => resampler.push(&left, &right, &mut |l, r| encoder.push(l, r))?,
            None => encoder.push(&left, &right),
        }
    }
    if let Some(resampler) = resampler.as_mut() {
        resampler.finish(&mut |l, r| encoder.push(l, r))?;
    }
    Ok(encoder.finish())
}

/// The gain from a `dj-stems/<version> gain=<g> ...` comment.
fn parse_comment(comment: Option<&str>) -> Result<f32, LoadError> {
    let comment = comment.unwrap_or_default();
    let mut words = comment.split_whitespace();
    let expected = format!("{STEMS_FORMAT}/{STEMS_VERSION}");
    if words.next() != Some(expected.as_str()) {
        return Err(LoadError::Format(format!(
            "not a {expected} stems file (comment {comment:?})"
        )));
    }
    let gain = words
        .find_map(|word| word.strip_prefix("gain="))
        .and_then(|g| g.parse::<f32>().ok())
        .filter(|g| g.is_finite() && *g > 0.0)
        .unwrap_or(1.0);
    Ok(gain)
}

/// Find the offset that lines the stems up with the track, by cross-correlating the
/// mono mix with the mono stem sum over a few windows, and measure how well the stems
/// then rebuild the mix. Returns `(offset, residual_db)`.
fn align(track: &Track, stems: &StemSet) -> (i64, f32) {
    let max_lag = (MAX_OFFSET_SECONDS * track.sample_rate() as f64) as usize;
    let len = track.ready_frames() as usize;
    if len < ALIGN_WINDOW * 2 {
        return (0, residual_db(track, stems, 0, &[0]));
    }
    let size = ALIGN_WINDOW * 2;
    let mut planner = RealFftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    let mut caches: [StemCache; STEM_COUNT] = std::array::from_fn(|_| StemCache::new());
    let mut mix = forward.make_input_vec();
    let mut sum = forward.make_input_vec();
    let mut mix_spectrum = forward.make_output_vec();
    let mut sum_spectrum = forward.make_output_vec();
    let mut total = vec![0.0f64; size];
    let mut correlation = inverse.make_output_vec();
    let starts = window_starts(len);

    for &start in &starts {
        mix.fill(0.0);
        sum.fill(0.0);
        for i in 0..ALIGN_WINDOW {
            let frame = (start + i) as u64;
            let (l, r) = track.frame_unchecked_ready(frame);
            mix[i] = l + r;
            sum[i] = stem_sum(stems, &mut caches, frame as i64);
        }
        if forward.process(&mut mix, &mut mix_spectrum).is_err()
            || forward.process(&mut sum, &mut sum_spectrum).is_err()
        {
            return (0, f32::INFINITY);
        }
        // conj(M) · S: correlation[lag] = Σ m[n] s[n + lag].
        for (m, s) in mix_spectrum.iter_mut().zip(&sum_spectrum) {
            *m = m.conj() * s;
        }
        if inverse
            .process(&mut mix_spectrum, &mut correlation)
            .is_err()
        {
            return (0, f32::INFINITY);
        }
        for (t, c) in total.iter_mut().zip(&correlation) {
            *t += *c as f64;
        }
    }

    let mut best = (0i64, f64::MIN);
    for lag in 0..=max_lag {
        for (signed, index) in [(lag as i64, lag), (-(lag as i64), (size - lag) % size)] {
            if total[index] > best.1 {
                best = (signed, total[index]);
            }
        }
    }
    let offset = best.0;
    (offset, residual_db(track, stems, offset, &starts))
}

/// Where to put the alignment windows: spread over the middle of the track, which is
/// rarely silent.
fn window_starts(len: usize) -> Vec<usize> {
    let usable = len.saturating_sub(ALIGN_WINDOW);
    (1..=ALIGN_WINDOWS)
        .map(|w| usable * w / (ALIGN_WINDOWS + 1))
        .collect()
}

fn stem_sum(stems: &StemSet, caches: &mut [StemCache; STEM_COUNT], frame: i64) -> f32 {
    let at = frame + stems.offset();
    dj_engine::Stem::ALL
        .iter()
        .zip(caches.iter_mut())
        .map(|(&stem, cache)| {
            let (l, r) = cache.frame(stems.stem(stem), at);
            l + r
        })
        .sum()
}

/// Energy of `mix − Σ stems` (with stems shifted by `offset`) relative to the mix, in
/// dB, over the windows at `starts`.
fn residual_db(track: &Track, stems: &StemSet, offset: i64, starts: &[usize]) -> f32 {
    let mut caches: [StemCache; STEM_COUNT] = std::array::from_fn(|_| StemCache::new());
    let ready = track.ready_frames();
    let (mut signal, mut error) = (0.0f64, 0.0f64);
    for &start in starts {
        for i in 0..ALIGN_WINDOW as u64 {
            let frame = start as u64 + i;
            if frame >= ready {
                break;
            }
            let (ml, mr) = track.frame_unchecked_ready(frame);
            let (mut sl, mut sr) = (0.0, 0.0);
            for (&stem, cache) in dj_engine::Stem::ALL.iter().zip(caches.iter_mut()) {
                let (l, r) = cache.frame(stems.stem(stem), frame as i64 + offset);
                sl += l;
                sr += r;
            }
            signal += (ml * ml + mr * mr) as f64;
            error += ((ml - sl).powi(2) + (mr - sr).powi(2)) as f64;
        }
    }
    match signal > 0.0 {
        true => (10.0 * (error.max(1e-12) / signal).log10()) as f32,
        false => f32::INFINITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_path_appends_suffix() {
        assert_eq!(
            stems_path(Path::new("/m/a b.mp3")),
            PathBuf::from("/m/a b.mp3.stems.flac")
        );
        assert!(is_stems_file(Path::new("/m/a b.mp3.stems.flac")));
        assert!(!is_stems_file(Path::new("/m/a b.flac")));
    }

    #[test]
    fn comment_gives_gain() {
        assert_eq!(
            parse_comment(Some("dj-stems/1 gain=1.5 stems=a")).unwrap(),
            1.5
        );
        assert_eq!(parse_comment(Some("dj-stems/1")).unwrap(), 1.0);
        assert!(parse_comment(Some("dj-stems/9 gain=1")).is_err());
        assert!(parse_comment(None).is_err());
    }

    /// Stems that are the track split in four, delayed by `delay` frames.
    fn track_and_stems(delay: i64) -> (std::sync::Arc<Track>, StemSet) {
        let n = ALIGN_WINDOW * 4;
        let signal = |i: i64, s: usize| -> f32 {
            let x = i as f32;
            match s {
                0 => 0.2 * (x * 0.031).sin(),
                1 => 0.2 * (x * 0.0071 + (x * 0.0003).sin() * 4.0).sin(),
                2 => 0.2 * ((x * 0.0013).sin() * (x * 0.17).sin()),
                _ => 0.2 * (((i * 7919) % 101) as f32 / 50.0 - 1.0),
            }
        };
        let (track, mut writer) = Track::new(dj_engine::TrackId(3), 48_000, n);
        let mix: Vec<f32> = (0..n as i64)
            .flat_map(|i| {
                let v: f32 = (0..4).map(|s| signal(i, s)).sum();
                [v, v]
            })
            .collect();
        writer.push_interleaved(&mix);
        writer.finish();
        let stems = std::array::from_fn(|s| {
            let mut encoder = StemEncoder::new(1.0);
            let samples: Vec<f32> = (0..n as i64).map(|i| signal(i - delay, s)).collect();
            encoder.push(&samples, &samples);
            encoder.finish()
        });
        (track, StemSet::new(dj_engine::TrackId(3), stems, 0))
    }

    #[test]
    fn alignment_finds_the_delay() {
        for delay in [0, 1105, -300] {
            let (track, stems) = track_and_stems(delay);
            let (offset, residual) = align(&track, &stems);
            assert_eq!(offset, delay, "delay {delay}");
            assert!(residual < -60.0, "residual {residual} dB");
        }
    }
}
