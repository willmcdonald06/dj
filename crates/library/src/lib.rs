//! Track library, stored in SQLite.
//!
//! Holds every track found by scanning (tags, duration, size and modification time),
//! its analysis (BPM, beatgrid, key: from a sidecar written by the offline analyzer, or
//! from the app's own analysis), and its hot cues and saved loops. Tracks are keyed by
//! their file path. Every change is written at once (in a transaction), so nothing is
//! lost if the app or the power goes.
//!
//! Several connections may be open at once (the UI and the background scanner each
//! have one); SQLite's WAL mode lets them read while another writes.
//!
//! Positions are stored in seconds, so they don't depend on the engine's sample rate.
//!
//! Not RT-safe; never called from the audio thread.

pub mod sidecar;

pub use sidecar::{SIDECAR_SUFFIX, Sidecar, SidecarError, sidecar_path};

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

/// Schema version, kept in SQLite's `user_version`.
const SCHEMA_VERSION: i32 = 4;
/// How long a connection waits for another's write to finish.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("library database: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("cannot create library folder {path}: {source}")]
    Folder {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("library at {0} is from a newer version of this app")]
    TooNew(PathBuf),
}

/// What a hot cue slot holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CueKind {
    Point { seconds: f64 },
    Loop { start: f64, end: f64 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoredCue {
    pub slot: u8,
    pub kind: CueKind,
}

/// Tags and length read from a file when it is scanned.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds.
    pub duration: Option<f64>,
}

/// Where a track's analysis came from. Sidecars win over the app's own analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisSource {
    App,
    Sidecar,
}

/// BPM, beatgrid and key, in seconds and pitch classes (no sample rate involved).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrackAnalysis {
    pub bpm: Option<f64>,
    /// A beat (the first downbeat, from sidecars) in seconds; with `bpm`, the grid.
    pub grid_anchor: Option<f64>,
    /// Pitch class (0 = C) and whether the key is minor.
    pub key: Option<(u8, bool)>,
}

/// One row of the library, as the browser shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: i64,
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub analysis: TrackAnalysis,
    pub analysis_source: Option<AnalysisSource>,
    /// When it was first scanned (Unix seconds).
    pub added: i64,
}

/// A crate (an unordered set of tracks) or a playlist (an ordered list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollectionKind {
    Crate,
    Playlist,
}

impl CollectionKind {
    fn as_str(self) -> &'static str {
        match self {
            CollectionKind::Crate => "crate",
            CollectionKind::Playlist => "playlist",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub kind: CollectionKind,
    /// Tracks in it.
    pub count: usize,
}

pub struct Library {
    conn: Connection,
}

impl Library {
    /// Where the library lives by default: `$XDG_DATA_HOME/dj/library.db` (usually
    /// `~/.local/share/dj/`) on Linux, `~/Library/Application Support/dj/` on macOS.
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let base = match cfg!(target_os = "macos") {
            true => home.join("Library/Application Support"),
            false => std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/share")),
        };
        base.join("dj").join("library.db")
    }

