//! Track loading: decode a file, convert it to the engine sample rate, and hand it to
//! the audio thread.
//!
//! [`Loader`] runs a manager thread that receives load requests, starts a worker thread
//! per load, and passes each track to the engine through its [`TrackSender`] as soon as
//! the first audio is decoded (decoding then continues in the background). A new load
//! onto a deck cancels the previous one still decoding for that deck. Once decoded, the
//! track is analysed (waveform summary and kick markers) on the same worker thread.
//!
//! # Freeing tracks off the audio thread
//!
//! The audio thread must never free memory, but it is the one that lets go of a track
//! when another is loaded or the deck is ejected. So the manager keeps its own `Arc` to
//! every track it hands over, and periodically drops the ones nobody else holds. The
//! audio thread's drop is then only a reference-count decrement; the free happens here.
//!
//! Not RT-safe.

mod decode;
mod resample;

pub use decode::{DecodeStats, FileTags, LoadError, TrackInfo, decode_file, read_tags};

/// File extensions the decoder handles, lower case.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "aiff", "aif", "m4a", "mp4", "ogg"];

/// Decode and analyse a whole file (for background library analysis). Not RT-safe;
/// takes a few hundred milliseconds per track and allocates the decoded track.
pub fn analyze_file(
    path: &std::path::Path,
    sample_rate: u32,
) -> Result<dj_analysis::TrackAnalysis, LoadError> {
    let never = std::sync::atomic::AtomicBool::new(false);
    let mut decoded = None;
    decode_file(
        path,
        dj_engine::TrackId(0),
        sample_rate,
        &never,
        &mut |track, _| decoded = Some(track),
    )?;
    let track = decoded.ok_or(LoadError::Empty)?;
    Ok(dj_analysis::analyze(&track))
}
pub use resample::{ResampleError, StereoResampler};

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use dj_analysis::TrackAnalysis;
use dj_engine::{DeckId, Track, TrackId, TrackSender};

/// How often the manager checks for tracks it can free when nothing else happens.
const COLLECT_INTERVAL: Duration = Duration::from_millis(250);

/// Results of load requests, for the UI.
#[derive(Debug)]
pub enum LoadEvent {
    /// The track is on the deck and playable; decoding may still be running.
    Loaded {
        deck: DeckId,
        info: TrackInfo,
        path: PathBuf,
        /// Time from the request to playable.
        latency: Duration,
    },
    /// Analysis of a fully decoded track is ready.
    Analyzed {
        deck: DeckId,
        id: TrackId,
        analysis: Arc<TrackAnalysis>,
        elapsed: Duration,
    },
    /// Decoding finished.
    Finished {
        deck: DeckId,
        id: TrackId,
        stats: DecodeStats,
    },
    Failed {
        deck: DeckId,
        path: PathBuf,
        error: LoadError,
    },
}

enum Message {
    Load {
        deck: DeckId,
        path: PathBuf,
        requested: Instant,
    },
    Playable {
        deck: DeckId,
        track: Arc<Track>,
        info: TrackInfo,
        path: PathBuf,
        requested: Instant,
    },
    Analyzed {
        deck: DeckId,
        id: TrackId,
        analysis: Arc<TrackAnalysis>,
        elapsed: Duration,
    },
    Done {
        deck: DeckId,
        id: TrackId,
        path: PathBuf,
        result: Result<DecodeStats, LoadError>,
    },
    Shutdown,
}

/// Handle to the loader thread. Dropping it stops the thread.
pub struct Loader {
    requests: mpsc::Sender<Message>,
    events: mpsc::Receiver<LoadEvent>,
    thread: Option<JoinHandle<()>>,
}

