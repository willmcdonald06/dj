//! Musical key detection, and key notations (Open Key, Camelot, musical).
//!
//! The in-app fallback until the offline analyzer (Milestone 6) supplies keys:
//!
//! 1. Short-time spectra (Hann window, [`FFT_SIZE`] at 48 kHz, half overlap) of the
//!    mono signal.
//! 2. Each frame's energy between A2 and C7 is folded onto the 12 pitch classes (A4 =
//!    440 Hz), and summed over the whole track into a chroma profile. Starting at A2
//!    keeps kick drums from colouring the low pitch classes.
//! 3. The profile is correlated with the 24 key profiles (12 tonics, major and minor)
//!    from Temperley's corpus study; the best match is the key. These did better on
//!    pop and dance music than the classic Krumhansl–Kessler ratings. The commonest
//!    mistake left is a minor key reported as its relative major (or the other way
//!    round), which still mixes in key.

use std::sync::Arc;

use realfft::{RealFftPlanner, RealToComplex};

/// FFT length: about 0.34 s at 48 kHz, ~2.9 Hz bins, fine enough to tell semitones
/// apart from C2 up.
pub const FFT_SIZE: usize = 16_384;
const HOP: usize = FFT_SIZE / 2;
/// Pitch range folded into the chroma profile.
const LOWEST_HZ: f32 = 110.0; // A2
const HIGHEST_HZ: f32 = 2093.0; // C7

/// Temperley's (Kostka–Payne corpus) key profiles, tonic first.
const MAJOR_PROFILE: [f32; 12] = [
    0.748, 0.060, 0.488, 0.082, 0.670, 0.460, 0.096, 0.715, 0.104, 0.366, 0.057, 0.400,
];
const MINOR_PROFILE: [f32; 12] = [
    0.712, 0.084, 0.474, 0.618, 0.049, 0.460, 0.105, 0.747, 0.404, 0.067, 0.133, 0.330,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Major,
    Minor,
}

/// A key: tonic pitch class (0 = C, 1 = C♯/D♭, … 11 = B) and mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub tonic: u8,
    pub mode: Mode,
}

/// How keys are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyNotation {
    /// Open Key (Traktor): 1d–12d major, 1m–12m minor; C major is 1d.
    #[default]
    OpenKey,
    /// Camelot (Mixed In Key): 1B–12B major, 1A–12A minor; C major is 8B.
    Camelot,
    /// Note names: C, F♯m, A♭ …
    Musical,
}

const NAMES: [&str; 12] = [
    "C", "D♭", "D", "E♭", "E", "F", "F♯", "G", "A♭", "A", "B♭", "B",
];
/// Minor keys are conventionally spelled with sharps here.
const MINOR_NAMES: [&str; 12] = [
    "C", "C♯", "D", "E♭", "E", "F", "F♯", "G", "G♯", "A", "B♭", "B",
];

impl Key {
    pub fn new(tonic: u8, mode: Mode) -> Self {
        Self {
            tonic: tonic % 12,
            mode,
        }
    }

    /// Position on the circle of fifths, 1–12, as Open Key numbers it: C major and
    /// A minor are 1, a fifth up is 2, and so on.
    pub fn wheel(&self) -> u8 {
        // Relative major's tonic, then its number of fifths from C.
        let major = match self.mode {
            Mode::Major => self.tonic,
            Mode::Minor => (self.tonic + 3) % 12,
        };
        (major * 7 % 12) + 1
    }

    /// The same key moved by `semitones` (what varispeed does to it).
    pub fn transposed(&self, semitones: i32) -> Self {
        Key::new(
            (self.tonic as i32 + semitones).rem_euclid(12) as u8,
            self.mode,
        )
    }

    /// Mixes well with `other`: same key, a step round the wheel, or the relative
    /// major/minor.
    pub fn compatible(&self, other: &Key) -> bool {
        let (a, b) = (self.wheel() as i32, other.wheel() as i32);
        let steps = (a - b).rem_euclid(12).min((b - a).rem_euclid(12));
        match self.mode == other.mode {
            true => steps <= 1,
            false => steps == 0,
        }
    }

    pub fn name(&self, notation: KeyNotation) -> String {
        match notation {
            KeyNotation::OpenKey => {
                let letter = match self.mode {
                    Mode::Major => "d",
                    Mode::Minor => "m",
                };
                format!("{}{letter}", self.wheel())
            }
            KeyNotation::Camelot => {
                let letter = match self.mode {
                    Mode::Major => "B",
                    Mode::Minor => "A",
                };
                // Camelot's wheel is Open Key's turned seven steps.
                format!("{}{letter}", (self.wheel() + 6) % 12 + 1)
            }
            KeyNotation::Musical => match self.mode {
                Mode::Major => NAMES[self.tonic as usize].to_string(),
                Mode::Minor => format!("{}m", MINOR_NAMES[self.tonic as usize]),
            },
        }
    }
}

