#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]

//! Incremental scene updates: slot reuse, dirty tracking, junction dots and
//! R-tree / instance consistency against a from-scratch build after every
//! edit.

use bw_core::{
    Applied, Circuit, ComponentId, ComponentKind, Edit, GateOp, GridPos, PinRef, Rotation, SegmentId, WireEnd,
};
use bw_render::{Item, NO_NET, Scene};
use glam::Vec2;

/// Applies an edit to both the circuit and the scene, like the app does.
fn edit(c: &mut Circuit, s: &mut Scene, e: &Edit, rev: u64) -> Applied {
    let a = c.apply(e).unwrap();
    s.apply(c, &a, rev);
    a
}

fn add(c: &mut Circuit, s: &mut Scene, kind: ComponentKind, x: i32, y: i32) -> ComponentId {
    edit(c, s, &Edit::AddComponent { kind, pos: GridPos::new(x, y), rotation: Rotation::R0 }, 0).component().unwrap()
}

/// Wires `from`'s first output to input `pin` of `to` (an L via (-3, 7)).
fn connect(c: &mut Circuit, s: &mut Scene, from: ComponentId, to: ComponentId, pin: u8) -> Vec<SegmentId> {
    let out = c.component(from).unwrap().kind.input_count();
    let e = Edit::AddWire {
        from: WireEnd::Pin(PinRef { component: from, pin: out }),
        to: WireEnd::Pin(PinRef { component: to, pin }),
        via: vec![GridPos::new(-3, 7)],
    };
    let a = edit(c, s, &e, 0);
    a.segments.into_iter().filter(|&id| c.segment(id).is_some()).collect()
}

fn and2() -> ComponentKind {
    ComponentKind::Gate { op: GateOp::And, inputs: 2 }
}

fn seg_at(c: &Circuit, p: GridPos) -> SegmentId {
    c.segments()
        .map(|(id, _)| id)
        .find(|&id| {
            let (a, b) = c.segment_ends(id).unwrap();
            (a.x == b.x && p.x == a.x && p.y > a.y.min(b.y) && p.y < a.y.max(b.y))
                || (a.y == b.y && p.y == a.y && p.x > a.x.min(b.x) && p.x < a.x.max(b.x))
        })
        .expect("no segment there")
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
    assert!(w1.len() >= 3, "routed path → several segments");

    // The LED shows the gate's net (the gate output's signal slot).
    let led_body = s.bodies()[s.body_slot(led).unwrap() as usize];
    assert_eq!(led_body.net, s.signal_slot(g, 0).unwrap());

    // Move: the body and the attached wires follow; the R-tree finds the
    // gate at its new place only.
    let _ = s.take_dirty();
    edit(&mut c, &mut s, &Edit::MoveComponent(g, GridPos::new(40, 30)), 1);
    s.validate(&c).unwrap();
    let d = s.take_dirty();
    assert_eq!(d.bodies, vec![s.body_slot(g).unwrap()]);
    assert!(!d.wires.is_empty() && d.wires.len() <= 6, "only the rubber-banded segments: {:?}", d.wires);
    assert_eq!(s.pick_body(Vec2::new(41.0, 31.0)), Some(g));
    assert_eq!(s.pick_body(Vec2::new(11.0, 1.0)), None);

    // Rotate.
    edit(&mut c, &mut s, &Edit::SetRotation(g, Rotation::R90), 2);
    s.validate(&c).unwrap();

    // Delete the gate: its body and both wires go; the LED loses its net.
    let g_slot = s.body_slot(g).unwrap();
    let g_sig = s.signal_slot(g, 0).unwrap();
    let w2_slots: Vec<u32> = w2.iter().filter_map(|&id| s.segment_slot(id)).collect();
    edit(&mut c, &mut s, &Edit::RemoveComponent(g), 3);
    s.validate(&c).unwrap();
    assert_eq!(c.segment_count(), 0);
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
    assert_eq!((s.bodies().len(), s.wires().len()), (nb, nw), "slots reused");
    s.validate(&c).unwrap();
    assert_eq!(s.signal_slot(g2, 0), Some(g_sig), "signal slot reused");
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, g_sig);
}

