// Shared declarations for all canvas shaders (prepended at build time).

struct Globals {
    view_proj: mat4x4<f32>,
    inv_view_proj: mat4x4<f32>,
    viewport_px: vec2<f32>,
    // Physical pixels per world (grid) unit.
    px_per_unit: f32,
    pixels_per_point: f32,
    // bit 0: net levels valid; bit 1: draw through the GPU-culled index lists
    flags: u32,
    // Number of signal slots in net_bits.
    net_count: u32,
    // Slots uploaded so far (drawn / culled range).
    body_count: u32,
    wire_count: u32,
    // World-space culling rectangle (min.xy, max.xy).
    cull_rect: vec4<f32>,
    // Workgroups of the culling dispatch.
    cull_groups: u32,
    // Symbol style: 0 ANSI, 1 IEC (selects the mesh table half).
    style: u32,
    _pad1: u32,
    _pad2: u32,
};

// Instance records (also the storage-buffer layout).
struct Body {
    min: vec2<f32>,
    size: vec2<f32>,
    kind: u32,
    // First signal slot of the body's outputs (LED: its driver), or NO_NET.
    net: u32,
};

struct Wire {
    a: vec2<f32>,
    b: vec2<f32>,
    net: u32,
    // bit 0: hidden (free slot)
    flags: u32,
};

@group(0) @binding(0) var<uniform> g: Globals;
// Four-state level per signal slot: 2 bits, 16 per word.
@group(0) @binding(1) var<storage, read> net_bits: array<u32>;

const NO_NET: u32 = 0xffffffffu;
const HIDDEN_KIND: u32 = 0xffffffffu;
const FLAG_NETS_VALID: u32 = 1u;
const FLAG_CULLED: u32 = 2u;
// A position outside the clip volume: every vertex of a hidden instance goes
// there, so it is clipped without rasterising anything.
const CLIPPED: vec4<f32> = vec4<f32>(2.0, 2.0, 2.0, 1.0);

const LVL_LOW: u32 = 0u;
const LVL_HIGH: u32 = 1u;
const LVL_Z: u32 = 2u;
const LVL_X: u32 = 3u;
// Colour of the unknown level (mirrors symbols::X_COLOR).
const X_COLOR: vec3<f32> = vec3<f32>(1.0, 0.55, 0.10);

// 0 = low, 1 = high, 2 = floating (also: no snapshot / unconnected),
// 3 = unknown.
fn net_level(net: u32) -> u32 {
    if ((g.flags & FLAG_NETS_VALID) == 0u || net == NO_NET || net >= g.net_count) {
        return LVL_Z;
    }
    return (net_bits[net >> 4u] >> ((net & 15u) * 2u)) & 3u;
}

// Wire / pin colour of a net level (mirrors symbols::level_color).
fn level_color(lvl: u32) -> vec3<f32> {
    if (lvl == LVL_HIGH) {
        return vec3<f32>(0.30, 0.95, 0.40);
    } else if (lvl == LVL_LOW) {
        return vec3<f32>(0.12, 0.42, 0.18);
    } else if (lvl == LVL_X) {
        return X_COLOR;
    }
    return vec3<f32>(0.45, 0.50, 0.58);
}

