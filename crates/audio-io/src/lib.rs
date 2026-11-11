//! Audio output through `cpal`: device selection, stream setup and the audio callback.
//!
//! # RT safety
//!
//! The data callback built in [`OpenDevice::start`] is **RT-safe**: it only calls
//! [`Engine::process`] (the engine arrives through a one-slot `rtrb` queue once the
//! stream is built), converts samples into the device format using a scratch buffer
//! allocated before the stream starts, and updates atomics in [`CallbackStats`]. In debug
//! builds the callback body runs inside `assert_no_alloc`, which aborts the process if
//! anything allocates (the binary must install `assert_no_alloc::AllocDisabler` as its
//! global allocator for the check to be active).
//!
//! The error callback is not on the hot path; it only bumps counters.
//!
//! Everything else in this crate (device enumeration, opening, stopping) is **not**
//! RT-safe and runs on the main thread.

mod stats;

pub use stats::{CallbackStats, HISTOGRAM_BUCKETS, StatsSnapshot};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use dj_engine::{Engine, EngineConfig, OutputMode, OutputRouting};

/// Largest chunk rendered by the engine in one go. Device buffers larger than this are
/// rendered in several chunks, so the scratch buffer has a fixed size.
pub const MAX_CHUNK_FRAMES: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no audio output device available")]
    NoDevice,
    #[error("output device {0:?} not found")]
    DeviceNotFound(String),
    #[error(
        "device {device:?} has no output config at {sample_rate} Hz with a supported sample format"
    )]
    NoSupportedConfig { device: String, sample_rate: u32 },
    #[error(transparent)]
    Cpal(#[from] cpal::Error),
    #[error("the audio stream stopped responding while restarting it")]
    Restart,
    #[error("per-deck outputs need a device with 4 outputs; {device:?} has {channels}")]
    NeedsFourChannels { device: String, channels: usize },
    #[error("{0}; the audio stream couldn't be restarted, so there is no sound")]
    Lost(Box<AudioError>),
}

/// Largest buffer the watchdog grows to.
pub const MAX_WATCHDOG_BUFFER: u32 = 2048;
/// More xruns than this in one check interval mean the buffer is too small.
const WATCHDOG_XRUNS: u64 = 5;
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

/// The watchdog reopened the stream with a bigger buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferGrown {
    pub from: u32,
    pub to: u32,
}

/// What the user asked for. The device may not support all of it; see [`OpenDevice`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSettings {
    /// Substring of the output device name to use. `None` picks the system default.
    pub device: Option<String>,
    pub sample_rate: u32,
    /// Requested period size in frames. Falls back to the device default when the
    /// device does not support it.
    pub buffer_frames: Option<u32>,
    /// Requested channel count. `None` prefers 4 (main + headphones), then 2.
    pub channels: Option<u16>,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            device: None,
            sample_rate: 48_000,
            buffer_frames: Some(128),
            channels: None,
        }
    }
}

/// Names of all output devices on the default host.
pub fn output_device_names() -> Result<Vec<String>, AudioError> {
    let host = cpal::default_host();
    let names = host.output_devices()?.map(|d| d.to_string()).collect();
    Ok(names)
}

/// A device with a chosen stream configuration, not yet running.
pub struct OpenDevice {
    device: cpal::Device,
    device_name: String,
    stream_config: cpal::StreamConfig,
    sample_format: SampleFormat,
    /// Whether `stream_config.buffer_size` is a fixed request we may retry without.
    fixed_buffer: bool,
}