/// Multi-output parts own contiguous signal slots (Q, Q̄; memory data), each
/// wire carries the slot of its net's driver, a bus shows its smallest
/// driver's slot, and a memory resized to more outputs gets a new range.
#[test]
fn signal_slots_of_multi_output_parts_and_buses() {
    use bw_core::{FlipFlopType, MemoryType};
    let mut c = Circuit::new();
    let mut s = Scene::build(&c, 0);
    let ff = add(&mut c, &mut s, ComponentKind::flip_flop(FlipFlopType::D), 0, 0);
    let t1 = add(&mut c, &mut s, ComponentKind::TriState, 10, 0);
    let t2 = add(&mut c, &mut s, ComponentKind::TriState, 10, 10);
    let led = add(&mut c, &mut s, ComponentKind::Led, 20, 0);
    let q = s.signal_slot(ff, 0).unwrap();
    assert_eq!(s.signal_slot(ff, 1), Some(q + 1), "Q̄ follows Q");
    assert_eq!(s.signal_slot(ff, 2), None);
    assert_eq!(s.bodies()[s.body_slot(ff).unwrap() as usize].net, q);
    // Q̄ → t1 enable: the wire carries Q̄'s slot.
    let e = Edit::AddWire {
        from: WireEnd::Pin(PinRef { component: ff, pin: 5 }),
        to: WireEnd::Pin(PinRef { component: t1, pin: 1 }),
        via: vec![],
    };
    let a = edit(&mut c, &mut s, &e, 1);
    let w = a.segments.iter().copied().find(|&id| c.segment(id).is_some()).unwrap();
    assert_eq!(s.wires()[s.segment_slot(w).unwrap() as usize].net, q + 1);
    // Bus: both tri-states drive the LED; it shows the smaller driver.
    connect(&mut c, &mut s, t1, led, 0);
    connect(&mut c, &mut s, t2, led, 0);
    s.validate(&c).unwrap();
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, s.signal_slot(t1, 0).unwrap());
    // Remove t1: the whole bus net now shows t2.
    edit(&mut c, &mut s, &Edit::RemoveComponent(t1), 2);
    s.validate(&c).unwrap();
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, s.signal_slot(t2, 0).unwrap());
    // Memory: 8 data outputs, then resized to 16.
    let ram = ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 4, data_bits: 8 };
    let m = add(&mut c, &mut s, ram, 30, 0);
    let first = s.signal_slot(m, 0).unwrap();
    assert_eq!(s.signal_slot(m, 7), Some(first + 7));
    let layout = s.layout_version();
    let wide = ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 4, data_bits: 16 };
    edit(&mut c, &mut s, &Edit::SetKind(m, wide), 3);
    assert!(s.signal_slot(m, 15).is_some());
    assert_ne!(s.layout_version(), layout, "the net mapping must be rebuilt");
    s.validate(&c).unwrap();
}

/// A branch through a junction: the dot appears and is coloured with the
/// net; splitting the net (deleting the trunk) recolours the cut-off side
/// as floating, and the dot goes.
#[test]
fn junctions_dots_and_net_colours_follow_wiring_edits() {
    let mut c = Circuit::new();
    let mut s = Scene::build(&c, 0);
    let sw = add(&mut c, &mut s, ComponentKind::Switch { on: false }, 0, 0);
    let l1 = add(&mut c, &mut s, ComponentKind::Led, 12, 0);
    let l2 = add(&mut c, &mut s, ComponentKind::Led, 12, 6);
    edit(
        &mut c,
        &mut s,
        &Edit::AddWire {
            from: WireEnd::Pin(PinRef { component: sw, pin: 0 }),
            to: WireEnd::Pin(PinRef { component: l1, pin: 0 }),
            via: vec![],
        },
        1,
    );
    let trunk = seg_at(&c, GridPos::new(6, 1));
    edit(
        &mut c,
        &mut s,
        &Edit::AddWire {
            from: WireEnd::OnSegment(trunk, GridPos::new(6, 1)),
            to: WireEnd::Pin(PinRef { component: l2, pin: 0 }),
            via: vec![GridPos::new(6, 7)],
        },
        2,
    );
    s.validate(&c).unwrap();
    assert_eq!(s.dot_count(), 1);
    let j = c.nodes().find(|(_, n)| n.is_junction()).unwrap().0;
    let dot = s.bodies()[s.dot_slot(j).unwrap() as usize];
    let sig = s.signal_slot(sw, 0).unwrap();
    assert_eq!(dot.net, sig);
    assert_eq!(s.bodies()[s.body_slot(l2).unwrap() as usize].net, sig);
    // Geometry: drag the branch's lower segment and move the junction; the
    // new segments inherit the colour, nothing else is touched.
    let low = seg_at(&c, GridPos::new(9, 7));
    edit(&mut c, &mut s, &Edit::DragSegment(low, 2), 3);
    s.validate(&c).unwrap();
    edit(&mut c, &mut s, &Edit::MoveNode(j, GridPos::new(8, 1)), 4);
    s.validate(&c).unwrap();
    // Cut the trunk between switch and junction: everything past the cut
    // floats (no driver), the switch's stub is trimmed away, the junction is
    // left with two segments (a bend: no dot).
    let cut = seg_at(&c, GridPos::new(3, 1));
    edit(&mut c, &mut s, &Edit::RemoveSegment(cut), 5);
    s.validate(&c).unwrap();
    assert_eq!(s.bodies()[s.body_slot(l1).unwrap() as usize].net, NO_NET);
    assert_eq!(s.bodies()[s.body_slot(l2).unwrap() as usize].net, NO_NET);
    assert!(c.pin_node(PinRef { component: sw, pin: 0 }).is_none());
    assert_eq!(s.dot_count(), 0);
    // Reconnect the switch onto the floating net through a new junction.
    let rest = seg_at(&c, GridPos::new(10, 1));
    edit(
        &mut c,
        &mut s,
        &Edit::AddWire {
            from: WireEnd::Pin(PinRef { component: sw, pin: 0 }),
            to: WireEnd::OnSegment(rest, GridPos::new(10, 1)),
            via: vec![GridPos::new(4, 1), GridPos::new(4, -2), GridPos::new(10, -2)],
        },
        6,
    );
    s.validate(&c).unwrap();
    assert_eq!(s.bodies()[s.body_slot(l2).unwrap() as usize].net, sig);
    // The old junction became a plain bend when the trunk was cut; the new
    // wire ends on a segment: one dot.
    assert_eq!(s.dot_count(), 1);
}

