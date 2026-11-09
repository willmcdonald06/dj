//! Renders scenes offscreen and checks the pixels. Skips (passes) when no GPU adapter is
//! available, as on CI runners without a software Vulkan driver.

use dj_ui::{Color, Offscreen, Scene, Text};

const W: u32 = 1280;
const H: u32 = 200;

fn offscreen() -> Option<Offscreen> {
    match Offscreen::new() {
        Ok(offscreen) => Some(offscreen),
        Err(err) => {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("skipping: no GPU ({err:#})");
            }
            None
        }
    }
}

/// Pixels in rows `y0..y1` that are clearly yellow (the text colour used below).
fn yellow_pixels(pixels: &[u8], y0: u32, y1: u32) -> usize {
    (y0..y1)
        .flat_map(|y| (0..W).map(move |x| ((y * W + x) * 4) as usize))
        .filter(|&i| pixels[i] > 150 && pixels[i + 1] > 120 && pixels[i + 2] < 90)
        .count()
}

fn text_scene(text: Text) -> Scene {
    Scene {
        background: Color::BLACK,
        texts: vec![text],
        ..Scene::default()
    }
}

const YELLOW: Color = Color::rgb(1.0, 0.85, 0.15);
const LONG: &str = "Couldn't load README.md onto deck A: unsupported or unrecognised format: \
                    unsupported feature: core (probe): no suitable format reader found";

#[test]
fn text_variants_all_render() {
    let Some(mut offscreen) = offscreen() else {
        return;
    };
    let cases = [
        (
            "short regular",
            Text::new("Hello", 10.0, 50.0, 15.0, YELLOW),
        ),
        (
            "short bold",
            Text::new("Hello", 10.0, 50.0, 15.0, YELLOW).bold(),
        ),
        ("long regular", Text::new(LONG, 10.0, 50.0, 15.3125, YELLOW)),
        (
            "long bold",
            Text::new(LONG, 10.0, 50.0, 15.3125, YELLOW).bold(),
        ),
        (
            "long bold clipped",
            Text::new(LONG, 10.0, 50.0, 15.3125, YELLOW)
                .bold()
                .max_width(600.0),
        ),
    ];
    let mut failures = Vec::new();
    for (name, text) in cases {
        let pixels = offscreen.render(&text_scene(text), W, H).unwrap();
        let count = yellow_pixels(&pixels, 40, 80);
        if count < 20 {
            failures.push(format!("{name}: {count} pixels"));
        }
    }
    assert!(failures.is_empty(), "text missing: {failures:?}");
}
