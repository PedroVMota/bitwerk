#![allow(clippy::unwrap_used, clippy::expect_used, clippy::many_single_char_names)]

//! Validates the WGSL shaders offline (no GPU needed), so shader errors are
//! caught by `cargo test` instead of at app start-up.

fn validate(src: &str, entry_points: &[&str]) {
    let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{}", e.emit_to_string(src)));
    let mut v = naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::default());
    v.validate(&module).unwrap_or_else(|e| panic!("{}", e.emit_to_string(src)));
    for ep in entry_points {
        assert!(module.entry_points.iter().any(|e| e.name == *ep), "missing entry point {ep}");
    }
}

#[test]
fn canvas_shaders_parse_and_validate() {
    validate(
        bw_render::shader_source(),
        &["vs_grid", "fs_grid", "vs_body", "fs_body", "vs_wire", "fs_wire", "vs_symbol", "fs_symbol"],
    );
}

#[test]
fn cull_shaders_parse_and_validate() {
    validate(bw_render::cull_shader_source(), &["cs_count", "cs_scan", "cs_write"]);
}

#[test]
fn scene_build_and_query() {
    use bw_core::{Circuit, ComponentKind, GateOp, GridPos, PinRef, Rotation};
    let mut c = Circuit::new();
    let s = c.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, 0), Rotation::R0).unwrap();
    let g = c
        .add_component(ComponentKind::Gate { op: GateOp::And, inputs: 2 }, GridPos::new(100, 0), Rotation::R0)
        .unwrap();
    c.connect(PinRef { component: s, pin: 0 }, PinRef { component: g, pin: 0 }, vec![GridPos::new(50, 1)]).unwrap();
    let scene = bw_render::Scene::build(&c, 7);
    assert_eq!(scene.bodies().len(), 2);
    assert_eq!(scene.wires().len(), 2);
    assert_eq!(scene.pick_body(glam::Vec2::new(101.0, 1.0)), Some(g));
    let mut n = 0;
    scene.query(glam::Vec2::new(-1.0, -1.0), glam::Vec2::new(3.0, 3.0), |_| n += 1);
    assert_eq!(n, 2); // switch body + first wire segment
}
