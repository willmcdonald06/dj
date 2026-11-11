//! Beatgrid estimation from the whole track's onset strength: the in-app analysis
//! that sync depends on.
//!
//! 1. **Onset envelope.** A short-time spectrum every [`HOP_SECONDS`]; the onset
//!    strength is the spectral flux (the summed rise in log magnitude), over the whole
//!    spectrum and over the kick range alone. Every attack counts, so tracks with
//!    sparse or syncopated kicks still have a clear pulse. A chromagram is kept per hop
//!    for the downbeat.
//! 2. **Tempo.** The envelope's autocorrelation, read at each candidate tempo, with
//!    the evidence from twice and half the period added in (a pulse at 128 BPM also
//!    repeats at 64), weighted by a broad preference for dance tempos. This picks the
//!    beat level; the period is still rough.
//! 3. **Period and phase.** The envelope's peaks are folded over the whole track at
//!    each period near the rough one; the right period stacks them into one sharp
//!    pile, and the pile's position is the phase. Kicks weigh extra, so the grid lands
//!    on the kick rather than an off-beat hi-hat or bass.
//! 4. **Line fit.** The attacks on that grid are fitted with a robust, weighted
//!    straight line (beat number against time) for the final period, at the fold's
//!    phase or the half-beat between: the mid band (kick body, snares, claps) and low
//!    band decide which is on the beat. Related tempos (2/3, 3/2, 3/4, 4/3: triplet
//!    feels) are tried too, and the grid the attacks sit on best wins.
//! 5. **Whole-number BPM** if it stays within a few milliseconds over the track.
//! 6. **Kick snap.** The grid moves onto the median attack of the kick markers on it
//!    (accurate to a millisecond or two); only its phase, so sparse kicks can't tilt it.
//! 7. **Downbeat.** Of the four ways to group beats into bars, the one whose first
//!    beats bring the most harmonic change (bar before against bar after) wins.

use std::sync::Arc;

use dj_engine::BeatGrid;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::GridEstimate;

/// Spectrum hop, about 5 ms.
pub const HOP_SECONDS: f64 = 0.0053;
/// Analysis window, about 21 ms.
const WINDOW_SECONDS: f64 = 0.021;
/// The kick range for the low-band onset envelope.
const LOW_BAND_HZ: f64 = 150.0;
/// Above this is the high band (hi-hats, cymbals); between is the mid band (kick
/// body and click, snares, claps, most instruments' attacks).
const HIGH_BAND_HZ: f64 = 4000.0;
/// Chroma from this range (bass and melody, below most cymbal noise).
const CHROMA_HZ: (f64, f64) = (60.0, 2000.0);
/// Bass pitch content (for bar changes) from below this.
const BASS_CHROMA_HZ: f64 = 250.0;
/// The envelope's slowly varying level is removed over this window.
const DETREND_SECONDS: f64 = 0.4;
/// Tempos considered.
const BPM_RANGE: (f64, f64) = (60.0, 200.0);
/// Preferred tempo (centre of the prior) and the prior's width in octaves.
const PREFERRED_BPM: f64 = 125.0;
const PRIOR_OCTAVES: f64 = 0.7;
/// Period search around the rough tempo, as a fraction.
const FOLD_RANGE: f64 = 0.015;
/// Phase bins for folding.
const FOLD_BINS: usize = 96;
/// Kick-range onsets weigh this much more than the rest when folding.
const LOW_WEIGHT: f32 = 2.0;
/// Kicks within this fraction of a beat of the folded grid are used for the fit.
const KICK_CAPTURE: f64 = 0.12;
/// Fewer on-grid kicks than this, and the fold's grid is kept as it is.
const MIN_FIT_KICKS: usize = 24;
/// A whole-number BPM is used if it drifts from the fit by at most this over the
/// track (seconds).
const ROUND_BPM_MAX_DRIFT: f64 = 0.008;
/// Kicks within this fraction of a beat of the grid set its phase...
const KICK_SNAP: f64 = 0.04;
/// ...if there are at least this many.
const MIN_SNAP_KICKS: usize = 16;
/// Tempos related to the rough one by a triplet feel, also tried.
const RELATED_TEMPOS: [f64; 4] = [2.0 / 3.0, 1.5, 0.75, 4.0 / 3.0];
/// A related tempo replaces the rough one if this much more onset weight sits on its
/// beats.
const RELATED_TEMPO_MARGIN: f64 = 1.15;
/// How much the low and mid bands count when choosing between the beat and the
/// half-beat between (log support ratios).
const PHASE_WEIGHTS: (f64, f64) = (1.0, 2.0);
/// Onsets within this fraction of a beat of a grid line count as on the grid (for
/// the confidence).
const ON_GRID: f64 = 0.07;

