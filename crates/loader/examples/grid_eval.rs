//! Score the in-app beatgrid against reference beats (from a neural beat tracker):
//! `cargo run --release -p dj-loader --example grid_eval -- REFS_DIR`, where each
//! `*.json` in REFS_DIR holds `{"path": …, "beats": [s…], "downbeats": [s…]}`.
//!
//! A track counts as right when at least [`GOOD_HITS`] of the reference beats land
//! within [`TOLERANCE_MS`] of a grid line and the tempo is at the same level (not half
//! or double). Tracks whose own reference beats don't follow a constant tempo (live
//! drummers, tempo changes) are listed apart: no constant grid can fit them. Reference
//! beats off the reference's own constant grid (trackers add some in drumless
//! breakdowns) are left out, and the rest move to the attack just before them (the
//! tracker marks the beat as heard; grids sit on the attack).
//!
//! Environment: `DETAIL=1` prints offsets per section; `PHASE=1` and `BARS=1` print
//! the beat-phase and downbeat features (tab-separated) used to tune them; `RATE` sets
//! the decoding rate.

#![allow(clippy::print_stdout)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dj_engine::{BeatGrid, TrackId};

static RATE_OVERRIDE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
#[allow(non_snake_case)]
fn rate() -> u32 {
    *RATE_OVERRIDE.get_or_init(|| {
        std::env::var("RATE")
            .ok()
            .and_then(|r| r.parse().ok())
            .unwrap_or(48_000)
    })
}
const TOLERANCE_MS: f64 = 25.0;
const GOOD_HITS: f64 = 0.9;
/// The reference tracker marks beats about this much after the attack that DJ grids
/// sit on (measured over the steady tracks); hits allow for it.
const BIAS_MS: f64 = 0.0;
/// Reference beats move to an attack this far before (and after) them, in ms.
const SNAP_MS: (f64, f64) = (35.0, 10.0);
/// Tracks whose on-grid reference beats cover less than this much of their length
/// have no steady tempo to find.
const STEADY: f64 = 0.6;

struct Reference {
    path: PathBuf,
    beats: Vec<f64>,
    downbeats: Vec<f64>,
}

