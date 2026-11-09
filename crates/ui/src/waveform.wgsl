// Draws one waveform view: per pixel column, the max of the 1-3 summary bins under it,
// as three layered bands around the centre line.

struct View {
    // x, y, width, height in physical pixels.
    rect: vec4<f32>,
    viewport: vec2<f32>,
    // Where the chosen level starts in the texture, and how many bins it has.
    level_offset: u32,
    level_len: u32,
    // Bin (at the chosen level) under the left edge, and the fractional part.
    first_bin: i32,
    tex_width: u32,
    bin_frac: f32,
    bins_per_px: f32,
    span: u32,
    brightness: f32,
    // 0 = spectrum colours, 1 = three bands.
    style: u32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> view: View;
@group(1) @binding(0) var bins: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let c = corners[index];
    let px = view.rect.xy + c * view.rect.zw;
    let clip = vec2(px.x / view.viewport.x * 2.0 - 1.0, 1.0 - px.y / view.viewport.y * 2.0);
    var out: VsOut;
    out.pos = vec4(clip, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let column = in.pos.x - view.rect.x;
    let position = view.bin_frac + column * view.bins_per_px;
    let base = view.first_bin + i32(floor(position));
    var v = vec4(0.0);
    for (var i = 0u; i < view.span; i++) {
        let bin = base + i32(i);
        if (bin >= 0 && bin < i32(view.level_len)) {
            let index = view.level_offset + u32(bin);
            let texel = vec2(i32(index % view.tex_width), i32(index / view.tex_width));
            v = max(v, textureLoad(bins, texel, 0));
        }
    }

    let half = view.rect.w * 0.5;
    let d = abs(in.pos.y - (view.rect.y + half)) / half;
    // Bands: r = low, g = mid, b = high, a = full-signal peak. A gentle curve makes
    // quieter bands readable.
    let low = pow(v.r, 0.8);
    let mid = pow(v.g, 0.8);
    let high = pow(v.b, 0.8);
    let peak = pow(v.a, 0.8);
    if (d > peak) {
        discard;
    }
    var color: vec3<f32>;
    if (view.style == 1u) {
        if (d <= high) {
            color = vec3(0.85, 0.85, 0.9);
        } else if (d <= mid) {
            color = vec3(0.9, 0.42, 0.08);
        } else if (d <= low) {
            color = vec3(0.06, 0.25, 0.85);
        } else {
            color = vec3(0.12, 0.12, 0.15);
        }
    } else {
        // Spectrum: the hue says where the energy is (red bass, green mids, blue
        // highs), the height how loud it is. Shaded darker towards the outer edge,
        // with a light core where the highs reach.
        // Weights favour the strongest band, so mixes stay saturated, not pastel.
        let wl = pow(low, 2.5);
        let wm = pow(mid, 2.5);
        let wh = pow(high, 2.5);
        let total = wl + wm + wh + 0.0001;
        var hue = (wl * vec3(1.0, 0.1, 0.03) + wm * vec3(0.25, 0.95, 0.12)
            + wh * vec3(0.15, 0.45, 1.0)) / total;
        hue = hue / max(max(hue.r, hue.g), max(hue.b, 0.001));
        let edge = d / max(peak, 0.001);
        color = hue * mix(1.0, 0.35, edge * edge);
        if (d <= high * 0.45) {
            color = mix(color, vec3(1.0), 0.3);
        }
    }
    return vec4(color * view.brightness, 1.0);
}
