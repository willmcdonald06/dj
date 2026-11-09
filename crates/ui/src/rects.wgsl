// Instanced shapes: filled or outlined rounded rectangles (optionally with a vertical
// gradient), arcs and triangles. Edges are anti-aliased from a signed distance in
// pixels, so axis-aligned edges on pixel boundaries stay crisp.

struct Instance {
    // x, y, width, height in physical pixels.
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    // Colour at the bottom edge (the same as `color` for a flat fill).
    @location(2) color2: vec4<f32>,
    // Corner radius, shape kind, viewport width and height.
    @location(3) misc: vec4<f32>,
    // Per-kind parameters (see `rects.rs`).
    @location(4) params: vec4<f32>,
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) color2: vec4<f32>,
    @location(2) rect: vec4<f32>,
    @location(3) misc: vec4<f32>,
    @location(4) params: vec4<f32>,
};

const KIND_FILL: i32 = 0;
const KIND_STROKE: i32 = 1;
const KIND_ARC: i32 = 2;
const KIND_TRIANGLE: i32 = 3;
const KIND_SPIKE: i32 = 4;
const PI: f32 = 3.14159265;

@vertex
fn vs_main(@builtin(vertex_index) index: u32, inst: Instance) -> VertexOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    // One pixel of slack all round, for the anti-aliased fringe.
    let min = inst.rect.xy - vec2(1.0);
    let max = inst.rect.xy + inst.rect.zw + vec2(1.0);
    let px = mix(min, max, corners[index]);
    let viewport = inst.misc.zw;
    var out: VertexOut;
    out.position = vec4<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0, 0.0, 1.0);
    out.color = inst.color;
    out.color2 = inst.color2;
    out.rect = inst.rect;
    out.misc = inst.misc;
    out.params = inst.params;
    return out;
}

fn round_box(p: vec2<f32>, half: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half.x, half.y));
    let q = abs(p) - half + vec2(r);
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn triangle(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>, c: vec2<f32>) -> f32 {
    let e0 = b - a;
    let e1 = c - b;
    let e2 = a - c;
    let v0 = p - a;
    let v1 = p - b;
    let v2 = p - c;
    let pq0 = v0 - e0 * clamp(dot(v0, e0) / dot(e0, e0), 0.0, 1.0);
    let pq1 = v1 - e1 * clamp(dot(v1, e1) / dot(e1, e1), 0.0, 1.0);
    let pq2 = v2 - e2 * clamp(dot(v2, e2) / dot(e2, e2), 0.0, 1.0);
    let s = sign(e0.x * e2.y - e0.y * e2.x);
    let d = min(
        min(vec2(dot(pq0, pq0), s * (v0.x * e0.y - v0.y * e0.x)),
            vec2(dot(pq1, pq1), s * (v1.x * e1.y - v1.y * e1.x))),
        vec2(dot(pq2, pq2), s * (v2.x * e2.y - v2.y * e2.x)),
    );
    return -sqrt(d.x) * sign(d.y);
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let half = in.rect.zw * 0.5;
    let p = in.position.xy - (in.rect.xy + half);
    let radius = in.misc.x;
    let kind = i32(round(in.misc.y));
    var d: f32;
    switch kind {
        case KIND_STROKE: {
            let w = in.params.x;
            d = abs(round_box(p, half - vec2(w * 0.5), max(radius - w * 0.5, 0.0))) - w * 0.5;
        }
        case KIND_ARC: {
            // A ring segment inscribed in the rect; angles are radians clockwise from
            // twelve o'clock.
            let outer = min(half.x, half.y);
            let thickness = min(in.params.x, outer);
            let r = length(p);
            let ring = abs(r - (outer - thickness * 0.5)) - thickness * 0.5;
            let angle = atan2(p.x, -p.y);
            let start = in.params.y;
            let end = in.params.z;
            let mid = (start + end) * 0.5;
            let span = (end - start) * 0.5;
            // Angular distance from the middle of the segment, wrapped to -pi..pi.
            var delta = angle - mid;
            delta = delta - 2.0 * PI * floor((delta + PI) / (2.0 * PI));
            let outside = (abs(delta) - span) * r;
            d = max(ring, outside);
            if (span >= PI) {
                d = ring;
            }
        }
        case KIND_TRIANGLE: {
            // Pointing right, then turned by params.x quarter turns clockwise.
            let turns = i32(round(in.params.x));
            var q = p;
            if (turns == 1) {
                q = vec2(p.y, -p.x);
            } else if (turns == 2) {
                q = vec2(-p.x, -p.y);
            } else if (turns == 3) {
                q = vec2(-p.y, p.x);
            }
            let h = min(half.x, half.y);
            d = triangle(q, vec2(-h * 0.8, -h), vec2(-h * 0.8, h), vec2(h, 0.0));
        }
        case KIND_SPIKE: {
            let turns = i32(round(in.params.x));
            var q = p;
            var h = half;
            if (turns == 1) {
                q = vec2(p.y, -p.x);
                h = half.yx;
            } else if (turns == 2) {
                q = vec2(-p.x, -p.y);
            } else if (turns == 3) {
                q = vec2(-p.y, p.x);
                h = half.yx;
            }
            d = triangle(q, vec2(-h.x, -h.y), vec2(-h.x, h.y), vec2(h.x, 0.0));
        }
        default: {
            d = round_box(p, half, radius);
        }
    }
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    let t = clamp((in.position.y - in.rect.y) / max(in.rect.w, 1.0), 0.0, 1.0);
    let color = mix(in.color, in.color2, t);
    return vec4(color.rgb, color.a * coverage);
}
