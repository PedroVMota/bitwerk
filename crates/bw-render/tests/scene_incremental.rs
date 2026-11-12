#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

//! Incremental scene updates: slot reuse, dirty tracking, and R-tree /
//! instance consistency against a from-scratch build after every edit.

use bw_core::{Applied, Circuit, ComponentId, ComponentKind, Edit, GateOp, GridPos, PinRef, Rotation, WireId};
use bw_render::{Change, Item, NO_NET, Scene};
use glam::Vec2;

fn change_of(a: &Applied) -> Change {
    match a {
        Applied::Component(id) => Change::Component(*id),
        Applied::Wire { id, .. } => Change::Wire(*id),
    }
}

/// Applies an edit to both the circuit and the scene, like the app does.
fn edit(c: &mut Circuit, s: &mut Scene, e: &Edit, rev: u64) -> Applied {
    let a = c.apply(e).unwrap();
    s.apply(c, &[change_of(&a)], rev);
    a
}

fn add(c: &mut Circuit, s: &mut Scene, kind: ComponentKind, x: i32, y: i32) -> ComponentId {
    match edit(c, s, &Edit::AddComponent { kind, pos: GridPos::new(x, y), rotation: Rotation::R0 }, 0) {
        Applied::Component(id) => id,
        Applied::Wire { .. } => unreachable!(),
    }
}

fn connect(c: &mut Circuit, s: &mut Scene, from: ComponentId, to: ComponentId, pin: u8) -> WireId {
    let out = c.component(from).unwrap().kind.input_count();
    let e = Edit::Connect {
        from: PinRef { component: from, pin: out },
        to: PinRef { component: to, pin },
        via: vec![GridPos::new(-3, 7)],
    };
    match edit(c, s, &e, 0) {
        Applied::Wire { id, .. } => id,
        Applied::Component(_) => unreachable!(),
    }
}

fn and2() -> ComponentKind {
    ComponentKind::Gate { op: GateOp::And, inputs: 2 }
}

#[test]
fn add_move_delete_keep_rtree_consistent_and_reuse_slots() {
    let mut c = Circuit::new();
    let mut s = Scene::build(&c, 0);
    let sw = add(&mut c, &mut s, ComponentKind::Switch { on: true }, 0, 0);
    let g = add(&mut c, &mut s, and2(), 10, 0);
    let led = add(&mut c, &mut s, ComponentKind::Led, 20, 0);
    let w1 = connect(&mut c, &mut s, sw, g, 0);
    let w2 = connect(&mut c, &mut s, g, led, 0);
    s.validate(&c).unwrap();
    assert_eq!(s.wire_slots(w1).unwrap().len(), 2, "via → two segments");

    // The LED shows the gate's net (the gate's own body slot).
    let led_body = s.bodies()[s.body_slot(led).unwrap() as usize];
    assert_eq!(led_body.net, s.body_slot(g).unwrap());

    // Move: the body and both attached wires follow; the R-tree finds the
    // gate at its new place only.
    let _ = s.take_dirty();
    edit(&mut c, &mut s, &Edit::MoveComponent(g, GridPos::new(40, 30)), 1);
    s.validate(&c).unwrap();
    let d = s.take_dirty();
    assert_eq!(d.bodies, vec![s.body_slot(g).unwrap()]);
    assert_eq!(d.wires.len(), 2, "one segment per wire ends at the gate: {:?}", d.wires);
    assert_eq!(s.pick_body(Vec2::new(41.0, 31.0)), Some(g));
    assert_eq!(s.pick_body(Vec2::new(11.0, 1.0)), None);

    // Rotate.
    edit(&mut c, &mut s, &Edit::SetRotation(g, Rotation::R90), 2);
    s.validate(&c).unwrap();

    // Delete the gate: its body and both wires go; the LED loses its net.
    let g_slot = s.body_slot(g).unwrap();
    let w2_slots = s.wire_slots(w2).unwrap().to_vec();
    edit(&mut c, &mut s, &Edit::RemoveComponent(g), 3);
    s.validate(&c).unwrap();
    assert!(s.body_slot(g).is_none());
    assert!(s.bodies()[g_slot as usize].is_hidden());
    assert!(w2_slots.iter().all(|&w| s.wires()[w as usize].is_hidden()));
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, NO_NET);
    let mut hits = 0;
    s.query(Vec2::new(-100.0, -100.0), Vec2::new(100.0, 100.0), |it| {
        if let Item::Body(b) = it {
            assert_ne!(b, g_slot, "deleted body still in the R-tree");
        }
        hits += 1;
    });
    assert_eq!(hits, s.instance_count());

    // Re-adding reuses the freed slots instead of growing the arrays.
    let (nb, nw) = (s.bodies().len(), s.wires().len());
    let g2 = add(&mut c, &mut s, and2(), 10, 0);
    assert_eq!(s.body_slot(g2), Some(g_slot));
    connect(&mut c, &mut s, g2, led, 0);
    connect(&mut c, &mut s, sw, g2, 1);
    assert_eq!((s.bodies().len(), s.wires().len()), (nb, nw), "slots reused");
    s.validate(&c).unwrap();
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, g_slot);
}

