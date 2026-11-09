//! Print kick-detection results for a file: `cargo run --release -p dj-loader --example
//! kicks -- FILE`. A development aid for tuning the detector on real music.

use std::sync::atomic::AtomicBool;

use dj_engine::TrackId;

#[allow(clippy::print_stdout)]
fn main() {
    let Some(path) = std::env::args().nth(1) else {
        println!("usage: kicks FILE");
        return;
    };
    let cancel = AtomicBool::new(false);
    let mut track = None;
    dj_loader::decode_file(path.as_ref(), TrackId(1), 48_000, &cancel, &mut |t, _| {
        track = Some(t)
    })
    .expect("decode");
    let track = track.expect("track");
    let analysis = dj_analysis::analyze(&track);
    let sr = analysis.sample_rate as f64;
    println!(
        "{} kicks over {:.0} s, kick BPM {:?}",
        analysis.kicks.len(),
        analysis.frames as f64 / sr,
        analysis.kick_bpm()
    );
    match &analysis.key {
        Some(k) => println!(
            "key: {} ({}, {}), correlation {:.2}, margin {:.3}",
            k.key.name(dj_analysis::KeyNotation::Musical),
            k.key.name(dj_analysis::KeyNotation::OpenKey),
            k.key.name(dj_analysis::KeyNotation::Camelot),
            k.correlation,
            k.margin
        ),
        None => println!("key: none"),
    }
    match &analysis.grid {
        Some(g) => println!(
            "grid: {:.3} BPM, anchor {:.1} ms, confidence {:.2}",
            g.grid.bpm(analysis.sample_rate),
            g.grid.anchor / sr * 1000.0,
            g.confidence
        ),
        None => println!("grid: none"),
    }
    let beat = analysis.kick_bpm().map(|b| 60.0 / b);
    let mut histogram = [0usize; 6];
    for w in analysis.kicks.windows(2) {
        let interval = (w[1] - w[0]) as f64 / sr;
        let Some(beat) = beat else { break };
        let ratio = interval / beat;
        let bucket = match ratio {
            r if r < 0.9 => 0,
            r if r < 1.1 => 1,
            r if r < 1.9 => 2,
            r if r < 2.1 => 3,
            r if r < 4.2 => 4,
            _ => 5,
        };
        histogram[bucket] += 1;
    }
    println!(
        "intervals in beats: <0.9 {} | ~1 {} | 1.1-1.9 {} | ~2 {} | 2-4 {} | >4 {}",
        histogram[0], histogram[1], histogram[2], histogram[3], histogram[4], histogram[5]
    );
    if std::env::var_os("KICKS_LIST").is_some() {
        let list: Vec<String> = analysis
            .kicks
            .windows(2)
            .take(60)
            .map(|w| format!("{:.1}", (w[1] - w[0]) as f64 / sr * 1000.0))
            .collect();
        println!("first intervals (ms): {}", list.join(" "));
    }
    // Timing jitter of consecutive ~1-beat intervals, in ms: how steady the markers are.
    if let Some(beat) = beat {
        let deviations: Vec<f64> = analysis
            .kicks
            .windows(2)
            .map(|w| (w[1] - w[0]) as f64 / sr)
            .filter(|i| (i / beat - 1.0).abs() < 0.1)
            .map(|i| (i - beat) * 1000.0)
            .collect();
        let mut abs: Vec<f64> = deviations.iter().map(|d| d.abs()).collect();
        abs.sort_by(f64::total_cmp);
        if !abs.is_empty() {
            println!(
                "1-beat interval deviation: median {:.2} ms, 90th {:.2} ms",
                abs[abs.len() / 2],
                abs[abs.len() * 9 / 10]
            );
        }
    }
}