/// Streams frames into an onset envelope and chromagram.
pub struct OnsetAnalyser {
    sample_rate: u32,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    hop: usize,
    /// The last window's worth of mono samples, oldest first.
    ring: Vec<f32>,
    filled: usize,
    since_hop: usize,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    previous: Vec<f32>,
    low_bins: usize,
    high_bins: usize,
    bass_bins: usize,
    chroma_map: Vec<Option<u8>>,
    envelope: Vec<f32>,
    low: Vec<f32>,
    mid: Vec<f32>,
    high: Vec<f32>,
    chroma: Vec<[f32; 12]>,
    bass: Vec<[f32; 12]>,
}

impl OnsetAnalyser {
    pub fn new(sample_rate: u32, expected_frames: u64) -> Self {
        let sr = f64::from(sample_rate.max(1));
        let size = ((WINDOW_SECONDS * sr) as usize).next_power_of_two().max(64);
        let hop = ((HOP_SECONDS * sr).round() as usize).max(1);
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(size);
        let window = (0..size)
            .map(|i| {
                let x = std::f32::consts::TAU * i as f32 / size as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        let bins = size / 2 + 1;
        let bin_hz = sr / size as f64;
        let low_bins = ((LOW_BAND_HZ / bin_hz).ceil() as usize).clamp(2, bins);
        let high_bins = ((HIGH_BAND_HZ / bin_hz).ceil() as usize).clamp(low_bins, bins);
        let bass_bins = ((BASS_CHROMA_HZ / bin_hz).ceil() as usize).clamp(1, bins);
        let chroma_map = (0..bins)
            .map(|b| {
                let hz = b as f64 * bin_hz;
                match (CHROMA_HZ.0..CHROMA_HZ.1).contains(&hz) {
                    true => {
                        let midi = 69.0 + 12.0 * (hz / 440.0).log2();
                        Some((midi.round() as i64).rem_euclid(12) as u8)
                    }
                    false => None,
                }
            })
            .collect();
        let hops = (expected_frames as usize / hop) + 1;
        Self {
            sample_rate,
            input: fft.make_input_vec(),
            output: fft.make_output_vec(),
            fft,
            window,
            hop,
            ring: vec![0.0; size],
            filled: 0,
            since_hop: 0,
            previous: vec![0.0; bins],
            low_bins,
            high_bins,
            bass_bins,
            chroma_map,
            envelope: Vec::with_capacity(hops),
            low: Vec::with_capacity(hops),
            mid: Vec::with_capacity(hops),
            high: Vec::with_capacity(hops),
            chroma: Vec::with_capacity(hops),
            bass: Vec::with_capacity(hops),
        }
    }

    pub fn push(&mut self, left: f32, right: f32) {
        let size = self.ring.len();
        self.ring[self.filled % size] = (left + right) * 0.5;
        self.filled += 1;
        self.since_hop += 1;
        if self.since_hop == self.hop {
            self.since_hop = 0;
            self.analyse_hop();
        }
    }

    fn analyse_hop(&mut self) {
        let size = self.ring.len();
        // The window ends at the newest sample.
        let start = self.filled % size;
        for (i, (slot, w)) in self.input.iter_mut().zip(&self.window).enumerate() {
            *slot = self.ring[(start + i) % size] * w;
        }
        if self.fft.process(&mut self.input, &mut self.output).is_err() {
            return;
        }
        let (mut all, mut low, mut mid, mut high) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut chroma = [0.0f32; 12];
        let mut bass = [0.0f32; 12];
        for (b, (c, prev)) in self.output.iter().zip(self.previous.iter_mut()).enumerate() {
            let magnitude = c.norm();
            let compressed = (1.0 + 10.0 * magnitude).ln();
            let rise = (compressed - *prev).max(0.0);
            *prev = compressed;
            all += rise;
            match () {
                _ if b < self.low_bins => low += rise,
                _ if b < self.high_bins => mid += rise,
                _ => high += rise,
            }
            if let Some(pc) = self.chroma_map[b] {
                chroma[pc as usize] += magnitude;
                if b < self.bass_bins {
                    bass[pc as usize] += magnitude;
                }
            }
        }
        self.envelope.push(all);
        self.low.push(low);
        self.mid.push(mid);
        self.high.push(high);
        self.chroma.push(chroma);
        self.bass.push(bass);
    }

    pub fn finish(self) -> Onsets {
        let hop_frames = self.hop as f64;
        let detrend =
            ((DETREND_SECONDS * f64::from(self.sample_rate) / hop_frames) as usize).max(1);
        // The envelope at hop i describes the window ending there, and an attack
        // shows up as soon as it enters the window: at its leading edge.
        let delay = 0.0;
        Onsets {
            hop_frames,
            delay_frames: delay,
            all: detrended(&self.envelope, detrend),
            low: detrended(&self.low, detrend),
            mid: detrended(&self.mid, detrend),
            high: detrended(&self.high, detrend),
            chroma: self.chroma,
            bass: self.bass,
        }
    }
}

/// Onset strength and chroma per hop.
pub struct Onsets {
    hop_frames: f64,
    /// Frames from a hop's position (hop index × hop) back to its window's centre.
    delay_frames: f64,
    pub all: Vec<f32>,
    pub low: Vec<f32>,
    pub mid: Vec<f32>,
    pub high: Vec<f32>,
    chroma: Vec<[f32; 12]>,
    bass: Vec<[f32; 12]>,
}

impl Onsets {
    /// How different the bass's pitch content is in hops `at..to` from `from..at`
    /// (0 = the same, 1 = nothing shared).
    fn bass_change(&self, from: usize, at: usize, to: usize) -> f64 {
        let n = self.bass.len();
        let mean = |a: usize, b: usize| -> Option<[f32; 12]> {
            let (a, b) = (a.min(n), b.min(n));
            if b <= a {
                return None;
            }
            let mut sum = [0.0f32; 12];
            for c in &self.bass[a..b] {
                for (s, v) in sum.iter_mut().zip(c) {
                    *s += v;
                }
            }
            let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
            (norm > 0.0).then(|| sum.map(|v| v / norm))
        };
        match (mean(from, at), mean(at, to)) {
            (Some(a), Some(b)) => {
                f64::from(1.0 - a.iter().zip(&b).map(|(x, y)| x * y).sum::<f32>())
            }
            _ => 0.0,
        }
    }

    /// The full-band envelope plus `low_weight` times the kick-range one, each scaled
    /// to an average of 1 first: a hi-hat's rise spreads over hundreds of frequency
    /// bins and a kick's over a few, so unscaled, hats would drown kicks.
    fn combined(&self, low_weight: f32) -> Vec<f32> {
        let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
        let (all, low) = (
            mean(&self.all).max(f32::MIN_POSITIVE),
            mean(&self.low).max(f32::MIN_POSITIVE),
        );
        self.all
            .iter()
            .zip(&self.low)
            .map(|(a, l)| a / all + low_weight * l / low)
            .collect()
    }

    fn frame_of(&self, hop: f64) -> f64 {
        hop * self.hop_frames - self.delay_frames
    }
}

/// `values` minus their moving average, floored at zero.
fn detrended(values: &[f32], window: usize) -> Vec<f32> {
    let mut sums = Vec::with_capacity(values.len() + 1);
    sums.push(0.0f64);
    for v in values {
        sums.push(sums[sums.len() - 1] + f64::from(*v));
    }
    let half = window / 2;
    (0..values.len())
        .map(|i| {
            let (a, b) = (i.saturating_sub(half), (i + half + 1).min(values.len()));
            let mean = (sums[b] - sums[a]) / (b - a) as f64;
            (values[i] - mean as f32).max(0.0)
        })
        .collect()
}

/// A peak of the onset envelope: when (frames) and how strong.
#[derive(Debug, Clone, Copy)]
struct Peak {
    frame: f64,
    weight: f32,
}

/// The beatgrid for a track, from its onsets, set on the attacks of its kick markers.
pub fn estimate(onsets: &Onsets, kicks: &[u64], sample_rate: u32) -> Option<GridEstimate> {
    let sr = f64::from(sample_rate);
    if onsets.all.len() < 200 {
        return None;
    }
    let rough_bpm = tempo(onsets, sr)?;
    let peaks = peaks(onsets);
    if peaks.len() < 16 {
        return None;
    }
    // A triplet feel can make a tempo 2/3, 3/2, 3/4 or 4/3 of the beat repeat more in
    // the autocorrelation; of those, keep the grid the attacks sit on clearly best.
    // (The on-grid window is a fraction of a beat, so this is fair between tempos.)
    let grid_at = |bpm: f64| -> Option<(BeatGrid, f64)> {
        let (period, anchor) = beat_grid(onsets, &peaks, 60.0 * sr / bpm)?;
        let grid = BeatGrid { anchor, period };
        Some((grid, on_grid(&peaks, &grid)))
    };
    let (mut grid, mut fit) = grid_at(rough_bpm)?;
    for ratio in RELATED_TEMPOS {
        let bpm = rough_bpm * ratio;
        if !(BPM_RANGE.0..=BPM_RANGE.1).contains(&bpm) {
            continue;
        }
        if let Some((other, other_fit)) = grid_at(bpm) {
            if other_fit > fit * RELATED_TEMPO_MARGIN {
                (grid, fit) = (other, other_fit);
            }
        }
    }
    let BeatGrid {
        mut period,
        mut anchor,
    } = grid;
    let span = onsets.frame_of(onsets.all.len() as f64);
    let round_period = 60.0 * sr / (60.0 * sr / period).round();
    if (round_period - period).abs() * span / period <= ROUND_BPM_MAX_DRIFT * sr {
        // Keep the grid where it was in the middle of the track.
        let middle = span / 2.0;
        let beat = ((middle - anchor) / period).round();
        anchor += beat * (period - round_period);
        period = round_period;
    }
    let anchor = downbeat(onsets, period, anchor) + kick_offset(kicks, period, anchor);
    let grid = BeatGrid {
        anchor: anchor.rem_euclid(period * 4.0),
        period,
    };
    Some(GridEstimate {
        grid,
        confidence: on_grid(&peaks, &grid),
    })
}

/// How far (frames) the kicks on the grid sit after its beats, by their median: the
/// onset envelope marks an attack within a few ms, and the kick markers to a
/// millisecond or two. Zero if too few kicks are on the grid to say.
fn kick_offset(kicks: &[u64], period: f64, anchor: f64) -> f64 {
    let window = KICK_SNAP * period;
    let mut offsets: Vec<f64> = kicks
        .iter()
        .map(|&k| {
            let x = (k as f64 - anchor).rem_euclid(period);
            match x > period / 2.0 {
                true => x - period,
                false => x,
            }
        })
        .filter(|o| o.abs() <= window)
        .collect();
    if offsets.len() < MIN_SNAP_KICKS {
        return 0.0;
    }
    offsets.sort_by(f64::total_cmp);
    offsets[offsets.len() / 2]
}

/// Period and phase (frames) near `rough_period`: the sharpest fold of the onset
/// peaks, then a straight-line fit to the attacks on it, at the fold's phase or the
/// half-beat between (an off-beat bass line or hi-hat can out-pile the kick).
fn beat_grid(onsets: &Onsets, peaks: &[Peak], rough_period: f64) -> Option<(f64, f64)> {
    let (period, anchor) = fold(peaks, rough_period)?;
    let points: Vec<(f64, f64)> = peaks
        .iter()
        .map(|p| (p.frame, f64::from(p.weight)))
        .collect();
    let fits = [0.0, 0.5].map(|shift| fit_points(&points, period, anchor + shift * period));
    let chosen = match fits {
        [Some(a), Some(b)] => Some(on_the_beat(a, b, onsets)),
        [a, b] => a.or(b),
    };
    Some(chosen.map_or((period, anchor), |fit| (fit.period, fit.anchor)))
}

/// The tempo (BPM) whose period the envelope repeats at most, with octave evidence
/// and a preference for dance tempos.
fn tempo(onsets: &Onsets, sr: f64) -> Option<f64> {
    let acf = autocorrelation(&onsets.combined(1.0));
    let hop_seconds = onsets.hop_frames / sr;
    let at = |bpm: f64| -> f64 {
        let lag = 60.0 / bpm / hop_seconds;
        let i = lag.floor() as usize;
        let f = lag - lag.floor();
        match (acf.get(i), acf.get(i + 1)) {
            (Some(a), Some(b)) => a * (1.0 - f) + b * f,
            _ => 0.0,
        }
    };
    let mut best: Option<(f64, f64)> = None;
    let mut bpm = BPM_RANGE.0;
    while bpm <= BPM_RANGE.1 {
        // The beat's own period, plus the bar-level repetition at two beats and the
        // subdivision at half a beat, which a true beat period also has.
        let evidence = at(bpm) + 0.5 * at(bpm / 2.0) + 0.25 * at(bpm * 2.0);
        let prior = (-0.5 * ((bpm / PREFERRED_BPM).log2() / PRIOR_OCTAVES).powi(2)).exp();
        let score = evidence * prior;
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((bpm, score));
        }
        bpm += 0.05;
    }
    best.filter(|(_, s)| *s > 0.0).map(|(bpm, _)| bpm)
}

/// Autocorrelation of `values` (mean removed), normalised so lag 0 is 1 and divided
/// by the overlap, via FFT.
fn autocorrelation(values: &[f32]) -> Vec<f64> {
    let n = values.len();
    let size = (2 * n).next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);
    let mean = values.iter().map(|&v| f64::from(v)).sum::<f64>() / n.max(1) as f64;
    let mut input = forward.make_input_vec();
    for (slot, v) in input.iter_mut().zip(values) {
        *slot = f64::from(*v) - mean;
    }
    let mut spectrum = forward.make_output_vec();
    if forward.process(&mut input, &mut spectrum).is_err() {
        return Vec::new();
    }
    for c in spectrum.iter_mut() {
        *c = Complex::new(c.norm_sqr(), 0.0);
    }
    let mut output = inverse.make_output_vec();
    if inverse.process(&mut spectrum, &mut output).is_err() {
        return Vec::new();
    }
    let zero = output[0].max(f64::MIN_POSITIVE);
    (0..n)
        .map(|lag| output[lag] / zero * n as f64 / (n - lag) as f64)
        .collect()
}

