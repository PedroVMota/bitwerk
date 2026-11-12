// Wire segments: one instanced quad per wire slot (straight segment).

struct WireOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) color: vec3<f32>,
};

@vertex
fn vs_wire(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> WireOut {
    let inst = wires[wire_slot(ii)];
    var o: WireOut;
    if ((inst.flags & 1u) != 0u) {
        o.pos = CLIPPED;
        return o;
    }
    let c = quad_corner(vi);
    let delta = inst.b - inst.a;
    let len = length(delta);
    var dir = vec2<f32>(1.0, 0.0);
    if (len > 1e-6) {
        dir = delta / len;
    }
    let normal = vec2<f32>(-dir.y, dir.x);
    let half_w = wire_half_width();
    let along = mix(-half_w, len + half_w, c.x);
    let across = mix(-half_w, half_w, c.y);
    let world = inst.a + dir * along + normal * across;
    let lvl = net_level(inst.net);
    o.pos = g.view_proj * vec4<f32>(world, 0.0, 1.0);
    o.color = level_color(lvl);
    return o;
}

@fragment
fn fs_wire(in: WireOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.color, 1.0);
}
