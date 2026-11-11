//! Track browser state: the library's tracks, filtered by source and search, sorted,
//! with a selection and scroll position. Drawing is in `frontend`.

use std::path::PathBuf;

use std::collections::{HashMap, HashSet};

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
    /// How well it fits what's playing (the Matching view).
    Match,
}

/// The BPM tolerances the Matching view offers.
pub const MATCH_TOLERANCES: [f64; 3] = [0.03, 0.06, 0.10];

/// What the Matching view matches against: the track leading the mix.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchRef {
    pub title: String,
    /// The tempo it plays at now.
    pub bpm: Option<f64>,
    /// The key it sounds in now.
    pub key: Option<Key>,
    /// Tracks to leave out: the ones on the decks.
    pub exclude: Vec<i64>,
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
    /// What the Matching view matches against, if anything is playing.
    pub matching: Option<MatchRef>,
    /// BPM tolerance for Matching, as a fraction (0.06 is ±6%).
    pub match_tolerance: f64,
    /// Tracks played this session, in order.
    pub history: Vec<i64>,
    played: HashSet<i64>,
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
    /// Tracks that fit the one leading the mix, in BPM and key.
    Matching,
    /// Tracks played this session, in order.
    History,
}

impl Filter {
    pub fn collection_id(&self) -> Option<i64> {
        match self {
            Filter::Collection { id, .. } => Some(*id),
            _ => None,
        }
    }

