//! Decode the generated fixtures in `test-data/` (see `tools/gen-test-data.sh`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use dj_engine::{
    Command, DeckAction, DeckId, Engine, EngineConfig, EngineParts, Track, TrackId, Transport,
};
use dj_loader::{LoadError, LoadEvent, Loader, TrackInfo, decode_file};

const RATE: u32 = 48_000;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-data")
        .join(name)
}

fn decode(name: &str) -> (Arc<Track>, TrackInfo) {
    let cancel = AtomicBool::new(false);
    let mut result = None;
    let stats = decode_file(&fixture(name), TrackId(1), RATE, &cancel, &mut |t, i| {
        result = Some((t, i));
    })
    .unwrap_or_else(|err| panic!("{name}: {err}"));
    let (track, info) = result.expect("on_playable called");
    assert!(track.is_complete());
    assert_eq!(track.len_frames(), stats.frames);
    assert_eq!(track.ready_frames(), stats.frames);
    (track, info)
}

/// Frequency of one channel from rising zero crossings over the middle of the track.
fn frequency(track: &Track, right: bool) -> f64 {
    let len = track.len_frames();
    let (start, end) = (len / 4, len * 3 / 4);
    let samples: Vec<f32> = (start..end)
        .map(|i| {
            let (l, r) = track.frame(i).unwrap();
            match right {
                true => r,
                false => l,
            }
        })
        .collect();
    let crossings = samples
        .windows(2)
        .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
        .count();
    crossings as f64 * RATE as f64 / (end - start) as f64
}

fn assert_close(actual: f64, expected: f64, tolerance: f64, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} not within {tolerance} of {expected}"
    );
}

#[test]
fn flac_44k1_stereo_is_resampled_with_channels_in_order() {
    let (track, info) = decode("tone-44k1-stereo.flac");
    assert_eq!(track.len_frames(), 2 * RATE as u64);
    assert_eq!(info.source_sample_rate, 44_100);
    assert_eq!(info.source_channels, 2);
    assert_eq!(info.title, "Test Tone");
    assert_eq!(info.artist.as_deref(), Some("dj test-data"));
    assert_close(frequency(&track, false), 440.0, 2.0, "left");
    assert_close(frequency(&track, true), 1000.0, 2.0, "right");
}

#[test]
fn mp3_48k_mono_is_duplicated_to_both_channels() {
    let (track, info) = decode("tone-48k-mono.mp3");
    // Gapless MP3: encoder delay and padding are trimmed.
    assert_eq!(track.len_frames(), 2 * RATE as u64);
    assert_eq!(info.source_channels, 1);
    assert_eq!(info.title, "Test Tone");
    assert_close(frequency(&track, false), 440.0, 2.0, "left");
    let (l, r) = track.frame(track.len_frames() / 2).unwrap();
    assert_eq!(l, r);
}

#[test]
fn aac_m4a_decodes() {
    let (track, _info) = decode("tone-44k1-stereo.m4a");
    // AAC in MP4 isn't gapless in symphonia: allow the priming frames.
    assert_close(
        track.len_frames() as f64,
        2.0 * RATE as f64,
        4096.0,
        "length",
    );
    assert_close(frequency(&track, false), 440.0, 2.0, "left");
    assert_close(frequency(&track, true), 1000.0, 2.0, "right");
}

#[test]
fn wav_96k_24bit_is_downsampled() {
    let (track, info) = decode("tone-96k-stereo.wav");
    assert_eq!(track.len_frames(), RATE as u64);
    assert_eq!(info.source_sample_rate, 96_000);
    // No tags: title falls back to the file name.
    assert_eq!(info.title, "tone-96k-stereo");
    assert_close(frequency(&track, false), 440.0, 2.0, "left");
    assert_close(frequency(&track, true), 1000.0, 2.0, "right");
    let peak = (0..track.len_frames())
        .map(|i| track.frame(i).unwrap().0.abs())
        .fold(0.0f32, f32::max);
    assert_close(peak as f64, 0.5, 0.02, "peak");
}

#[test]
fn tags_are_read_without_decoding() {
    let tags = dj_loader::read_tags(&fixture("tone-44k1-stereo.flac")).unwrap();
    assert_eq!(tags.title.as_deref(), Some("Test Tone"));
    assert_eq!(tags.artist.as_deref(), Some("dj test-data"));
    assert!((tags.duration.unwrap() - 2.0).abs() < 0.01);
    let wav = dj_loader::read_tags(&fixture("tone-96k-stereo.wav")).unwrap();
    assert_eq!(wav.title, None);
}