/// Local maxima of the envelope (kick-range ones weighted extra), at sub-hop
/// precision.
fn peaks(onsets: &Onsets) -> Vec<Peak> {
    let combined = onsets.combined(LOW_WEIGHT);
    let mean = combined.iter().sum::<f32>() / combined.len().max(1) as f32;
    let mut found = Vec::new();
    for i in 2..combined.len().saturating_sub(2) {
        let v = combined[i];
        if v <= mean || combined[i - 2..=i + 2].iter().any(|&w| w > v) {
            continue;
        }
        // Parabolic interpolation for the peak's position between hops.
        let (a, c) = (combined[i - 1], combined[i + 1]);
        let denominator = a - 2.0 * v + c;
        let shift = match denominator.abs() > f32::EPSILON {
            true => (0.5 * (a - c) / denominator).clamp(-0.5, 0.5),
            false => 0.0,
        };
        found.push(Peak {
            frame: onsets.frame_of(i as f64 + f64::from(shift)),
            weight: v - mean,
        });
    }
    found
}

/// Fold the peaks at periods around `rough_period`; returns the period whose fold is
/// sharpest and the phase (frames) of its pile.
fn fold(peaks: &[Peak], rough_period: f64) -> Option<(f64, f64)> {
    let span = peaks.last()?.frame - peaks.first()?.frame;
    let beats = (span / rough_period).max(1.0);
    // Steps small enough that the last peak moves by under half a bin.
    let step = rough_period / FOLD_BINS as f64 / beats / 2.0;
    let total: f32 = peaks.iter().map(|p| p.weight).sum();
    let score = |period: f64| -> (f64, f64) {
        let mut bins = [0.0f32; FOLD_BINS];
        for p in peaks {
            let position = (p.frame / period).rem_euclid(1.0) * FOLD_BINS as f64;
            let i = position as usize % FOLD_BINS;
            let f = (position - position.floor()) as f32;
            bins[i] += p.weight * (1.0 - f);
            bins[(i + 1) % FOLD_BINS] += p.weight * f;
        }
        // Smooth over three bins, then take the pile.
        let smooth: Vec<f32> = (0..FOLD_BINS)
            .map(|i| {
                bins[(i + FOLD_BINS - 1) % FOLD_BINS] * 0.5
                    + bins[i]
                    + bins[(i + 1) % FOLD_BINS] * 0.5
            })
            .collect();
        let (top, height) = smooth
            .iter()
            .copied()
            .enumerate()
            .fold((0, 0.0f32), |best, (i, v)| match v > best.1 {
                true => (i, v),
                false => best,
            });
        let (a, c) = (
            smooth[(top + FOLD_BINS - 1) % FOLD_BINS],
            smooth[(top + 1) % FOLD_BINS],
        );
        let denominator = a - 2.0 * height + c;
        let shift = match denominator.abs() > f32::EPSILON {
            true => (0.5 * (a - c) / denominator).clamp(-0.5, 0.5),
            false => 0.0,
        };
        let phase = (top as f64 + f64::from(shift)) / FOLD_BINS as f64 * period;
        (f64::from(height / total.max(f32::MIN_POSITIVE)), phase)
    };
    let mut best = (rough_period, 0.0, 0.0);
    let mut period = rough_period * (1.0 - FOLD_RANGE);
    while period <= rough_period * (1.0 + FOLD_RANGE) {
        let (s, phase) = score(period);
        if s > best.1 {
            best = (period, s, phase);
        }
        period += step;
    }
    Some((best.0, best.2))
}

