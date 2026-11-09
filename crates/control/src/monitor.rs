//! MIDI monitor: logs every incoming message, decoded, and what the matching mapping
//! (if any) turns it into. Used to build and check mappings. Not RT-safe.
//!
//! Ports with a known mapping are put into the mode the mapping uses (for the DJ-505,
//! Serato mode with keep-alives), so the monitor shows exactly what the app receives.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use dj_engine::EngineSnapshot;

use crate::{Mapping, MidiError, MidiMessage, MidiPorts, RawMessage, hex, mapping_for_port};

struct Port {
    ports: MidiPorts,
    mapping: Option<Box<dyn Mapping>>,
}

/// Monitor all MIDI inputs whose name contains `filter` (all inputs if `None`), passing
/// each formatted line to `print`. Runs for `duration`, or forever if `None`. Clock and
/// active-sensing messages are hidden unless `show_timing` is set.
pub fn run(
    filter: Option<&str>,
    duration: Option<Duration>,
    show_timing: bool,
    print: &mut dyn FnMut(&str),
) -> Result<(), MidiError> {
    let (inputs, outputs) = crate::port_names()?;
    print(&format!("MIDI inputs:  {inputs:?}"));
    print(&format!("MIDI outputs: {outputs:?}"));

    let wanted = filter.map(str::to_lowercase);
    let (sender, inbox) = mpsc::channel::<(usize, Vec<u8>)>();
    let mut open = Vec::new();
    for name in inputs {
        if let Some(wanted) = &wanted {
            if !name.to_lowercase().contains(wanted) {
                continue;
            }
        }
        let index = open.len();
        let sender = sender.clone();
        let mut ports = MidiPorts::open(&name, move |_, bytes| {
            let _ = sender.send((index, bytes.to_vec()));
        })?;
        let mut mapping = mapping_for_port(&name);
        match mapping.as_mut() {
            Some(mapping) => {
                print(&format!(
                    "listening on {name:?} (mapping: {})",
                    mapping.name()
                ));
                mapping.on_connect(&mut |bytes| ports.send(bytes));
            }
            None => print(&format!("listening on {name:?} (no mapping)")),
        }
        open.push(Port { ports, mapping });
    }
    if open.is_empty() {
        print("no matching MIDI inputs");
        return Ok(());
    }
    match show_timing {
        true => print("move controls to see their messages; Ctrl-C quits"),
        false => print(
            "move controls to see their messages; Ctrl-C quits \
             (clock and active-sensing messages hidden: --midi-monitor-all shows them)",
        ),
    }

    let started = Instant::now();
    let idle = EngineSnapshot::default();
    loop {
        if duration.is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        match inbox.recv_timeout(Duration::from_millis(50)) {
            Ok((index, bytes)) => {
                if !show_timing && crate::is_timing(&bytes) {
                    continue;
                }
                let port = &mut open[index];
                let mut commands = Vec::new();
                let mut events = Vec::new();
                if let Some(mapping) = port.mapping.as_mut() {
                    let raw = RawMessage {
                        timestamp_us: 0,
                        bytes: &bytes,
                    };
                    mapping.on_message(raw, &mut |c| commands.push(c), &mut |e| events.push(e));
                }
                let mut mapped = String::new();
                if !commands.is_empty() {
                    mapped.push_str(&format!("  → {commands:?}"));
                }
                if !events.is_empty() {
                    mapped.push_str(&format!("  → UI {events:?}"));
                }
                print(&format!(
                    "[{}] {:<9} {}{mapped}",
                    port.ports.name(),
                    hex(&bytes),
                    MidiMessage::parse(&bytes),
                ));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Keep controllers in their mode (keep-alives).
        let now = Instant::now();
        for port in &mut open {
            if let Some(mapping) = port.mapping.as_mut() {
                let ports = &mut port.ports;
                mapping.update_feedback(&idle, now, &mut |bytes| ports.send(bytes));
            }
        }
    }
    for port in &mut open {
        if let Some(mapping) = port.mapping.as_mut() {
            let ports = &mut port.ports;
            mapping.on_disconnect(&mut |bytes| ports.send(bytes));
        }
    }
    Ok(())
}
