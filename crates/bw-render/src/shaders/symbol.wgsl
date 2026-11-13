// Component symbols: one indirect draw per kind code (bucket). The culling
// pass groups the visible bodies by bucket; draw `b` has
// first_vertex = b << 16, so `vertex_index` carries both the bucket (high
// bits) and the vertex within the instance's mesh (low bits). Each instance
// looks up its own mesh (kind × variant × style) in `sym_info`; vertices
// past the end of a shorter mesh are clipped. State (LED lit, switch lever,
// pin colours) comes from the per-slot four-state levels, never from the
// mesh; an output pin stub reads the level of its own output (signal slot
// `net + pin`). Displays read their display signals the same way: segment
// `k` of a 7-segment display is signal `net + k`, bit `k` of a numeric
// output too (the digits are computed here from those levels).

const SYM_BUCKETS: u32 = 24u;
const SYM_VARIANTS: u32 = 256u;
const SYM_INFO_VCOUNT: u32 = 24u;
const SYM_INFO_TABLE: u32 = 48u;
// Per-definition macro meshes (symbols/atlas.rs INFO_MACRO): (start, count)
// of definition `kind >> 18` of a macro instance.
const SYM_INFO_MACRO: u32 = 24624u;
const SYM_MACRO_CODE: u32 = 17u;
// Per-annotation meshes (symbols/atlas.rs INFO_NOTE): (start, count) of
// note slot `kind >> 18` of a text annotation (slot 0: the static symbol).
const SYM_INFO_NOTE: u32 = 32816u;
const SYM_TEXT_CODE: u32 = 22u;

struct SymOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) color: vec3<f32>,
};

// Half line width (world units) per width class; mirrors symbols::Width.
fn sym_half_width(wclass: u32) -> f32 {
    switch wclass {
        case 1u: { return max(0.055, 0.6 / g.px_per_unit); }
        case 2u: { return wire_half_width(); }
        case 3u: { return max(0.04, 0.5 / g.px_per_unit); }
        default: { return 0.0; }
    }
}

// Unlit segment / digit colour (mirrors symbols::ghost).
fn ghost(ink: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(0.2, 0.2, 0.2) + ink * 0.1;
}

// 7-segment patterns (bit k = segment a + k) of 0..9, A, b, C, d, E, F.
fn seg_pattern(n: u32) -> u32 {
    switch n & 15u {
        case 0u: { return 0x3fu; }
        case 1u: { return 0x06u; }
        case 2u: { return 0x5bu; }
        case 3u: { return 0x4fu; }
        case 4u: { return 0x66u; }
        case 5u: { return 0x6du; }
        case 6u: { return 0x7du; }
        case 7u: { return 0x07u; }
        case 8u: { return 0x7fu; }
        case 9u: { return 0x6fu; }
        case 10u: { return 0x77u; }
        case 11u: { return 0x7cu; }
        case 12u: { return 0x39u; }
        case 13u: { return 0x5eu; }
        case 14u: { return 0x79u; }
        default: { return 0x71u; }
    }
}

const GLYPH_DASH: u32 = 0x40u;
const GLYPH_Z: u32 = 0x100u;
const GLYPH_X: u32 = 0x200u;