#[test]
fn deleting_the_only_segment_updates_led_and_frees_it() {
    let mut c = Circuit::new();
    let mut s = Scene::build(&c, 0);
    let sw = add(&mut c, &mut s, ComponentKind::Switch { on: false }, 0, 0);
    let led = add(&mut c, &mut s, ComponentKind::Led, 10, 0);
    let w = connect(&mut c, &mut s, sw, led, 0);
    assert_ne!(s.bodies()[s.body_slot(led).unwrap() as usize].net, NO_NET);
    for id in w {
        if c.segment(id).is_some() {
            edit(&mut c, &mut s, &Edit::RemoveSegment(id), 1);
            s.validate(&c).unwrap();
            assert!(s.segment_slot(id).is_none());
        }
    }
    assert_eq!(c.segment_count(), 0);
    assert_eq!(s.bodies()[s.body_slot(led).unwrap() as usize].net, NO_NET);
}

/// Deterministic pseudo-random numbers (no dependency).
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

/// Deterministic pseudo-random edit storm on a mid-sized circuit: component
/// moves / rotations / adds / deletes and wiring edits (new wires, branches
/// that split segments, segment deletes and drags, junction / bend moves).
/// The incremental scene must equal a fresh build after every single edit.
#[test]
fn random_edit_sequence_matches_full_build() {
    let mut c = bw_core::stress::generate(800).unwrap();
    let mut s = Scene::build(&c, 0);
    s.validate(&c).unwrap();
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let mut applied = 0;
    let mut wiring = 0;
    for step in 0..600u64 {
        let ids: Vec<ComponentId> = c.components().map(|k| k.id).collect();
        let segs: Vec<SegmentId> = c.segments().map(|(id, _)| id).collect();
        let id = ids[r.below(ids.len())];
        let pin_of = |r: &mut Rng, id: ComponentId| {
            let n = c.component(id).unwrap().kind.pin_count();
            PinRef { component: id, pin: r.below(usize::from(n)) as u8 }
        };
        let e = match r.below(10) {
            0 => Edit::MoveComponent(id, GridPos::new(r.below(2000) as i32, r.below(400) as i32)),
            1 => Edit::SetRotation(id, c.component(id).unwrap().rotation.rotated_cw()),
            2 => Edit::RemoveComponent(id),
            3 => Edit::AddComponent {
                kind: ComponentKind::Not,
                pos: GridPos::new(r.below(2000) as i32, 5),
                rotation: Rotation::R0,
            },
            4 => Edit::RemoveSegment(segs[r.below(segs.len())]),
            5 => {
                // Branch from the middle of a segment to some pin.
                let sg = segs[r.below(segs.len())];
                let (a, b) = c.segment_ends(sg).unwrap();
                let p = GridPos::new((a.x + b.x) / 2, (a.y + b.y) / 2);
                let other = ids[r.below(ids.len())];
                let to = pin_of(&mut r, other);
                Edit::AddWire { from: WireEnd::OnSegment(sg, p), to: WireEnd::Pin(to), via: vec![p.offset(0, 3)] }
            }
            6 => Edit::DragSegment(segs[r.below(segs.len())], r.below(7) as i32 - 3),
            7 => {
                let free: Vec<_> = c.nodes().filter(|(_, n)| n.is_free()).map(|(id, _)| id).collect();
                let n = free[r.below(free.len())];
                let p = c.node(n).unwrap().pos;
                Edit::MoveNode(n, p.offset(r.below(5) as i32 - 2, r.below(5) as i32 - 2))
            }
            _ => {
                let to = ids[r.below(ids.len())];
                Edit::AddWire {
                    from: WireEnd::Pin(pin_of(&mut r, id)),
                    to: WireEnd::Pin(pin_of(&mut r, to)),
                    via: vec![GridPos::new(r.below(2000) as i32, r.below(400) as i32)],
                }
            }
        };
        let Ok(a) = c.apply(&e) else { continue };
        s.apply(&c, &a, step + 1);
        applied += 1;
        if matches!(e, Edit::AddWire { .. } | Edit::RemoveSegment(_) | Edit::DragSegment(..) | Edit::MoveNode(..)) {
            wiring += 1;
        }
        s.validate(&c).unwrap_or_else(|err| panic!("after step {step} ({e:?}): {err}"));
    }
    assert!(applied > 400 && wiring > 150, "only {applied} edits ({wiring} wiring) applied");
}

