//! The control thread: connects to a controller, maps its messages to engine commands
//! and keeps its LEDs in sync with the engine. Not RT-safe.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dj_engine::EngineHandle;

use crate::learn::{Learned, MidiInput, Target, port_key};
use crate::{
    Mapping, MidiMessage, MidiPorts, RawMessage, UiEvent, is_timing, mapping_for_port, port_names,
};

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
    /// Folder of learned mappings; `None` turns MIDI learn off.
    pub mappings: Option<PathBuf>,
}

/// What the UI asks of the control thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlRequest {
    /// Bind the next control moved on the controller to `target`.
    Learn(Target),
    CancelLearn,
    /// Forget every learned control for the connected controller.
    ClearLearned,
}

/// Handle to the control thread. Dropping it switches the controller's LEDs off and
/// stops the thread.
pub struct Controller {
    stop: Arc<AtomicBool>,
    requests: mpsc::Sender<ControlRequest>,
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
        let (requests, inbox) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("control".into())
            .spawn(move || run(engine, settings, ui, inbox, thread_stop))?;
        Ok(Self {
            stop,
            requests,
            thread: Some(thread),
        })
    }

    /// For sending MIDI learn requests to the control thread.
    pub fn requests(&self) -> mpsc::Sender<ControlRequest> {
        self.requests.clone()
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
    /// The built-in mapping for this controller, if there is one.
    mapping: Option<Box<dyn Mapping>>,
    /// Learned controls, checked before the built-in mapping.
    learned: Learned,
}

impl Active {
    fn name(&self) -> String {
        match &self.mapping {
            Some(mapping) => mapping.name().to_string(),
            None => port_key(self.ports.name()).to_string(),
        }
    }

    fn connected_event(&self) -> UiEvent {
        UiEvent::Connected {
            name: self.name(),
            built_in: self.mapping.is_some(),
            learned: self.learned.bindings.len(),
        }
    }
}