// Glyph of digit `d` (0 = rightmost) of a numeric output whose bit k is
// signal `net + k`: segment mask, plus GLYPH_Z / GLYPH_X when the digit
// shows a floating / unknown input as a dash. `variant` = bits - 1 (0..4),
// format (4..6: binary, hex, decimal), signed (6).
fn numeric_glyph(net: u32, variant: u32, d: u32) -> u32 {
    if (net == NO_NET) {
        return GLYPH_DASH | GLYPH_Z;
    }
    let bits = (variant & 15u) + 1u;
    let fmt = (variant >> 4u) & 3u;
    var value = 0u;
    var zmask = 0u;
    var xmask = 0u;
    for (var k = 0u; k < bits; k++) {
        let l = net_level(net + k);
        if (l == LVL_HIGH) {
            value |= 1u << k;
        } else if (l == LVL_Z) {
            zmask |= 1u << k;
        } else if (l == LVL_X) {
            xmask |= 1u << k;
        }
    }
    // Inputs this digit depends on (decimal: all of them).
    var dep = 0xffffffffu;
    if (fmt == 0u) {
        dep = 1u << d;
    } else if (fmt == 1u) {
        dep = 15u << (4u * d);
    }
    if ((xmask & dep) != 0u) {
        return GLYPH_DASH | GLYPH_X;
    }
    if ((zmask & dep) != 0u) {
        return GLYPH_DASH | GLYPH_Z;
    }
    if (fmt == 0u) {
        return seg_pattern((value >> d) & 1u);
    }
    if (fmt == 1u) {
        return seg_pattern((value >> (4u * d)) & 15u);
    }
    // Decimal; two's complement when signed. Leading zeros are blank and a
    // minus sign sits left of the first digit.
    var v = value;
    var neg = false;
    if (((variant >> 6u) & 1u) == 1u && ((value >> (bits - 1u)) & 1u) == 1u) {
        neg = true;
        v = (1u << bits) - value;
    }
    var nd = 1u;
    var t = v / 10u;
    while (t != 0u) {
        nd += 1u;
        t = t / 10u;
    }
    if (d < nd) {
        var p = v;
        for (var i = 0u; i < d; i++) {
            p = p / 10u;
        }
        return seg_pattern(p % 10u);
    }
    if (d == nd && neg) {
        return GLYPH_DASH;
    }
    return 0u;
}

// Colour of segment `seg` of a 7-segment display: lit by display signal
// `net + seg` at 1 (common cathode) or 0 (common anode); floating segments
// grey, unknown ones in the X colour.
fn segment_color(net: u32, seg: u32, anode: u32, ink: vec3<f32>) -> vec3<f32> {
    var l = LVL_Z;
    if (net != NO_NET) {
        l = net_level(net + seg);
    }
    if (l == LVL_X) {
        return X_COLOR;
    }
    if (l == LVL_Z) {
        return vec3<f32>(0.38, 0.39, 0.42);
    }
    if ((l == LVL_HIGH) != (anode == 1u)) {
        return ink;
    }
    return ghost(ink);
}

// Colour of segment `seg` of digit `d` of a numeric output.
fn digit_color(net: u32, variant: u32, d: u32, seg: u32, ink: vec3<f32>) -> vec3<f32> {
    let glyph = numeric_glyph(net, variant, d);
    if (((glyph >> seg) & 1u) == 0u) {
        return ghost(ink);
    }
    if ((glyph & GLYPH_X) != 0u) {
        return X_COLOR;
    }
    if ((glyph & GLYPH_Z) != 0u) {
        return vec3<f32>(0.55, 0.57, 0.62);
    }
    return ink;
}

// Colour of a role (mirrors symbols::shade_ink); `ink` is the instance's
// display colour.
fn sym_color(role: u32, code: u32, lvl: u32, ink: vec3<f32>) -> vec3<f32> {
    let accent = kind_accent(code);
    let outline = mix(accent, vec3<f32>(0.92, 0.92, 0.92), 0.45);
    switch role {
        case 1u: {
            // Bodies tint green while the output is high (not displays:
            // their first signal is a segment / bit, not an output).
            if (lvl == 1u && code != SEVEN_SEG_CODE && code != NUMERIC_CODE) {
                return vec3<f32>(0.19, 0.25, 0.21);
            }
            return vec3<f32>(0.16, 0.17, 0.20);
        }
        case 2u: {
            if (lvl == LVL_HIGH) {
                return ink;
            } else if (lvl == LVL_LOW) {
                return ink * 0.28;
            } else if (lvl == LVL_X) {
                return X_COLOR;
            }
            return vec3<f32>(0.32, 0.32, 0.32);
        }
        case 3u: { return level_color(lvl); }
        case 8u: { return ink; }
        default: { return outline; }
    }
}