/// A detected key and how clearly it won.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyEstimate {
    pub key: Key,
    /// Correlation of the profile with the key's template, -1..1.
    pub correlation: f32,
    /// Lead over the runner-up key's correlation.
    pub margin: f32,
}

/// Accumulates a chroma profile from frames pushed one at a time.
pub struct KeyDetector {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// Pitch class of each FFT bin, if it is in range.
    bin_class: Vec<Option<u8>>,
    buffer: Vec<f32>,
    filled: usize,
    scratch_in: Vec<f32>,
    scratch_out: Vec<realfft::num_complex::Complex<f32>>,
    chroma: [f64; 12],
}

impl KeyDetector {
    pub fn new(sample_rate: u32) -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| {
                let x = i as f32 / FFT_SIZE as f32;
                0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
            })
            .collect();
        let bin_hz = sample_rate as f32 / FFT_SIZE as f32;
        let bin_class = (0..=FFT_SIZE / 2)
            .map(|bin| {
                let hz = bin as f32 * bin_hz;
                match (LOWEST_HZ..=HIGHEST_HZ).contains(&hz) {
                    // MIDI note 69 is A4 = 440 Hz; pitch class 0 is C.
                    true => {
                        let note = 69.0 + 12.0 * (hz / 440.0).log2();
                        Some((note.round() as i32).rem_euclid(12) as u8)
                    }
                    false => None,
                }
            })
            .collect();
        let scratch_in = fft.make_input_vec();
        let scratch_out = fft.make_output_vec();
        Self {
            fft,
            window,
            bin_class,
            buffer: vec![0.0; FFT_SIZE],
            filled: 0,
            scratch_in,
            scratch_out,
            chroma: [0.0; 12],
        }
    }

    #[inline]
    pub fn push(&mut self, left: f32, right: f32) {
        self.buffer[self.filled] = (left + right) * 0.5;
        self.filled += 1;
        if self.filled == FFT_SIZE {
            self.analyse_frame();
            // Keep the second half for the next (half-overlapping) frame.
            self.buffer.copy_within(HOP.., 0);
            self.filled = FFT_SIZE - HOP;
        }
    }

    fn analyse_frame(&mut self) {
        for ((input, sample), weight) in self
            .scratch_in
            .iter_mut()
            .zip(&self.buffer)
            .zip(&self.window)
        {
            *input = sample * weight;
        }
        if self
            .fft
            .process(&mut self.scratch_in, &mut self.scratch_out)
            .is_err()
        {
            return;
        }
        let mut frame = [0.0f64; 12];
        for (value, class) in self.scratch_out.iter().zip(&self.bin_class) {
            if let Some(class) = class {
                frame[*class as usize] += value.norm() as f64;
            }
        }
        // Compress each frame so loud passages don't drown out the rest of the track.
        let total: f64 = frame.iter().sum();
        if total > 1e-9 {
            for (sum, value) in self.chroma.iter_mut().zip(frame) {
                *sum += (value / total).sqrt();
            }
        }
    }

    /// The best-matching key, or `None` for silence.
    pub fn finish(self) -> Option<KeyEstimate> {
        if self.chroma.iter().all(|&c| c == 0.0) {
            return None;
        }
        let chroma = self.chroma.map(|c| c as f32);
        let mut scores: Vec<(Key, f32)> = (0..12u8)
            .flat_map(|tonic| {
                [(Mode::Major, &MAJOR_PROFILE), (Mode::Minor, &MINOR_PROFILE)].map(
                    |(mode, profile)| {
                        let rotated: [f32; 12] =
                            std::array::from_fn(|i| profile[(i + 12 - tonic as usize) % 12]);
                        (Key::new(tonic, mode), correlation(&chroma, &rotated))
                    },
                )
            })
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (key, best) = scores[0];
        Some(KeyEstimate {
            key,
            correlation: best,
            margin: best - scores[1].1,
        })
    }
}

