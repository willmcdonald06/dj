//! Kick-drum onset detection from low-frequency energy flux.
//!
//! The markers come from the audio itself, so they stay right even when a beatgrid is
//! wrong; the beatmatching display draws them on the waveform.
//!
//! 1. Mono, band-limited to the kick range (high-pass 30 Hz, low-pass 150 Hz).
//! 2. Energy over a sliding [`ENERGY_WINDOW_SECONDS`] window, longer than one cycle of
//!    a kick's fundamental so it doesn't ripple with the waveform, sampled every
//!    [`HOP_SECONDS`], on a log scale.
//! 3. Flux: the rise in log energy over the loudest window in the
//!    [`REFERENCE_SECONDS`] before, so a decaying kick beating against a bass line
//!    doesn't look like a new attack.
//! 4. Peak picking: local maxima of the flux above an adaptive threshold (a multiple of
//!    the median flux around it), on a hop loud enough to matter, at least
//!    [`MIN_INTERVAL_SECONDS`] apart.
//! 5. Refinement: a linear predictor is fitted to the band-limited signal just before
//!    the attack, and the onset is placed where the signal stops following it. A steady
//!    bass line is predictable and a kick isn't, so this works whatever phase the bass
//!    is in when the kick lands (a plain envelope threshold is off by up to a quarter
//!    of a bass cycle). The result is shifted back by the filters' delay.

use dj_engine::dsp::Biquad;

/// Coarse hop for detection.
pub const HOP_SECONDS: f32 = 0.0027;
/// Length of the energy window.
pub const ENERGY_WINDOW_SECONDS: f32 = 0.025;
/// How far back the flux looks for the level to rise above.
pub const REFERENCE_SECONDS: f32 = 0.15;
/// Approximate rate of the stored band-limited signal used for refinement.
const REFINE_RATE_HZ: f32 = 6000.0;
/// Signal before an attack used to fit the predictor.
const TRAINING_SECONDS: f32 = 0.04;
/// Predictor order.
const LPC_ORDER: usize = 12;
/// The onset is where the prediction error first exceeds this multiple of its level
/// before the attack...
const RESIDUAL_NOISE_RATIO: f32 = 6.0;
/// ...and this fraction of its peak in the attack.
/// Window for the local-maximum test, each side.
const PEAK_WINDOW_SECONDS: f32 = 0.03;
/// Window for the adaptive threshold, each side.
const THRESHOLD_WINDOW_SECONDS: f32 = 0.5;
/// Flux must exceed this multiple of the local median flux...
const THRESHOLD_RATIO: f32 = 4.0;
/// ...and this absolute rise in natural-log energy (about 6.5 dB).
const MIN_FLUX: f32 = 1.5;
/// Hop energy after the onset must reach this fraction of the loud level of the
/// track (its 95th-percentile hop energy), to ignore rumbles in quiet passages.
const MIN_LEVEL: f32 = 0.05;
/// A rise in flux starts where the flux first exceeds this fraction of its peak.
const RISE_FRACTION: f32 = 0.3;
/// Of two rises closer than [`MIN_INTERVAL_SECONDS`], the later replaces the earlier
/// only if its flux is this many times larger.
const LATER_ONSET_RATIO: f32 = 2.0;
/// Kicks closer than this are one kick (faster than 400 BPM).
pub const MIN_INTERVAL_SECONDS: f32 = 0.15;
const ONSET_FRACTION: f32 = 0.2;
/// Filter delay at kick frequencies, measured with synthetic kicks (see tests).
const FILTER_DELAY_SECONDS: f32 = 0.00215;

/// Collects the band-limited signal envelope from frames pushed one at a time.
pub struct KickDetector {
    sample_rate: u32,
    filters: [Biquad; 3],
    hop: usize,
    /// Decimation factor of `signal`.
    decimation: usize,
    in_hop: usize,
    hop_energy: f32,
    in_decimation: usize,
    /// Mean square per coarse hop.
    energy: Vec<f32>,
    /// The band-limited signal, decimated to about [`REFINE_RATE_HZ`].
    signal: Vec<f32>,
}

impl KickDetector {
    pub fn new(sample_rate: u32, expected_frames: u64) -> Self {
        let sr = sample_rate as f32;
        let hop = ((HOP_SECONDS * sr).round() as usize).max(1);
        let decimation = ((sr / REFINE_RATE_HZ).round() as usize).max(1);
        Self {
            sample_rate,
            filters: [
                Biquad::highpass(sr, 30.0, std::f32::consts::FRAC_1_SQRT_2),
                Biquad::lowpass(sr, 150.0, std::f32::consts::FRAC_1_SQRT_2),
                Biquad::lowpass(sr, 150.0, std::f32::consts::FRAC_1_SQRT_2),
            ],
            hop,
            decimation,
            in_hop: 0,
            hop_energy: 0.0,
            in_decimation: 0,
            energy: Vec::with_capacity((expected_frames / hop as u64 + 1) as usize),
            signal: Vec::with_capacity((expected_frames / decimation as u64 + 1) as usize),
        }
    }