    /// Whether the view has its own order (shown in the # column).
    pub fn is_ordered(&self) -> bool {
        self.is_playlist() || *self == Filter::History
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
            matching: None,
            match_tolerance: MATCH_TOLERANCES[1],
            history: Vec::new(),
            played: HashSet::new(),
        }
    }

    /// Whether `track` was played this session.
    pub fn was_played(&self, track: i64) -> bool {
        self.played.contains(&track)
    }

    /// Note that `track` has been played (once per session).
    pub fn mark_played(&mut self, track: i64) {
        if self.played.insert(track) {
            self.history.push(track);
            if self.filter == Filter::History {
                self.rebuild();
            }
        }
    }

    /// Set what Matching matches against; re-filters if that view is open.
    pub fn set_matching(&mut self, matching: Option<MatchRef>) {
        if self.matching == matching {
            return;
        }
        self.matching = matching;
        if self.filter == Filter::Matching {
            self.rebuild();
        }
    }

    /// Step the Matching BPM tolerance to the next choice.
    pub fn cycle_match_tolerance(&mut self) {
        let i = MATCH_TOLERANCES
            .iter()
            .position(|&t| (t - self.match_tolerance).abs() < 1e-9)
            .map_or(0, |i| (i + 1) % MATCH_TOLERANCES.len());
        self.match_tolerance = MATCH_TOLERANCES[i];
        self.rebuild();
    }

    /// How well `track` fits the Matching reference: lower is better; `None` if it
    /// doesn't fit. Same key ranks above neighbouring keys, then the nearer tempo.
    pub fn match_score(&self, track: &Track) -> Option<f64> {
        let reference = self.matching.as_ref()?;
        // Nothing to match on yet (still being analysed).
        if reference.exclude.contains(&track.id)
            || (reference.bpm.is_none() && reference.key.is_none())
        {
            return None;
        }
        let tempo = match (reference.bpm, track.analysis.bpm) {
            (Some(want), Some(bpm)) => Some(tempo_distance(want, bpm)?),
            (Some(_), None) => return None,
            (None, _) => None,
        };
        if tempo.is_some_and(|t| t > self.match_tolerance) {
            return None;
        }
        let key = match (reference.key, track_key(track)) {
            (Some(want), Some(key)) if key == want => 0.0,
            (Some(want), Some(key)) if key.compatible(&want) => 1.0,
            (Some(_), _) => return None,
            (None, _) => 0.0,
        };
        Some(key + tempo.unwrap_or(0.0))
    }

    pub fn set_tracks(&mut self, tracks: Vec<Track>) {
        self.tracks = tracks;
        self.rebuild();
    }

    /// `id` and its copies: the same song in another folder or on a USB drive. A copy
    /// has the same title and artist (ignoring case), or the same file name and length
    /// (tags often differ between copies).
    pub fn copies_of(&self, id: i64) -> Vec<i64> {
        let Some(original) = self.tracks.iter().find(|t| t.id == id) else {
            return vec![id];
        };
        let song = |t: &Track| {
            (
                t.title.trim().to_lowercase(),
                t.artist.as_deref().unwrap_or("").trim().to_lowercase(),
            )
        };
        let file = |t: &Track| {
            t.path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
        };
        let same_length = |t: &Track| match (t.duration, original.duration) {
            (Some(a), Some(b)) => (a - b).abs() < 1.0,
            _ => true,
        };
        let (wanted, wanted_file) = (song(original), file(original));
        self.tracks
            .iter()
            .filter(|t| {
                t.id == id || song(t) == wanted || (file(t) == wanted_file && same_length(t))
            })
            .map(|t| t.id)
            .collect()
    }

    /// The library track for a file, among all tracks (not just those shown).
    pub fn track_at(&self, path: &std::path::Path) -> Option<&Track> {
        self.tracks.iter().find(|t| t.path == path)
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
            Filter::History => self
                .history
                .iter()
                .position(|&id| id == track)
                .map(|i| i + 1),
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
        let had_own_order = self.filter.is_ordered() || self.filter == Filter::Matching;
        self.filter = filter;
        self.scroll = 0;
        if self.filter.is_ordered() {
            self.sort = Column::Position;
            self.ascending = true;
        } else if self.filter == Filter::Matching {
            self.sort = Column::Match;
            self.ascending = true;
        } else if had_own_order {
            self.sort = Column::Added;
            self.ascending = false;
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
            Filter::History => Some(
                self.history
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| (id, i))
                    .collect(),
            ),
            _ => None,
        };
        let scores: HashMap<i64, f64> = match self.filter {
            Filter::Matching => self
                .tracks
                .iter()
                .filter_map(|t| Some((t.id, self.match_score(t)?)))
                .collect(),
            _ => HashMap::new(),
        };
        let mut view: Vec<usize> = (0..self.tracks.len())
            .filter(|&i| {
                let track = &self.tracks[i];
                let in_source = match &self.filter {
                    Filter::All => true,
                    Filter::Folder(root) => track.path.starts_with(root),
                    Filter::Collection { .. } | Filter::History => positions
                        .as_ref()
                        .is_some_and(|p| p.contains_key(&track.id)),
                    Filter::Matching => scores.contains_key(&track.id),
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
                Column::Match => {
                    let score = |t: &Track| scores.get(&t.id).copied().unwrap_or(f64::MAX);
                    score(a)
                        .partial_cmp(&score(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                }
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

/// How far apart two tempos are, as a fraction, counting half and double time as the
/// same tempo.
pub fn tempo_distance(want: f64, bpm: f64) -> Option<f64> {
    if want <= 0.0 || bpm <= 0.0 {
        return None;
    }
    [0.5, 1.0, 2.0]
        .iter()
        .map(|factor| (bpm * factor / want - 1.0).abs())
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
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

    fn keyed(id: i64, title: &str, bpm: Option<f64>, pitch: u8, minor: bool) -> Track {
        Track {
            id,
            path: PathBuf::from(format!("/music/{title}.mp3")),
            title: title.into(),
            artist: None,
            album: None,
            duration: Some(200.0),
            analysis: TrackAnalysis {
                bpm,
                grid_anchor: None,
                key: Some((pitch, minor)),
            },
            analysis_source: None,
            added: id,
        }
    }

    #[test]
    fn matching_finds_tracks_in_key_and_tempo_best_first() {
        let mut b = Browser::new();
        b.set_tracks(vec![
            keyed(1, "same", Some(128.0), 9, true),      // Am
            keyed(2, "neighbour", Some(126.0), 4, true), // Em, 1.6% off
            keyed(3, "half time", Some(64.5), 9, true),  // Am at half tempo
            keyed(4, "clash", Some(128.0), 6, true),     // F#m
            keyed(5, "too fast", Some(140.0), 9, true),  // 9% off
            keyed(6, "no tempo", None, 9, true),
            keyed(7, "on deck", Some(128.0), 9, true),
            keyed(8, "relative", Some(129.0), 0, false), // C major
        ]);
        b.set_filter(Filter::Matching);
        assert!(titles(&b).is_empty(), "nothing playing, nothing to match");
        b.set_matching(Some(MatchRef {
            title: "playing".into(),
            bpm: Some(128.0),
            key: Some(Key::new(9, Mode::Minor)),
            exclude: vec![7],
        }));
        assert_eq!(titles(&b), ["same", "half time", "relative", "neighbour"]);
        // While the playing track is still being analysed: nothing, not everything.
        let analysed = b.matching.clone();
        b.set_matching(Some(MatchRef {
            title: "new".into(),
            bpm: None,
            key: None,
            exclude: vec![],
        }));
        assert!(titles(&b).is_empty());
        b.set_matching(analysed);
        b.cycle_match_tolerance();
        assert_eq!(b.match_tolerance, 0.10);
        assert!(titles(&b).contains(&"too fast"));
    }

    #[test]
    fn copies_are_the_same_title_and_artist() {
        let mut b = browser();
        let mut copy = track(9, "/usb/jam.mp3", "jam ", "ABOVE & BEYOND", 128.0, 1);
        copy.album = Some("other".into());
        let mut tracks: Vec<Track> = b.tracks.clone();
        tracks.push(copy);
        b.set_tracks(tracks);
        assert_eq!(b.copies_of(1), [1, 9]);
        // Differently tagged, but the same file name and length.
        let mut tracks: Vec<Track> = b.tracks.clone();
        tracks.push(track(
            10,
            "/usb/rules.mp3",
            "Rules (tagged)",
            "Dua",
            116.0,
            1,
        ));
        b.set_tracks(tracks);
        assert_eq!(b.copies_of(2), [2, 10]);
    }

    #[test]
    fn history_lists_played_tracks_in_order() {
        let mut b = browser();
        b.mark_played(3);
        b.mark_played(1);
        b.mark_played(3);
        b.set_filter(Filter::History);
        assert_eq!(titles(&b), ["Hot Off The Press", "Jam"]);
        assert_eq!(b.playlist_position(1), Some(2));
        assert!(b.was_played(1) && !b.was_played(2));
        b.set_filter(Filter::All);
        assert_eq!(b.sort, Column::Added);
    }

    #[test]
    fn half_and_double_time_count_as_the_same_tempo() {
        assert!(tempo_distance(128.0, 64.0).unwrap() < 1e-9);
        assert!(tempo_distance(87.0, 174.0).unwrap() < 1e-9);
        assert!((tempo_distance(100.0, 103.0).unwrap() - 0.03).abs() < 1e-9);
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
