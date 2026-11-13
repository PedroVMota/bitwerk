// Component bodies at far zoom (level of detail): one instanced quad per
// body slot, coloured by kind. Closer up the symbol meshes are drawn instead
// (symbol.wgsl).

struct BodyOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) @interpolate(flat) fill: vec3<f32>,
    @location(3) @interpolate(flat) border: vec3<f32>,
};

@vertex
fn vs_body(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> BodyOut {
    let inst = bodies[body_slot(ii)];
    var o: BodyOut;
    // Junction dots exist only as symbols (they would be specks here).
    if (inst.kind == HIDDEN_KIND || (inst.kind & 0xffu) == JUNCTION_CODE) {
        o.pos = CLIPPED;
        return o;
    }
    let c = quad_corner(vi);
    let world = inst.min + c * inst.size;
    let lvl = net_level(inst.net);
    let code = inst.kind & 0xffu;
    let accent = kind_accent(code);
    var fill = vec3<f32>(0.20, 0.21, 0.24);
    if ((code >= 7u && code <= 9u) || code == 16u) {
        // Sources and LEDs show their level in the fill colour.
        if (lvl == LVL_HIGH) {
            fill = accent;
        } else if (lvl == LVL_LOW) {
            fill = accent * 0.25;
        } else if (lvl == LVL_X) {
            fill = X_COLOR;
        } else {
            fill = vec3<f32>(0.3, 0.3, 0.3);
        }
    } else if (lvl == LVL_HIGH) {
        fill = vec3<f32>(0.22, 0.27, 0.22);
    }
    o.pos = g.view_proj * vec4<f32>(world, 0.0, 1.0);
    o.local = c * inst.size;
    o.size = inst.size;
    o.fill = fill;
    o.border = accent;
    return o;
}

@fragment
fn fs_body(in: BodyOut) -> @location(0) vec4<f32> {
    // LOD: below ~3 px per unit draw flat squares (no border work).
    if (g.px_per_unit < 3.0) {
        return vec4<f32>(in.border * 0.8, 1.0);
    }
    let edge = min(in.local, in.size - in.local) * g.px_per_unit;
    let d = min(edge.x, edge.y);
    let border_px = max(1.0, 0.12 * g.px_per_unit);
    let t = smoothstep(border_px - 0.5, border_px + 0.5, d);
    return vec4<f32>(mix(in.border, in.fill, t), 1.0);
}