    #[inline]
    pub fn push(&mut self, left: f32, right: f32) {
        let mut x = (left + right) * 0.5;
        for filter in &mut self.filters {
            x = filter.process(x);
        }
        self.hop_energy += x * x;
        self.in_hop += 1;
        if self.in_hop == self.hop {
            self.energy.push(self.hop_energy / self.hop as f32);
            self.hop_energy = 0.0;
            self.in_hop = 0;
        }
        // Everything above ~150 Hz is gone, so plain decimation doesn't alias.
        if self.in_decimation == 0 {
            self.signal.push(x);
        }
        self.in_decimation += 1;
        if self.in_decimation == self.decimation {
            self.in_decimation = 0;
        }
    }

    /// Onset positions in frames, ascending.
    pub fn finish(self) -> Vec<u64> {
        let sr = self.sample_rate as f32;
        let hop_seconds = self.hop as f32 / sr;
        let window = ((ENERGY_WINDOW_SECONDS / hop_seconds).round() as usize).max(1);
        // Mean square over the `window` hops ending at each hop.
        let mut windowed = Vec::with_capacity(self.energy.len());
        let mut sum = 0.0f64;
        for (n, &e) in self.energy.iter().enumerate() {
            sum += e as f64;
            if n >= window {
                sum -= self.energy[n - window] as f64;
            }
            windowed.push((sum.max(0.0) / window as f64) as f32);
        }
        let log: Vec<f32> = windowed.iter().map(|&e| (e + 1e-9).ln()).collect();
        let reference = (REFERENCE_SECONDS / hop_seconds).round() as usize;
        let flux: Vec<f32> = (0..log.len())
            .map(|n| match n >= window {
                true => {
                    let to = n - window;
                    let from = to.saturating_sub(reference);
                    let before = log[from..=to].iter().copied().fold(f32::MIN, f32::max);
                    (log[n] - before).max(0.0)
                }
                false => 0.0,
            })
            .collect();

        let loud = percentile(&windowed, 0.95);
        let min_energy = loud * MIN_LEVEL;
        let peak_window = (PEAK_WINDOW_SECONDS / hop_seconds).ceil() as usize;
        let threshold_window = (THRESHOLD_WINDOW_SECONDS / hop_seconds).ceil() as usize;
        let min_gap = (MIN_INTERVAL_SECONDS / hop_seconds).ceil() as usize;
        let thresholds = local_medians(&flux, threshold_window);

        let mut onsets: Vec<(usize, f32)> = Vec::new();
        for n in 0..flux.len() {
            let f = flux[n];
            if f < MIN_FLUX || f < thresholds[n] * THRESHOLD_RATIO {
                continue;
            }
            let from = n.saturating_sub(peak_window);
            let to = (n + peak_window + 1).min(flux.len());
            if flux[from..to].iter().any(|&other| other > f) {
                continue;
            }
            let after = &windowed[n..(n + peak_window).min(windowed.len())];
            if after.iter().copied().fold(0.0, f32::max) < min_energy {
                continue;
            }
            match onsets.last_mut() {
                // Two rises close together: the first is the kick and the second is
                // usually something layered on it (a bass note), so the first wins
                // unless the second is much stronger.
                Some((last, last_flux)) if n - *last < min_gap => {
                    if f > *last_flux * LATER_ONSET_RATIO {
                        *last = n;
                        *last_flux = f;
                    }
                }
                _ => onsets.push((n, f)),
            }
        }

        let delay = (FILTER_DELAY_SECONDS * sr).round() as u64;
        onsets
            .into_iter()
            .map(|(peak, f)| {
                // The flux may keep rising after the kick (a bass note landing just
                // after it), so search from where this rise began, not from its peak.
                let mut rise = peak;
                while rise > 0 && flux[rise - 1] > f * RISE_FRACTION {
                    rise -= 1;
                }
                self.refine(rise, peak, window).saturating_sub(delay)
            })
            .collect()
    }