impl OpenDevice {
    /// Pick a device and stream configuration matching `settings` as closely as possible.
    pub fn open(settings: &AudioSettings) -> Result<Self, AudioError> {
        let host = cpal::default_host();
        let device = find_device(&host, settings.device.as_deref())?;
        let device_name = device.to_string();

        let chosen =
            choose_config(&device, settings).ok_or_else(|| AudioError::NoSupportedConfig {
                device: device_name.clone(),
                sample_rate: settings.sample_rate,
            })?;

        let buffer_size = match (settings.buffer_frames, chosen.buffer_range) {
            (Some(frames), Some((min, max))) if (min..=max).contains(&frames) => {
                cpal::BufferSize::Fixed(frames)
            }
            // Range unknown: try the request and fall back to default if it fails.
            (Some(frames), None) => cpal::BufferSize::Fixed(frames),
            _ => cpal::BufferSize::Default,
        };
        let fixed_buffer = matches!(buffer_size, cpal::BufferSize::Fixed(_));

        Ok(Self {
            device,
            device_name,
            stream_config: cpal::StreamConfig {
                channels: chosen.channels,
                sample_rate: settings.sample_rate,
                buffer_size,
            },
            sample_format: chosen.sample_format,
            fixed_buffer,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn channels(&self) -> usize {
        self.stream_config.channels as usize
    }

    pub fn sample_rate(&self) -> u32 {
        self.stream_config.sample_rate
    }

    pub fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }

    pub fn buffer_size(&self) -> cpal::BufferSize {
        self.stream_config.buffer_size
    }

    /// The engine configuration matching this device.
    pub fn engine_config(&self) -> EngineConfig {
        EngineConfig::new(self.sample_rate(), self.channels())
    }

    /// Start the stream, moving `engine` onto the audio thread.
    pub fn start(mut self, engine: Engine) -> Result<AudioOutput, AudioError> {
        let stats = Arc::new(CallbackStats::new());
        let mode = engine.config().routing.mode;
        let running = self.run(&mut Some(engine), &stats)?;
        Ok(AudioOutput {
            mode,
            open: self,
            running: Some(running),
            stats,
            watchdog: Watchdog::new(),
        })
    }

    /// Build a stream (falling back to the device's default buffer if a fixed size is
    /// rejected), hand it the engine and start it. The engine is taken out of `engine`
    /// only once a stream exists, so on a failed build the caller still has it.
    fn run(
        &mut self,
        engine: &mut Option<Engine>,
        stats: &Arc<CallbackStats>,
    ) -> Result<Running, AudioError> {
        let built = match self.build_stream(stats) {
            Ok(built) => built,
            Err(err) if self.fixed_buffer => {
                tracing::warn!(
                    "fixed buffer size {:?} rejected ({err}); using device default",
                    self.stream_config.buffer_size
                );
                self.stream_config.buffer_size = cpal::BufferSize::Default;
                self.fixed_buffer = false;
                self.build_stream(stats)?
            }
            Err(err) => return Err(err),
        };
        // Hand the engine over only once a stream exists, so a failed build never
        // swallows it. The callback picks it up on its next run.
        let Built {
            stream,
            mut handoff,
            returned,
            release,
        } = built;
        let Some(engine) = engine.take() else {
            return Err(AudioError::Restart);
        };
        if handoff.push(engine).is_err() {
            unreachable!("handoff queue has room for exactly one engine");
        }
        stream.play()?;
        Ok(Running {
            _stream: stream,
            returned,
            release,
        })
    }

    fn build_stream(&self, stats: &Arc<CallbackStats>) -> Result<Built, AudioError> {
        match self.sample_format {
            SampleFormat::F32 => self.build_typed::<f32>(stats),
            SampleFormat::I32 => self.build_typed::<i32>(stats),
            SampleFormat::I16 => self.build_typed::<i16>(stats),
            _ => Err(AudioError::NoSupportedConfig {
                device: self.device_name.clone(),
                sample_rate: self.sample_rate(),
            }),
        }
    }

    fn build_typed<T>(&self, stats: &Arc<CallbackStats>) -> Result<Built, AudioError>
    where
        T: SizedSample + FromSample<f32> + Send + 'static,
    {
        let (handoff, receiver) = rtrb::RingBuffer::new(1);
        let (give_back, returned) = rtrb::RingBuffer::new(1);
        let release = Arc::new(AtomicBool::new(false));
        let mut renderer = Renderer::new(
            self.channels(),
            self.sample_rate(),
            Arc::clone(stats),
            receiver,
        );
        renderer.give_back = Some((Arc::clone(&release), give_back));

        let data_callback = move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            renderer.render(data);
        };

        let error_stats = Arc::clone(stats);
        let error_callback = move |err: cpal::Error| {
            error_stats.record_error(err.kind());
        };

        let stream = self.device.build_output_stream(
            self.stream_config,
            data_callback,
            error_callback,
            None,
        )?;
        Ok(Built {
            stream,
            handoff,
            returned,
            release,
        })
    }
}

struct Built {
    stream: cpal::Stream,
    handoff: rtrb::Producer<Engine>,
    returned: rtrb::Consumer<Engine>,
    release: Arc<AtomicBool>,
}

/// A started stream and the way to get its engine back.
struct Running {
    _stream: cpal::Stream,
    /// The callback puts the engine here once `release` is set.
    returned: rtrb::Consumer<Engine>,
    release: Arc<AtomicBool>,
}

struct Watchdog {
    last_check: Instant,
    last_xruns: u64,
}

impl Watchdog {
    fn new() -> Self {
        Self {
            last_check: Instant::now(),
            last_xruns: 0,
        }
    }
}

/// A running output stream. Dropping it stops audio.
pub struct AudioOutput {
    open: OpenDevice,
    mode: OutputMode,
    running: Option<Running>,
    stats: Arc<CallbackStats>,
    watchdog: Watchdog,
}

impl AudioOutput {
    pub fn device_name(&self) -> &str {
        &self.open.device_name
    }

