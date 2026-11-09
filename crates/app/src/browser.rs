//! Track browser state: the library's tracks, filtered by source and search, sorted,
//! with a selection and scroll position. Drawing is in `frontend`.

use std::path::PathBuf;

use std::collections::HashMap;

use dj_analysis::{Key, Mode};
use dj_library::{Collection, CollectionKind, Track};

use crate::library_worker::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// A playlist's own order.
    Position,
    Title,
    Artist,
    Bpm,
    Key,
    Length,
    Added,
}

pub struct Browser {
    tracks: Vec<Track>,
    /// Indices into `tracks`, filtered and sorted.
    view: Vec<usize>,
    /// Track id of the selection, kept across reloads.
    selected: Option<i64>,
    /// First visible row of the view.
    pub scroll: usize,
    pub sort: Column,
    pub ascending: bool,
    pub search: String,
    pub search_focused: bool,
    pub sources: Vec<Source>,
    pub collections: Vec<Collection>,
    /// What to show.
    pub filter: Filter,
    pub status: String,
}

/// Which tracks the browser shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    /// Tracks under a folder or drive.
    Folder(PathBuf),
    /// A crate or playlist: its id, kind and track ids in order.
    Collection {
        id: i64,
        kind: CollectionKind,
        order: Vec<i64>,
    },
}

impl Filter {
    pub fn collection_id(&self) -> Option<i64> {
        match self {
            Filter::Collection { id, .. } => Some(*id),
            _ => None,
        }
    }

    pub fn is_playlist(&self) -> bool {
        matches!(
            self,
            Filter::Collection {
                kind: CollectionKind::Playlist,
                ..
            }
        )
    }
}

