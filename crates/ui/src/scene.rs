//! A retained-free description of one frame: what to draw, in physical pixels.
//!
//! Layers are drawn in this order: [`Scene::rects`], [`Scene::waveforms`],
//! [`Scene::overlay`], [`Scene::texts`], then [`Scene::popup_rects`] and
//! [`Scene::popup_texts`] over everything.

use std::sync::Arc;

use dj_analysis::WaveformSummary;

/// A colour in linear RGB (blending happens in linear space; the surface is sRGB).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0.0, 0.0, 0.0);
    pub const WHITE: Color = Color::rgb(1.0, 1.0, 1.0);

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }
}

/// What a [`Rect`] draws inside its bounds.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Shape {
    /// The whole (rounded) rectangle.
    #[default]
    Fill,
    /// Just the (rounded) outline, `width` pixels thick, inside the bounds.
    Stroke { width: f32 },
    /// A ring segment inscribed in the bounds: `thickness` pixels thick (at least the
    /// radius: a pie slice or disc), from `start` to `end` radians clockwise from
    /// twelve o'clock. A span of 2π or more is a full ring.
    Arc {
        thickness: f32,
        start: f32,
        end: f32,
    },
    /// A triangle filling the bounds, pointing right, turned clockwise by quarter turns
    /// (1 points down, 2 left, 3 up).
    Triangle { quarter_turns: u8 },
    /// A triangle stretched to the bounds: its base along one edge, its tip at the
    /// middle of the opposite edge. Points right, turned like [`Shape::Triangle`].
    Spike { quarter_turns: u8 },
}

/// A shape in an axis-aligned box. Origin is the top-left of the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: Color,
    /// Colour at the bottom edge, for a vertical gradient from `color`.
    pub bottom: Option<Color>,
    /// Corner radius in pixels.
    pub radius: f32,
    pub shape: Shape,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32, color: Color) -> Self {
        Self {
            x,
            y,
            width,
            height,
            color,
            bottom: None,
            radius: 0.0,
            shape: Shape::Fill,
        }
    }

    pub const fn rounded(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    /// Fade from `color` at the top to `bottom`.
    pub const fn gradient(mut self, bottom: Color) -> Self {
        self.bottom = Some(bottom);
        self
    }

    pub const fn stroke(mut self, width: f32) -> Self {
        self.shape = Shape::Stroke { width };
        self
    }

    pub const fn shape(mut self, shape: Shape) -> Self {
        self.shape = shape;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Weight {
    #[default]
    Regular,
    Bold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// One line of text. `x` is the left edge, centre or right edge depending on `align`;
/// `y` is the top of the line.
#[derive(Debug, Clone, PartialEq)]
pub struct Text {
    pub content: String,
    pub x: f32,
    pub y: f32,
    /// Font size in physical pixels.
    pub size: f32,
    pub color: Color,
    pub weight: Weight,
    pub align: Align,
    /// Clip anything beyond this width.
    pub max_width: Option<f32>,
}

impl Text {
    pub fn new(content: impl Into<String>, x: f32, y: f32, size: f32, color: Color) -> Self {
        Self {
            content: content.into(),
            x,
            y,
            size,
            color,
            weight: Weight::Regular,
            align: Align::Left,
            max_width: None,
        }
    }

    pub fn bold(mut self) -> Self {
        self.weight = Weight::Bold;
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn max_width(mut self, width: f32) -> Self {
        self.max_width = Some(width);
        self
    }
}

/// How a waveform is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaveformStyle {
    /// One colour per column from its spectrum: red bass, green mids, blue highs.
    #[default]
    Spectrum,
    /// Low (blue), mid (orange) and high (white) bands layered around the centre.
    ThreeBand,
}

/// A waveform drawn into a rectangle, around a centre line. The summary is uploaded to the GPU once and reused
/// for as long as the same `Arc` is passed.
#[derive(Debug, Clone)]
pub struct WaveformView {
    pub data: Arc<WaveformSummary>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Track frame at the left edge (may be negative: before the start is blank).
    pub start_frame: f64,
    pub frames_per_pixel: f64,
    /// 1.0 is normal; lower dims the waveform.
    pub brightness: f32,
    pub style: WaveformStyle,
}

#[derive(Debug, Clone)]
pub struct Scene {
    pub background: Color,
    pub rects: Vec<Rect>,
    pub waveforms: Vec<WaveformView>,
    /// Rectangles drawn over the waveforms (markers, playheads).
    pub overlay: Vec<Rect>,
    pub texts: Vec<Text>,
    /// Drawn over everything else: popups, toasts, drag feedback.
    pub popup_rects: Vec<Rect>,
    pub popup_texts: Vec<Text>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            background: Color::BLACK,
            rects: Vec::new(),
            waveforms: Vec::new(),
            overlay: Vec::new(),
            texts: Vec::new(),
            popup_rects: Vec::new(),
            popup_texts: Vec::new(),
        }
    }
}
