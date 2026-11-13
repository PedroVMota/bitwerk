// Component symbols: one indirect draw per kind code (bucket). The culling
// pass groups the visible bodies by bucket; draw `b` has
// first_vertex = b << 16, so `vertex_index` carries both the bucket (high
// bits) and the vertex within the instance's mesh (low bits). Each instance
// looks up its own mesh (kind × variant × style) in `sym_info`; vertices
// past the end of a shorter mesh are clipped. State (LED lit, switch lever,
// pin colours) comes from the per-slot four-state levels, never from the
// mesh; an output pin stub reads the level of its own output (signal slot
// `net + pin`).

const SYM_BUCKETS: u32 = 19u;
const SYM_VARIANTS: u32 = 256u;
const SYM_INFO_VCOUNT: u32 = 19u;
const SYM_INFO_TABLE: u32 = 38u;
// Per-definition macro meshes (symbols/atlas.rs INFO_MACRO): (start, count)
// of definition `kind >> 18` of a macro instance.
const SYM_INFO_MACRO: u32 = 19494u;
const SYM_MACRO_CODE: u32 = 17u;

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

// Colour of a role (mirrors symbols::shade).
fn sym_color(role: u32, code: u32, lvl: u32) -> vec3<f32> {
    let accent = kind_accent(code);
    let outline = mix(accent, vec3<f32>(0.92, 0.92, 0.92), 0.45);
    switch role {
        case 1u: {
            if (lvl == 1u) {
                return vec3<f32>(0.19, 0.25, 0.21);
            }
            return vec3<f32>(0.16, 0.17, 0.20);
        }
        case 2u: {
            if (lvl == LVL_HIGH) {
                return accent;
            } else if (lvl == LVL_LOW) {
                return accent * 0.28;
            } else if (lvl == LVL_X) {
                return X_COLOR;
            }
            return vec3<f32>(0.32, 0.32, 0.32);
        }
        case 3u: { return level_color(lvl); }
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
    // Bucket bases are written by the culling pass into sym_info[0..19).
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
    var role_lvl = lvl;
    let role = v.attr & 0xfu;
    let pin = (v.attr >> 8u) & 0xffu;
    if (role == 3u && pin != 0u && inst.net != NO_NET) {
        role_lvl = net_level(inst.net + pin);
    }
    o.color = sym_color(role, code, role_lvl);
    return o;
}

@fragment
fn fs_symbol(in: SymOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
