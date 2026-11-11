//! Recording the main output to WAV files.
//!
//! A writer thread owns the engine's [`RecordSource`]: it arms the engine's tap, drains
//! the queued main-bus audio every few milliseconds and writes it as 24-bit stereo WAV.
//! Not RT-safe (file I/O); it never touches the audio thread except through the tap's
//! lock-free queue and atomics.
//!
//! Crash safety: a recording is written as `<name>.wav.part` with its header brought up
//! to date every second, and renamed to `<name>.wav` when it stops. If the app dies
//! mid-recording, the next start repairs the header of any leftover `.part` file from
//! its length and renames it, so at most the last second's header update is lost, not
//! the audio.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dj_engine::{RECORD_CHANNELS, RecordSource};

/// How often the writer drains the engine's queue.
const POLL: Duration = Duration::from_millis(20);
/// How often the WAV header is rewritten with the current length.
const HEADER_EVERY: Duration = Duration::from_secs(1);
/// After disarming, wait this long for the last callback to finish queueing (longer
/// than the largest device buffer, 2048 frames, at any normal rate).
const SETTLE: Duration = Duration::from_millis(120);
const BYTES_PER_SAMPLE: u32 = 3;
const HEADER_BYTES: u64 = 44;
const PART_SUFFIX: &str = ".part";
/// Folder recordings go into, under the music folder.
pub const RECORDINGS_FOLDER: &str = "Recordings";

/// What the writer thread reports back to the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordEvent {
    Started {
        path: PathBuf,
    },
    Stopped {
        /// Every file written: more than one if the recording passed the 4 GB WAV limit.
        files: Vec<PathBuf>,
        seconds: f64,
        /// Audio lost because the disk couldn't keep up (filled with silence).
        dropped_seconds: f64,
    },
    Failed {
        error: String,
    },
    /// Unfinished recordings from an earlier run that were repaired.
    Recovered {
        files: Vec<PathBuf>,
    },
}

enum Request {
    Start,
    Stop,
}

/// UI-side handle to the writer thread. Dropping it finishes any recording.
pub struct Recorder {
    requests: Option<mpsc::Sender<Request>>,
    events: mpsc::Receiver<RecordEvent>,
    /// Frames written in the current recording; 0 when stopped.
    frames: Arc<AtomicU64>,
    recording: bool,
    sample_rate: u32,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Start the writer thread. Recordings go into `folder`, which is created when the
    /// first recording starts.
    pub fn spawn(source: RecordSource, folder: PathBuf) -> std::io::Result<Self> {
        let (requests, receive_requests) = mpsc::channel();
        let (send_events, events) = mpsc::channel();
        let frames = Arc::new(AtomicU64::new(0));
        let sample_rate = source.sample_rate();
        let writer = Writer {
            source,
            folder,
            events: send_events,
            frames: Arc::clone(&frames),
            active: None,
            buffer: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || writer.run(receive_requests))?;
        Ok(Self {
            requests: Some(requests),
            events,
            frames,
            recording: false,
            sample_rate,
            thread: Some(thread),
        })
    }

    /// Whether a recording is running (as far as the UI has been told).
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Length of the running recording.
    pub fn elapsed_seconds(&self) -> f64 {
        self.frames.load(Ordering::Relaxed) as f64 / self.sample_rate as f64
    }

    pub fn toggle(&mut self) {
        let request = match self.recording {
            true => Request::Stop,
            false => Request::Start,
        };
        // Flip now so the button answers at once; a failure to start comes back as an
        // event and flips it back.
        self.recording = !self.recording;
        if let Some(requests) = &self.requests {
            let _ = requests.send(request);
        }
    }