#[derive(Debug)]
struct Score {
    name: String,
    /// Reference tempo, and how steady the reference is (its own hit rate).
    ref_bpm: f64,
    steady: f64,
    ours_bpm: Option<f64>,
    confidence: f64,
    /// Fraction of reference beats on our grid.
    hits: f64,
    /// Fraction of reference downbeats on our bar lines.
    bars: f64,
    /// Median offset of reference beats from our grid (ms; positive: ours early),
    /// and how much it changes from the first to the last tenth of the track.
    bias_ms: f64,
    drift_ms: f64,
    verdict: &'static str,
}

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        println!("usage: grid_eval REFS_DIR [NAME_FILTER]");
        return;
    };
    let filter = std::env::args().nth(2).unwrap_or_default().to_lowercase();
    let mut refs: Vec<Reference> = std::fs::read_dir(&dir)
        .expect("refs dir")
        .filter_map(|e| {
            let path = e.ok()?.path();
            let text = std::fs::read_to_string(&path).ok()?;
            let json: serde_json::Value = serde_json::from_str(&text).ok()?;
            let list = |key: &str| -> Vec<f64> {
                json[key]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_f64()).collect())
                    .unwrap_or_default()
            };
            Some(Reference {
                path: PathBuf::from(json["path"].as_str()?),
                beats: list("beats"),
                downbeats: list("downbeats"),
            })
        })
        .filter(|r| r.path.to_string_lossy().to_lowercase().contains(&filter))
        .collect();
    refs.sort_by(|a, b| a.path.cmp(&b.path));

    let next = AtomicUsize::new(0);
    let scores = Mutex::new(Vec::new());
    let refs = Arc::new(refs);
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(reference) = refs.get(i) else {
                        break;
                    };
                    if let Some(score) = score(reference) {
                        scores.lock().unwrap().push(score);
                    }
                }
            });
        }
    });
    let mut scores = scores.into_inner().unwrap();
    scores.sort_by(|a, b| a.verdict.cmp(b.verdict).then(a.name.cmp(&b.name)));
    for s in &scores {
        println!(
            "{:<10} ref {:>6.2} (steady {:>3.0}%)  ours {:>7}  conf {:.2}  hits {:>3.0}%  bars {:>3.0}%  bias {:>4.0}  drift {:>4.0}  {}",
            s.verdict,
            s.ref_bpm,
            s.steady * 100.0,
            s.ours_bpm.map_or("none".to_string(), |b| format!("{b:.2}")),
            s.confidence,
            s.hits * 100.0,
            s.bars * 100.0,
            s.bias_ms,
            s.drift_ms,
            s.name
        );
    }
    let steady: Vec<&Score> = scores.iter().filter(|s| s.steady >= STEADY).collect();
    let count = |v: &str| steady.iter().filter(|s| s.verdict == v).count();
    println!(
        "\n{} tracks, {} with a steady tempo. Of those: {} right ({:.1}%), {} right but bars off, \
         {} half/double, {} off-beat, {} wrong tempo, {} no grid. Mean beat hits {:.1}%.",
        scores.len(),
        steady.len(),
        count("ok") + count("ok-bars"),
        (count("ok") + count("ok-bars")) as f64 / steady.len().max(1) as f64 * 100.0,
        count("ok-bars"),
        count("octave"),
        count("offbeat"),
        count("wrong"),
        count("none"),
        steady.iter().map(|s| s.hits).sum::<f64>() / steady.len().max(1) as f64 * 100.0,
    );
    let mut tempo_right: Vec<&&Score> = steady
        .iter()
        .filter(|s| {
            s.ours_bpm
                .is_some_and(|b| (b / s.ref_bpm - 1.0).abs() < 0.002)
        })
        .collect();
    tempo_right.sort_by(|a, b| a.bias_ms.total_cmp(&b.bias_ms));
    let mut drifts: Vec<f64> = tempo_right.iter().map(|s| s.drift_ms.abs()).collect();
    drifts.sort_by(f64::total_cmp);
    if !tempo_right.is_empty() {
        println!(
            "Right tempo: {}. Median bias {:.1} ms; |drift| median {:.1} ms, 90th percentile {:.1} ms, max {:.1} ms.",
            tempo_right.len(),
            tempo_right[tempo_right.len() / 2].bias_ms,
            drifts[drifts.len() / 2],
            drifts[drifts.len() * 9 / 10],
            drifts[drifts.len() - 1],
        );
    }
}

