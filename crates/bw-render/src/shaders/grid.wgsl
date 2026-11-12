// Background + grid: one full-screen triangle, everything computed per pixel.

struct GridOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex
fn vs_grid(@builtin(vertex_index) i: u32) -> GridOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    let ndc = uv * 2.0 - 1.0;
    var o: GridOut;
    o.pos = vec4<f32>(ndc, 0.0, 1.0);
    o.ndc = ndc;
    return o;
}

fn dot_mask(world: vec2<f32>, step: f32, radius_px: f32) -> f32 {
    let d = abs(fract(world / step + 0.5) - 0.5) * step * g.px_per_unit;
    return 1.0 - smoothstep(radius_px - 0.5, radius_px + 0.5, length(d));
}

@fragment
fn fs_grid(in: GridOut) -> @location(0) vec4<f32> {
    let w4 = g.inv_view_proj * vec4<f32>(in.ndc, 0.0, 1.0);
    let world = w4.xy / w4.w;
    let bg = vec3<f32>(0.115, 0.12, 0.135);
    let dot_col = vec3<f32>(0.32, 0.34, 0.38);
    var col = bg;
    // LOD: fine dots only when they are at least 6 px apart; coarse every 10.
    if (g.px_per_unit >= 6.0) {
        col = mix(col, dot_col, dot_mask(world, 1.0, 0.9 * g.pixels_per_point) * 0.8);
    }
    if (g.px_per_unit * 10.0 >= 8.0) {
        col = mix(col, dot_col * 1.25, dot_mask(world, 10.0, 1.3 * g.pixels_per_point));
    }
    return vec4<f32>(col, 1.0);
}