    /// The next event from the writer, if any.
    pub fn try_event(&mut self) -> Option<RecordEvent> {
        let event = self.events.try_recv().ok()?;
        match &event {
            RecordEvent::Started { .. } => self.recording = true,
            RecordEvent::Stopped { .. } | RecordEvent::Failed { .. } => self.recording = false,
            RecordEvent::Recovered { .. } => {}
        }
        Some(event)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Closing the channel makes the thread finish the recording and exit.
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A recording in progress.
struct Active {
    /// Name without extension, e.g. `2026-11-10 21-04-33`.
    stem: String,
    wav: WavWriter,
    /// Finished files (when a recording spans more than one).
    done: Vec<PathBuf>,
    frames: u64,
    /// The engine's dropped-frame total when this recording started.
    dropped_at_start: u64,
    dropped: u64,
    last_header: Instant,
}

struct Writer {
    source: RecordSource,
    folder: PathBuf,
    events: mpsc::Sender<RecordEvent>,
    frames: Arc<AtomicU64>,
    active: Option<Active>,
    /// Reused between drains.
    buffer: Vec<f32>,
}

impl Writer {
    fn run(mut self, requests: mpsc::Receiver<Request>) {
        let recovered = recover_parts(&self.folder);
        if !recovered.is_empty() {
            self.send(RecordEvent::Recovered { files: recovered });
        }
        loop {
            match requests.recv_timeout(POLL) {
                Ok(Request::Start) => self.start(),
                Ok(Request::Stop) => self.stop(),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.stop();
                    return;
                }
            }
            if let Err(err) = self.pump() {
                self.fail(format!("recording stopped: {err}"));
            }
        }
    }

    fn send(&self, event: RecordEvent) {
        let _ = self.events.send(event);
    }

    fn start(&mut self) {
        if self.active.is_some() {
            return;
        }
        let stem = unique_stem(&self.folder, &local_timestamp());
        let created = std::fs::create_dir_all(&self.folder).and_then(|()| {
            WavWriter::create(
                &part_path(&self.folder, &stem, 1),
                self.source.sample_rate(),
            )
        });
        let wav = match created {
            Ok(wav) => wav,
            Err(err) => {
                let error = format!("can't record into {}: {err}", self.folder.display());
                tracing::error!("{error}");
                self.send(RecordEvent::Failed { error });
                return;
            }
        };
        self.source.discard();
        self.source.set_armed(true);
        self.frames.store(0, Ordering::Relaxed);
        let path = final_path(&self.folder, &stem, 1);
        tracing::info!("recording to {}", path.display());
        self.active = Some(Active {
            stem,
            wav,
            done: Vec::new(),
            frames: 0,
            dropped_at_start: self.source.dropped_frames(),
            dropped: 0,
            last_header: Instant::now(),
        });
        self.send(RecordEvent::Started { path });
    }

    /// Write whatever the engine has queued.
    fn pump(&mut self) -> std::io::Result<()> {
        let Some(active) = &mut self.active else {
            return Ok(());
        };
        // Audio the engine had to drop: keep the timeline right with silence.
        let dropped = self.source.dropped_frames() - active.dropped_at_start;
        if dropped > active.dropped {
            let missing = (dropped - active.dropped) as usize;
            active.dropped = dropped;
            self.buffer.clear();
            self.buffer.resize(missing * RECORD_CHANNELS, 0.0);
            write_split(
                active,
                &self.folder,
                self.source.sample_rate(),
                &self.buffer,
            )?;
        }
        self.buffer.clear();
        self.source.drain_into(&mut self.buffer);
        write_split(
            active,
            &self.folder,
            self.source.sample_rate(),
            &self.buffer,
        )?;
        self.frames.store(active.frames, Ordering::Relaxed);
        if active.last_header.elapsed() >= HEADER_EVERY {
            active.wav.update_header()?;
            active.last_header = Instant::now();
        }
        Ok(())
    }

    fn stop(&mut self) {
        if self.active.is_none() {
            return;
        }
        self.source.set_armed(false);
        std::thread::sleep(SETTLE);
        if let Err(err) = self.pump() {
            self.fail(format!("recording stopped: {err}"));
            return;
        }
        let Some(active) = self.active.take() else {
            return;
        };
        let rate = self.source.sample_rate() as f64;
        let mut files = active.done;
        let part = active.wav.path.clone();
        match active.wav.finish().and_then(|()| finalize(&part)) {
            Ok(path) => files.push(path),
            Err(err) => {
                self.fail(format!("couldn't finish {}: {err}", part.display()));
                return;
            }
        }
        self.frames.store(0, Ordering::Relaxed);
        for file in &files {
            tracing::info!("recorded {}", file.display());
        }
        self.send(RecordEvent::Stopped {
            files,
            seconds: active.frames as f64 / rate,
            dropped_seconds: active.dropped as f64 / rate,
        });
    }

    /// Stop after an I/O error, keeping what was written (its header is repaired on the
    /// next start if this one can't be).
    fn fail(&mut self, error: String) {
        tracing::error!("{error}");
        self.source.set_armed(false);
        if let Some(active) = self.active.take() {
            let part = active.wav.path.clone();
            if active.wav.finish().is_ok() {
                let _ = finalize(&part);
            }
        }
        self.frames.store(0, Ordering::Relaxed);
        self.send(RecordEvent::Failed { error });
    }
}

/// Write `samples`, moving on to a new file when the current one reaches the WAV size
/// limit.
fn write_split(
    active: &mut Active,
    folder: &Path,
    sample_rate: u32,
    mut samples: &[f32],
) -> std::io::Result<()> {
    while !samples.is_empty() {
        let room = (active.wav.room_frames() as usize) * RECORD_CHANNELS;
        if room == 0 {
            let number = active.done.len() + 2;
            let next = WavWriter::create(&part_path(folder, &active.stem, number), sample_rate)?;
            let full = std::mem::replace(&mut active.wav, next);
            let part = full.path.clone();
            full.finish()?;
            active.done.push(finalize(&part)?);
            continue;
        }
        let (now, later) = samples.split_at(room.min(samples.len()));
        active.wav.write(now)?;
        active.frames += (now.len() / RECORD_CHANNELS) as u64;
        samples = later;
    }
    Ok(())
}

/// 24-bit PCM stereo WAV, written as it goes.
struct WavWriter {
    path: PathBuf,
    file: BufWriter<File>,
    sample_rate: u32,
    data_bytes: u64,
    /// Scratch for converting samples.
    bytes: Vec<u8>,
}

/// Largest data chunk a WAV file can describe, in whole frames.
const MAX_DATA_BYTES: u64 = {
    let frame = BYTES_PER_SAMPLE as u64 * RECORD_CHANNELS as u64;
    (u32::MAX as u64 - HEADER_BYTES) / frame * frame
};

impl WavWriter {
    fn create(path: &Path, sample_rate: u32) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&wav_header(sample_rate, 0))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            sample_rate,
            data_bytes: 0,
            bytes: Vec::new(),
        })
    }

    fn room_frames(&self) -> u64 {
        (MAX_DATA_BYTES - self.data_bytes) / (BYTES_PER_SAMPLE as u64 * RECORD_CHANNELS as u64)
    }

    fn write(&mut self, samples: &[f32]) -> std::io::Result<()> {
        self.bytes.clear();
        for &sample in samples {
            let value = (sample.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
            self.bytes.extend_from_slice(&value.to_le_bytes()[..3]);
        }
        self.file.write_all(&self.bytes)?;
        self.data_bytes += self.bytes.len() as u64;
        Ok(())
    }

    /// Rewrite the header with the current length, so the file plays if the app dies.
    fn update_header(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let file = self.file.get_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&wav_header(self.sample_rate, self.data_bytes as u32))?;
        file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        self.update_header()?;
        self.file.get_ref().sync_all()
    }
}