/// A grid fitted to onsets.
struct Fit {
    period: f64,
    anchor: f64,
}

/// Refine the grid with the onsets on it (`(frame, weight)`): a weighted straight-line
/// fit of onset time against beat number, dropping outliers. `None` if too few onsets
/// sit on the grid.
fn fit_points(points: &[(f64, f64)], period: f64, anchor: f64) -> Option<Fit> {
    let mut on: Vec<(f64, f64, f64)> = points
        .iter()
        .filter_map(|&(t, w)| {
            let beat = ((t - anchor) / period).round();
            let off = (t - anchor - beat * period) / period;
            (off.abs() <= KICK_CAPTURE).then_some((beat, t, w))
        })
        .collect();
    for _ in 0..4 {
        if on.len() < MIN_FIT_KICKS {
            return None;
        }
        let (slope, intercept) = line(&on)?;
        let mut residuals: Vec<f64> = on
            .iter()
            .map(|(b, t, _)| (t - (intercept + slope * b)).abs())
            .collect();
        residuals.sort_by(f64::total_cmp);
        let limit = (residuals[residuals.len() / 2] * 3.0).max(period * 0.01);
        on.retain(|(b, t, _)| (t - (intercept + slope * b)).abs() <= limit);
    }
    if on.len() < MIN_FIT_KICKS {
        return None;
    }
    let (slope, intercept) = line(&on)?;
    // A fit that moved the tempo far from the fold's is fitting something else.
    match (slope / period - 1.0).abs() < FOLD_RANGE / 3.0 {
        true => Some(Fit {
            period: slope,
            anchor: intercept,
        }),
        false => None,
    }
}

