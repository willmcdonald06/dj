//! A virtual MIDI controller for trying MIDI learn without hardware (Linux/ALSA):
//! `cargo run -p dj-control --example fake_controller -- SECONDS` creates a port called
//! "Fake Controller" and, every second, turns a knob (CC 1 on channel 1) up a step.
#[cfg(target_os = "linux")]
fn main() {
    use midir::os::unix::VirtualOutput;
    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let mut port = midir::MidiOutput::new("Fake Controller")
        .expect("ALSA sequencer")
        .create_virtual("out")
        .expect("virtual port");
    for second in 0..seconds {
        let _ = port.send(&[0xB0, 0x01, (second * 8 % 128) as u8]);
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {}