    pub fn stream_config(&self) -> cpal::StreamConfig {
        self.open.stream_config
    }

    pub fn sample_format(&self) -> SampleFormat {
        self.open.sample_format
    }

    /// Shared callback statistics (xruns, callback durations) for the debug overlay.
    pub fn stats(&self) -> Arc<CallbackStats> {
        Arc::clone(&self.stats)
    }

    /// Call regularly (e.g. every UI frame). Once a second, if the stream is
    /// underrunning, reopens it with twice the buffer, up to [`MAX_WATCHDOG_BUFFER`].
    /// Small buffers can starve when the sound server runs a bigger cycle (PipeWire
    /// does whenever another program asks for higher latency), and that sounds like
    /// crackling and slowed playback.
    pub fn watchdog(&mut self) -> Option<Result<BufferGrown, AudioError>> {
        if self.watchdog.last_check.elapsed() < WATCHDOG_INTERVAL {
            return None;
        }
        let stats = self.stats.snapshot();
        let new_xruns = stats.xruns.saturating_sub(self.watchdog.last_xruns);
        self.watchdog = Watchdog {
            last_check: Instant::now(),
            last_xruns: stats.xruns,
        };
        let current = match self.open.stream_config.buffer_size {
            cpal::BufferSize::Fixed(frames) => frames,
            cpal::BufferSize::Default => stats.last_frames as u32,
        };
        if new_xruns <= WATCHDOG_XRUNS || current >= MAX_WATCHDOG_BUFFER || current == 0 {
            return None;
        }
        let to = (current * 2).clamp(256, MAX_WATCHDOG_BUFFER);
        tracing::warn!("{new_xruns} xruns in a second at {current} frames; reopening with {to}");
        let result = self
            .restart_with_buffer(to)
            .map(|()| BufferGrown { from: current, to });
        // Don't judge the new stream by the old one's xruns.
        self.watchdog = Watchdog {
            last_check: Instant::now(),
            last_xruns: self.stats.snapshot().xruns,
        };
        Some(result)
    }

    /// The fixed period size in use, or `None` when the device picks it.
    pub fn buffer_frames(&self) -> Option<u32> {
        match self.open.stream_config.buffer_size {
            cpal::BufferSize::Fixed(frames) => Some(frames),
            cpal::BufferSize::Default => None,
        }
    }

