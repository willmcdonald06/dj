//! Where everything goes, in physical pixels.
//!
//! ```text
//! ┌──────────────────────── top bar ────────────────────────┐
//! │ deck A waveform (full width)                            │
//! │ transients: deck A spikes down, deck B spikes up         │
//! │ deck B waveform (full width)                            │
//! ├───────────────┬─────────────────┬───────────────────────┤
//! │ deck A panel  │      mixer      │ deck B panel          │
//! ├───────────────┴─────────────────┴───────────────────────┤
//! │ browser                                                 │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! The waveforms always span the whole window, one above the other, sharing a centre
//! playhead. The browser takes what is left.

use dj_engine::DeckId;
use dj_ui::FrameSize;

/// A rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Area {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub fn centre_x(&self) -> f32 {
        self.x + self.width / 2.0
    }

    /// Shrunk by `by` on every side.
    pub fn inset(&self, by: f32) -> Self {
        Self::new(
            self.x + by,
            self.y + by,
            (self.width - 2.0 * by).max(0.0),
            (self.height - 2.0 * by).max(0.0),
        )
    }

    /// The top `height` of this area, and the rest below a `gap`.
    pub fn split_top(&self, height: f32, gap: f32) -> (Self, Self) {
        let height = height.min(self.height);
        let top = Self::new(self.x, self.y, self.width, height);
        let rest = Self::new(
            self.x,
            self.y + height + gap,
            self.width,
            (self.height - height - gap).max(0.0),
        );
        (top, rest)
    }

    /// The left `width` of this area, and the rest after a `gap`.
    pub fn split_left(&self, width: f32, gap: f32) -> (Self, Self) {
        let width = width.min(self.width);
        let left = Self::new(self.x, self.y, width, self.height);
        let rest = Self::new(
            self.x + width + gap,
            self.y,
            (self.width - width - gap).max(0.0),
            self.height,
        );
        (left, rest)
    }

    /// The right `width` of this area, and the rest before a `gap`.
    pub fn split_right(&self, width: f32, gap: f32) -> (Self, Self) {
        let width = width.min(self.width);
        let right = Self::new(self.right() - width, self.y, width, self.height);
        let rest = Self::new(
            self.x,
            self.y,
            (self.width - width - gap).max(0.0),
            self.height,
        );
        (right, rest)
    }

    /// `count` equal columns with `gap` between them.
    pub fn columns(&self, count: usize, gap: f32) -> impl Iterator<Item = Area> + use<> {
        let count = count.max(1);
        let width = (self.width - gap * (count - 1) as f32) / count as f32;
        let area = *self;
        (0..count).map(move |i| {
            Area::new(
                area.x + i as f32 * (width + gap),
                area.y,
                width,
                area.height,
            )
        })
    }
}

pub struct Layout {
    pub width: f32,
    pub height: f32,
    pub top_bar: Area,
    /// Scrolling waveforms, deck A above deck B.
    pub waveforms: [Area; 2],
    /// Between the waveforms: both decks' transient markers.
    pub transients: Area,
    /// Deck panels: A left of the mixer, B right of it.
    pub decks: [Area; 2],
    pub mixer: Area,
    pub browser: Area,
}

/// Logical heights.
const TOP_BAR: f32 = 34.0;
const TRANSIENTS: f32 = 40.0;
const DECK_PANEL: f32 = 262.0;
const MIXER_WIDTH: f32 = 292.0;
const GAP: f32 = 6.0;

impl Layout {
    pub fn new(size: FrameSize) -> Self {
        let s = size.scale;
        let width = size.width as f32;
        let height = size.height as f32;
        let gap = GAP * s;
        let window = Area::new(0.0, 0.0, width, height);

        let (top_bar, rest) = window.split_top(TOP_BAR * s, 0.0);
        // Waveforms grow with the window, within limits; the browser gets the rest.
        let wave_h = (height * 0.14).clamp(80.0 * s, 210.0 * s);
        let (wave_a, rest) = rest.split_top(wave_h, 0.0);
        let (transients, rest) = rest.split_top(TRANSIENTS * s, 0.0);
        let (wave_b, rest) = rest.split_top(wave_h, gap);
        let (decks_row, rest) = rest.inset_x(gap).split_top(DECK_PANEL * s, gap);
        let browser = Area::new(rest.x, rest.y, rest.width, (rest.height - gap).max(0.0));

        let mixer_w = MIXER_WIDTH * s;
        let deck_w = (decks_row.width - mixer_w - 2.0 * gap) / 2.0;
        let (deck_a, rest) = decks_row.split_left(deck_w, gap);
        let (mixer, deck_b) = rest.split_left(mixer_w, gap);
        Self {
            width,
            height,
            top_bar,
            waveforms: [wave_a, wave_b],
            transients,
            decks: [deck_a, deck_b],
            mixer,
            browser,
        }
    }

    pub fn deck(&self, deck: DeckId) -> Area {
        self.decks[deck.index()]
    }

    pub fn waveform(&self, deck: DeckId) -> Area {
        self.waveforms[deck.index()]
    }
}

impl Area {
    /// Shrunk by `by` at the left and right.
    pub fn inset_x(&self, by: f32) -> Self {
        Self::new(
            self.x + by,
            self.y,
            (self.width - 2.0 * by).max(0.0),
            self.height,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_fits_the_target_panel_without_overlaps() {
        let layout = Layout::new(FrameSize {
            width: 1280,
            height: 800,
            scale: 1.0,
        });
        let order = [
            layout.top_bar,
            layout.waveforms[0],
            layout.transients,
            layout.waveforms[1],
            layout.decks[0],
            layout.browser,
        ];
        for pair in order.windows(2) {
            assert!(pair[0].bottom() <= pair[1].y + 0.01, "{pair:?}");
        }
        assert!(layout.browser.bottom() <= 800.0);
        assert!(layout.browser.height > 200.0, "{:?}", layout.browser);
        assert!(layout.decks[0].right() < layout.mixer.x);
        assert!(layout.mixer.right() < layout.decks[1].x);
        assert!(layout.decks[1].right() <= 1280.0);
        // The waveforms span the window.
        assert_eq!(layout.waveforms[0].width, 1280.0);
    }

    #[test]
    fn columns_split_evenly() {
        let cols: Vec<Area> = Area::new(0.0, 0.0, 100.0, 10.0).columns(4, 4.0).collect();
        assert_eq!(cols.len(), 4);
        assert_eq!(cols[0].width, 22.0);
        assert_eq!(cols[3].right(), 100.0);
    }
}
