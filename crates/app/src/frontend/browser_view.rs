//! The browser, always on screen under the decks: sources, crates and playlists on the
//! left; a toolbar and the track list on the right.

use dj_analysis::{Key as MusicalKey, KeyNotation};
use dj_engine::DeckId;
use dj_library::CollectionKind;
use dj_ui::{Align, Color, Rect};

use super::layout::Area;
use super::painter::{Painter, ink_on};
use super::theme::*;
use super::waves::deck_letter;
use super::{Control, UiAction, clock};
use crate::browser::{Browser, Column, Filter, track_key};

/// What the browser view draws.
pub struct BrowserView<'a> {
    pub area: Area,
    pub browser: &'a Browser,
    /// Arrow keys browse (rather than move tempo).
    pub focused: bool,
    /// Where Enter loads.
    pub selected_deck: DeckId,
    /// Library ids of the tracks on each deck.
    pub loaded: [Option<i64>; 2],
    /// Keys on the decks, to mark compatible ones.
    pub deck_keys: [Option<MusicalKey>; 2],
    pub notation: KeyNotation,
    /// A track being dragged: its title and the pointer.
    pub drag: Option<(&'a str, (f32, f32))>,
    /// A crate or playlist being named, and the text so far.
    pub naming: Option<(i64, &'a str)>,
    pub delete_armed: bool,
}

impl Painter {
    /// Returns how many track rows fit.
    pub fn browser(&mut self, v: BrowserView<'_>) -> usize {
        self.panel(v.area);
        if v.focused {
            let accent = DECK_COLORS[v.selected_deck.index()];
            self.push(
                Rect::new(
                    v.area.x,
                    v.area.y,
                    v.area.width,
                    v.area.height,
                    accent.with_alpha(0.55),
                )
                .rounded(self.px(6.0))
                .stroke(self.px(1.0)),
            );
        }
        self.hit(v.area, Control::BrowserArea);
        let inner = v.area.inset(self.px(8.0));
        let (side, main) = inner.split_left(self.px(196.0), self.px(10.0));
        self.sidebar(&v, side);
        let (toolbar, table) = main.split_top(self.px(28.0), self.px(6.0));
        self.toolbar(&v, toolbar);
        let visible = self.track_table(&v, table);

        if let Some((title, (px, py))) = v.drag {
            self.popup(|p| {
                let w = p.px(240.0);
                let h = p.px(24.0);
                let ghost = Area::new(px + p.px(12.0), py + p.px(8.0), w, h);
                p.rounded(ghost, p.px(4.0), DECK_COLORS[0].with_alpha(0.92));
                p.label(
                    ghost.inset_x(p.px(8.0)),
                    format!("+ {title}"),
                    12.0,
                    TEXT,
                    Align::Left,
                );
            });
        }
        visible
    }

    fn sidebar(&mut self, v: &BrowserView<'_>, area: Area) {
        self.well(area);
        let browser = v.browser;
        let dragging = v.drag.is_some();
        let pointer = v.drag.map_or((-1.0, -1.0), |d| d.1);
        let row_h = self.px(22.0);
        let pad = self.px(10.0);
        let bottom = area.bottom() - self.px(4.0);
        let mut y = area.y + self.px(4.0);

        let heading = |painter: &mut Painter, y: f32, label: &str| {
            let head = Area::new(area.x + pad, y, area.width - 2.0 * pad, row_h);
            painter.bold(head, label, 9.0, SOFT_TEXT, Align::Left);
        };
        heading(self, y, "COLLECTION");
        y += row_h;
        let mut sources: Vec<(String, bool, Filter, Control)> = vec![(
            "All tracks".to_string(),
            false,
            Filter::All,
            Control::BrowserSource(0),
        )];
        for (i, s) in browser.sources.iter().enumerate() {
            sources.push((
                s.name.clone(),
                s.removable,
                Filter::Folder(s.path.clone()),
                Control::BrowserSource(i + 1),
            ));
        }
        for (name, usb, filter, control) in sources {
            if y + row_h > bottom {
                break;
            }
            let row = Area::new(area.x, y, area.width, row_h);
            let active = browser.filter == filter;
            self.side_row(row, active, false);
            let text_area = Area::new(row.x + pad, row.y, row.width - 2.0 * pad, row.height);
            let text_area = match usb {
                true => {
                    let (chip, rest) = text_area.split_left(self.px(28.0), self.px(6.0));
                    let chip = Area::new(
                        chip.x,
                        chip.y + self.px(4.0),
                        chip.width,
                        chip.height - self.px(8.0),
                    );
                    self.rounded(chip, self.px(3.0), WARNING);
                    self.bold(chip, "USB", 8.0, ink_on(WARNING), Align::Center);
                    rest
                }
                false => text_area,
            };
            self.label(text_area, name, 12.0, side_ink(active), Align::Left);
            self.hit(row, control);
            y += row_h;
        }

        for (title, kind) in [
            ("CRATES", CollectionKind::Crate),
            ("PLAYLISTS", CollectionKind::Playlist),
        ] {
            y += self.px(6.0);
            if y + row_h > bottom {
                break;
            }
            heading(self, y, title);
            let plus = Area::new(
                area.right() - pad - self.px(20.0),
                y + self.px(2.0),
                self.px(20.0),
                row_h - self.px(4.0),
            );
            self.button(plus, "+", false, TEXT, UiAction::NewCollection(kind));
            y += row_h;
            for (index, collection) in browser.collections.iter().enumerate() {
                if collection.kind != kind || y + row_h > bottom {
                    continue;
                }
                let row = Area::new(area.x, y, area.width, row_h);
                let viewing = browser.filter.collection_id() == Some(collection.id);
                let drop_target = dragging && row.contains(pointer.0, pointer.1);
                self.side_row(row, viewing, drop_target);
                let text_area = Area::new(row.x + pad, row.y, row.width - 2.0 * pad, row.height);
                let (count, name_area) = text_area.split_right(self.px(36.0), self.px(4.0));
                match v.naming {
                    Some((id, text)) if id == collection.id => {
                        self.label(name_area, format!("{text}▏"), 12.0, TEXT, Align::Left)
                    }
                    _ => {
                        self.label(
                            name_area,
                            collection.name.clone(),
                            12.0,
                            side_ink(viewing),
                            Align::Left,
                        );
                        self.label(
                            count,
                            format!("{}", collection.count),
                            10.0,
                            SOFT_TEXT,
                            Align::Right,
                        );
                    }
                }
                self.hit(row, Control::Collection(index));
                y += row_h;
            }
        }
    }

    /// A sidebar row's background: highlighted when viewed or a drop target.
    fn side_row(&mut self, row: Area, active: bool, drop_target: bool) {
        let r = self.px(3.0);
        let inset = Area::new(
            row.x + self.px(4.0),
            row.y + self.px(1.0),
            row.width - self.px(8.0),
            row.height - self.px(2.0),
        );
        match (drop_target, active) {
            (true, _) => self.rounded(inset, r, DECK_COLORS[0].with_alpha(0.5)),
            (false, true) => self.rounded(inset, r, SELECTED_ROW),
            (false, false) => {}
        }
    }

    fn toolbar(&mut self, v: &BrowserView<'_>, area: Area) {
        let browser = v.browser;
        let gap = self.px(6.0);
        let (search, rest) = area.split_left(self.px(260.0).min(area.width * 0.35), gap * 2.0);
        self.well(search);
        if browser.search_focused {
            self.push(
                Rect::new(
                    search.x,
                    search.y,
                    search.width,
                    search.height,
                    DECK_COLORS[0],
                )
                .rounded(self.px(3.0))
                .stroke(self.px(1.0)),
            );
        }
        let (text, color) = match (browser.search.is_empty(), browser.search_focused) {
            (true, false) => ("Search  ( / )".to_string(), DIM_TEXT),
            (_, true) => (format!("{}▏", browser.search), TEXT),
            (false, false) => (browser.search.clone(), TEXT),
        };
        self.label(
            search.inset_x(self.px(10.0)),
            text,
            12.0,
            color,
            Align::Left,
        );
        self.hit(search, Control::SearchBox);

        let mut rest = rest;
        for deck in DeckId::ALL {
            let (button, after) = rest.split_left(self.px(74.0), gap);
            let label = format!("LOAD {}", deck_letter(deck));
            self.tinted_button(
                button,
                &label,
                false,
                DECK_COLORS[deck.index()],
                UiAction::LoadSelected(deck),
            );
            rest = after;
        }
        rest = Area::new(rest.x + gap, rest.y, rest.width - gap, rest.height);
        let (rescan, after) = rest.split_left(self.px(70.0), gap);
        self.button(rescan, "RESCAN", false, TEXT, UiAction::Rescan);
        rest = after;
        // Viewing a crate or playlist: manage it.
        if browser.filter.collection_id().is_some() {
            rest = Area::new(rest.x + gap, rest.y, rest.width - gap, rest.height);
            let delete = match v.delete_armed {
                true => "SURE?",
                false => "DELETE",
            };
            let mut actions: Vec<(&str, bool, UiAction)> = vec![
                ("RENAME", false, UiAction::RenameCollection),
                (delete, v.delete_armed, UiAction::DeleteCollection),
                ("REMOVE", false, UiAction::RemoveFromCollection),
            ];
            if browser.filter.is_playlist() {
                actions.push(("UP", false, UiAction::MoveInPlaylist(-1)));
                actions.push(("DOWN", false, UiAction::MoveInPlaylist(1)));
            }
            for (label, on, action) in actions {
                let (button, after) = rest.split_left(self.px(62.0), gap);
                self.tinted_button(button, label, on, SOFT_TEXT, action);
                rest = after;
            }
        }
        let counts = format!("{} of {} tracks", browser.len(), browser.total());
        let status = match browser.status.is_empty() {
            true => counts,
            false => format!("{}  ·  {counts}", browser.status),
        };
        let status_area = Area::new(
            rest.x + gap,
            rest.y,
            (rest.width - gap).max(0.0),
            rest.height,
        );
        self.label(status_area, status, 10.0, SOFT_TEXT, Align::Right);
    }

    fn track_table(&mut self, v: &BrowserView<'_>, area: Area) -> usize {
        let browser = v.browser;
        let row_h = self.px(22.0);
        let playlist = browser.filter.is_playlist();
        let badge_w = self.px(28.0);
        let number_w = match playlist {
            true => self.px(34.0),
            false => 0.0,
        };
        let bpm_w = self.px(64.0);
        let key_w = self.px(56.0);
        let time_w = self.px(56.0);
        let flexible = (area.width - badge_w - number_w - bpm_w - key_w - time_w).max(0.0);
        let title_w = flexible * 0.56;
        let artist_w = flexible - title_w;
        // (label, width, sort column, alignment)
        let columns: [(&str, f32, Option<Column>, Align); 7] = [
            ("", badge_w, None, Align::Left),
            (
                "#",
                number_w,
                Some(Column::Position).filter(|_| playlist),
                Align::Right,
            ),
            ("TITLE", title_w, Some(Column::Title), Align::Left),
            ("ARTIST", artist_w, Some(Column::Artist), Align::Left),
            ("BPM", bpm_w, Some(Column::Bpm), Align::Right),
            ("KEY", key_w, Some(Column::Key), Align::Center),
            ("TIME", time_w, Some(Column::Length), Align::Right),
        ];
        let cell_pad = self.px(8.0);
        let (header, list) = area.split_top(self.px(20.0), 0.0);
        self.rounded(header, self.px(3.0), WELL);
        let mut x = header.x;
        for (label, w, column, align) in columns {
            let cell = Area::new(x, header.y, w, header.height);
            if let Some(column) = column {
                let active = browser.sort == column;
                let arrow = match (active, browser.ascending) {
                    (true, true) => " ▲",
                    (true, false) => " ▼",
                    (false, _) => "",
                };
                let ink = match active {
                    true => TEXT,
                    false => SOFT_TEXT,
                };
                self.bold(
                    cell.inset_x(cell_pad / 2.0),
                    format!("{label}{arrow}"),
                    9.0,
                    ink,
                    align,
                );
                self.hit(cell, Control::BrowserSort(column));
            }
            x += w;
        }

        let visible = (list.height / row_h).floor().max(1.0) as usize;
        self.hit(list, Control::BrowserList);
        let selected = browser.selected_row();
        for (offset, track) in browser
            .rows()
            .skip(browser.scroll)
            .take(visible)
            .enumerate()
        {
            let row_index = browser.scroll + offset;
            let row = Area::new(list.x, list.y + offset as f32 * row_h, list.width, row_h);
            let is_selected = Some(row_index) == selected;
            match (is_selected, offset % 2) {
                (true, _) => {
                    self.rounded(row, self.px(3.0), SELECTED_ROW);
                    let accent = DECK_COLORS[v.selected_deck.index()];
                    self.fill(Area::new(row.x, row.y, self.px(2.0), row.height), accent);
                }
                (false, 1) => self.fill(row, Color::rgba(1.0, 1.0, 1.0, 0.012)),
                (false, _) => {}
            }
            // Which deck it is on.
            let mut x = row.x;
            for deck in DeckId::ALL {
                if v.loaded[deck.index()] == Some(track.id) {
                    let chip = Area::new(
                        x + self.px(6.0) + deck.index() as f32 * self.px(0.0),
                        row.y + self.px(4.0),
                        self.px(16.0),
                        row.height - self.px(8.0),
                    );
                    let color = DECK_COLORS[deck.index()];
                    self.rounded(chip, self.px(3.0), color);
                    self.bold(chip, deck_letter(deck), 9.0, ink_on(color), Align::Center);
                }
            }
            x += badge_w;
            if playlist {
                if let Some(position) = browser.playlist_position(track.id) {
                    let cell = Area::new(x, row.y, number_w, row.height).inset_x(cell_pad / 2.0);
                    self.label(cell, format!("{position}"), 11.0, SOFT_TEXT, Align::Right);
                }
                x += number_w;
            }
            let ink = match is_selected {
                true => Color::rgb(0.95, 0.95, 0.97),
                false => TEXT,
            };
            let cell = |x: f32, w: f32| Area::new(x, row.y, w, row.height).inset_x(cell_pad / 2.0);
            self.label(
                cell(x, title_w),
                track.title.clone(),
                12.0,
                ink,
                Align::Left,
            );
            x += title_w;
            self.label(
                cell(x, artist_w),
                track.artist.clone().unwrap_or_default(),
                12.0,
                SOFT_TEXT,
                Align::Left,
            );
            x += artist_w;
            let bpm = track
                .analysis
                .bpm
                .map(|b| format!("{b:.1}"))
                .unwrap_or_default();
            self.label(cell(x, bpm_w), bpm, 12.0, ink, Align::Right);
            x += bpm_w;
            if let Some(key) = track_key(track) {
                let compatible = v.deck_keys.iter().flatten().any(|d| key.compatible(d));
                let chip = Area::new(
                    x + self.px(8.0),
                    row.y + self.px(3.0),
                    key_w - self.px(16.0),
                    row.height - self.px(6.0),
                );
                let fill = key_color(key);
                match compatible {
                    true => {
                        self.rounded(chip, self.px(3.0), fill);
                        self.bold(
                            chip,
                            key.name(v.notation),
                            10.0,
                            ink_on(fill),
                            Align::Center,
                        );
                    }
                    false => self.bold(chip, key.name(v.notation), 10.0, fill, Align::Center),
                }
            }
            x += key_w;
            let time = track
                .duration
                .map(|d| clock((d * 10.0) as u64, 10))
                .map(|c| {
                    c.trim_end_matches(|ch: char| ch != '.')
                        .trim_end_matches('.')
                        .to_string()
                })
                .unwrap_or_default();
            self.label(cell(x, time_w), time, 12.0, SOFT_TEXT, Align::Right);
            self.hit(row, Control::BrowserRow(row_index));
        }
        if browser.total() == 0 {
            let message = "No tracks yet. The library scans ~/Music and USB drives (--music DIR adds \
                           folders); you can also drop files on a waveform.";
            let (line, _) = list.split_top(row_h * 2.0, 0.0);
            self.label(
                line.inset_x(cell_pad),
                message,
                12.0,
                SOFT_TEXT,
                Align::Left,
            );
        }
        visible
    }
}

fn side_ink(active: bool) -> Color {
    match active {
        true => Color::rgb(0.95, 0.95, 0.97),
        false => TEXT,
    }
}