/// Macro instances: placing, wiring and redefining (the interface changes,
/// so every instance changes size and its wires are re-attached or
/// detached) patch the scene exactly like a full rebuild.
#[test]
fn macro_instances_and_definition_edits_match_full_build() {
    use bw_core::{MacroDef, MacroId, TagDir};
    use std::sync::Arc;
    let tag = |dir| ComponentKind::Tag { dir, tristate: false };
    let mut d = MacroDef::new("Gate pair");
    let a = d.body.add_component(tag(TagDir::In), GridPos::new(0, 0), Rotation::R0).unwrap();
    let b = d.body.add_component(tag(TagDir::In), GridPos::new(0, 4), Rotation::R0).unwrap();
    let g = d.body.add_component(and2(), GridPos::new(10, 0), Rotation::R0).unwrap();
    let y = d.body.add_component(tag(TagDir::Out), GridPos::new(20, 0), Rotation::R0).unwrap();
    for (f, t) in [((a, 0), (g, 0)), ((b, 0), (g, 1)), ((g, 2), (y, 0))] {
        d.body
            .apply(&Edit::AddWire {
                from: WireEnd::Pin(PinRef { component: f.0, pin: f.1 }),
                to: WireEnd::Pin(PinRef { component: t.0, pin: t.1 }),
                via: vec![],
            })
            .unwrap();
    }
    d.sync_pins();
    let mut c = Circuit::new();
    let mut s = Scene::build(&c, 0);
    let id = MacroId(0);
    edit(&mut c, &mut s, &Edit::DefineMacro { id, def: Arc::new(d.clone()) }, 1);
    let shape = ComponentKind::Macro(c.macros().get(id).unwrap().shape);
    let mut insts = Vec::new();
    for i in 0..6 {
        insts.push(add(&mut c, &mut s, shape, 20, 12 * i));
    }
    let sw = add(&mut c, &mut s, ComponentKind::Switch { on: true }, 0, 0);
    for &m in &insts {
        connect(&mut c, &mut s, sw, m, 1);
    }
    let led = add(&mut c, &mut s, ComponentKind::Led, 50, 0);
    connect(&mut c, &mut s, insts[0], led, 0);
    s.validate(&c).unwrap();
    // The instance output drives the LED's net.
    let led_body = s.bodies()[s.body_slot(led).unwrap() as usize];
    assert_eq!(led_body.net, s.signal_slot(insts[0], 0).unwrap());
    // Redefine: drop input A (pin 0), add a second output.
    d.body.remove_component(a).unwrap();
    let z = d.body.add_component(tag(TagDir::Out), GridPos::new(20, 6), Rotation::R0).unwrap();
    d.body
        .apply(&Edit::AddWire {
            from: WireEnd::Pin(PinRef { component: g, pin: 2 }),
            to: WireEnd::Pin(PinRef { component: z, pin: 0 }),
            via: vec![],
        })
        .unwrap();
    d.sync_pins();
    let a = edit(&mut c, &mut s, &Edit::DefineMacro { id, def: Arc::new(d) }, 2);
    assert_eq!(a.components, insts);
    s.validate(&c).unwrap();
    for &m in &insts {
        let k = c.component(m).unwrap().kind;
        assert_eq!((k.input_count(), k.output_count()), (1, 2));
        assert!(c.pin_node(PinRef { component: m, pin: 0 }).is_some(), "B (was pin 1) keeps its wire");
    }
    // Remove instances and the definition.
    for &m in &insts {
        edit(&mut c, &mut s, &Edit::RemoveComponent(m), 3);
        s.validate(&c).unwrap();
    }
    edit(&mut c, &mut s, &Edit::RemoveMacro(id), 4);
    assert!(c.macros().is_empty());
    s.validate(&c).unwrap();
}
