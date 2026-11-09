//! End-to-end check of the control thread against a fake DJ-505 made of virtual ALSA
//! MIDI ports. Needs the ALSA sequencer (`/dev/snd/seq`), so it is ignored by default:
//!
//! ```sh
//! cargo test -p dj-control --test virtual_dj505 -- --ignored
//! ```
#![cfg(target_os = "linux")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dj_control::dj505::SYSEX_INIT;
use dj_control::{ControlSettings, Controller};
use dj_engine::{Engine, EngineConfig, EngineParts};
use midir::os::unix::{VirtualInput, VirtualOutput};
use midir::{Ignore, MidiInput, MidiOutput};

/// Client name of the fake controller; the mapping matches on "DJ-505".
const FAKE: &str = "DJ-505 virtual test";

#[test]
#[ignore = "needs the ALSA sequencer"]
fn fake_dj505_drives_the_engine_and_gets_leds() {
    // What the fake controller receives (SysEx, keep-alives, LEDs).
    let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let log = Arc::clone(&received);
    let mut input = MidiInput::new(FAKE).unwrap();
    input.ignore(Ignore::None);
    let _leds = input
        .create_virtual(
            "in",
            move |_, bytes, _| log.lock().unwrap().push(bytes.to_vec()),
            (),
        )
        .unwrap();
    // What the fake controller sends.
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
    let _controller = Controller::spawn(
        control,
        ControlSettings {
            port: Some("virtual test".into()),
        },
        std::sync::mpsc::channel().0,
    )
    .unwrap();

    // Move deck B's channel fader to 1/4 until the engine shows it (the controller
    // connects on its first scan).
    let mut buf = vec![0.0; 128 * 4];
    let deadline = Instant::now() + Duration::from_secs(5);
    let raw: u16 = 16383 / 4;
    loop {
        controls.send(&[0xB1, 0x1C, (raw >> 7) as u8]).unwrap();
        controls.send(&[0xB1, 0x3A, (raw & 0x7F) as u8]).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        engine.process(&mut buf, 128);
        let fader = handle.snapshot().channels[1].fader;
        if (fader - 0.25).abs() < 0.001 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fader never arrived (at {fader})"
        );
    }

    // PFL on deck A, and its LED comes back on.
    controls.send(&[0x90, 0x1B, 0x7F]).unwrap();
    controls.send(&[0x90, 0x1B, 0x00]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        std::thread::sleep(Duration::from_millis(20));
        engine.process(&mut buf, 128);
        let lit = received.lock().unwrap().contains(&vec![0x90, 0x1B, 0x7F]);
        if handle.snapshot().channels[0].pfl && lit {
            break;
        }
        assert!(Instant::now() < deadline, "PFL or its LED never came on");
    }

    let received = received.lock().unwrap();
    assert!(
        received.contains(&SYSEX_INIT[0].to_vec()),
        "Serato mode SysEx"
    );
    assert!(received.contains(&vec![0xBF, 0x64, 0x00]), "keep-alive");
}