/// For tuning: the two candidate grids half a beat apart, and each band's support
/// for each (`[kicks, low, mid, high]`, each `[a, b]`).
pub fn phase_candidates(
    onsets: &Onsets,
    kicks: &[u64],
    sample_rate: u32,
) -> Option<([BeatGrid; 2], [[f64; 2]; 4])> {
    let sr = f64::from(sample_rate);
    let rough_bpm = tempo(onsets, sr)?;
    let peaks = peaks(onsets);
    let (period, anchor) = fold(&peaks, 60.0 * sr / rough_bpm)?;
    let points: Vec<(f64, f64)> = peaks
        .iter()
        .map(|p| (p.frame, f64::from(p.weight)))
        .collect();
    let [Some(a), Some(b)] =
        [0.0, 0.5].map(|shift| fit_points(&points, period, anchor + shift * period))
    else {
        return None;
    };
    let votes = |frames: &mut dyn Iterator<Item = (f64, f64)>| -> [f64; 2] {
        let list: Vec<(f64, f64)> = frames.collect();
        [&a, &b].map(|fit| support(fit, &list))
    };
    let band = |v: &[f32]| {
        votes(
            &mut v
                .iter()
                .enumerate()
                .map(|(i, &w)| (onsets.frame_of(i as f64), f64::from(w))),
        )
    };
    let grids = [&a, &b].map(|f| BeatGrid {
        anchor: f.anchor,
        period: f.period,
    });
    Some((
        grids,
        [
            votes(&mut kicks.iter().map(|&k| (k as f64, 1.0))),
            band(&onsets.low),
            band(&onsets.mid),
            band(&onsets.high),
        ],
    ))
}

