// Instance data for drawing: pulled from storage buffers by slot. With
// FLAG_CULLED the instance index goes through the list of visible slots
// produced by the culling compute pass (see cull.wgsl).

@group(1) @binding(0) var<storage, read> bodies: array<Body>;
@group(1) @binding(1) var<storage, read> wires: array<Wire>;
@group(1) @binding(2) var<storage, read> vis_bodies: array<u32>;
@group(1) @binding(3) var<storage, read> vis_wires: array<u32>;

fn body_slot(ii: u32) -> u32 {
    if ((g.flags & FLAG_CULLED) != 0u) {
        return vis_bodies[ii];
    }
    return ii;
}

fn wire_slot(ii: u32) -> u32 {
    if ((g.flags & FLAG_CULLED) != 0u) {
        return vis_wires[ii];
    }
    return ii;
}

// Symbol mesh atlas (static, uploaded once) and its lookup table; see
// symbols/atlas.rs for the layout of `sym_info`.
struct SymVertex {
    pos: vec2<f32>,
    ext: vec2<f32>,
    // bits 0..4 role, 4..6 width class, 6..8 layer
    attr: u32,
    // Upright-label anchor (x, y in 1/256 units) or 0xffffffff.
    anchor: u32,
};

@group(1) @binding(4) var<storage, read> atlas: array<SymVertex>;
@group(1) @binding(5) var<storage, read> sym_info: array<u32>;