fn wav_header(sample_rate: u32, data_bytes: u32) -> [u8; HEADER_BYTES as usize] {
    let channels = RECORD_CHANNELS as u16;
    let block_align = channels * BYTES_PER_SAMPLE as u16;
    let mut header = [0u8; HEADER_BYTES as usize];
    let mut at = 0;
    let mut put = |bytes: &[u8]| {
        header[at..at + bytes.len()].copy_from_slice(bytes);
        at += bytes.len();
    };
    put(b"RIFF");
    put(&(data_bytes.saturating_add(HEADER_BYTES as u32 - 8)).to_le_bytes());
    put(b"WAVEfmt ");
    put(&16u32.to_le_bytes());
    put(&1u16.to_le_bytes()); // PCM
    put(&channels.to_le_bytes());
    put(&sample_rate.to_le_bytes());
    put(&(sample_rate * block_align as u32).to_le_bytes());
    put(&block_align.to_le_bytes());
    put(&(BYTES_PER_SAMPLE as u16 * 8).to_le_bytes());
    put(b"data");
    put(&data_bytes.to_le_bytes());
    header
}

/// `<stem>.wav` for the first file of a recording, `<stem> part N.wav` after that.
fn final_path(folder: &Path, stem: &str, number: usize) -> PathBuf {
    match number {
        1 => folder.join(format!("{stem}.wav")),
        n => folder.join(format!("{stem} part {n}.wav")),
    }
}