/// Onset weight within [`ON_GRID`] of the beats of `fit`.
fn support(fit: &Fit, frames: &[(f64, f64)]) -> f64 {
    frames
        .iter()
        .filter(|(t, _)| {
            let phase = ((t - fit.anchor) / fit.period).rem_euclid(1.0);
            phase.min(1.0 - phase) <= ON_GRID
        })
        .map(|(_, w)| w)
        .sum()
}

/// Of two grids half a beat apart, the one on the beat. The mid band decides most
/// (kick body and click, snares and claps land on beats; off-beat hi-hats are above
/// it and off-beat bass lines below), helped by the low band (the kick's weight).
/// Measured on real music against a neural beat tracker, this picks the right phase
/// where kicks alone, the low band or all onsets often don't.
fn on_the_beat(a: Fit, b: Fit, onsets: &Onsets) -> Fit {
    let ratio = |band: &[f32]| -> f64 {
        let frames: Vec<(f64, f64)> = band
            .iter()
            .enumerate()
            .map(|(i, &w)| (onsets.frame_of(i as f64), f64::from(w)))
            .collect();
        let (x, y) = (support(&a, &frames), support(&b, &frames));
        ((x + 1e-3) / (y + 1e-3)).ln()
    };
    let vote = PHASE_WEIGHTS.0 * ratio(&onsets.low) + PHASE_WEIGHTS.1 * ratio(&onsets.mid);
    match vote >= 0.0 {
        true => a,
        false => b,
    }
}