impl Browser {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            view: Vec::new(),
            selected: None,
            scroll: 0,
            sort: Column::Added,
            ascending: false,
            search: String::new(),
            search_focused: false,
            sources: Vec::new(),
            collections: Vec::new(),
            filter: Filter::All,
            status: String::new(),
        }
    }

    pub fn set_tracks(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.rebuild();
    }

    pub fn total(&self) -> usize {
        self.tracks.len()
    }

    /// Rows currently shown.
    pub fn rows(&self) -> impl Iterator<Item = &Track> {
        self.view.iter().map(|&i| &self.tracks[i])
    }

    pub fn len(&self) -> usize {
        self.view.len()
    }

    pub fn selected(&self) -> Option<&Track> {
        let id = self.selected?;
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn selected_row(&self) -> Option<usize> {
        let id = self.selected?;
        self.view.iter().position(|&i| self.tracks[i].id == id)
    }

    pub fn select_row(&mut self, row: usize) {
        if let Some(&i) = self.view.get(row) {
            self.selected = Some(self.tracks[i].id);
        }
    }

    /// Move the selection by `delta` rows, keeping it on screen (`visible` rows).
    pub fn move_selection(&mut self, delta: i32, visible: usize) {
        if self.view.is_empty() {
            return;
        }
        let current = match self.selected_row() {
            Some(row) => row as i64 + delta as i64,
            None => 0,
        };
        let row = current.clamp(0, self.view.len() as i64 - 1) as usize;
        self.select_row(row);
        self.scroll_to(row, visible);
    }

    /// Scroll so `row` is visible.
    pub fn scroll_to(&mut self, row: usize, visible: usize) {
        let visible = visible.max(1);
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + visible {
            self.scroll = row + 1 - visible;
        }
    }

    pub fn scroll_by(&mut self, rows: i32, visible: usize) {
        let max = self.view.len().saturating_sub(visible.max(1));
        self.scroll = (self.scroll as i64 + rows as i64).clamp(0, max as i64) as usize;
    }

    /// A track's place in the playlist being shown (1-based), for the # column.
    pub fn playlist_position(&self, track: i64) -> Option<usize> {
        match &self.filter {
            Filter::Collection {
                kind: CollectionKind::Playlist,
                order,
                ..
            } => order.iter().position(|&id| id == track).map(|i| i + 1),
            _ => None,
        }
    }

    /// Sort by `column`; the same column again flips the direction.
    pub fn sort_by(&mut self, column: Column) {
        match self.sort == column {
            true => self.ascending = !self.ascending,
            false => {
                self.sort = column;
                // Newest first is what you want for "added"; A–Z for the rest.
                self.ascending = column != Column::Added;
            }
        }
        self.rebuild();
    }

    /// Show `filter`. Playlists open in their own order.
    pub fn set_filter(&mut self, filter: Filter) {
        let was_playlist = self.filter.is_playlist();
        self.filter = filter;
        self.scroll = 0;
        match (self.filter.is_playlist(), was_playlist) {
            (true, _) => {
                self.sort = Column::Position;
                self.ascending = true;
            }
            (false, true) => {
                self.sort = Column::Added;
                self.ascending = false;
            }
            (false, false) => {}
        }
        self.rebuild();
    }

    /// The selected track's id.
    pub fn selected_id(&self) -> Option<i64> {
        self.selected
    }

    pub fn set_search(&mut self, search: String) {
        self.search = search;
        self.scroll = 0;
        self.rebuild();
    }

    /// Re-filter and re-sort after the tracks, search, source or sort changed.
    pub fn rebuild(&mut self) {
        let words: Vec<String> = self
            .search
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        // Position of each track in a crate or playlist; tracks not in it are hidden.
        let positions: Option<HashMap<i64, usize>> = match &self.filter {
            Filter::Collection { order, .. } => {
                Some(order.iter().enumerate().map(|(i, &id)| (id, i)).collect())
            }
            _ => None,
        };
        let mut view: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| {
                let track = &self.tracks[i];
                let in_source = match &self.filter {
                    Filter::All => true,
                    Filter::Folder(root) => track.path.starts_with(root),
                    Filter::Collection { .. } => positions
                        .as_ref()
                        .is_some_and(|p| p.contains_key(&track.id)),
                };
                in_source && matches_search(track, &words)
            })
            .collect();
        let position = |track: &Track| {
            positions
                .as_ref()
                .and_then(|p| p.get(&track.id).copied())
                .unwrap_or(usize::MAX)
        };
        let tracks = &self.tracks;
        view.sort_by(|&a, &b| {
            let (a, b) = (&tracks[a], &tracks[b]);
            let order = match self.sort {
                Column::Position => position(a).cmp(&position(b)),
                Column::Title => compare_text(&a.title, &b.title),
                Column::Artist => compare_text(
                    a.artist.as_deref().unwrap_or(""),
                    b.artist.as_deref().unwrap_or(""),
                ),
                Column::Bpm => a
                    .analysis
                    .bpm
                    .partial_cmp(&b.analysis.bpm)
                    .unwrap_or(std::cmp::Ordering::Equal),
                Column::Key => key_order(a).cmp(&key_order(b)),
                Column::Length => a
                    .duration
                    .partial_cmp(&b.duration)
                    .unwrap_or(std::cmp::Ordering::Equal),
                Column::Added => a.added.cmp(&b.added).then(a.id.cmp(&b.id)),
            };
            match self.ascending {
                true => order,
                false => order.reverse(),
            }
        });
        self.view = view;
        self.scroll = self.scroll.min(self.view.len().saturating_sub(1));
    }
}

/// Every search word must appear in the title, artist, album or file name.
fn matches_search(track: &Track, words: &[String]) -> bool {
    if words.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {}",
        track.title,
        track.artist.as_deref().unwrap_or(""),
        track.album.as_deref().unwrap_or(""),
        track
            .path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    )
    .to_lowercase();
    words.iter().all(|word| haystack.contains(word.as_str()))
}

