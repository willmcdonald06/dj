//! Text rendering with glyphon. Shaped text is cached between frames, keyed by its
//! content, size and weight, so unchanged labels cost nothing to lay out again.

use std::collections::HashMap;

use glyphon::{
    Attrs, Buffer, Cache, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer as GlyphonRenderer, Viewport, Weight as GlyphonWeight,
    fontdb,
};

use crate::scene::{Align, Color, Text, Weight};

const FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSans-Bold.ttf");
const FAMILY: &str = "Noto Sans";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    content: String,
    size_bits: u32,
    bold: bool,
}

struct Shaped {
    buffer: Buffer,
    width: f32,
    used: bool,
}

pub struct TextRenderer {
    fonts: FontSystem,
    swash: SwashCache,
    atlas: TextAtlas,
    viewport: Viewport,
    renderer: GlyphonRenderer,
    cache: HashMap<Key, Shaped>,
}

impl TextRenderer {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        // The bundled font always works; system fonts add fallback for other scripts
        // in track titles.
        let mut db = fontdb::Database::new();
        db.load_font_data(FONT_REGULAR.to_vec());
        db.load_font_data(FONT_BOLD.to_vec());
        db.load_system_fonts();
        db.set_sans_serif_family(FAMILY);
        let locale = std::env::var("LANG")
            .ok()
            .and_then(|l| l.split('.').next().map(str::to_string))
            .unwrap_or_else(|| "en-US".to_string())
            .replace('_', "-");
        let fonts = FontSystem::new_with_locale_and_db(locale, db);

        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer =
            GlyphonRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
        Self {
            fonts,
            swash: SwashCache::new(),
            atlas,
            viewport,
            renderer,
            cache: HashMap::new(),
        }
    }

    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texts: &[Text],
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        self.viewport.update(queue, Resolution { width, height });
        for shaped in self.cache.values_mut() {
            shaped.used = false;
        }
        // Shape anything new first, so the borrow for the text areas below is shared.
        for text in texts {
            let key = key(text);
            if let Some(shaped) = self.cache.get_mut(&key) {
                shaped.used = true;
                continue;
            }
            let shaped = shape(&mut self.fonts, text);
            self.cache.insert(key, shaped);
        }

        let areas = texts.iter().filter_map(|text| {
            let shaped = self.cache.get(&key(text))?;
            let left = match text.align {
                Align::Left => text.x,
                Align::Center => text.x - shaped.width / 2.0,
                Align::Right => text.x - shaped.width,
            };
            let right = match text.max_width {
                Some(max) => left + max,
                None => width as f32,
            };
            Some(TextArea {
                buffer: &shaped.buffer,
                left,
                top: text.y,
                scale: 1.0,
                bounds: TextBounds {
                    left: left.floor() as i32,
                    top: text.y.floor() as i32,
                    right: right.ceil() as i32,
                    bottom: (text.y + text.size * 1.5).ceil() as i32,
                },
                default_color: to_glyphon(text.color),
                custom_glyphs: &[],
            })
        });
        self.renderer.prepare(
            device,
            queue,
            &mut self.fonts,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash,
        )?;
        self.cache.retain(|_, shaped| shaped.used);
        Ok(())
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) -> anyhow::Result<()> {
        self.renderer.render(&self.atlas, &self.viewport, pass)?;
        Ok(())
    }

    /// Free atlas space for glyphs not used this frame. Call after presenting.
    pub fn trim(&mut self) {
        self.atlas.trim();
    }
}

fn key(text: &Text) -> Key {
    Key {
        content: text.content.clone(),
        size_bits: text.size.to_bits(),
        bold: text.weight == Weight::Bold,
    }
}

fn shape(fonts: &mut FontSystem, text: &Text) -> Shaped {
    let mut buffer = Buffer::new(fonts, Metrics::new(text.size, text.size * 1.3));
    buffer.set_size(None, None);
    let weight = match text.weight {
        Weight::Regular => GlyphonWeight::NORMAL,
        Weight::Bold => GlyphonWeight::BOLD,
    };
    let attrs = Attrs::new().family(Family::SansSerif).weight(weight);
    buffer.set_text(&text.content, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(fonts, false);
    let width = buffer
        .layout_runs()
        .map(|run| run.line_w)
        .fold(0.0, f32::max);
    Shaped {
        buffer,
        width,
        used: true,
    }
}

/// Our colours are linear; glyphon takes sRGB bytes.
fn to_glyphon(color: Color) -> glyphon::Color {
    let encode = |c: f32| {
        let c = c.clamp(0.0, 1.0);
        let srgb = match c <= 0.003_130_8 {
            true => c * 12.92,
            false => 1.055 * c.powf(1.0 / 2.4) - 0.055,
        };
        (srgb * 255.0).round() as u8
    };
    glyphon::Color::rgba(
        encode(color.r),
        encode(color.g),
        encode(color.b),
        (color.a.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}