// Local (unrotated) symbol point → world, for a body whose rotated bounds
// start at `min` with size `size`. Same mapping as Component::pin_pos.
fn sym_to_world(p: vec2<f32>, min: vec2<f32>, size: vec2<f32>, rot: u32) -> vec2<f32> {
    // Unrotated body size.
    var w = size.x;
    var h = size.y;
    if ((rot & 1u) == 1u) {
        w = size.y;
        h = size.x;
    }
    var r = p;
    switch rot {
        case 1u: { r = vec2<f32>(h - p.y, p.x); }
        case 2u: { r = vec2<f32>(w - p.x, h - p.y); }
        case 3u: { r = vec2<f32>(p.y, w - p.x); }
        default: {}
    }
    return min + r;
}

@vertex
fn vs_symbol(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> SymOut {
    var o: SymOut;
    o.pos = CLIPPED;
    let bucket = min(vi >> 16u, SYM_BUCKETS - 1u);
    let local = vi & 0xffffu;
    // Bucket bases are written by the culling pass into sym_info[0..24).
    let slot = vis_bodies[sym_info[bucket] + ii];
    let inst = bodies[slot];
    if (inst.kind == HIDDEN_KIND) {
        return o;
    }
    let code = min(inst.kind & 0xffu, SYM_BUCKETS - 1u);
    let variant = (inst.kind >> 8u) & 0xffu;
    let rot = (inst.kind >> 16u) & 3u;
    var mi = SYM_INFO_TABLE + ((min(g.style, 1u) * SYM_BUCKETS + code) * SYM_VARIANTS + variant) * 2u;
    if (code == SYM_MACRO_CODE) {
        mi = SYM_INFO_MACRO + (inst.kind >> 18u) * 2u;
    } else if (code == SYM_TEXT_CODE && (inst.kind >> 18u) != 0u) {
        mi = SYM_INFO_NOTE + (inst.kind >> 18u) * 2u;
    }
    let start = sym_info[mi];
    let count = sym_info[mi + 1u];
    if (local >= count) {
        return o;
    }
    let v = atlas[start + local];
    let lvl = net_level(inst.net);
    let layer = (v.attr >> 6u) & 3u;
    if ((layer == 1u && lvl != 1u) || (layer == 2u && lvl == 1u)) {
        return o;
    }
    let p = v.pos + v.ext * sym_half_width((v.attr >> 4u) & 3u);
    var world = sym_to_world(p, inst.min, inst.size, rot);
    if (v.anchor != 0xffffffffu) {
        // Labels: only the anchor rotates, the glyphs stay upright.
        let an = vec2<f32>(f32(v.anchor & 0xffffu), f32(v.anchor >> 16u)) / 256.0;
        world = sym_to_world(an, inst.min, inst.size, rot) + (p - an);
    }
    o.pos = g.view_proj * vec4<f32>(world, 0.0, 1.0);
    let role = v.attr & 0xfu;
    let pin = (v.attr >> 8u) & 0xffu;
    // The display colour only matters for state, segments, digits and ink
    // (most vertices skip the palette lookup).
    var ink = vec3<f32>(0.0, 0.0, 0.0);
    if (role == 2u || role >= 6u) {
        ink = inst_ink(inst.kind, code);
    }
    if (role == 6u) {
        o.color = segment_color(inst.net, pin, variant & 1u, ink);
    } else if (role == 7u) {
        o.color = digit_color(inst.net, variant, (v.attr >> 16u) & 0xffu, pin, ink);
    } else {
        var role_lvl = lvl;
        if (role == 3u && pin != 0u && inst.net != NO_NET) {
            role_lvl = net_level(inst.net + pin);
        }
        o.color = sym_color(role, code, role_lvl, ink);
    }
    return o;
}

@fragment
fn fs_symbol(in: SymOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