    /// Place an onset precisely. The attack lies within the energy window ending at
    /// hop `rise` (where the flux started rising), and before hop `peak`.
    fn refine(&self, rise: usize, peak: usize, window: usize) -> u64 {
        let sr = self.sample_rate as f32;
        let per_hop = self.hop as f32 / self.decimation as f32;
        let to_signal = |h: usize| (h as f32 * per_hop) as usize;
        let start = to_signal(rise.saturating_sub(window + 2));
        let end = to_signal(peak + 2).min(self.signal.len());
        let training = (TRAINING_SECONDS * sr / self.decimation as f32) as usize;
        let fallback = (rise.saturating_sub(window) * self.hop) as u64;
        if start >= end || start < training + LPC_ORDER {
            return fallback;
        }

        let fit = &self.signal[start - training..start];
        let coefficients = lpc(fit, LPC_ORDER);
        let residual = |i: usize| {
            let predicted: f32 = coefficients
                .iter()
                .enumerate()
                .map(|(k, a)| a * self.signal[i - 1 - k])
                .sum();
            self.signal[i] - predicted
        };

        let noise_from = start - training / 2;
        let noise = ((noise_from..start)
            .map(|i| residual(i).powi(2))
            .sum::<f32>()
            / (start - noise_from) as f32)
            .sqrt();
        let errors: Vec<f32> = (start..end).map(|i| residual(i).abs()).collect();
        let peak = errors.iter().copied().fold(0.0, f32::max);
        let mark = (noise * RESIDUAL_NOISE_RATIO).max(peak * ONSET_FRACTION);
        match errors.iter().position(|&e| e >= mark) {
            Some(first) => ((start + first) * self.decimation) as u64,
            None => fallback,
        }
    }
}

/// Linear-prediction coefficients `a` (so that `x[n] ≈ Σ a[k] x[n-1-k]`) by the
/// autocorrelation method and Levinson-Durbin recursion. All zeros for silence.
fn lpc(x: &[f32], order: usize) -> Vec<f32> {
    let r: Vec<f64> = (0..=order)
        .map(|lag| {
            x.iter()
                .zip(&x[lag.min(x.len())..])
                .map(|(a, b)| *a as f64 * *b as f64)
                .sum()
        })
        .collect();
    let mut a = vec![0.0f64; order];
    if r[0] <= 1e-12 {
        return vec![0.0; order];
    }
    // Slight lag windowing keeps the recursion well conditioned on pure tones.
    let r0 = r[0] * (1.0 + 1e-6);
    let mut error = r0;
    for i in 0..order {
        let mut acc = r[i + 1];
        for j in 0..i {
            acc -= a[j] * r[i - j];
        }
        let k = acc / error;
        let previous = a.clone();
        a[i] = k;
        for j in 0..i {
            a[j] = previous[j] - k * previous[i - 1 - j];
        }
        error *= 1.0 - k * k;
        if error <= 0.0 {
            break;
        }
    }
    a.into_iter().map(|c| c as f32).collect()
}

fn percentile(values: &[f32], fraction: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let index = ((sorted.len() - 1) as f32 * fraction) as usize;
    sorted[index]
}