fn part_path(folder: &Path, stem: &str, number: usize) -> PathBuf {
    let mut path = final_path(folder, stem, number).into_os_string();
    path.push(PART_SUFFIX);
    PathBuf::from(path)
}

/// Rename `<name>.wav.part` to `<name>.wav`.
fn finalize(part: &Path) -> std::io::Result<PathBuf> {
    let name = part.to_string_lossy();
    let done = PathBuf::from(name.strip_suffix(PART_SUFFIX).unwrap_or(&name));
    std::fs::rename(part, &done)?;
    Ok(done)
}

/// A name not used by an earlier recording (two started in the same second).
fn unique_stem(folder: &Path, timestamp: &str) -> String {
    let taken =
        |stem: &str| final_path(folder, stem, 1).exists() || part_path(folder, stem, 1).exists();
    let mut stem = timestamp.to_string();
    let mut n = 2;
    while taken(&stem) {
        stem = format!("{timestamp} ({n})");
        n += 1;
    }
    stem
}

/// Repair and rename `.wav.part` files left by a run that didn't stop cleanly.
fn recover_parts(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut recovered = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .to_string_lossy()
            .ends_with(&format!(".wav{PART_SUFFIX}"))
        {
            continue;
        }
        match repair_header(&path).and_then(|()| finalize(&path)) {
            Ok(done) => {
                tracing::warn!("recovered unfinished recording {}", done.display());
                recovered.push(done);
            }
            Err(err) => tracing::error!("can't recover {}: {err}", path.display()),
        }
    }
    recovered.sort();
    recovered
}

/// Set the header's lengths from the file's size (whole frames only).
fn repair_header(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut header = [0u8; HEADER_BYTES as usize];
    std::io::Read::read_exact(&mut file, &mut header)?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err(std::io::Error::other("not a WAV file"));
    }
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let frame = BYTES_PER_SAMPLE as u64 * RECORD_CHANNELS as u64;
    let data =
        (file.metadata()?.len().saturating_sub(HEADER_BYTES)).min(MAX_DATA_BYTES) / frame * frame;
    file.set_len(HEADER_BYTES + data)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&wav_header(sample_rate, data as u32))?;
    file.sync_all()
}

/// The local date and time as `YYYY-MM-DD HH-MM-SS`, for file names.
fn local_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    // SAFETY: `localtime_r` only writes the `tm` we pass and reads `now`; both live on
    // this stack frame. A zeroed `tm` is a valid value of the plain C struct.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        match libc::localtime_r(&now, &mut tm).is_null() {
            true => None,
            false => Some(tm),
        }
    };
    match tm {
        Some(tm) => format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        ),
        None => format!("recording {now}"),
    }
}