    pub fn channels(&self) -> usize {
        self.open.channels()
    }

    pub fn sample_rate(&self) -> u32 {
        self.open.sample_rate()
    }

    /// The output mode the engine is routed for.
    pub fn output_mode(&self) -> OutputMode {
        self.mode
    }

    /// Reopen the stream with a `frames` buffer, keeping the engine and its state.
    pub fn restart_with_buffer(&mut self, frames: u32) -> Result<(), AudioError> {
        let mut engine = Some(self.take_engine()?);
        self.open.stream_config.buffer_size = cpal::BufferSize::Fixed(frames);
        self.open.fixed_buffer = true;
        self.running = Some(self.open.run(&mut engine, &self.stats)?);
        Ok(())
    }

    /// Move the engine to another device, buffer size or output mode without losing
    /// its state (decks keep playing after a short gap). `settings.sample_rate` must be
    /// the engine's rate. If the new setup can't be opened, the old one is restored and
    /// the error returned; [`AudioError::Lost`] means even that failed.
    pub fn reconfigure(
        &mut self,
        settings: &AudioSettings,
        mode: OutputMode,
    ) -> Result<(), AudioError> {
        // Choosing a config doesn't touch the running stream.
        let mut open = OpenDevice::open(settings)?;
        let routing = OutputRouting::for_mode(mode, open.channels()).ok_or_else(|| {
            AudioError::NeedsFourChannels {
                device: open.device_name.clone(),
                channels: open.channels(),
            }
        })?;
        let mut engine = self.take_engine()?;
        let old_routing = engine.config().routing;
        engine.set_routing(routing);
        let mut engine = Some(engine);
        match open.run(&mut engine, &self.stats) {
            Ok(running) => {
                tracing::info!(
                    "audio: {} — {} ch, buffer {:?}, {mode:?}",
                    open.device_name,
                    open.channels(),
                    open.stream_config.buffer_size
                );
                self.open = open;
                self.mode = mode;
                self.running = Some(running);
                self.watchdog = Watchdog::new();
                self.watchdog.last_xruns = self.stats.snapshot().xruns;
                Ok(())
            }
            Err(err) => {
                tracing::warn!(
                    "can't switch audio ({err}); going back to {}",
                    self.open.device_name
                );
                if let Some(engine) = engine.as_mut() {
                    engine.set_routing(old_routing);
                }
                match self.open.run(&mut engine, &self.stats) {
                    Ok(running) => {
                        self.running = Some(running);
                        Err(err)
                    }
                    Err(_) => Err(AudioError::Lost(Box::new(err))),
                }
            }
        }
    }