fn run(
    mut engine: EngineHandle,
    settings: ControlSettings,
    ui: mpsc::Sender<UiEvent>,
    requests: mpsc::Receiver<ControlRequest>,
    stop: Arc<AtomicBool>,
) {
    // A control waiting to be learned.
    let mut learning: Option<Target> = None;
    let (sender, inbox) = mpsc::channel::<Incoming>();
    let mut active: Option<Active> = None;
    let mut next_scan = Instant::now();
    let mut last_feedback = Instant::now();
    let mut warned_unmapped = false;

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next_scan {
            next_scan = now + SCAN_INTERVAL;
            let was = active.is_some();
            active = match active.take() {
                Some(current) => check_still_present(current),
                None => connect(&settings, &sender, &mut warned_unmapped),
            };
            match (&active, was) {
                (Some(now), false) => {
                    let _ = ui.send(now.connected_event());
                }
                (None, true) => {
                    let _ = ui.send(UiEvent::Disconnected);
                }
                _ => {}
            }
        }

        for request in requests.try_iter() {
            match request {
                ControlRequest::Learn(target) => learning = Some(target),
                ControlRequest::CancelLearn => learning = None,
                ControlRequest::ClearLearned => {
                    if let Some(active) = active.as_mut() {
                        active.learned.clear();
                        if let Err(err) = active.learned.save() {
                            tracing::warn!("can't save the learned mapping: {err}");
                        }
                        let _ = ui.send(UiEvent::LearnCleared);
                        let _ = ui.send(active.connected_event());
                    }
                }
            }
        }

        match inbox.recv_timeout(POLL_INTERVAL) {
            Ok(first) => {
                if let Some(active) = active.as_mut() {
                    // Handle everything that is waiting, not just one message.
                    for message in std::iter::once(first).chain(inbox.try_iter()) {
                        if let Some(target) = learning {
                            if learn(active, target, &message.bytes, &ui) {
                                learning = None;
                            }
                            // While learning, the controller does nothing else.
                            continue;
                        }
                        let raw = RawMessage {
                            timestamp_us: message.timestamp_us,
                            bytes: &message.bytes,
                        };
                        let mut emit = |command| {
                            if let Err(err) = engine.send(command) {
                                tracing::warn!("{err}");
                            }
                        };
                        // The UI may be gone during shutdown; nothing to do then.
                        let mut to_ui = |event| {
                            let _ = ui.send(event);
                        };
                        let parsed = MidiMessage::parse(raw.bytes);
                        if active.learned.on_message(&parsed, &mut emit, &mut to_ui) {
                            continue;
                        }
                        if let Some(mapping) = active.mapping.as_mut() {
                            mapping.on_message(raw, &mut emit, &mut to_ui);
                        }
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
                let Active {
                    ports,
                    mapping,
                    learned,
                } = active;
                if let Some(mapping) = mapping.as_mut() {
                    mapping.update_feedback(&state, now, &mut |bytes| ports.send(bytes));
                }
                learned.update_feedback(&state, &mut |bytes| ports.send(bytes));
            }
        }
    }

    if let Some(Active {
        mut ports,
        mapping: Some(mut mapping),
        ..
    }) = active
    {
        mapping.on_disconnect(&mut |bytes| ports.send(bytes));
        tracing::info!("disconnected from {}", mapping.name());
    }
}

/// Try to bind the control that sent `bytes` to `target`. Returns whether learning is
/// done. Releases, clocks and other messages are ignored: they don't pick a control.
fn learn(active: &mut Active, target: Target, bytes: &[u8], ui: &mpsc::Sender<UiEvent>) -> bool {
    if is_timing(bytes) {
        return false;
    }
    let message = MidiMessage::parse(bytes);
    if matches!(message, MidiMessage::NoteOff { .. }) {
        return false;
    }
    let Some((input, _, _)) = MidiInput::from_message(&message) else {
        return false;
    };
    if !target.accepts(input) {
        let _ = ui.send(UiEvent::LearnRefused { input, target });
        return false;
    }
    active.learned.bind(input, target);
    match active.learned.save() {
        Ok(()) => tracing::info!("learned {input} → {target}"),
        Err(err) => tracing::warn!("can't save the learned mapping: {err}"),
    }
    let _ = ui.send(UiEvent::Learned { input, target });
    let _ = ui.send(active.connected_event());
    true
}

/// Ports that are never a controller.
fn is_system_port(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("midi through") || name.contains("rtmidi")
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
    let candidates: Vec<String> = inputs
        .into_iter()
        .filter(|name| match &wanted {
            Some(wanted) => name.to_lowercase().contains(wanted),
            None => !is_system_port(name),
        })
        .collect();
    let learned_for = |name: &str| match &settings.mappings {
        Some(folder) => Learned::load(folder, name),
        None => Learned::default(),
    };
    // First a controller we know (built in or learned), else any controller, so it
    // can be learned.
    let known = candidates.iter().find_map(|name| {
        let mapping = mapping_for_port(name);
        let learned = learned_for(name);
        match mapping.is_some() || !learned.bindings.is_empty() {
            true => Some((name.clone(), mapping, learned)),
            false => None,
        }
    });
    let chosen = known.or_else(|| match settings.mappings {
        Some(_) => candidates
            .first()
            .map(|name| (name.clone(), None, learned_for(name))),
        None => None,
    });
    let Some((name, mut mapping, learned)) = chosen else {
        if wanted.is_some() && !*warned_unmapped {
            tracing::warn!("no usable MIDI port matches {wanted:?}; try --list-midi");
            *warned_unmapped = true;
        }
        return None;
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
            if let Some(mapping) = mapping.as_mut() {
                mapping.on_connect(&mut |bytes| ports.send(bytes));
            }
            let active = Active {
                ports,
                mapping,
                learned,
            };
            tracing::info!(
                "connected to {} on {name:?} ({} learned controls)",
                active.name(),
                active.learned.bindings.len()
            );
            Some(active)
        }
        Err(err) => {
            tracing::warn!("{err}");
            None
        }
    }
}

/// Drop the connection if its port has gone (the controller was unplugged).
fn check_still_present(active: Active) -> Option<Active> {
    match port_names() {
        Ok((inputs, _)) if !inputs.iter().any(|n| n == active.ports.name()) => {
            tracing::warn!("{} disconnected", active.name());
            None
        }
        _ => Some(active),
    }
}