/// `h:mm:ss` or `m:ss`, for the REC timer and notices.
pub fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    match h {
        0 => format!("{m}:{s:02}"),
        h => format!("{h}:{m:02}:{s:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_wav(path: &Path) -> (u32, Vec<i32>) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, bytes.len());
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        assert_eq!(data + 44, bytes.len());
        let samples = bytes[44..]
            .chunks_exact(3)
            .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
            .collect();
        (rate, samples)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dj-recorder-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn records_the_engine_main_bus_to_a_wav() {
        use dj_engine::{Command, Engine, EngineConfig, EngineParts};
        let folder = scratch("main");
        let EngineParts {
            mut engine,
            ui: mut handle,
            recording,
            ..
        } = Engine::new(EngineConfig::new(48_000, 2));
        handle.send(Command::SetToneEnabled(true)).unwrap();
        let mut recorder = Recorder::spawn(recording, folder.clone()).unwrap();
        recorder.toggle();
        let started = loop {
            if let Some(event) = recorder.try_event() {
                break event;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let RecordEvent::Started { path } = started else {
            panic!("expected Started, got {started:?}");
        };
        // Half a second of audio, rendered faster than real time.
        let mut out = vec![0.0; 480 * 2];
        let mut rendered = Vec::new();
        for _ in 0..50 {
            engine.process(&mut out, 480);
            rendered.extend_from_slice(&out);
            std::thread::sleep(Duration::from_millis(2));
        }
        recorder.toggle();
        let stopped = loop {
            if let Some(event) = recorder.try_event() {
                break event;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let RecordEvent::Stopped {
            files,
            seconds,
            dropped_seconds,
        } = stopped
        else {
            panic!("expected Stopped, got {stopped:?}");
        };
        assert_eq!(files, vec![path.clone()]);
        assert_eq!(dropped_seconds, 0.0);
        assert!((seconds - 0.5).abs() < 1e-9, "{seconds}");
        let (rate, samples) = read_wav(&path);
        assert_eq!(rate, 48_000);
        assert_eq!(samples.len(), rendered.len());
        for (&got, &want) in samples.iter().zip(&rendered) {
            assert!((got - (want * 8_388_607.0).round() as i32).abs() <= 1);
        }
        assert!(samples.iter().any(|&s| s != 0));
        drop(recorder);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn leftover_part_files_are_repaired_on_start() {
        let folder = scratch("recover");
        std::fs::create_dir_all(&folder).unwrap();
        let part = part_path(&folder, "crashed", 1);
        let mut wav = WavWriter::create(&part, 44_100).unwrap();
        wav.write(&[0.5; 1000]).unwrap();
        // A crash: buffered data reaches the disk but the header still says 0 bytes,
        // plus a torn half frame at the end.
        wav.file.flush().unwrap();
        wav.file.get_mut().write_all(&[1, 2, 3, 4]).unwrap();
        drop(wav);

        let recovered = recover_parts(&folder);
        assert_eq!(recovered, vec![final_path(&folder, "crashed", 1)]);
        let (rate, samples) = read_wav(&recovered[0]);
        assert_eq!(rate, 44_100);
        assert_eq!(samples.len(), 1000);
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn names_and_clock() {
        let folder = scratch("names");
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(
            unique_stem(&folder, "2026-01-01 10-00-00"),
            "2026-01-01 10-00-00"
        );
        File::create(final_path(&folder, "2026-01-01 10-00-00", 1)).unwrap();
        assert_eq!(
            unique_stem(&folder, "2026-01-01 10-00-00"),
            "2026-01-01 10-00-00 (2)"
        );
        assert!(part_path(&folder, "x", 2).ends_with("x part 2.wav.part"));
        assert_eq!(clock(65.4), "1:05");
        assert_eq!(clock(3725.0), "1:02:05");
        assert_eq!(local_timestamp().len(), 19);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