    /// Stop the stream and get the engine back from the callback.
    fn take_engine(&mut self) -> Result<Engine, AudioError> {
        let Some(mut running) = self.running.take() else {
            return Err(AudioError::Restart);
        };
        running.release.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        let engine = loop {
            if let Ok(engine) = running.returned.pop() {
                break engine;
            }
            if Instant::now() >= deadline {
                // The callback isn't running; leave the old stream in place.
                running.release.store(false, Ordering::Release);
                self.running = Some(running);
                return Err(AudioError::Restart);
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        drop(running);
        Ok(engine)
    }
}

/// Audio-callback state: the engine, a scratch buffer and stats. RT-safe after
/// construction.
struct Renderer {
    channels: usize,
    sample_rate: u32,
    scratch: Vec<f32>,
    stats: Arc<CallbackStats>,
    /// Receives the engine once the stream has been built successfully.
    incoming: rtrb::Consumer<Engine>,
    engine: Option<Engine>,
    /// When the flag is set, the engine goes back through this queue (for restarting
    /// the stream) and the callback plays silence from then on.
    give_back: Option<(Arc<AtomicBool>, rtrb::Producer<Engine>)>,
    /// The thread's scheduling has been checked (once, on the first callback).
    checked_realtime: bool,
}

impl Renderer {
    fn new(
        channels: usize,
        sample_rate: u32,
        stats: Arc<CallbackStats>,
        incoming: rtrb::Consumer<Engine>,
    ) -> Self {
        Self {
            channels,
            sample_rate,
            scratch: vec![0.0; MAX_CHUNK_FRAMES * channels],
            stats,
            incoming,
            engine: None,
            give_back: None,
            checked_realtime: false,
        }
    }

    /// RT-safe. Renders `data.len() / channels` frames in chunks of at most
    /// [`MAX_CHUNK_FRAMES`]. Outputs silence until the engine has arrived.
    fn render<T>(&mut self, data: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
    {
        let started = Instant::now();
        let channels = self.channels;
        if !self.checked_realtime {
            // One system call, once per stream.
            self.checked_realtime = true;
            self.stats.record_realtime(thread_is_realtime());
        }

        no_alloc(|| {
            if let Some((release, queue)) = &mut self.give_back {
                if release.load(Ordering::Acquire) {
                    if let Some(engine) = self.engine.take() {
                        // A move into preallocated queue space, not an allocation.
                        if let Err(rtrb::PushError::Full(engine)) = queue.push(engine) {
                            self.engine = Some(engine);
                        }
                    }
                    data.fill(T::EQUILIBRIUM);
                    return;
                }
            }
            if self.engine.is_none() {
                // Moving the engine out of the queue is a memcpy, not an allocation.
                self.engine = self.incoming.pop().ok();
            }
            let Some(engine) = self.engine.as_mut() else {
                data.fill(T::EQUILIBRIUM);
                return;
            };
            for chunk in data.chunks_mut(MAX_CHUNK_FRAMES * channels) {
                let frames = chunk.len() / channels;
                let rendered = &mut self.scratch[..frames * channels];
                engine.process(rendered, frames);
                for (dst, &src) in chunk.iter_mut().zip(rendered.iter()) {
                    *dst = T::from_sample(src);
                }
            }
        });

        let frames = data.len() / channels;
        self.stats
            .record_callback(started.elapsed(), frames, self.sample_rate);
    }
}

/// Whether the calling thread runs under a real-time scheduling policy.
fn thread_is_realtime() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: queries the calling thread's own policy; no memory is passed.
        let policy = unsafe { libc::sched_getscheduler(0) };
        policy == libc::SCHED_FIFO || policy == libc::SCHED_RR
    }
    // CoreAudio and WASAPI give their callback threads their own real-time scheduling.
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(debug_assertions)]
#[inline]
fn no_alloc<R>(f: impl FnOnce() -> R) -> R {
    assert_no_alloc::assert_no_alloc(f)
}

#[cfg(not(debug_assertions))]
#[inline]
fn no_alloc<R>(f: impl FnOnce() -> R) -> R {
    f()
}

fn find_device(host: &cpal::Host, wanted: Option<&str>) -> Result<cpal::Device, AudioError> {
    match wanted {
        None => host.default_output_device().ok_or(AudioError::NoDevice),
        Some(wanted) => {
            let needle = wanted.to_lowercase();
            host.output_devices()?
                .find(|d| d.to_string().to_lowercase().contains(&needle))
                .ok_or_else(|| AudioError::DeviceNotFound(wanted.to_string()))
        }
    }
}

struct ChosenConfig {
    channels: u16,
    sample_format: SampleFormat,
    buffer_range: Option<(u32, u32)>,
}

/// Rank the device's output configs and pick the best one at the requested rate.
fn choose_config(device: &cpal::Device, settings: &AudioSettings) -> Option<ChosenConfig> {
    let ranges = device.supported_output_configs().ok()?;
    ranges
        .filter(|r| (r.min_sample_rate()..=r.max_sample_rate()).contains(&settings.sample_rate))
        .filter_map(|r| {
            let format_rank = format_rank(r.sample_format())?;
            let channel_rank = channel_rank(r.channels(), settings.channels)?;
            let buffer_range = match r.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => Some((*min, *max)),
                cpal::SupportedBufferSize::Unknown => None,
            };
            let chosen = ChosenConfig {
                channels: r.channels(),
                sample_format: r.sample_format(),
                buffer_range,
            };
            Some(((channel_rank, format_rank), chosen))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, chosen)| chosen)
}

/// Lower is better. `None` means unusable.
fn format_rank(format: SampleFormat) -> Option<u8> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I32 => Some(1),
        SampleFormat::I16 => Some(2),
        _ => None,
    }
}

