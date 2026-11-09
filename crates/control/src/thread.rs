//! The control thread: connects to a controller, maps its messages to engine commands
//! and keeps its LEDs in sync with the engine. Not RT-safe.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dj_engine::EngineHandle;

use crate::{Mapping, MidiPorts, RawMessage, UiEvent, mapping_for_port, port_names};

/// How often to look for a controller (when none is connected) or check that the
/// connected one is still there.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// How often LEDs are refreshed from the engine state.
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(15);
/// Longest wait for a MIDI message before doing periodic work.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlSettings {
    /// Use only MIDI input ports whose name contains this (case-insensitive). `None`
    /// picks the first port with a known mapping.
    pub port: Option<String>,
}

/// Handle to the control thread. Dropping it switches the controller's LEDs off and
/// stops the thread.
pub struct Controller {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Controller {
    /// Start the control thread. UI events (browsing, loading) go to `ui`.
    pub fn spawn(
        engine: EngineHandle,
        settings: ControlSettings,
        ui: mpsc::Sender<UiEvent>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("control".into())
            .spawn(move || run(engine, settings, ui, thread_stop))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Incoming {
    timestamp_us: u64,
    bytes: Vec<u8>,
}

struct Active {
    ports: MidiPorts,
    mapping: Box<dyn Mapping>,
}

fn run(
    mut engine: EngineHandle,
    settings: ControlSettings,
    ui: mpsc::Sender<UiEvent>,
    stop: Arc<AtomicBool>,
) {
    let (sender, inbox) = mpsc::channel::<Incoming>();
    let mut active: Option<Active> = None;
    let mut next_scan = Instant::now();
    let mut last_feedback = Instant::now();
    let mut warned_unmapped = false;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next_scan {
            next_scan = now + SCAN_INTERVAL;
            active = match active.take() {
                Some(current) => check_still_present(current),
                None => connect(&settings, &sender, &mut warned_unmapped),
            };
        }

        match inbox.recv_timeout(POLL_INTERVAL) {
            Ok(first) => {
                if let Some(active) = active.as_mut() {
                    // Handle everything that is waiting, not just one message.
                    for message in std::iter::once(first).chain(inbox.try_iter()) {
                        let raw = RawMessage {
                            timestamp_us: message.timestamp_us,
                            bytes: &message.bytes,
                        };
                        active.mapping.on_message(
                            raw,
                            &mut |command| {
                                if let Err(err) = engine.send(command) {
                                    tracing::warn!("{err}");
                                }
                            },
                            // The UI may be gone during shutdown; nothing to do then.
                            &mut |event| {
                                let _ = ui.send(event);
                            },
                        );
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // We hold a sender ourselves, so this can't happen; stop if it does.
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(active) = active.as_mut() {
            let now = Instant::now();
            if now.duration_since(last_feedback) >= FEEDBACK_INTERVAL {
                last_feedback = now;
                let state = engine.snapshot();
                let Active { ports, mapping } = active;
                mapping.update_feedback(&state, now, &mut |bytes| ports.send(bytes));
            }
        }
    }

    if let Some(Active {
        mut ports,
        mut mapping,
    }) = active
    {
        mapping.on_disconnect(&mut |bytes| ports.send(bytes));
        tracing::info!("disconnected from {}", mapping.name());
    }
}

/// Find a controller with a mapping and connect to it.
fn connect(
    settings: &ControlSettings,
    sender: &mpsc::Sender<Incoming>,
    warned_unmapped: &mut bool,
) -> Option<Active> {
    let (inputs, _) = match port_names() {
        Ok(names) => names,
        Err(err) => {
            if !*warned_unmapped {
                tracing::warn!("{err}");
                *warned_unmapped = true;
            }
            return None;
        }
    };
    let wanted = settings.port.as_ref().map(|p| p.to_lowercase());
    let candidates = inputs.into_iter().filter(|name| match &wanted {
        Some(wanted) => name.to_lowercase().contains(wanted),
        None => true,
    });
    for name in candidates {
        let Some(mut mapping) = mapping_for_port(&name) else {
            if wanted.is_some() && !*warned_unmapped {
                tracing::warn!("no mapping for MIDI port {name:?}; try --midi-monitor");
                *warned_unmapped = true;
            }
            continue;
        };
        let sender = sender.clone();
        let opened = MidiPorts::open(&name, move |timestamp_us, bytes| {
            let _ = sender.send(Incoming {
                timestamp_us,
                bytes: bytes.to_vec(),
            });
        });
        match opened {
            Ok(mut ports) => {
                tracing::info!("connected to {} on {name:?}", mapping.name());
                mapping.on_connect(&mut |bytes| ports.send(bytes));
                return Some(Active { ports, mapping });
            }
            Err(err) => tracing::warn!("{err}"),
        }
    }
    None
}

/// Drop the connection if its port has gone (the controller was unplugged).
fn check_still_present(active: Active) -> Option<Active> {
    match port_names() {
        Ok((inputs, _)) if !inputs.iter().any(|n| n == active.ports.name()) => {
            tracing::warn!("{} disconnected", active.mapping.name());
            None
        }
        _ => Some(active),
    }
}