impl Loader {
    /// Start the loader. `tracks` is the engine's track queue; `engine_rate` is the
    /// rate tracks are converted to.
    pub fn spawn(engine_rate: u32, tracks: TrackSender) -> std::io::Result<Self> {
        let (requests, inbox) = mpsc::channel();
        let (events_out, events) = mpsc::channel();
        let manager = Manager {
            engine_rate,
            tracks,
            inbox,
            outbox: requests.clone(),
            events: events_out,
            next_id: 1,
            jobs: [None, None],
            held: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("loader".into())
            .spawn(move || manager.run())?;
        Ok(Self {
            requests,
            events,
            thread: Some(thread),
        })
    }

    /// Load `path` onto `deck`. The result arrives as a [`LoadEvent`].
    pub fn load(&self, deck: DeckId, path: PathBuf) {
        let message = Message::Load {
            deck,
            path,
            requested: Instant::now(),
        };
        if self.requests.send(message).is_err() {
            tracing::error!("loader thread has stopped");
        }
    }

    /// The next load event, if any. Never blocks.
    pub fn try_event(&self) -> Option<LoadEvent> {
        self.events.try_recv().ok()
    }

    /// Wait up to `timeout` for the next load event.
    pub fn wait_event(&self, timeout: Duration) -> Option<LoadEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        let _ = self.requests.send(Message::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Job {
    id: TrackId,
    cancel: Arc<AtomicBool>,
}

struct Manager {
    engine_rate: u32,
    tracks: TrackSender,
    inbox: mpsc::Receiver<Message>,
    /// For workers to report back.
    outbox: mpsc::Sender<Message>,
    events: mpsc::Sender<LoadEvent>,
    next_id: u64,
    /// The current load for each deck.
    jobs: [Option<Job>; 2],
    /// Every track handed to the engine and not yet freed.
    held: Vec<Arc<Track>>,
}

impl Manager {
    fn run(mut self) {
        loop {
            match self.inbox.recv_timeout(COLLECT_INTERVAL) {
                Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(message) => self.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.collect();
        }
        for job in self.jobs.iter().flatten() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Load {
                deck,
                path,
                requested,
            } => self.start(deck, path, requested),
            Message::Playable {
                deck,
                track,
                info,
                path,
                requested,
            } => {
                if !self.is_current(deck, info.id) {
                    return;
                }
                self.held.push(Arc::clone(&track));
                if let Err(load) = self.tracks.send(deck, track) {
                    tracing::error!("engine track queue full; dropping load of {:?}", load.deck);
                    return;
                }
                self.emit(LoadEvent::Loaded {
                    deck,
                    info,
                    path,
                    latency: requested.elapsed(),
                });
            }
            Message::Analyzed {
                deck,
                id,
                analysis,
                elapsed,
            } => {
                if self.is_current(deck, id) {
                    self.emit(LoadEvent::Analyzed {
                        deck,
                        id,
                        analysis,
                        elapsed,
                    });
                }
            }
            Message::Done {
                deck,
                id,
                path,
                result,
            } => {
                if self.is_current(deck, id) {
                    self.jobs[deck.index()] = None;
                }
                let event = match result {
                    Ok(stats) => LoadEvent::Finished { deck, id, stats },
                    Err(LoadError::Cancelled) => return,
                    Err(error) => LoadEvent::Failed { deck, path, error },
                };
                self.emit(event);
            }
            Message::Shutdown => {}
        }
    }

    fn start(&mut self, deck: DeckId, path: PathBuf, requested: Instant) {
        if let Some(previous) = self.jobs[deck.index()].take() {
            previous.cancel.store(true, Ordering::Relaxed);
        }
        let id = TrackId(self.next_id);
        self.next_id += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.jobs[deck.index()] = Some(Job {
            id,
            cancel: Arc::clone(&cancel),
        });

        let outbox = self.outbox.clone();
        let engine_rate = self.engine_rate;
        let spawned = std::thread::Builder::new()
            .name(format!("decode-{deck:?}"))
            .spawn(move || {
                let mut decoded = None;
                let mut on_playable = |track: Arc<Track>, info: TrackInfo| {
                    decoded = Some(Arc::clone(&track));
                    let _ = outbox.send(Message::Playable {
                        deck,
                        track,
                        info,
                        path: path.clone(),
                        requested,
                    });
                };
                let result = decode_file(&path, id, engine_rate, &cancel, &mut on_playable);
                if let (Ok(_), Some(track)) = (&result, decoded) {
                    if !cancel.load(Ordering::Relaxed) {
                        let started = Instant::now();
                        let analysis = Arc::new(dj_analysis::analyze(&track));
                        let _ = outbox.send(Message::Analyzed {
                            deck,
                            id,
                            analysis,
                            elapsed: started.elapsed(),
                        });
                    }
                }
                let _ = outbox.send(Message::Done {
                    deck,
                    id,
                    path,
                    result,
                });
            });
        if let Err(err) = spawned {
            tracing::error!("cannot start decode thread: {err}");
            self.jobs[deck.index()] = None;
        }
    }

    fn is_current(&self, deck: DeckId, id: TrackId) -> bool {
        matches!(&self.jobs[deck.index()], Some(job) if job.id == id)
    }

    /// Free tracks that only we still hold.
    fn collect(&mut self) {
        self.held.retain(|track| Arc::strong_count(track) > 1);
    }

    fn emit(&self, event: LoadEvent) {
        // The UI may have gone away during shutdown; nothing to do then.
        let _ = self.events.send(event);
    }
}