/// Lower is better. `None` means unusable.
fn channel_rank(available: u16, requested: Option<u16>) -> Option<u16> {
    match requested {
        Some(wanted) if available == wanted => Some(0),
        Some(_) => None,
        None => match available {
            4 => Some(0),
            2 => Some(1),
            // More channels than needed is fine (extras get silence), fewer is a last
            // resort.
            n if n > 4 => Some(2 + n),
            n => Some(1000 + n),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_four_channels_then_two() {
        assert!(channel_rank(4, None) < channel_rank(2, None));
        assert!(channel_rank(2, None) < channel_rank(8, None));
        assert!(channel_rank(8, None) < channel_rank(1, None));
    }

    #[test]
    fn explicit_channel_request_is_exact() {
        assert_eq!(channel_rank(2, Some(2)), Some(0));
        assert_eq!(channel_rank(4, Some(2)), None);
    }

    #[test]
    fn renderer_converts_and_chunks_large_buffers() {
        let dj_engine::EngineParts {
            engine,
            ui: mut handle,
            ..
        } = Engine::new(EngineConfig::new(48_000, 2));
        handle
            .send(dj_engine::Command::SetToneEnabled(true))
            .unwrap();
        let stats = Arc::new(CallbackStats::new());
        let (mut handoff, incoming) = rtrb::RingBuffer::new(1);
        let mut renderer = Renderer::new(2, 48_000, Arc::clone(&stats), incoming);

        // Before the engine arrives: silence.
        let mut data = vec![1i16; 64];
        renderer.render(&mut data);
        assert!(data.iter().all(|&s| s == 0));

        handoff.push(engine).ok().unwrap();
        let frames = MAX_CHUNK_FRAMES * 2 + 17;
        let mut data = vec![0i16; frames * 2];
        renderer.render(&mut data);

        assert!(data[frames..].iter().any(|&s| s != 0));
        assert_eq!(handle.snapshot().frames_processed, frames as u64);
        assert_eq!(stats.snapshot().callbacks, 2);
    }

    #[test]
    fn renderer_gives_the_engine_back_when_released() {
        let dj_engine::EngineParts { engine, .. } = Engine::new(EngineConfig::new(48_000, 2));
        let stats = Arc::new(CallbackStats::new());
        let (mut handoff, incoming) = rtrb::RingBuffer::new(1);
        let (give_back, mut returned) = rtrb::RingBuffer::new(1);
        let release = Arc::new(AtomicBool::new(false));
        let mut renderer = Renderer::new(2, 48_000, stats, incoming);
        renderer.give_back = Some((Arc::clone(&release), give_back));
        handoff.push(engine).ok().unwrap();
        let mut data = vec![0.0f32; 256];
        renderer.render(&mut data);
        assert!(returned.pop().is_err());

        release.store(true, Ordering::Release);
        renderer.render(&mut data);
        assert!(returned.pop().is_ok(), "engine handed back");
        assert!(data.iter().all(|&s| s == 0.0));
    }
}

// Make `assert_no_alloc` live in this crate's tests, so the renderer test fails (aborts)
// if the callback path allocates. The check only exists in debug builds.
#[cfg(all(test, debug_assertions))]
#[global_allocator]
static TEST_ALLOCATOR: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;
