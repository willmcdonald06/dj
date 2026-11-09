//! MIDI ports through `midir`. Not RT-safe.

use midir::{Ignore, MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};

const CLIENT_NAME: &str = "dj";

#[derive(Debug, thiserror::Error)]
pub enum MidiError {
    #[error("cannot start MIDI: {0}")]
    Init(String),
    #[error("MIDI input port {0:?} not found")]
    NotFound(String),
    #[error("cannot connect to MIDI port {port:?}: {message}")]
    Connect { port: String, message: String },
}

/// Names of the MIDI input and output ports: `(inputs, outputs)`.
pub fn port_names() -> Result<(Vec<String>, Vec<String>), MidiError> {
    let input = MidiInput::new(CLIENT_NAME).map_err(|e| MidiError::Init(e.to_string()))?;
    let output = MidiOutput::new(CLIENT_NAME).map_err(|e| MidiError::Init(e.to_string()))?;
    let inputs = input
        .ports()
        .iter()
        .filter_map(|p| input.port_name(p).ok())
        .collect();
    let outputs = output
        .ports()
        .iter()
        .filter_map(|p| output.port_name(p).ok())
        .collect();
    Ok((inputs, outputs))
}

/// An open input port and, if one matches, the output port of the same device.
pub struct MidiPorts {
    name: String,
    _input: MidiInputConnection<()>,
    output: Option<MidiOutputConnection>,
}

impl MidiPorts {
    /// Open the input port named exactly `input_name`. `on_message` runs on `midir`'s
    /// thread with a timestamp in microseconds and the raw bytes.
    pub fn open<F>(input_name: &str, mut on_message: F) -> Result<Self, MidiError>
    where
        F: FnMut(u64, &[u8]) + Send + 'static,
    {
        let mut input = MidiInput::new(CLIENT_NAME).map_err(|e| MidiError::Init(e.to_string()))?;
        // Receive everything, including SysEx, for the monitor.
        input.ignore(Ignore::None);
        let port = input
            .ports()
            .into_iter()
            .find(|p| input.port_name(p).ok().as_deref() == Some(input_name))
            .ok_or_else(|| MidiError::NotFound(input_name.to_string()))?;
        let connection = input
            .connect(
                &port,
                "dj-in",
                move |stamp, bytes, _| on_message(stamp, bytes),
                (),
            )
            .map_err(|e| MidiError::Connect {
                port: input_name.to_string(),
                message: e.to_string(),
            })?;

        let output = match open_output(input_name) {
            Ok(output) => output,
            Err(err) => {
                tracing::warn!("no MIDI output for {input_name:?}, so no LEDs: {err}");
                None
            }
        };

        Ok(Self {
            name: input_name.to_string(),
            _input: connection,
            output,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Send a message to the device. Errors are logged, not returned: a lost LED
    /// update shouldn't stop the controller.
    pub fn send(&mut self, bytes: &[u8]) {
        if let Some(output) = self.output.as_mut() {
            if let Err(err) = output.send(bytes) {
                tracing::debug!("MIDI send to {:?} failed: {err}", self.name);
            }
        }
    }
}

/// The output port belonging to the same device as `input_name`: the same name, or
/// failing that the same client name (the part before ':' on ALSA).
fn open_output(input_name: &str) -> Result<Option<MidiOutputConnection>, MidiError> {
    let output = MidiOutput::new(CLIENT_NAME).map_err(|e| MidiError::Init(e.to_string()))?;
    let device = input_name.split(':').next().unwrap_or(input_name);
    let ports = output.ports();
    let named = |p: &midir::MidiOutputPort| output.port_name(p).unwrap_or_default();
    let port = ports
        .iter()
        .find(|p| named(p) == input_name)
        .or_else(|| ports.iter().find(|p| named(p).starts_with(device)));
    let Some(port) = port else {
        return Ok(None);
    };
    let port = port.clone();
    let connection = output
        .connect(&port, "dj-out")
        .map_err(|e| MidiError::Connect {
            port: input_name.to_string(),
            message: e.to_string(),
        })?;
    Ok(Some(connection))
}