/// Weighted least-squares line through `(x, y, weight)` points: (slope, intercept).
fn line(points: &[(f64, f64, f64)]) -> Option<(f64, f64)> {
    let n: f64 = points.iter().map(|p| p.2).sum();
    if n <= 0.0 {
        return None;
    }
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), (x, y, w)| (a + w * x, b + w * y));
    let (mx, my) = (sx / n, sy / n);
    let (mut sxx, mut sxy) = (0.0, 0.0);
    for (x, y, w) in points {
        sxx += w * (x - mx) * (x - mx);
        sxy += w * (x - mx) * (y - my);
    }
    (sxx > 0.0).then(|| {
        let slope = sxy / sxx;
        (slope, my - slope * mx)
    })
}

/// The downbeat feature used: harmonic change from the bar before to the bar after.
const BAR_CHANGE: usize = 1;
/// Number of downbeat features; see [`downbeat_features`].
pub const DOWNBEAT_FEATURES: usize = 9;

/// For each of the four beats of the bar (counting from `grid.anchor`), how strongly
/// each feature marks it as the first beat, each normalised to sum to 1 over the four:
/// harmonic change over two beats and over a bar; low, mid and high-band attack
/// strength; the rise in loudness into the next bar; and the change in the bass's
/// pitch content.
pub fn downbeat_features(onsets: &Onsets, grid: &BeatGrid) -> [[f64; DOWNBEAT_FEATURES]; 4] {
    let (period, anchor) = (grid.period, grid.anchor);
    let hops = onsets.chroma.len();
    let hop_of = |frame: f64| {
        ((frame + onsets.delay_frames) / onsets.hop_frames)
            .round()
            .max(0.0) as usize
    };
    let range = |from: f64, to: f64| (hop_of(from).min(hops), hop_of(to).min(hops));
    let mean_chroma = |from: f64, to: f64| -> Option<[f32; 12]> {
        let (a, b) = range(from, to);
        if b <= a {
            return None;
        }
        let mut sum = [0.0f32; 12];
        for c in &onsets.chroma[a..b] {
            for (s, v) in sum.iter_mut().zip(c) {
                *s += v;
            }
        }
        let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
        (norm > 0.0).then(|| sum.map(|v| v / norm))
    };
    let change = |from: f64, at: f64, to: f64| -> f64 {
        match (mean_chroma(from, at), mean_chroma(at, to)) {
            (Some(a), Some(b)) => {
                f64::from(1.0 - a.iter().zip(&b).map(|(x, y)| x * y).sum::<f32>())
            }
            _ => 0.0,
        }
    };
    let energy = |from: f64, to: f64| -> f64 {
        let (a, b) = range(from, to);
        onsets.all[a..b.max(a)]
            .iter()
            .map(|&v| f64::from(v))
            .sum::<f64>()
            / (b.max(a + 1) - a) as f64
    };
    let attack = |band: &[f32], at: f64| -> f64 {
        let h = hop_of(at);
        let window = &band[h.saturating_sub(2).min(band.len())..(h + 3).min(band.len())];
        f64::from(window.iter().copied().fold(0.0, f32::max))
    };
    let first = (-anchor / period).ceil() as i64;
    let last = ((onsets.frame_of(hops as f64) - anchor) / period).floor() as i64;
    let mut features = [[0.0f64; DOWNBEAT_FEATURES]; 4];
    for beat in first..=last {
        let at = anchor + beat as f64 * period;
        let slot = beat.rem_euclid(4) as usize;
        let f = &mut features[slot];
        f[0] += change(at - 2.0 * period, at, at + 2.0 * period);
        f[1] += change(at - 4.0 * period, at, at + 4.0 * period);
        f[2] += attack(&onsets.low, at);
        f[3] += attack(&onsets.mid, at);
        f[4] += attack(&onsets.high, at);
        let (before, after) = (energy(at - 4.0 * period, at), energy(at, at + 4.0 * period));
        f[5] += (after - before).max(0.0);
        f[6] += onsets.bass_change(hop_of(at - period), hop_of(at), hop_of(at + period));
        f[7] += change(at - 8.0 * period, at, at + 8.0 * period);
        f[8] += change(at - 16.0 * period, at, at + 16.0 * period);
    }
    for k in 0..DOWNBEAT_FEATURES {
        let total: f64 = features.iter().map(|f| f[k]).sum();
        for f in features.iter_mut() {
            f[k] /= total.max(f64::MIN_POSITIVE);
        }
    }
    features
}