fn compare_text(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

/// Keys sort round the wheel (1m, 1d, 2m, 2d …), unknown last.
fn key_order(track: &Track) -> (u8, u8) {
    match track_key(track) {
        Some(key) => (key.wheel(), (key.mode == Mode::Major) as u8),
        None => (99, 0),
    }
}

/// A library track's key as a musical key.
pub fn track_key(track: &Track) -> Option<Key> {
    let (pitch, minor) = track.analysis.key?;
    let mode = match minor {
        true => Mode::Minor,
        false => Mode::Major,
    };
    Some(Key::new(pitch, mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dj_library::TrackAnalysis;

    fn track(id: i64, path: &str, title: &str, artist: &str, bpm: f64, added: i64) -> Track {
        Track {
            id,
            path: PathBuf::from(path),
            title: title.into(),
            artist: Some(artist.into()),
            album: None,
            duration: Some(200.0),
            analysis: TrackAnalysis {
                bpm: Some(bpm),
                grid_anchor: None,
                key: Some(((id % 12) as u8, id % 2 == 0)),
            },
            analysis_source: None,
            added,
        }
    }

    fn browser() -> Browser {
        let mut b = Browser::new();
        b.set_tracks(vec![
            track(
                1,
                "/music/house/jam.mp3",
                "Jam",
                "Above & Beyond",
                128.0,
                10,
            ),
            track(
                2,
                "/music/pop/rules.mp3",
                "New Rules",
                "Dua Lipa",
                116.0,
                30,
            ),
            track(
                3,
                "/usb/press.mp3",
                "Hot Off The Press",
                "Will K",
                131.0,
                20,
            ),
        ]);
        b
    }

    fn titles(b: &Browser) -> Vec<&str> {
        b.rows().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn newest_first_then_sorts_and_flips() {
        let mut b = browser();
        assert_eq!(titles(&b), ["New Rules", "Hot Off The Press", "Jam"]);
        b.sort_by(Column::Bpm);
        assert_eq!(titles(&b), ["New Rules", "Jam", "Hot Off The Press"]);
        b.sort_by(Column::Bpm);
        assert_eq!(titles(&b), ["Hot Off The Press", "Jam", "New Rules"]);
        b.sort_by(Column::Artist);
        assert_eq!(titles(&b), ["Jam", "New Rules", "Hot Off The Press"]);
    }

    #[test]
    fn search_matches_every_word_anywhere() {
        let mut b = browser();
        b.set_search("dua rules".into());
        assert_eq!(titles(&b), ["New Rules"]);
        b.set_search("press.mp3".into());
        assert_eq!(titles(&b), ["Hot Off The Press"]);
        b.set_search("nothing".into());
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn sources_filter_by_folder() {
        let mut b = browser();
        b.set_filter(Filter::Folder(PathBuf::from("/music")));
        assert_eq!(b.len(), 2);
        b.set_filter(Filter::Folder(PathBuf::from("/usb")));
        assert_eq!(titles(&b), ["Hot Off The Press"]);
    }

    #[test]
    fn playlists_keep_their_order_and_crates_sort() {
        let mut b = browser();
        b.set_filter(Filter::Collection {
            id: 9,
            kind: CollectionKind::Playlist,
            order: vec![3, 1],
        });
        assert_eq!(titles(&b), ["Hot Off The Press", "Jam"]);
        assert_eq!(b.playlist_position(1), Some(2));
        b.set_filter(Filter::Collection {
            id: 10,
            kind: CollectionKind::Crate,
            order: vec![3, 1],
        });
        assert_eq!(b.sort, Column::Added, "back to the library's sort");
        assert_eq!(titles(&b), ["Hot Off The Press", "Jam"]);
        assert_eq!(b.playlist_position(1), None);
    }

    #[test]
    fn selection_moves_clamps_and_survives_reloads() {
        let mut b = browser();
        b.move_selection(1, 10);
        assert_eq!(
            b.selected().unwrap().title,
            "New Rules",
            "first move selects the top"
        );
        b.move_selection(5, 10);
        assert_eq!(b.selected().unwrap().title, "Jam");
        b.move_selection(-1, 10);
        let id = b.selected().unwrap().id;
        b.set_tracks(b.tracks.clone());
        assert_eq!(b.selected().unwrap().id, id);
    }

    #[test]
    fn scrolling_keeps_the_selection_visible() {
        let mut b = browser();
        b.move_selection(0, 1);
        b.move_selection(2, 1);
        assert_eq!(b.scroll, 2);
        b.scroll_by(-5, 1);
        assert_eq!(b.scroll, 0);
    }
}
