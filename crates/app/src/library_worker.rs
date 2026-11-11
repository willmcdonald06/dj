//! Background library maintenance, on its own thread with its own database connection:
//!
//! - **Scanning:** the music folders and any USB drives, for new, changed and removed
//!   tracks (only new or changed files are opened, to read their tags). Folders are
//!   rescanned every [`RESCAN_INTERVAL`] or on request; drives are looked for every
//!   [`DRIVE_POLL_INTERVAL`], and scanned when one appears.
//! - **Sidecars:** analysis files from the offline analyzer are imported as they appear
//!   or change.
//! - **Analysis:** tracks with no analysis are decoded and analysed one at a time, so
//!   the browser can show BPM and key for everything.
//!
//! Not RT-safe. Uses one core while analysing, at the lowest CPU priority. While a
//! deck plays ([`LibraryWorker::set_paused`]) analysis and the timed rescans wait:
//! decoding whole tracks allocates and frees hundreds of megabytes a minute, and on a
//! machine short of memory that stalls the audio thread long enough to drop out. A
//! RESCAN pressed by hand, or a drive plugged in, still scans at once.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dj_analysis::{ANALYSIS_VERSION, TrackAnalysis as Analysis};
use dj_engine::BeatGrid;
use dj_library::{AnalysisSource, Library, Sidecar, TrackAnalysis, TrackTags, sidecar_path};

const RESCAN_INTERVAL: Duration = Duration::from_secs(120);
const DRIVE_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Grids the in-app analysis is less sure of than this are not used for sync. The
/// confidence is the share of the track's onset weight on the grid's beats; measured
/// against a neural beat tracker, every right grid scored at least 0.18, and most
/// tracks below it have no steady tempo (live drummers, tempo changes).
pub const MIN_GRID_CONFIDENCE: f64 = 0.18;
/// Sample rate for background analysis (results are stored in seconds).
const ANALYSIS_RATE: u32 = 48_000;

/// A place tracks come from, for the browser's source list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub path: PathBuf,
    pub removable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    /// Tracks were added, changed or analysed: reload the browser's list.
    Changed,
    /// The folders and drives available, when they change.
    Sources(Vec<Source>),
    /// What the worker is doing, for the status line ("" when idle).
    Status(String),
}

enum Request {
    Rescan,
}