/// Move `anchor` to the beat that starts bars: the one across which the harmony
/// changes most from the bar before to the bar after. (Measured against a neural
/// tracker's downbeats, this alone beats adding bass, attack strength or loudness
/// cues; see [`downbeat_features`].)
fn downbeat(onsets: &Onsets, period: f64, anchor: f64) -> f64 {
    let features = downbeat_features(onsets, &BeatGrid { anchor, period });
    let best = (0..4)
        .max_by(|&a, &b| features[a][BAR_CHANGE].total_cmp(&features[b][BAR_CHANGE]))
        .unwrap_or(0);
    anchor + best as f64 * period
}

/// The share of onset weight within [`ON_GRID`] of a beat: 0..1.
fn on_grid(peaks: &[Peak], grid: &BeatGrid) -> f64 {
    let total: f64 = peaks.iter().map(|p| f64::from(p.weight)).sum();
    let on: f64 = peaks
        .iter()
        .filter(|p| {
            let phase = grid.beat_at(p.frame).rem_euclid(1.0);
            phase.min(1.0 - phase) <= ON_GRID
        })
        .map(|p| f64::from(p.weight))
        .sum();
    on / total.max(f64::MIN_POSITIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 44_100;

    /// A drum loop: kick on every beat, snare (noise) on 2 and 4, hats on the
    /// off-beats, a chord change every bar, starting at `offset` seconds.
    fn drums(bpm: f64, seconds: f64, offset: f64) -> Vec<(f32, f32)> {
        let sr = f64::from(SR);
        let period = 60.0 / bpm;
        let mut noise = 1u32;
        let chords = [
            [261.6, 329.6, 392.0],
            [220.0, 261.6, 329.6],
            [174.6, 220.0, 261.6],
            [196.0, 246.9, 293.7],
        ];
        (0..(seconds * sr) as usize)
            .map(|i| {
                let t = i as f64 / sr - offset;
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (noise >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                if t < 0.0 {
                    return (0.0, 0.0);
                }
                let beat = (t / period).floor();
                let since = (t - beat * period) as f32;
                let kick = (std::f32::consts::TAU * 55.0 * since).sin() * (-since * 25.0).exp();
                let snare = match beat as i64 % 2 == 1 {
                    true => white * 0.5 * (-since * 30.0).exp(),
                    false => 0.0,
                };
                let off = (t - (beat + 0.5) * period) as f32;
                let hat = match off >= 0.0 {
                    true => white * 0.2 * (-off * 120.0).exp(),
                    false => 0.0,
                };
                let bar = (beat / 4.0).floor() as usize % 4;
                let pad: f32 = chords[bar]
                    .iter()
                    .map(|f| (std::f64::consts::TAU * f * t).sin() as f32 * 0.08)
                    .sum();
                let s = kick * 0.9 + snare + hat + pad;
                (s, s)
            })
            .collect()
    }

    fn grid_of(frames: &[(f32, f32)]) -> GridEstimate {
        let analysis = crate::analyze_frames(frames.iter().copied(), SR, frames.len() as u64);
        analysis.grid.expect("a grid")
    }

    fn check(bpm: f64, offset: f64) {
        let estimate = grid_of(&drums(bpm, 60.0, offset));
        let found = estimate.grid.bpm(SR);
        assert!((found - bpm).abs() < 0.02, "bpm {found}, expected {bpm}");
        // Bars start on the first kick (bar 0's chord).
        let bar = estimate.grid.period * 4.0;
        let error = (estimate.grid.anchor - offset * f64::from(SR)).rem_euclid(bar);
        let signed = match error > bar / 2.0 {
            true => error - bar,
            false => error,
        };
        let error_ms = signed.abs() / f64::from(SR) * 1000.0;
        assert!(
            error_ms < 2.0,
            "downbeat off by {error_ms:.1} ms at {bpm} BPM"
        );
        assert!(
            estimate.confidence > 0.5,
            "confidence {}",
            estimate.confidence
        );
    }

    #[test]
    fn finds_tempo_phase_and_downbeat_of_a_drum_loop() {
        check(128.0, 0.3);
        check(123.4, 1.1);
        check(96.0, 0.05);
        check(150.0, 0.7);
    }

    #[test]
    fn silence_has_no_grid() {
        let silence = vec![(0.0, 0.0); SR as usize * 10];
        let analysis = crate::analyze_frames(silence.iter().copied(), SR, silence.len() as u64);
        assert!(analysis.grid.is_none_or(|g| g.confidence < 0.3));
    }
}
