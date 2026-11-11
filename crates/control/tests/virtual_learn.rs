//! End-to-end MIDI learn against an unknown controller made of virtual ALSA MIDI ports.
//! Needs the ALSA sequencer (`/dev/snd/seq`), so it is ignored by default:
//!
//! ```sh
//! cargo test -p dj-control --test virtual_learn -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use dj_control::{ControlRequest, ControlSettings, Controller, DeckTarget, Target, UiEvent};
use dj_engine::{DeckId, Engine, EngineConfig, EngineParts};
use midir::MidiOutput;
use midir::os::unix::VirtualOutput;

const FAKE: &str = "Learnable Pad virtual test";

/// Wait up to two seconds for a UI event matching `want`.
fn wait_for(events: &mpsc::Receiver<UiEvent>, want: impl Fn(&UiEvent) -> bool) -> UiEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(event) if want(&event) => return event,
            Ok(_) => {}
            Err(_) => panic!("timed out waiting for an event"),
        }
    }
}

#[test]
#[ignore = "needs the ALSA sequencer"]
fn learns_a_fader_on_an_unknown_controller() {
    let folder = std::env::temp_dir().join(format!("dj-learn-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&folder);
    let mut controls = MidiOutput::new(FAKE)
        .unwrap()
        .create_virtual("out")
        .unwrap();

    let EngineParts {
        mut engine,
        ui: mut handle,
        control,
        ..
    } = Engine::new(EngineConfig::new(48_000, 4));
    let (to_ui, events) = mpsc::channel();
    let controller = Controller::spawn(
        control,
        ControlSettings {
            port: Some("learnable pad".into()),
            mappings: Some(folder.clone()),
        },
        to_ui,
    )
    .unwrap();

    // No built-in mapping knows it, but it connects so it can be learned.
    let connected = wait_for(&events, |e| matches!(e, UiEvent::Connected { .. }));
    assert!(matches!(
        connected,
        UiEvent::Connected {
            built_in: false,
            learned: 0,
            ..
        }
    ));

    // Clicked deck A's fader on screen; a note is refused, the CC is taken.
    let fader = Target::Deck(DeckId::A, DeckTarget::Fader);
    controller
        .requests()
        .send(ControlRequest::Learn(fader))
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    controls.send(&[0x90, 0x10, 0x7F]).unwrap();
    wait_for(&events, |e| matches!(e, UiEvent::LearnRefused { .. }));
    controls.send(&[0xB2, 0x07, 0x40]).unwrap();
    let learned = wait_for(&events, |e| matches!(e, UiEvent::Learned { .. }));
    assert_eq!(
        learned,
        UiEvent::Learned {
            input: dj_control::MidiInput::Cc {
                channel: 2,
                control: 7
            },
            target: fader
        }
    );

    // Now the knob drives the fader.
    controls.send(&[0xB2, 0x07, 127]).unwrap();
    let mut buf = vec![0.0; 128 * 4];
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        std::thread::sleep(Duration::from_millis(20));
        engine.process(&mut buf, 128);
        if handle.snapshot().channels[0].fader == 1.0 {
            break;
        }
        assert!(Instant::now() < deadline, "fader never moved");
    }

    // And it was saved for next time.
    let saved: Vec<_> = std::fs::read_dir(&folder).unwrap().flatten().collect();
    assert_eq!(saved.len(), 1);
    let text = std::fs::read_to_string(saved[0].path()).unwrap();
    assert!(
        text.contains("cc 3 0x07") && text.contains("deck A fader"),
        "{text}"
    );
    drop(controller);
    let _ = std::fs::remove_dir_all(&folder);
}