/// Pearson correlation of two 12-element profiles.
fn correlation(a: &[f32; 12], b: &[f32; 12]) -> f32 {
    let mean = |x: &[f32; 12]| x.iter().sum::<f32>() / 12.0;
    let (ma, mb) = (mean(a), mean(b));
    let (mut num, mut da, mut db) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        num += (x - ma) * (y - mb);
        da += (x - ma) * (x - ma);
        db += (y - mb) * (y - mb);
    }
    match da > 0.0 && db > 0.0 {
        true => num / (da * db).sqrt(),
        false => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn hz(midi: i32) -> f32 {
        440.0 * 2f32.powf((midi - 69) as f32 / 12.0)
    }

    /// Chords (MIDI notes) one after another, `seconds` each, with a little bass on
    /// each root, played twice.
    fn progression(chords: &[&[i32]], seconds: f32) -> Vec<f32> {
        let per = (seconds * SR as f32) as usize;
        let mut out = Vec::new();
        for _ in 0..2 {
            for chord in chords {
                for i in 0..per {
                    let t = i as f32 / SR as f32;
                    let mut s = 0.0;
                    for &note in *chord {
                        s += (std::f32::consts::TAU * hz(note) * t).sin() * 0.15;
                        // Some overtones, as real instruments have.
                        s += (std::f32::consts::TAU * hz(note) * 2.0 * t).sin() * 0.05;
                    }
                    s += (std::f32::consts::TAU * hz(chord[0] - 24) * t).sin() * 0.2;
                    out.push(s);
                }
            }
        }
        out
    }

    fn detect(samples: &[f32]) -> Key {
        let mut detector = KeyDetector::new(SR);
        for &s in samples {
            detector.push(s, s);
        }
        detector.finish().unwrap().key
    }

    #[test]
    fn detects_major_and_minor_progressions() {
        // C major: I–IV–V–I.
        let c_major = progression(
            &[&[60, 64, 67], &[65, 69, 72], &[67, 71, 74], &[60, 64, 67]],
            1.5,
        );
        assert_eq!(detect(&c_major), Key::new(0, Mode::Major));
        // A minor: i–iv–V(E major)–i.
        let a_minor = progression(
            &[&[57, 60, 64], &[62, 65, 69], &[64, 68, 71], &[57, 60, 64]],
            1.5,
        );
        assert_eq!(detect(&a_minor), Key::new(9, Mode::Minor));
        // F♯ minor, to check transposition: i–iv–V–i.
        let fs_minor = progression(
            &[&[54, 57, 61], &[59, 62, 66], &[61, 65, 68], &[54, 57, 61]],
            1.5,
        );
        assert_eq!(detect(&fs_minor), Key::new(6, Mode::Minor));
    }

    #[test]
    fn silence_has_no_key() {
        let mut detector = KeyDetector::new(SR);
        for _ in 0..SR * 2 {
            detector.push(0.0, 0.0);
        }
        assert!(detector.finish().is_none());
    }

    #[test]
    fn notations() {
        let c = Key::new(0, Mode::Major);
        let am = Key::new(9, Mode::Minor);
        let fsm = Key::new(6, Mode::Minor);
        let ab = Key::new(8, Mode::Major);
        assert_eq!(c.name(KeyNotation::OpenKey), "1d");
        assert_eq!(am.name(KeyNotation::OpenKey), "1m");
        assert_eq!(Key::new(7, Mode::Major).name(KeyNotation::OpenKey), "2d");
        assert_eq!(Key::new(5, Mode::Major).name(KeyNotation::OpenKey), "12d");
        assert_eq!(fsm.name(KeyNotation::OpenKey), "4m");
        assert_eq!(c.name(KeyNotation::Camelot), "8B");
        assert_eq!(am.name(KeyNotation::Camelot), "8A");
        assert_eq!(fsm.name(KeyNotation::Camelot), "11A");
        assert_eq!(ab.name(KeyNotation::Camelot), "4B");
        assert_eq!(ab.name(KeyNotation::Musical), "A♭");
        assert_eq!(fsm.name(KeyNotation::Musical), "F♯m");
    }

    #[test]
    fn compatibility_follows_the_wheel() {
        let am = Key::new(9, Mode::Minor);
        assert!(am.compatible(&Key::new(9, Mode::Minor)));
        assert!(am.compatible(&Key::new(4, Mode::Minor))); // Em, a fifth up (2m)
        assert!(am.compatible(&Key::new(2, Mode::Minor))); // Dm (12m)
        assert!(am.compatible(&Key::new(0, Mode::Major))); // relative major
        assert!(!am.compatible(&Key::new(7, Mode::Major))); // G: different number and mode
        assert!(!am.compatible(&Key::new(11, Mode::Minor))); // Bm, two steps
    }

    #[test]
    fn transposing_moves_the_tonic() {
        assert_eq!(
            Key::new(11, Mode::Major).transposed(1),
            Key::new(0, Mode::Major)
        );
        assert_eq!(
            Key::new(0, Mode::Minor).transposed(-1),
            Key::new(11, Mode::Minor)
        );
    }
}