pub struct LibraryWorker {
    requests: mpsc::Sender<Request>,
    events: mpsc::Receiver<WorkerEvent>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LibraryWorker {
    pub fn spawn(database: PathBuf, folders: Vec<PathBuf>) -> std::io::Result<Self> {
        let folders = folders
            .into_iter()
            .map(|f| std::fs::canonicalize(&f).unwrap_or(f))
            .collect();
        let (requests, inbox) = mpsc::channel();
        let (outbox, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let paused = Arc::new(AtomicBool::new(false));
        let thread_paused = Arc::clone(&paused);
        let thread = std::thread::Builder::new()
            .name("library".into())
            .spawn(move || {
                lowest_priority();
                let library = match Library::open(&database) {
                    Ok(library) => library,
                    Err(err) => {
                        tracing::error!("library worker: {err}");
                        return;
                    }
                };
                Worker {
                    library,
                    folders,
                    drives: Vec::new(),
                    inbox,
                    outbox,
                    stop: thread_stop,
                    paused: thread_paused,
                }
                .run();
            })?;
        Ok(Self {
            requests,
            events,
            stop,
            paused,
            thread: Some(thread),
        })
    }

    /// Hold off background analysis and timed rescans (while a deck plays).
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn rescan(&self) {
        let _ = self.requests.send(Request::Rescan);
    }

    pub fn try_event(&self) -> Option<WorkerEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for LibraryWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.requests.send(Request::Rescan);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Worker {
    library: Library,
    folders: Vec<PathBuf>,
    drives: Vec<Source>,
    inbox: mpsc::Receiver<Request>,
    outbox: mpsc::Sender<WorkerEvent>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

/// Run this thread at the lowest CPU priority (Linux), so it always gives way.
fn lowest_priority() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: plain syscalls on this thread's own id; no memory is passed.
        let result = unsafe {
            let tid = libc::gettid();
            libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19)
        };
        if result != 0 {
            tracing::debug!("couldn't lower the library thread's priority");
        }
    }
}

impl Worker {
    fn run(mut self) {
        self.send(WorkerEvent::Sources(self.sources()));
        let mut next_scan = Instant::now();
        let mut next_drive_poll = Instant::now();
        // A scan asked for (RESCAN, a new drive): done even while paused.
        let mut scan_now = true;
        let mut said_paused = false;
        while !self.stopping() {
            let now = Instant::now();
            let paused = self.paused.load(Ordering::Relaxed);
            if now >= next_drive_poll {
                next_drive_poll = now + DRIVE_POLL_INTERVAL;
                let drives = removable_drives();
                if drives != self.drives {
                    self.drives = drives;
                    self.send(WorkerEvent::Sources(self.sources()));
                    scan_now = true;
                }
            }
            if scan_now || (now >= next_scan && !paused) {
                scan_now = false;
                self.scan_all();
                next_scan = Instant::now() + RESCAN_INTERVAL;
            }
            let analysed = match paused {
                true => {
                    let waiting = self
                        .library
                        .needing_analysis(ANALYSIS_VERSION, 1)
                        .is_ok_and(|next| !next.is_empty());
                    if !said_paused && waiting {
                        self.send(WorkerEvent::Status(
                            "Analysis paused while a deck plays".to_string(),
                        ));
                        said_paused = true;
                    }
                    false
                }
                false => {
                    if std::mem::take(&mut said_paused) {
                        self.send(WorkerEvent::Status(String::new()));
                    }
                    self.analyse_next()
                }
            };
            // With work left, keep going; when idle, wait for a request or a timer.
            let wait = match analysed {
                true => Duration::ZERO,
                false => DRIVE_POLL_INTERVAL,
            };
            match self.inbox.recv_timeout(wait) {
                Ok(Request::Rescan) => scan_now = true,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn send(&self, event: WorkerEvent) {
        let _ = self.outbox.send(event);
    }

    fn sources(&self) -> Vec<Source> {
        let folders = self.folders.iter().map(|path| Source {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            path: path.clone(),
            removable: false,
        });
        folders.chain(self.drives.iter().cloned()).collect()
    }

    fn scan_all(&mut self) {
        let roots: Vec<PathBuf> = self.sources().into_iter().map(|s| s.path).collect();
        for root in roots {
            if self.stopping() {
                return;
            }
            self.send(WorkerEvent::Status(format!("Scanning {}…", root.display())));
            match self.scan(&root) {
                Ok(()) => self.send(WorkerEvent::Changed),
                Err(err) => tracing::warn!("scanning {}: {err}", root.display()),
            }
        }
        self.send(WorkerEvent::Status(String::new()));
    }

    /// Scan one folder tree: add or update changed files, import new sidecars, and
    /// mark tracks that have gone as missing.
    fn scan(&mut self, root: &Path) -> Result<(), dj_library::LibraryError> {
        // Canonical paths throughout, matching how loaded tracks are looked up.
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let root = root.as_path();
        let mut files = Vec::new();
        collect_audio(root, &mut files);
        let now = unix_now();
        for (i, file) in files.iter().enumerate() {
            if self.stopping() {
                return Ok(());
            }
            let Ok(meta) = std::fs::metadata(file) else {
                continue;
            };
            let (size, mtime) = (meta.len(), mtime_of(&meta));
            if self.library.needs_scan(file, size, mtime)? {
                let tags = match dj_loader::read_tags(file) {
                    Ok(tags) => TrackTags {
                        title: tags.title,
                        artist: tags.artist,
                        album: tags.album,
                        duration: tags.duration,
                    },
                    Err(err) => {
                        tracing::debug!("{}: {err}", file.display());
                        continue;
                    }
                };
                self.library.upsert_scanned(file, size, mtime, &tags, now)?;
            }
            self.import_sidecar(file)?;
            if i % 200 == 199 {
                self.send(WorkerEvent::Changed);
            }
        }
        self.library.mark_missing(root, &files)?;
        Ok(())
    }

    /// Import `file`'s sidecar if it is new or has changed since the last import.
    fn import_sidecar(&mut self, file: &Path) -> Result<(), dj_library::LibraryError> {
        let path = sidecar_path(file);
        let Ok(meta) = std::fs::metadata(&path) else {
            return Ok(());
        };
        let mtime = mtime_of(&meta);
        if self.library.sidecar_mtime(file)? == Some(mtime) {
            return Ok(());
        }
        match Sidecar::read(&path) {
            Ok(sidecar) => {
                let analysis = TrackAnalysis {
                    bpm: sidecar.bpm,
                    grid_anchor: sidecar.first_downbeat,
                    key: sidecar.key(),
                };
                self.library.set_analysis(
                    file,
                    &analysis,
                    AnalysisSource::Sidecar,
                    sidecar.version,
                    Some(mtime),
                )?;
            }
            Err(err) => tracing::warn!("{err}"),
        }
        Ok(())
    }

    /// Analyse one track that needs it. Returns whether there was one.
    fn analyse_next(&mut self) -> bool {
        let next = match self.library.needing_analysis(ANALYSIS_VERSION, 1) {
            Ok(next) => next,
            Err(err) => {
                tracing::warn!("{err}");
                return false;
            }
        };
        let Some(path) = next.into_iter().next() else {
            return false;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.send(WorkerEvent::Status(format!("Analysing {name}…")));
        let analysis = match dj_loader::analyze_file(&path, ANALYSIS_RATE) {
            Ok(analysis) => to_library(&analysis),
            Err(err) => {
                tracing::debug!("analysing {}: {err}", path.display());
                // Store an empty analysis so a broken file isn't retried forever.
                TrackAnalysis::default()
            }
        };
        if let Err(err) = self.library.set_analysis(
            &path,
            &analysis,
            AnalysisSource::App,
            ANALYSIS_VERSION,
            None,
        ) {
            tracing::warn!("{err}");
        }
        self.send(WorkerEvent::Changed);
        self.send(WorkerEvent::Status(String::new()));
        true
    }
}

/// The in-app analysis in library terms (seconds, pitch classes).
pub fn to_library(analysis: &Analysis) -> TrackAnalysis {
    let rate = analysis.sample_rate as f64;
    let grid = trusted_grid(analysis);
    TrackAnalysis {
        bpm: grid.map(|g| g.bpm(analysis.sample_rate)),
        grid_anchor: grid.map(|g| g.anchor / rate),
        key: analysis
            .key
            .map(|k| (k.key.tonic, matches!(k.key.mode, dj_analysis::Mode::Minor))),
    }
}

/// The in-app grid, if the analysis is sure enough of it.
pub fn trusted_grid(analysis: &Analysis) -> Option<BeatGrid> {
    analysis
        .grid
        .filter(|g| g.confidence >= MIN_GRID_CONFIDENCE)
        .map(|g| g.grid)
}

/// An audio file to list in the library (stems files are audio but belong to their
/// track).
fn is_audio(path: &Path) -> bool {
    !dj_loader::is_stems_file(path)
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| dj_loader::AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// All audio files under `dir`, skipping hidden files and folders (macOS `._` files,
/// `.Trashes` and the like).
fn collect_audio(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_audio(&path, out),
            Ok(kind) if kind.is_file() && is_audio(&path) => out.push(path),
            _ => {}
        }
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Mounted removable drives (USB sticks, SD cards).
fn removable_drives() -> Vec<Source> {
    let mut drives = Vec::new();
    #[cfg(target_os = "linux")]
    {
        // Desktop automounters put drives under /run/media/$USER or /media/$USER;
        // a bare /media/<name> is common on single-user systems like the CM5.
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let Some(point) = line.split_whitespace().nth(1) else {
                    continue;
                };
                // /proc/mounts escapes spaces as \040.
                let point = point.replace("\\040", " ");
                if point.starts_with("/run/media/") || point.starts_with("/media/") {
                    drives.push(PathBuf::from(point));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let path = entry.path();
                // The system disk shows up here as a link to /.
                if std::fs::canonicalize(&path).is_ok_and(|p| p != Path::new("/")) {
                    drives.push(path);
                }
            }
        }
    }
    drives.sort();
    drives
        .into_iter()
        .map(|path| Source {
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path,
            removable: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_audio_and_skips_hidden_files() {
        let dir = std::env::temp_dir().join(format!("dj-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join(".hidden")).unwrap();
        for name in [
            "a.mp3",
            "sub/b.FLAC",
            "notes.txt",
            "._a.mp3",
            ".hidden/c.mp3",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        let mut found = Vec::new();
        collect_audio(&dir, &mut found);
        let names: Vec<String> = found
            .iter()
            .map(|p| p.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert_eq!(names, ["a.mp3", "sub/b.FLAC"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
