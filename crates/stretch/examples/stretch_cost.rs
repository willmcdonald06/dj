//! Time key-locked playback per audio callback: `cargo run --release -p dj-stretch
//! --example stretch_cost`. Two decks at +8%, 128-frame callbacks, 20 s of audio.

use std::time::{Duration, Instant};

use dj_stretch::{Quality, Signalsmith, TimeStretcher};

#[allow(clippy::print_stdout)]
fn main() {
    const SR: u32 = 48_000;
    const PERIOD: usize = 128;
    for quality in [Quality::Default, Quality::Cheaper] {
        let mut decks = [Signalsmith::new(SR, quality), Signalsmith::new(SR, quality)];
        let input: Vec<f32> = (0..SR as usize * 30)
            .map(|i| ((i as f32 * 0.05).sin() + (i as f32 * 0.31).sin()) * 0.3)
            .collect();
        let mut out = ([0.0f32; PERIOD], [0.0f32; PERIOD]);
        let mut read = 0.0f64;
        let mut times = Vec::new();
        let callbacks = SR as usize * 20 / PERIOD;
        for _ in 0..callbacks {
            let start = read as usize;
            read += PERIOD as f64 * 1.08;
            let slice = &input[start..read as usize];
            let t = Instant::now();
            for deck in &mut decks {
                deck.process((slice, slice), (&mut out.0, &mut out.1));
            }
            times.push(t.elapsed());
        }
        times.sort();
        let budget = Duration::from_secs_f64(PERIOD as f64 / SR as f64);
        let pct = |d: Duration| d.as_secs_f64() / budget.as_secs_f64() * 100.0;
        println!(
            "{quality:?}: median {:.1}% of the {:.2} ms budget, 99th {:.1}%, max {:.1}%",
            pct(times[times.len() / 2]),
            budget.as_secs_f64() * 1000.0,
            pct(times[times.len() * 99 / 100]),
            pct(times[times.len() - 1]),
        );
    }
}