    /// Open (creating if needed) the library at `path`.
    pub fn open(path: &Path) -> Result<Self, LibraryError> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder).map_err(|source| LibraryError::Folder {
                path: folder.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        // WAL: writes are quick, readers don't block, and a crash mid-write can't
        // corrupt the database.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let library = Self { conn };
        library.migrate(path)?;
        Ok(library)
    }

    /// A throwaway library, for tests.
    pub fn open_in_memory() -> Result<Self, LibraryError> {
        let library = Self {
            conn: Connection::open_in_memory()?,
        };
        library.migrate(Path::new(":memory:"))?;
        Ok(library)
    }

    fn migrate(&self, path: &Path) -> Result<(), LibraryError> {
        self.conn.pragma_update(None, "foreign_keys", true)?;
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(LibraryError::TooNew(path.to_path_buf()));
        }
        if version < 1 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE tracks (
                     id   INTEGER PRIMARY KEY,
                     path TEXT NOT NULL UNIQUE
                 );
                 CREATE TABLE hot_cues (
                     track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                     slot     INTEGER NOT NULL,
                     kind     TEXT    NOT NULL CHECK (kind IN ('point', 'loop')),
                     start    REAL    NOT NULL,
                     end      REAL,
                     PRIMARY KEY (track_id, slot)
                 );
                                  PRAGMA user_version = 1;
                 COMMIT;",
            )?;
        }
        if version < 2 {
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE tracks ADD COLUMN size INTEGER;
                 ALTER TABLE tracks ADD COLUMN mtime INTEGER;
                 ALTER TABLE tracks ADD COLUMN title TEXT;
                 ALTER TABLE tracks ADD COLUMN artist TEXT;
                 ALTER TABLE tracks ADD COLUMN album TEXT;
                 ALTER TABLE tracks ADD COLUMN duration REAL;
                 ALTER TABLE tracks ADD COLUMN bpm REAL;
                 ALTER TABLE tracks ADD COLUMN grid_anchor REAL;
                 ALTER TABLE tracks ADD COLUMN key_pitch INTEGER;
                 ALTER TABLE tracks ADD COLUMN key_minor INTEGER;
                 ALTER TABLE tracks ADD COLUMN analysis_source TEXT;
                 ALTER TABLE tracks ADD COLUMN analysis_version INTEGER;
                 ALTER TABLE tracks ADD COLUMN sidecar_mtime INTEGER;
                 ALTER TABLE tracks ADD COLUMN added INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE tracks ADD COLUMN missing INTEGER NOT NULL DEFAULT 0;
                                  PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }
        if version < 3 {
            self.conn.execute_batch(
                "BEGIN;
                 CREATE TABLE collections (
                     id      INTEGER PRIMARY KEY,
                     name    TEXT    NOT NULL,
                     kind    TEXT    NOT NULL CHECK (kind IN ('crate', 'playlist')),
                     created INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE collection_tracks (
                     collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
                     track_id      INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                     position      INTEGER NOT NULL,
                     PRIMARY KEY (collection_id, track_id)
                 );
                 PRAGMA user_version = 3;
                 COMMIT;",
            )?;
        }
        if version < 4 {
            // A beatgrid set by hand: kept apart from the analysis, which can be
            // redone, and preferred over it.
            self.conn.execute_batch(
                "BEGIN;
                 ALTER TABLE tracks ADD COLUMN user_bpm REAL;
                 ALTER TABLE tracks ADD COLUMN user_grid_anchor REAL;
                 PRAGMA user_version = 4;
                 COMMIT;",
            )?;
        }
        Ok(())
    }

    /// All crates and playlists, by kind then name.
    pub fn collections(&self) -> Result<Vec<Collection>, LibraryError> {
        let mut statement = self.conn.prepare(
            "SELECT c.id, c.name, c.kind, COUNT(t.track_id) FROM collections c
             LEFT JOIN collection_tracks t ON t.collection_id = c.id
             GROUP BY c.id ORDER BY c.kind, c.name COLLATE NOCASE, c.id",
        )?;
        let rows = statement.query_map([], |r| {
            let kind: String = r.get(2)?;
            Ok(Collection {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: match kind.as_str() {
                    "playlist" => CollectionKind::Playlist,
                    _ => CollectionKind::Crate,
                },
                count: r.get::<_, i64>(3)? as usize,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn create_collection(
        &mut self,
        name: &str,
        kind: CollectionKind,
        now: i64,
    ) -> Result<i64, LibraryError> {
        self.conn.execute(
            "INSERT INTO collections (name, kind, created) VALUES (?1, ?2, ?3)",
            params![name, kind.as_str(), now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_collection(&mut self, id: i64, name: &str) -> Result<(), LibraryError> {
        self.conn.execute(
            "UPDATE collections SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Delete a crate or playlist (its tracks stay in the library).
    pub fn delete_collection(&mut self, id: i64) -> Result<(), LibraryError> {
        self.conn
            .execute("DELETE FROM collections WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Add a track at the end. Returns false if it was already there.
    pub fn add_to_collection(&mut self, id: i64, track: i64) -> Result<bool, LibraryError> {
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO collection_tracks (collection_id, track_id, position)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1
                              FROM collection_tracks WHERE collection_id = ?1))",
            params![id, track],
        )?;
        Ok(added > 0)
    }

    pub fn remove_from_collection(&mut self, id: i64, track: i64) -> Result<(), LibraryError> {
        self.conn.execute(
            "DELETE FROM collection_tracks WHERE collection_id = ?1 AND track_id = ?2",
            params![id, track],
        )?;
        Ok(())
    }

    /// Track ids in a collection, in order.
    pub fn collection_tracks(&self, id: i64) -> Result<Vec<i64>, LibraryError> {
        let mut statement = self.conn.prepare(
            "SELECT track_id FROM collection_tracks WHERE collection_id = ?1 ORDER BY position",
        )?;
        let rows = statement.query_map([id], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Move a track one place earlier (`delta` -1) or later (+1) in a playlist.
    pub fn move_in_collection(
        &mut self,
        id: i64,
        track: i64,
        delta: i32,
    ) -> Result<(), LibraryError> {
        let order = self.collection_tracks(id)?;
        let Some(from) = order.iter().position(|&t| t == track) else {
            return Ok(());
        };
        let to = (from as i64 + delta as i64).clamp(0, order.len() as i64 - 1) as usize;
        if to == from {
            return Ok(());
        }
        let mut order = order;
        let moved = order.remove(from);
        order.insert(to, moved);
        let tx = self.conn.transaction()?;
        for (position, track) in order.iter().enumerate() {
            tx.execute(
                "UPDATE collection_tracks SET position = ?3 WHERE collection_id = ?1 AND track_id = ?2",
                params![id, track, position as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether the file at `path` needs (re)reading: new, or changed since last scan.
    pub fn needs_scan(&self, path: &Path, size: u64, mtime: i64) -> Result<bool, LibraryError> {
        let known: Option<(Option<i64>, Option<i64>, i64)> = self
            .conn
            .query_row(
                "SELECT size, mtime, missing FROM tracks WHERE path = ?1",
                [path.to_string_lossy()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(match known {
            Some((Some(s), Some(m), 0)) => s != size as i64 || m != mtime,
            _ => true,
        })
    }

    /// Record a scanned file. If its contents changed, its analysis from the app is
    /// dropped (a sidecar's stays until the sidecar is re-imported).
    pub fn upsert_scanned(
        &mut self,
        path: &Path,
        size: u64,
        mtime: i64,
        tags: &TrackTags,
        now: i64,
    ) -> Result<(), LibraryError> {
        let tx = self.conn.transaction()?;
        let previous: Option<(Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT size, mtime FROM tracks WHERE path = ?1",
                [path.to_string_lossy()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match previous {
            None => {
                tx.execute(
                    "INSERT INTO tracks (path, size, mtime, title, artist, album, duration, added)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        path.to_string_lossy(),
                        size as i64,
                        mtime,
                        tags.title,
                        tags.artist,
                        tags.album,
                        tags.duration,
                        now
                    ],
                )?;
            }
            Some((old_size, old_mtime)) => {
                let changed = old_size != Some(size as i64) || old_mtime != Some(mtime);
                tx.execute(
                    "UPDATE tracks SET size = ?2, mtime = ?3, title = ?4, artist = ?5,
                         album = ?6, duration = ?7, missing = 0,
                         added = CASE WHEN added = 0 THEN ?8 ELSE added END
                     WHERE path = ?1",
                    params![
                        path.to_string_lossy(),
                        size as i64,
                        mtime,
                        tags.title,
                        tags.artist,
                        tags.album,
                        tags.duration,
                        now
                    ],
                )?;
                if changed && old_size.is_some() {
                    tx.execute(
                        "UPDATE tracks SET bpm = NULL, grid_anchor = NULL, key_pitch = NULL,
                             key_minor = NULL, analysis_source = NULL, analysis_version = NULL
                         WHERE path = ?1 AND analysis_source = 'app'",
                        [path.to_string_lossy()],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark tracks under `root` that weren't seen in a scan as missing (they stay in
    /// the database, with their cues, in case the file comes back).
    pub fn mark_missing(&mut self, root: &Path, seen: &[PathBuf]) -> Result<usize, LibraryError> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS seen (path TEXT PRIMARY KEY); DELETE FROM seen;",
        )?;
        {
            let mut insert = tx.prepare("INSERT OR IGNORE INTO seen (path) VALUES (?1)")?;
            for path in seen {
                insert.execute([path.to_string_lossy()])?;
            }
        }
        let prefix = format!("{}/", root.to_string_lossy().trim_end_matches('/'));
        let changed = tx.execute(
            "UPDATE tracks SET missing = 1
             WHERE missing = 0 AND substr(path, 1, length(?1)) = ?1
               AND path NOT IN (SELECT path FROM seen)",
            [prefix],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// The modification time of the sidecar last imported for `path`, if any.
    pub fn sidecar_mtime(&self, path: &Path) -> Result<Option<i64>, LibraryError> {
        let mtime = self
            .conn
            .query_row(
                "SELECT sidecar_mtime FROM tracks WHERE path = ?1",
                [path.to_string_lossy()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(mtime)
    }

    /// Store a track's analysis. The app's own never replaces a sidecar's.
    pub fn set_analysis(
        &mut self,
        path: &Path,
        analysis: &TrackAnalysis,
        source: AnalysisSource,
        version: u32,
        sidecar_mtime: Option<i64>,
    ) -> Result<(), LibraryError> {
        let (pitch, minor) = match analysis.key {
            Some((pitch, minor)) => (Some(pitch as i64), Some(minor as i64)),
            None => (None, None),
        };
        let source_text = match source {
            AnalysisSource::App => "app",
            AnalysisSource::Sidecar => "sidecar",
        };
        self.conn.execute(
            "UPDATE tracks SET bpm = ?2, grid_anchor = ?3, key_pitch = ?4, key_minor = ?5,
                 analysis_source = ?6, analysis_version = ?7,
                 sidecar_mtime = COALESCE(?8, sidecar_mtime)
             WHERE path = ?1 AND (?6 = 'sidecar' OR analysis_source IS NULL
                                  OR analysis_source = 'app')",
            params![
                path.to_string_lossy(),
                analysis.bpm,
                analysis.grid_anchor,
                pitch,
                minor,
                source_text,
                version,
                sidecar_mtime
            ],
        )?;
        Ok(())
    }

    /// Paths with no analysis (or the app's from an older analyzer version), oldest
    /// first, at most `limit`.
    pub fn needing_analysis(
        &self,
        version: u32,
        limit: usize,
    ) -> Result<Vec<PathBuf>, LibraryError> {
        let mut statement = self.conn.prepare(
            "SELECT path FROM tracks
             WHERE missing = 0 AND (analysis_source IS NULL
                   OR (analysis_source = 'app' AND analysis_version < ?1))
             ORDER BY added, id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![version, limit as i64], |r| {
            Ok(PathBuf::from(r.get::<_, String>(0)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every track that is present, in no particular order.
    pub fn tracks(&self) -> Result<Vec<Track>, LibraryError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {TRACK_COLUMNS} FROM tracks WHERE missing = 0 AND size IS NOT NULL"
        ))?;
        let rows = statement.query_map([], track_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The track at `path`, if scanned.
    pub fn track(&self, path: &Path) -> Result<Option<Track>, LibraryError> {
        let track = self
            .conn
            .query_row(
                &format!("SELECT {TRACK_COLUMNS} FROM tracks WHERE path = ?1"),
                [path.to_string_lossy()],
                track_from_row,
            )
            .optional()?;
        Ok(track)
    }

    fn track_id(&self, path: &Path) -> Result<Option<i64>, LibraryError> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM tracks WHERE path = ?1",
                [path.to_string_lossy()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// The hot cues saved for the track at `path`, by slot.
    /// The beatgrid set by hand for `path`, if any: BPM and a downbeat in seconds.
    pub fn user_grid(&self, path: &Path) -> Result<Option<(f64, f64)>, LibraryError> {
        let grid = self
            .conn
            .query_row(
                "SELECT user_bpm, user_grid_anchor FROM tracks WHERE path = ?1",
                [path.to_string_lossy()],
                |r| Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<f64>>(1)?)),
            )
            .optional()?;
        Ok(match grid {
            Some((Some(bpm), Some(anchor))) => Some((bpm, anchor)),
            _ => None,
        })
    }

    /// Keep (or with `None`, forget) a beatgrid set by hand for `path`: BPM and a
    /// downbeat in seconds. It survives re-analysis and sidecars.
    pub fn set_user_grid(
        &mut self,
        path: &Path,
        grid: Option<(f64, f64)>,
    ) -> Result<(), LibraryError> {
        let (bpm, anchor) = match grid {
            Some((bpm, anchor)) => (Some(bpm), Some(anchor)),
            None => (None, None),
        };
        self.conn.execute(
            "UPDATE tracks SET user_bpm = ?2, user_grid_anchor = ?3 WHERE path = ?1",
            params![path.to_string_lossy(), bpm, anchor],
        )?;
        Ok(())
    }

    pub fn hot_cues(&self, path: &Path) -> Result<Vec<StoredCue>, LibraryError> {
        let Some(track) = self.track_id(path)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.conn.prepare(
            "SELECT slot, kind, start, end FROM hot_cues WHERE track_id = ?1 ORDER BY slot",
        )?;
        let rows = statement.query_map([track], |r| {
            let slot: u8 = r.get(0)?;
            let kind: String = r.get(1)?;
            let start: f64 = r.get(2)?;
            let end: Option<f64> = r.get(3)?;
            let kind = match (kind.as_str(), end) {
                ("loop", Some(end)) => CueKind::Loop { start, end },
                _ => CueKind::Point { seconds: start },
            };
            Ok(StoredCue { slot, kind })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Replace all hot cues for the track at `path`.
    pub fn save_hot_cues(&mut self, path: &Path, cues: &[StoredCue]) -> Result<(), LibraryError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO tracks (path) VALUES (?1) ON CONFLICT(path) DO NOTHING",
            [path.to_string_lossy()],
        )?;
        let track: i64 = tx.query_row(
            "SELECT id FROM tracks WHERE path = ?1",
            [path.to_string_lossy()],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM hot_cues WHERE track_id = ?1", [track])?;
        for cue in cues {
            let (kind, start, end) = match cue.kind {
                CueKind::Point { seconds } => ("point", seconds, None),
                CueKind::Loop { start, end } => ("loop", start, Some(end)),
            };
            tx.execute(
                "INSERT INTO hot_cues (track_id, slot, kind, start, end) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![track, cue.slot, kind, start, end],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// A grid set by hand wins over the analysis's.
const TRACK_COLUMNS: &str = "id, path, title, artist, album, duration, \
                             COALESCE(user_bpm, bpm), COALESCE(user_grid_anchor, grid_anchor), \
                             key_pitch, key_minor, analysis_source, added";

fn track_from_row(r: &Row<'_>) -> rusqlite::Result<Track> {
    let path = PathBuf::from(r.get::<_, String>(1)?);
    let title: Option<String> = r.get(2)?;
    let title = title.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let key = match (r.get::<_, Option<i64>>(8)?, r.get::<_, Option<i64>>(9)?) {
        (Some(pitch), Some(minor)) => Some(((pitch % 12) as u8, minor != 0)),
        _ => None,
    };
    let source = match r.get::<_, Option<String>>(10)?.as_deref() {
        Some("sidecar") => Some(AnalysisSource::Sidecar),
        Some("app") => Some(AnalysisSource::App),
        _ => None,
    };
    Ok(Track {
        id: r.get(0)?,
        path,
        title,
        artist: r.get(3)?,
        album: r.get(4)?,
        duration: r.get(5)?,
        analysis: TrackAnalysis {
            bpm: r.get(6)?,
            grid_anchor: r.get(7)?,
            key,
        },
        analysis_source: source,
        added: r.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_cues_round_trip_and_are_replaced() {
        let mut library = Library::open_in_memory().unwrap();
        let track = Path::new("/music/a.flac");
        assert!(library.hot_cues(track).unwrap().is_empty());
        let cues = [
            StoredCue {
                slot: 0,
                kind: CueKind::Point { seconds: 1.5 },
            },
            StoredCue {
                slot: 3,
                kind: CueKind::Loop {
                    start: 10.0,
                    end: 12.0,
                },
            },
        ];
        library.save_hot_cues(track, &cues).unwrap();
        assert_eq!(library.hot_cues(track).unwrap(), cues);
        library.save_hot_cues(track, &cues[1..]).unwrap();
        assert_eq!(library.hot_cues(track).unwrap(), &cues[1..]);
        assert!(
            library
                .hot_cues(Path::new("/music/b.flac"))
                .unwrap()
                .is_empty()
        );
    }

    fn tags(title: &str) -> TrackTags {
        TrackTags {
            title: Some(title.into()),
            artist: Some("Artist".into()),
            album: None,
            duration: Some(200.0),
        }
    }

    #[test]
    fn scanning_adds_updates_and_marks_missing() {
        let mut library = Library::open_in_memory().unwrap();
        let a = Path::new("/music/a.mp3");
        let b = Path::new("/music/sub/b.flac");
        assert!(library.needs_scan(a, 10, 1).unwrap());
        library.upsert_scanned(a, 10, 1, &tags("A"), 100).unwrap();
        library
            .upsert_scanned(b, 20, 1, &TrackTags::default(), 100)
            .unwrap();
        assert!(!library.needs_scan(a, 10, 1).unwrap());
        assert!(library.needs_scan(a, 11, 1).unwrap());

        let mut tracks = library.tracks().unwrap();
        tracks.sort_by_key(|t| t.id);
        assert_eq!(tracks[0].title, "A");
        assert_eq!(tracks[1].title, "b", "no tag: file name");

        assert_eq!(
            library
                .mark_missing(Path::new("/music"), &[a.to_path_buf()])
                .unwrap(),
            1
        );
        assert_eq!(library.tracks().unwrap().len(), 1);
        assert!(
            library.needs_scan(b, 20, 1).unwrap(),
            "missing files are rescanned"
        );
        library
            .upsert_scanned(b, 20, 1, &TrackTags::default(), 200)
            .unwrap();
        assert_eq!(library.tracks().unwrap().len(), 2, "back again");
    }

    #[test]
    fn sidecar_analysis_wins_and_app_analysis_is_redone_when_needed() {
        let mut library = Library::open_in_memory().unwrap();
        let a = Path::new("/music/a.mp3");
        library.upsert_scanned(a, 10, 1, &tags("A"), 100).unwrap();
        assert_eq!(library.needing_analysis(3, 10).unwrap(), [a.to_path_buf()]);

        let app = TrackAnalysis {
            bpm: Some(127.9),
            grid_anchor: None,
            key: Some((9, true)),
        };
        library
            .set_analysis(a, &app, AnalysisSource::App, 3, None)
            .unwrap();
        assert!(library.needing_analysis(3, 10).unwrap().is_empty());
        assert_eq!(
            library.needing_analysis(4, 10).unwrap().len(),
            1,
            "newer analyzer"
        );

        let sidecar = TrackAnalysis {
            bpm: Some(128.0),
            grid_anchor: Some(0.5),
            key: Some((0, false)),
        };
        library
            .set_analysis(a, &sidecar, AnalysisSource::Sidecar, 1, Some(55))
            .unwrap();
        library
            .set_analysis(a, &app, AnalysisSource::App, 4, None)
            .unwrap();
        let track = library.track(a).unwrap().unwrap();
        assert_eq!(
            track.analysis, sidecar,
            "the app doesn't overwrite a sidecar"
        );
        assert_eq!(track.analysis_source, Some(AnalysisSource::Sidecar));
        assert_eq!(library.sidecar_mtime(a).unwrap(), Some(55));
        assert!(library.needing_analysis(4, 10).unwrap().is_empty());

        // A changed file loses the app's analysis but keeps the sidecar's.
        library.upsert_scanned(a, 99, 2, &tags("A"), 300).unwrap();
        assert_eq!(library.track(a).unwrap().unwrap().analysis, sidecar);
    }

    #[test]
    fn version_1_libraries_are_upgraded_keeping_cues() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tracks (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
             CREATE TABLE hot_cues (track_id INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                 slot INTEGER NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('point', 'loop')),
                 start REAL NOT NULL, end REAL, PRIMARY KEY (track_id, slot));
             INSERT INTO tracks (path) VALUES ('/m/a.mp3');
             INSERT INTO hot_cues VALUES (1, 0, 'point', 1.5, NULL);
             PRAGMA user_version = 1;",
        )
        .unwrap();
        let library = Library { conn };
        library.migrate(Path::new(":memory:")).unwrap();
        assert_eq!(library.hot_cues(Path::new("/m/a.mp3")).unwrap().len(), 1);
        assert!(library.needs_scan(Path::new("/m/a.mp3"), 1, 1).unwrap());
    }

    #[test]
    fn crates_and_playlists() {
        let mut library = Library::open_in_memory().unwrap();
        for name in ["a", "b", "c"] {
            let path = PathBuf::from(format!("/m/{name}.mp3"));
            library.upsert_scanned(&path, 1, 1, &tags(name), 0).unwrap();
        }
        let ids: Vec<i64> = {
            let mut t = library.tracks().unwrap();
            t.sort_by_key(|t| t.id);
            t.iter().map(|t| t.id).collect()
        };
        let crate_id = library
            .create_collection("Warm up", CollectionKind::Crate, 0)
            .unwrap();
        let list = library
            .create_collection("Set", CollectionKind::Playlist, 0)
            .unwrap();
        assert!(library.add_to_collection(list, ids[2]).unwrap());
        assert!(library.add_to_collection(list, ids[0]).unwrap());
        assert!(library.add_to_collection(list, ids[1]).unwrap());
        assert!(
            !library.add_to_collection(list, ids[1]).unwrap(),
            "no duplicates"
        );
        library.add_to_collection(crate_id, ids[0]).unwrap();
        assert_eq!(
            library.collection_tracks(list).unwrap(),
            [ids[2], ids[0], ids[1]]
        );

        library.move_in_collection(list, ids[1], -1).unwrap();
        assert_eq!(
            library.collection_tracks(list).unwrap(),
            [ids[2], ids[1], ids[0]]
        );
        library.move_in_collection(list, ids[2], -1).unwrap();
        assert_eq!(
            library.collection_tracks(list).unwrap()[0],
            ids[2],
            "already first"
        );

        library.remove_from_collection(list, ids[1]).unwrap();
        library.rename_collection(crate_id, "Peak time").unwrap();
        let collections = library.collections().unwrap();
        assert_eq!(collections.len(), 2);
        assert_eq!(collections[0].name, "Peak time");
        assert_eq!(
            (collections[0].kind, collections[0].count),
            (CollectionKind::Crate, 1)
        );
        assert_eq!(
            (collections[1].kind, collections[1].count),
            (CollectionKind::Playlist, 2)
        );

        library.delete_collection(list).unwrap();
        assert_eq!(library.collections().unwrap().len(), 1);
        assert_eq!(
            library.tracks().unwrap().len(),
            3,
            "tracks stay in the library"
        );
    }

    #[test]
    fn a_grid_set_by_hand_beats_the_analysis_until_cleared() {
        let mut library = Library::open_in_memory().unwrap();
        let path = Path::new("/music/a.flac");
        library.upsert_scanned(path, 1, 1, &tags("A"), 0).unwrap();
        let analysed = TrackAnalysis {
            bpm: Some(127.9),
            grid_anchor: Some(0.1),
            key: None,
        };
        library
            .set_analysis(path, &analysed, AnalysisSource::App, 1, None)
            .unwrap();
        assert_eq!(library.user_grid(path).unwrap(), None);
        library.set_user_grid(path, Some((128.0, 0.25))).unwrap();
        // Re-analysis doesn't touch it.
        library
            .set_analysis(path, &analysed, AnalysisSource::App, 2, None)
            .unwrap();
        assert_eq!(library.user_grid(path).unwrap(), Some((128.0, 0.25)));
        let track = library.track(path).unwrap().unwrap();
        assert_eq!(track.analysis.bpm, Some(128.0));
        assert_eq!(track.analysis.grid_anchor, Some(0.25));
        library.set_user_grid(path, None).unwrap();
        assert_eq!(
            library.track(path).unwrap().unwrap().analysis.bpm,
            Some(127.9)
        );
    }

    #[test]
    fn survives_reopening() {
        let dir = std::env::temp_dir().join(format!("dj-library-test-{}", std::process::id()));
        let path = dir.join("library.db");
        let cue = StoredCue {
            slot: 7,
            kind: CueKind::Point { seconds: 42.0 },
        };
        {
            let mut library = Library::open(&path).unwrap();
            library.save_hot_cues(Path::new("/x.mp3"), &[cue]).unwrap();
        }
        let library = Library::open(&path).unwrap();
        assert_eq!(library.hot_cues(Path::new("/x.mp3")).unwrap(), [cue]);
        drop(library);
        let _ = std::fs::remove_dir_all(dir);
    }
}