// kind code: 0..5 gates (AND OR NAND NOR XOR XNOR), 6 NOT, 7 switch, 8 clock,
// 9 LED, 10 tri-state, 11..13 D / RS / JK flip-flops, 14 RAM, 15 ROM,
// 16 macro tag, 17 macro instance, 18 VCC / ground, 19 sequence generator,
// 20 7-segment display, 21 numeric output, 22 text, 23 junction dot
// (mirrors symbols::SymbolKind::code and ::accent).
const MACRO_CODE: u32 = 17u;
const CONST_CODE: u32 = 18u;
const SEQ_CODE: u32 = 19u;
const SEVEN_SEG_CODE: u32 = 20u;
const NUMERIC_CODE: u32 = 21u;
const TEXT_CODE: u32 = 22u;
const JUNCTION_CODE: u32 = 23u;
fn kind_accent(kind: u32) -> vec3<f32> {
    switch kind {
        case 0u, 2u: { return vec3<f32>(0.35, 0.62, 0.95); }
        case 1u, 3u: { return vec3<f32>(0.95, 0.62, 0.30); }
        case 4u, 5u: { return vec3<f32>(0.70, 0.45, 0.95); }
        case 6u, 10u: { return vec3<f32>(0.85, 0.85, 0.40); }
        case 7u: { return vec3<f32>(0.40, 0.85, 0.55); }
        case 8u, 19u: { return vec3<f32>(0.40, 0.85, 0.85); }
        case 11u, 12u, 13u: { return vec3<f32>(0.95, 0.55, 0.78); }
        case 14u, 15u: { return vec3<f32>(0.55, 0.78, 0.95); }
        case 16u: { return vec3<f32>(0.95, 0.80, 0.35); }
        case 17u: { return vec3<f32>(0.78, 0.78, 0.84); }
        case 18u: { return vec3<f32>(0.92, 0.66, 0.38); }
        case 22u: { return vec3<f32>(0.80, 0.82, 0.86); }
        case 23u: { return vec3<f32>(0.45, 0.50, 0.58); }
        default: { return vec3<f32>(0.95, 0.35, 0.35); }
    }
}

// Default display colour of a kind (mirrors symbols::SymbolKind::ink).
fn kind_ink(kind: u32) -> vec3<f32> {
    switch kind {
        case 19u, 20u: { return vec3<f32>(0.95, 0.35, 0.35); }
        case 21u, 22u: { return vec3<f32>(0.86, 0.88, 0.92); }
        default: { return kind_accent(kind); }
    }
}

// Display colours of the 16-entry palette (mirrors symbols::palette_rgb;
// black is drawn as the default foreground on the dark canvas).
fn palette_color(i: u32) -> vec3<f32> {
    // A switch, not a local array: no per-invocation table copy.
    switch i & 15u {
        case 0u: { return vec3<f32>(0.95, 0.25, 0.22); }
        case 1u: { return vec3<f32>(1.00, 0.58, 0.12); }
        case 2u: { return vec3<f32>(0.98, 0.92, 0.20); }
        case 3u: { return vec3<f32>(0.45, 0.98, 0.30); }
        case 4u: { return vec3<f32>(0.18, 0.72, 0.28); }
        case 5u: { return vec3<f32>(0.25, 0.92, 0.95); }
        case 6u: { return vec3<f32>(0.32, 0.48, 1.00); }
        case 7u: { return vec3<f32>(0.32, 0.38, 0.82); }
        case 8u: { return vec3<f32>(0.70, 0.38, 0.88); }
        case 9u: { return vec3<f32>(0.98, 0.32, 0.92); }
        case 10u: { return vec3<f32>(1.00, 1.00, 1.00); }
        case 11u: { return vec3<f32>(0.62, 0.63, 0.66); }
        case 12u: { return vec3<f32>(0.86, 0.88, 0.92); }
        case 13u: { return vec3<f32>(0.74, 0.47, 0.22); }
        case 14u: { return vec3<f32>(0.68, 0.68, 0.34); }
        default: { return vec3<f32>(0.16, 0.70, 0.68); }
    }
}

// Display colour of an instance: its palette colour (packed kind: flag 16 |
// index, at bits 18..23, or 8..13 for text), else the kind's default.
fn inst_ink(kind: u32, code: u32) -> vec3<f32> {
    var c = (kind >> 18u) & 31u;
    if (code == TEXT_CODE) {
        c = (kind >> 8u) & 31u;
    }
    if (code == MACRO_CODE || (c & 16u) == 0u) {
        return kind_ink(code);
    }
    return palette_color(c & 15u);
}

// Wires are 0.12 grid units wide, but never thinner than 1 physical pixel.
fn wire_half_width() -> f32 {
    return max(0.06, 0.5 / g.px_per_unit);
}

// Two triangles covering the unit square, indexed by vertex_index (0..6).
fn quad_corner(i: u32) -> vec2<f32> {
    var c = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    return c[i];
}