#[test]
fn disconnect_updates_led_and_frees_segments() {
    let mut c = Circuit::new();
    let mut s = Scene::build(&c, 0);
    let sw = add(&mut c, &mut s, ComponentKind::Switch { on: false }, 0, 0);
    let led = add(&mut c, &mut s, ComponentKind::Led, 10, 0);
    let w = connect(&mut c, &mut s, sw, led, 0);
    assert_ne!(s.bodies()[s.body_slot(led).unwrap() as usize].net, NO_NET);
    edit(&mut c, &mut s, &Edit::Disconnect(w), 1);
    s.validate(&c).unwrap();
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, NO_NET);
    assert!(s.wire_slots(w).is_none());
    assert!((s.free_ratio() - 0.5).abs() < 1e-6, "2 of 4 slots free");
}

/// Deterministic pseudo-random edit storm on a mid-sized circuit; the
/// incremental scene must equal a fresh build after every single edit.
#[test]
fn random_edit_sequence_matches_full_build() {
    let mut c = bw_core::stress::generate(800).unwrap();
    let mut s = Scene::build(&c, 0);
    s.validate(&c).unwrap();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut rnd = |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % n.max(1) as u64) as usize
    };
    let mut applied = 0;
    for step in 0..400u64 {
        let ids: Vec<ComponentId> = c.components().map(|k| k.id).collect();
        let id = ids[rnd(ids.len())];
        let e = match rnd(6) {
            0 => Edit::MoveComponent(id, GridPos::new(rnd(2000) as i32, rnd(400) as i32)),
            1 => Edit::SetRotation(id, c.component(id).unwrap().rotation.rotated_cw()),
            2 => Edit::RemoveComponent(id),
            3 => Edit::AddComponent {
                kind: ComponentKind::Not,
                pos: GridPos::new(rnd(2000) as i32, 5),
                rotation: Rotation::R0,
            },
            4 => {
                let wires: Vec<WireId> = c.wires().map(|w| w.id).collect();
                if wires.is_empty() {
                    continue;
                }
                Edit::Disconnect(wires[rnd(wires.len())])
            }
            _ => {
                // Connect some output to some free input, if the model allows it.
                let to = ids[rnd(ids.len())];
                let from = id;
                let (Some(f), Some(t)) = (c.component(from), c.component(to)) else { continue };
                if f.kind.output_count() == 0 || t.kind.input_count() == 0 {
                    continue;
                }
                Edit::Connect {
                    from: PinRef { component: from, pin: f.kind.input_count() },
                    to: PinRef { component: to, pin: 0 },
                    via: vec![],
                }
            }
        };
        let Ok(a) = c.apply(&e) else { continue };
        s.apply(&c, &[change_of(&a)], step + 1);
        applied += 1;
        s.validate(&c).unwrap_or_else(|err| panic!("after step {step} ({e:?}): {err}"));
    }
    assert!(applied > 250, "only {applied} edits applied");
}