/// Median of `values` in a window of `radius` each side, at a stride for speed: the
/// median is computed every `radius / 4` points and held between them.
fn local_medians(values: &[f32], radius: usize) -> Vec<f32> {
    let stride = (radius / 4).max(1);
    let mut out = vec![0.0; values.len()];
    let mut scratch = Vec::with_capacity(radius * 2 + 1);
    let mut i = 0;
    while i < values.len() {
        let from = i.saturating_sub(radius);
        let to = (i + radius + 1).min(values.len());
        scratch.clear();
        scratch.extend_from_slice(&values[from..to]);
        let mid = scratch.len() / 2;
        let (_, median, _) = scratch.select_nth_unstable_by(mid, f32::total_cmp);
        let median = *median;
        let end = (i + stride).min(values.len());
        out[i..end].fill(median);
        i = end;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const SR: u32 = 48_000;

    /// Deterministic noise in -1..1.
    pub(crate) struct Noise(u32);

    impl Noise {
        pub(crate) fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
        }
    }

    /// A synthetic kick starting at sample 0: a pitch sweep from 120 Hz to 45 Hz with
    /// an exponential decay.
    pub(crate) fn kick(t: f32) -> f32 {
        if !(0.0..=0.4).contains(&t) {
            return 0.0;
        }
        let f0 = 120.0f32;
        let f1 = 45.0f32;
        let k = 30.0f32; // sweep rate
        let phase = std::f32::consts::TAU * (f1 * t + (f0 - f1) * (1.0 - (-k * t).exp()) / k);
        phase.sin() * (-t / 0.12).exp() * 0.9
    }

    /// `seconds` of four-on-the-floor kicks at `bpm`, starting at `first` seconds.
    /// Returns the samples and the true onset frames.
    pub(crate) fn kick_track(
        bpm: f32,
        first: f32,
        seconds: f32,
        extra: impl Fn(usize) -> f32,
    ) -> (Vec<f32>, Vec<u64>) {
        let frames = (seconds * SR as f32) as usize;
        let period = 60.0 / bpm;
        let onsets: Vec<f32> = (0..)
            .map(|i| first + i as f32 * period)
            .take_while(|&t| t < seconds - 0.5)
            .collect();
        let samples = (0..frames)
            .map(|i| {
                let t = i as f32 / SR as f32;
                let k: f32 = onsets.iter().map(|&o| kick(t - o)).sum();
                k + extra(i)
            })
            .collect();
        let truth = onsets
            .iter()
            .map(|&o| (o * SR as f32).round() as u64)
            .collect();
        (samples, truth)
    }

    fn detect(samples: &[f32]) -> Vec<u64> {
        let mut detector = KickDetector::new(SR, samples.len() as u64);
        for &s in samples {
            detector.push(s, s);
        }
        detector.finish()
    }

    fn assert_matches(found: &[u64], truth: &[u64], tolerance_ms: f32) {
        assert_eq!(found.len(), truth.len(), "found {found:?}\ntruth {truth:?}");
        // `ONSET_DEBUG=1 cargo test -p dj-analysis -- --nocapture` prints every error,
        // for tuning.
        #[allow(clippy::print_stderr)]
        if std::env::var_os("ONSET_DEBUG").is_some() {
            let errors: Vec<String> = found
                .iter()
                .zip(truth)
                .map(|(f, t)| format!("{:+.2}", (*f as f32 - *t as f32) / SR as f32 * 1000.0))
                .collect();
            eprintln!("errors (ms): {}", errors.join(" "));
        }
        for (f, t) in found.iter().zip(truth) {
            let error_ms = (*f as f32 - *t as f32) / SR as f32 * 1000.0;
            assert!(
                error_ms.abs() <= tolerance_ms,
                "onset at {t} found at {f} ({error_ms:+.2} ms)"
            );
        }
    }

    #[test]
    fn clean_kicks_are_found_to_the_millisecond() {
        let (samples, truth) = kick_track(128.0, 0.25, 10.0, |_| 0.0);
        assert_matches(&detect(&samples), &truth, 0.75);
    }

    #[test]
    fn kicks_are_found_over_bass_and_hats() {
        let mut noise = Noise(12345);
        let hats: Vec<f32> = (0..SR as usize * 10).map(|_| noise.next()).collect();
        let (samples, truth) = kick_track(124.0, 0.1, 10.0, |i| {
            let t = i as f32 / SR as f32;
            // A sustained 55 Hz bass line, and an off-beat hi-hat (noise bursts) that
            // the low-pass should ignore.
            // The bass fades in: a bass entering abruptly is itself a low-frequency
            // onset.
            let bass = (std::f32::consts::TAU * 55.0 * t).sin() * 0.25 * t.min(1.0);
            let beat = 60.0 / 124.0;
            let into_offbeat = (t + beat / 2.0) % beat;
            let hat = hats[i] * 0.3 * (-into_offbeat / 0.02).exp();
            bass + hat
        });
        assert_matches(&detect(&samples), &truth, 1.0);
    }

    #[test]
    fn quiet_and_steady_signals_have_no_onsets() {
        assert!(detect(&vec![0.0; SR as usize * 3]).is_empty());
        let hum: Vec<f32> = (0..SR as usize * 3)
            .map(|i| (std::f32::consts::TAU * 60.0 * i as f32 / SR as f32).sin() * 0.5)
            .collect();
        // One onset where the hum starts, none after.
        assert!(detect(&hum).len() <= 1);
    }

    #[test]
    fn lpc_predicts_a_sine() {
        let x: Vec<f32> = (0..400).map(|i| (i as f32 * 0.05).sin()).collect();
        let a = lpc(&x, 4);
        let max_error = (4..x.len())
            .map(|n| {
                let p: f32 = (0..4).map(|k| a[k] * x[n - 1 - k]).sum();
                (x[n] - p).abs()
            })
            .fold(0.0, f32::max);
        // The autocorrelation method is biased at the window edges, so not exact.
        assert!(max_error < 0.05, "{max_error}");
        assert!(lpc(&[0.0; 50], 4).iter().all(|&c| c == 0.0));
    }

    #[test]
    fn local_median_is_the_windowed_median() {
        let values: Vec<f32> = (0..100).map(|i| (i % 10) as f32).collect();
        let medians = local_medians(&values, 20);
        assert!(
            medians.iter().all(|&m| (4.0..=5.0).contains(&m)),
            "{medians:?}"
        );
    }
}