fn score(reference: &Reference) -> Option<Score> {
    let cancel = AtomicBool::new(false);
    let mut track = None;
    dj_loader::decode_file(&reference.path, TrackId(1), rate(), &cancel, &mut |t, _| {
        track = Some(t)
    })
    .ok()?;
    let track = track?;
    let analysis = dj_analysis::analyze(&track);
    let sr = f64::from(rate());
    let beats: Vec<f64> = reference.beats.iter().map(|b| b * sr).collect();
    let downbeats: Vec<f64> = reference.downbeats.iter().map(|b| b * sr).collect();
    let reference_grid = dj_analysis::beatgrid::fit_beats(&beats, rate())?;
    // The tracker sometimes adds beats in drumless breakdowns; score against the
    // beats on its own constant grid. "Steady" is how much of the track they cover.
    let beats = on_grid(&beats, &reference_grid, 1.0);
    let downbeats = on_grid(&downbeats, &reference_grid, 1.0);
    // The tracker marks the beat as heard, a little after the attack; DJ grids sit on
    // the attack. Move each reference beat to the attack just before it, if any.
    let attacks: Vec<f64> = dj_analysis::transients::detect(&analysis.waveform)
        .iter()
        .map(|t| t.frame as f64)
        .collect();
    let snap = |t: f64| -> f64 {
        let (early, late) = (SNAP_MS.0 / 1000.0 * sr, SNAP_MS.1 / 1000.0 * sr);
        let from = attacks.partition_point(|&a| a < t - early);
        attacks[from..]
            .iter()
            .take_while(|&&a| a <= t + late)
            .min_by(|a, b| (*a - t).abs().total_cmp(&(*b - t).abs()))
            .copied()
            .unwrap_or(t)
    };
    let beats: Vec<f64> = beats.iter().map(|&t| snap(t)).collect();
    let downbeats: Vec<f64> = downbeats.iter().map(|&t| snap(t)).collect();
    let length_beats = analysis.frames as f64 / reference_grid.period;
    let steady = (beats.len() as f64 / length_beats).min(1.0);
    let name = Path::new(&reference.path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ref_bpm = reference_grid.bpm(rate());
    let Some(estimate) = analysis.grid else {
        return Some(Score {
            name,
            ref_bpm,
            steady,
            ours_bpm: None,
            confidence: 0.0,
            hits: 0.0,
            bars: 0.0,
            bias_ms: 0.0,
            drift_ms: 0.0,
            verdict: "none",
        });
    };
    let grid = estimate.grid;
    if std::env::var_os("DETAIL").is_some() {
        // Median signed offset (ms) of the reference beats from our grid, per 32 beats.
        let offsets: Vec<String> = beats
            .chunks(32)
            .map(|chunk| {
                let mut o: Vec<f64> = chunk
                    .iter()
                    .map(|&t| {
                        let x = (t - grid.anchor).rem_euclid(grid.period);
                        let x = match x > grid.period / 2.0 {
                            true => x - grid.period,
                            false => x,
                        };
                        x / sr * 1000.0
                    })
                    .collect();
                o.sort_by(f64::total_cmp);
                format!("{:.0}", o[o.len() / 2])
            })
            .collect();
        let bar_slot: Vec<String> = downbeats
            .chunks(16)
            .map(|chunk| {
                let mut counts = [0; 4];
                for &t in chunk {
                    counts[(grid.beat_at(t).round() as i64).rem_euclid(4) as usize] += 1;
                }
                format!("{counts:?}")
            })
            .collect();
        println!(
            "{}: offsets {}  downbeat slots {}",
            reference.path.display(),
            offsets.join(" "),
            bar_slot.join(" ")
        );
        // Our kicks against the reference grid, per 32 kicks on it.
        let kick_offsets: Vec<f64> = analysis
            .kicks
            .iter()
            .map(|&k| signed_offset_ms(k as f64, &reference_grid))
            .filter(|o| o.abs() < 40.0)
            .collect();
        let kicks: Vec<String> = kick_offsets
            .chunks(32)
            .map(|c| {
                let mut c = c.to_vec();
                c.sort_by(f64::total_cmp);
                format!("{:.0}", c[c.len() / 2])
            })
            .collect();
        println!(
            "  kicks vs reference grid: {}  (ref {:.4} BPM, ours {:.4})",
            kicks.join(" "),
            reference_grid.bpm(rate()),
            grid.bpm(rate())
        );
        let mut ours: Vec<f64> = analysis
            .kicks
            .iter()
            .map(|&k| signed_offset_ms(k as f64, &grid))
            .filter(|o| o.abs() < 40.0)
            .collect();
        ours.sort_by(f64::total_cmp);
        if !ours.is_empty() {
            println!(
                "  kicks vs our grid: median {:.1} ms over {} kicks",
                ours[ours.len() / 2],
                ours.len()
            );
        }
        // Per minute of track: median kick offset from our grid (within ±150 ms).
        let minute = 60.0 * sr;
        let per_minute: Vec<String> = (0..((analysis.frames as f64 / minute).ceil() as usize))
            .map(|m| {
                let mut v: Vec<f64> = analysis
                    .kicks
                    .iter()
                    .filter(|&&k| (k as f64 / minute) as usize == m)
                    .map(|&k| signed_offset_ms(k as f64, &grid))
                    .filter(|o| o.abs() < 150.0)
                    .collect();
                v.sort_by(f64::total_cmp);
                match v.is_empty() {
                    true => "-".to_string(),
                    false => format!("{:.0}({})", v[v.len() / 2], v.len()),
                }
            })
            .collect();
        println!("  kicks per minute vs our grid: {}", per_minute.join(" "));
    }
    let offsets: Vec<f64> = beats.iter().map(|&t| signed_offset_ms(t, &grid)).collect();
    let median = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let tenth = (offsets.len() / 10).max(1);
    let bias_ms = median(&offsets);
    let drift_ms = median(&offsets[offsets.len() - tenth..]) - median(&offsets[..tenth]);
    // Allow for the reference's convention: its beats sit a little after the attack.
    let shift = BIAS_MS / 1000.0 * sr / grid.period;
    if std::env::var_os("PHASE").is_some() {
        let frames = track.ready_frames();
        let mut onsets = dj_analysis::tempo::OnsetAnalyser::new(rate(), frames);
        for i in 0..frames {
            let (l, r) = track.frame_unchecked_ready(i);
            onsets.push(l, r);
        }
        let onsets = onsets.finish();
        if let Some((grids, votes)) =
            dj_analysis::tempo::phase_candidates(&onsets, &analysis.kicks, rate())
        {
            let right = grids.map(|g| hit_rate(&beats, &g, 1.0, 0.0));
            println!(
                "PHASE\t{:.3}\t{:.3}\t{}\t{}",
                right[0],
                right[1],
                votes
                    .iter()
                    .map(|v| format!("{:.1}\t{:.1}", v[0], v[1]))
                    .collect::<Vec<_>>()
                    .join("\t"),
                name
            );
        }
    }
    if std::env::var_os("BARS").is_some() && hit_rate(&beats, &grid, 1.0, 0.0) >= GOOD_HITS {
        let frames = track.ready_frames();
        let mut onsets = dj_analysis::tempo::OnsetAnalyser::new(rate(), frames);
        for i in 0..frames {
            let (l, r) = track.frame_unchecked_ready(i);
            onsets.push(l, r);
        }
        let onsets = onsets.finish();
        let features = dj_analysis::tempo::downbeat_features(&onsets, &grid);
        let mut counts = [0usize; 4];
        for &t in &downbeats {
            counts[(grid.beat_at(t).round() as i64).rem_euclid(4) as usize] += 1;
        }
        let total: usize = counts.iter().sum();
        let (truth, top) = counts
            .iter()
            .enumerate()
            .max_by_key(|c| c.1)
            .map(|(i, c)| (i, *c))
            .unwrap_or((0, 0));
        if total >= 8 && top * 10 >= total * 7 {
            let flat: Vec<String> = features
                .iter()
                .flatten()
                .map(|v| format!("{v:.4}"))
                .collect();
            println!("BARS\t{truth}\t{}\t{name}", flat.join("\t"));
        }
    }
    let hits = hit_rate(&beats, &grid, 1.0, shift);
    let bars = hit_rate(&downbeats, &grid, 4.0, shift / 4.0);
    let ratio = grid.period / reference_grid.period;
    let verdict = match () {
        _ if hits >= GOOD_HITS && bars >= 0.8 => "ok",
        _ if hits >= GOOD_HITS => "ok-bars",
        _ if (ratio - 2.0).abs() < 0.02 || (ratio - 0.5).abs() < 0.01 => "octave",
        _ if (ratio - 1.0).abs() < 0.002
            && hit_rate(&beats, &grid, 1.0, 0.5 + shift) >= GOOD_HITS =>
        {
            "offbeat"
        }
        _ => "wrong",
    };
    Some(Score {
        name,
        ref_bpm,
        steady,
        ours_bpm: Some(grid.bpm(rate())),
        confidence: estimate.confidence,
        hits,
        bars,
        bias_ms,
        drift_ms,
        verdict,
    })
}

/// Signed offset (ms) of `t` from the nearest line of `grid`.
fn signed_offset_ms(t: f64, grid: &BeatGrid) -> f64 {
    let x = (t - grid.anchor).rem_euclid(grid.period);
    let x = match x > grid.period / 2.0 {
        true => x - grid.period,
        false => x,
    };
    x / f64::from(rate()) * 1000.0
}

/// The `times` within the tolerance of a line every `beats` beats of `grid`.
fn on_grid(times: &[f64], grid: &BeatGrid, beats: f64) -> Vec<f64> {
    times
        .iter()
        .copied()
        .filter(|&t| hit_rate(&[t], grid, beats, 0.0) > 0.0)
        .collect()
}

/// Fraction of `times` within the tolerance of a line every `beats` beats of `grid`,
/// shifted by `shift` beats.
fn hit_rate(times: &[f64], grid: &BeatGrid, beats: f64, shift: f64) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    let tolerance = TOLERANCE_MS / 1000.0 * f64::from(rate());
    let span = grid.period * beats;
    let hits = times
        .iter()
        .filter(|&&t| {
            let offset = (t - grid.anchor - shift * grid.period).rem_euclid(span);
            offset.min(span - offset) <= tolerance
        })
        .count();
    hits as f64 / times.len() as f64
}