#[test]
fn missing_and_non_audio_files_fail() {
    let cancel = AtomicBool::new(false);
    let mut never = |_: Arc<Track>, _: TrackInfo| panic!("should not become playable");
    let missing = decode_file(
        &fixture("no-such-file.flac"),
        TrackId(1),
        RATE,
        &cancel,
        &mut never,
    );
    assert!(matches!(missing, Err(LoadError::Open(_))), "{missing:?}");
    let text = decode_file(&fixture("README.md"), TrackId(1), RATE, &cancel, &mut never);
    assert!(matches!(text, Err(LoadError::Format(_))), "{text:?}");
}

#[test]
fn cancelled_decode_stops() {
    let cancel = AtomicBool::new(true);
    let mut playable = None;
    let result = decode_file(
        &fixture("tone-44k1-stereo.flac"),
        TrackId(1),
        RATE,
        &cancel,
        &mut |t, _| playable = Some(t),
    );
    assert!(matches!(result, Err(LoadError::Cancelled)));
    if let Some(track) = playable {
        assert!(track.is_complete(), "abandoned track is marked finished");
    }
}

fn wait_for(loader: &Loader, mut matches: impl FnMut(&LoadEvent) -> bool) -> LoadEvent {
    for _ in 0..100 {
        if let Some(event) = loader.wait_event(Duration::from_millis(100)) {
            if matches(&event) {
                return event;
            }
        }
    }
    panic!("timed out waiting for load event");
}

#[test]
fn loader_puts_track_on_deck_and_it_plays() {
    let EngineParts {
        mut engine,
        ui: mut handle,
        tracks,
        ..
    } = Engine::new(EngineConfig::new(RATE, 2));
    let loader = Loader::spawn(RATE, tracks).unwrap();
    loader.load(DeckId::A, fixture("tone-44k1-stereo.flac"));

    let loaded = wait_for(&loader, |e| matches!(e, LoadEvent::Loaded { .. }));
    let LoadEvent::Loaded { deck, info, .. } = loaded else {
        unreachable!()
    };
    assert_eq!(deck, DeckId::A);
    wait_for(&loader, |e| matches!(e, LoadEvent::Finished { .. }));

    let mut buf = vec![0.0; 4096 * 2];
    engine.process(&mut buf, 4096);
    let deck = handle.snapshot().decks[0];
    assert_eq!(deck.track, Some(info.id));
    assert_eq!(deck.length, 2 * RATE as u64);
    assert!(deck.loaded_fully);

    handle
        .send(Command::deck(DeckId::A, DeckAction::PlayPause))
        .unwrap();
    engine.process(&mut buf, 4096);
    assert_eq!(handle.snapshot().decks[0].transport, Transport::Playing);
    assert!(buf.iter().any(|&s| s.abs() > 0.1), "deck produces audio");
}

#[test]
fn second_load_onto_a_deck_wins_and_failures_are_reported() {
    let EngineParts {
        mut engine,
        ui: mut handle,
        tracks,
        ..
    } = Engine::new(EngineConfig::new(RATE, 2));
    let loader = Loader::spawn(RATE, tracks).unwrap();
    loader.load(DeckId::B, fixture("no-such-file.mp3"));
    let failed = wait_for(&loader, |e| matches!(e, LoadEvent::Failed { .. }));
    assert!(matches!(
        failed,
        LoadEvent::Failed {
            deck: DeckId::B,
            ..
        }
    ));

    loader.load(DeckId::B, fixture("tone-44k1-stereo.flac"));
    loader.load(DeckId::B, fixture("tone-48k-mono.mp3"));
    let mut last_finished = None;
    for _ in 0..2 {
        // The first load may be cancelled before or after it becomes playable; only
        // the second is guaranteed to finish.
        let event = wait_for(&loader, |e| matches!(e, LoadEvent::Finished { .. }));
        if let LoadEvent::Finished { id, .. } = event {
            last_finished = Some(id);
            if id == TrackId(3) {
                break;
            }
        }
    }
    assert_eq!(last_finished, Some(TrackId(3)));

    let mut buf = vec![0.0; 256];
    engine.process(&mut buf, 128);
    assert_eq!(handle.snapshot().decks[1].track, Some(TrackId(3)));
}
