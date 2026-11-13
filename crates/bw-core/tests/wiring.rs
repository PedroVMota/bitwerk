//! Wire graph behaviour: junctions, clean-up, net merge validation,
//! rubber-banding and replay determinism.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines, clippy::many_single_char_names)]

use std::collections::BTreeSet;

use bw_core::{Circuit, CircuitError, ComponentId, ComponentKind, Edit, GridPos, PinRef, Rotation, SegmentId, WireEnd};

fn pin(component: ComponentId, pin: u8) -> PinRef {
    PinRef { component, pin }
}

fn add(c: &mut Circuit, kind: ComponentKind, x: i32, y: i32) -> ComponentId {
    c.add_component(kind, GridPos::new(x, y), Rotation::R0).unwrap()
}

/// The nets as sets of pins (a canonical partition).
fn partition(c: &Circuit) -> BTreeSet<Vec<PinRef>> {
    let nets = c.nets();
    (0..nets.len())
        .map(|i| {
            let mut v = nets.net(i).to_vec();
            v.sort_unstable();
            v
        })
        .collect()
}

fn seg_at(c: &Circuit, p: GridPos) -> SegmentId {
    c.segments()
        .find(|&(id, _)| {
            let (a, b) = c.segment_ends(id).unwrap();
            (a.x == b.x && p.x == a.x && p.y > a.y.min(b.y) && p.y < a.y.max(b.y))
                || (a.y == b.y && p.y == a.y && p.x > a.x.min(b.x) && p.x < a.x.max(b.x))
        })
        .map(|(id, _)| id)
        .expect("no segment there")
}

/// Switch at (0, 0) → LED 1 at (10, 0); out pin (2, 1), LED input (10, 1).
fn straight() -> (Circuit, ComponentId, ComponentId) {
    let mut c = Circuit::new();
    let sw = add(&mut c, ComponentKind::Switch { on: true }, 0, 0);
    let led = add(&mut c, ComponentKind::Led, 10, 0);
    c.connect(pin(sw, 0), pin(led, 0), vec![]).unwrap();
    assert_eq!(c.segment_count(), 1);
    (c, sw, led)
}

#[test]
fn starting_a_wire_on_a_segment_splits_it_into_a_junction() {
    let (mut c, sw, led) = straight();
    let led2 = add(&mut c, ComponentKind::Led, 10, 6);
    let s = seg_at(&c, GridPos::new(5, 1));
    let a = c
        .apply(&Edit::AddWire {
            from: WireEnd::OnSegment(s, GridPos::new(5, 1)),
            to: WireEnd::Pin(pin(led2, 0)),
            via: vec![GridPos::new(5, 7)],
        })
        .unwrap();
    assert!(a.connectivity);
    // (2,1)-(5,1), (5,1)-(10,1), (5,1)-(5,7), (5,7)-(10,7).
    assert_eq!(c.segment_count(), 4);
    let junctions: Vec<_> = c.nodes().filter(|(_, n)| n.is_junction()).collect();
    assert_eq!(junctions.len(), 1, "exactly one solder dot");
    assert_eq!(junctions[0].1.pos, GridPos::new(5, 1));
    assert_eq!(junctions[0].1.degree(), 3);
    assert_eq!(c.net_pins(pin(sw, 0)), vec![pin(sw, 0), pin(led, 0), pin(led2, 0)]);
    c.check().unwrap();

    // Ending on a segment works the same way (and an end on a node joins it).
    let led3 = add(&mut c, ComponentKind::Led, 10, 12);
    let bend = c.nodes().find(|(_, n)| n.pos == GridPos::new(5, 7)).unwrap().0;
    c.apply(&Edit::AddWire { from: WireEnd::Pin(pin(led3, 0)), to: WireEnd::Node(bend), via: vec![] }).unwrap();
    assert_eq!(c.junction_count(), 2, "the bend became a junction");
    assert_eq!(c.drivers_of(pin(led3, 0)), vec![pin(sw, 0)]);
    c.check().unwrap();
}

#[test]
fn deleting_a_branch_trims_dangling_wire_and_merges_the_straight_line() {
    let (mut c, sw, led) = straight();
    let led2 = add(&mut c, ComponentKind::Led, 10, 6);
    let s = seg_at(&c, GridPos::new(5, 1));
    c.apply(&Edit::AddWire {
        from: WireEnd::OnSegment(s, GridPos::new(5, 1)),
        to: WireEnd::Pin(pin(led2, 0)),
        via: vec![GridPos::new(5, 7)],
    })
    .unwrap();
    // Delete the vertical part of the branch: the rest of the branch,
    // (5,7)-(10,7), dangles and is trimmed together with LED 2's pin node;
    // the junction loses a segment and its two collinear halves merge.
    let v = seg_at(&c, GridPos::new(5, 4));
    let a = c.apply(&Edit::RemoveSegment(v)).unwrap();
    assert!(a.connectivity);
    assert_eq!(c.segment_count(), 1, "{:?}", c.segments().collect::<Vec<_>>());
    c.check().unwrap();
    assert!(c.pin_node(pin(led2, 0)).is_none());
    assert_eq!(c.junction_count(), 0);
    assert_eq!(c.net_pins(pin(sw, 0)), vec![pin(sw, 0), pin(led, 0)]);
    assert_eq!(c.net_pins(pin(led2, 0)), vec![pin(led2, 0)]);
    let line = seg_at(&c, GridPos::new(5, 1));
    assert_eq!(c.segment_ends(line).unwrap(), (GridPos::new(2, 1), GridPos::new(10, 1)));
    // Deleting the last segment of a wire removes its pin nodes too.
    c.apply(&Edit::RemoveSegment(line)).unwrap();
    assert!(c.pin_node(pin(sw, 0)).is_none() && c.pin_node(pin(led, 0)).is_none());
    c.check().unwrap();
}

#[test]
fn merging_nets_refuses_two_drivers_and_allows_tristate_buses() {
    // Two switches, each driving its own LED.
    let mut c = Circuit::new();
    let s1 = add(&mut c, ComponentKind::Switch { on: true }, 0, 0);
    let s2 = add(&mut c, ComponentKind::Switch { on: false }, 0, 10);
    let l1 = add(&mut c, ComponentKind::Led, 12, 0);
    let l2 = add(&mut c, ComponentKind::Led, 12, 10);
    c.connect(pin(s1, 0), pin(l1, 0), vec![]).unwrap();
    c.connect(pin(s2, 0), pin(l2, 0), vec![]).unwrap();
    let before = partition(&c);
    let (a, b) = (seg_at(&c, GridPos::new(6, 1)), seg_at(&c, GridPos::new(6, 11)));
    let join = Edit::AddWire {
        from: WireEnd::OnSegment(a, GridPos::new(6, 1)),
        to: WireEnd::OnSegment(b, GridPos::new(6, 11)),
        via: vec![],
    };
    let err = c.apply(&join).unwrap_err();
    assert_eq!(err, CircuitError::TwoDrivers(pin(s1, 0), pin(s2, 0)));
    assert!(err.to_string().contains("only tri-state outputs may share a net"), "{err}");
    assert_eq!(partition(&c), before, "a refused edit changes nothing");
    assert_eq!(c.segment_count(), 2);
    c.check().unwrap();
    // A loop inside one net is fine (no merge).
    c.apply(&Edit::AddWire {
        from: WireEnd::OnSegment(a, GridPos::new(4, 1)),
        to: WireEnd::OnSegment(seg_at(&c, GridPos::new(9, 1)), GridPos::new(9, 1)),
        via: vec![GridPos::new(4, 4), GridPos::new(9, 4)],
    })
    .unwrap();
    assert_eq!(partition(&c), before);
    c.check().unwrap();

    // The same with tri-state drivers: a bus, accepted.
    let mut c = Circuit::new();
    let t1 = add(&mut c, ComponentKind::TriState, 0, 0);
    let t2 = add(&mut c, ComponentKind::TriState, 0, 10);
    let l1 = add(&mut c, ComponentKind::Led, 12, 1);
    let l2 = add(&mut c, ComponentKind::Led, 12, 11);
    c.connect(pin(t1, 2), pin(l1, 0), vec![]).unwrap();
    c.connect(pin(t2, 2), pin(l2, 0), vec![]).unwrap();
    let (a, b) = (seg_at(&c, GridPos::new(8, 2)), seg_at(&c, GridPos::new(8, 12)));
    c.apply(&Edit::AddWire {
        from: WireEnd::OnSegment(a, GridPos::new(8, 2)),
        to: WireEnd::OnSegment(b, GridPos::new(8, 12)),
        via: vec![],
    })
    .unwrap();
    assert_eq!(c.drivers_of(pin(l2, 0)), vec![pin(t1, 2), pin(t2, 2)]);
    assert_eq!(c.junction_count(), 2);
    // A plain output still cannot join the bus.
    let s = add(&mut c, ComponentKind::Switch { on: true }, 20, 20);
    let bus = seg_at(&c, GridPos::new(8, 6));
    let e = c.apply(&Edit::AddWire {
        from: WireEnd::Pin(pin(s, 0)),
        to: WireEnd::OnSegment(bus, GridPos::new(8, 6)),
        via: vec![],
    });
    assert!(matches!(e, Err(CircuitError::TwoDrivers(..))), "{e:?}");
    c.check().unwrap();
}

#[test]
fn moving_and_rotating_components_rubber_band_their_wires() {
    let mut c = Circuit::new();
    let sw = add(&mut c, ComponentKind::Switch { on: true }, 0, 0);
    let g = add(&mut c, ComponentKind::Gate { op: bw_core::GateOp::And, inputs: 2 }, 12, 4);
    let led = add(&mut c, ComponentKind::Led, 24, 0);
    // An L from the switch to in0, a straight-ish wire to the LED.
    c.connect(pin(sw, 0), pin(g, 0), vec![GridPos::new(6, 1), GridPos::new(6, 5)]).unwrap();
    c.connect(pin(g, 2), pin(led, 0), vec![]).unwrap();
    let nets = partition(&c);
    for (i, to) in [(14, 4), (14, 9), (3, 9), (30, -7), (12, 4)].into_iter().enumerate() {
        let a = c.apply(&Edit::MoveComponent(g, GridPos::new(to.0, to.1))).unwrap();
        assert!(!a.connectivity, "a move never changes nets");
        c.check().unwrap_or_else(|e| panic!("move {i}: {e}"));
        assert_eq!(partition(&c), nets, "move {i} kept every attachment");
        for p in [0u8, 2] {
            let n = c.pin_node(pin(g, p)).unwrap();
            assert_eq!(Some(c.node(n).unwrap().pos), c.component(g).unwrap().pin_pos(p));
        }
    }
    for rot in [Rotation::R90, Rotation::R180, Rotation::R270, Rotation::R0] {
        c.apply(&Edit::SetRotation(g, rot)).unwrap();
        c.check().unwrap();
        assert_eq!(partition(&c), nets);
    }
    // Sliding: a horizontal move of the gate slides the L's bend instead of
    // adding another one.
    let segs = c.segment_count();
    c.apply(&Edit::MoveComponent(g, GridPos::new(12, 6))).unwrap();
    assert_eq!(c.segment_count(), segs, "the bend slid, no new segments");
    c.check().unwrap();
}

#[test]
fn bends_junctions_and_segments_can_be_dragged() {
    let (mut c, _sw, _led) = straight();
    let led2 = add(&mut c, ComponentKind::Led, 10, 6);
    let s = seg_at(&c, GridPos::new(5, 1));
    c.apply(&Edit::AddWire {
        from: WireEnd::OnSegment(s, GridPos::new(5, 1)),
        to: WireEnd::Pin(pin(led2, 0)),
        via: vec![GridPos::new(5, 7)],
    })
    .unwrap();
    let nets = partition(&c);
    // Move the junction right: its three segments stay orthogonal.
    let j = c.nodes().find(|(_, n)| n.is_junction()).unwrap().0;
    let a = c.apply(&Edit::MoveNode(j, GridPos::new(7, 1))).unwrap();
    assert!(!a.connectivity);
    c.check().unwrap();
    assert_eq!(partition(&c), nets);
    // Moving it off the line adds bends (the pins cannot follow).
    c.apply(&Edit::MoveNode(j, GridPos::new(7, 3))).unwrap();
    c.check().unwrap();
    assert_eq!(partition(&c), nets);
    // Pins cannot be moved on their own.
    let pn = c.pin_node(pin(led2, 0)).unwrap();
    assert_eq!(c.apply(&Edit::MoveNode(pn, GridPos::new(0, 0))), Err(CircuitError::NotFreeNode(pn)));
    // Drag the lower horizontal segment of the branch down by 3.
    let low = seg_at(&c, GridPos::new(8, 7));
    c.apply(&Edit::DragSegment(low, 3)).unwrap();
    c.check().unwrap();
    assert_eq!(partition(&c), nets);
    assert!(c.segments().any(|(id, _)| {
        let (a, b) = c.segment_ends(id).unwrap();
        a.y == 10 && b.y == 10
    }));
}

/// Deterministic pseudo-random numbers (no dependency).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap()
    }
    fn coord(&mut self) -> i32 {
        i32::try_from(self.below(40)).unwrap()
    }
}

/// A random wiring edit (may be invalid: the model must reject it cleanly).
fn random_edit(c: &Circuit, r: &mut Rng) -> Edit {
    let comps: Vec<_> = c.components().map(|x| x.id).collect();
    let segs: Vec<SegmentId> = c.segments().map(|(id, _)| id).collect();
    let nodes: Vec<_> = c.nodes().filter(|(_, n)| n.is_free()).map(|(id, _)| id).collect();
    let any_pin = |r: &mut Rng| {
        let id = comps[r.below(comps.len())];
        let n = c.component(id).unwrap().kind.pin_count();
        pin(id, u8::try_from(r.below(usize::from(n))).unwrap())
    };
    let any_end = |r: &mut Rng| -> WireEnd {
        if !segs.is_empty() && r.below(3) == 0 {
            let s = segs[r.below(segs.len())];
            let (a, b) = c.segment_ends(s).unwrap();
            let t = |lo: i32, hi: i32, r: &mut Rng| {
                lo.min(hi) + i32::try_from(r.below(usize::try_from((hi - lo).abs() + 1).unwrap())).unwrap()
            };
            WireEnd::OnSegment(s, GridPos::new(t(a.x, b.x, r), t(a.y, b.y, r)))
        } else if !nodes.is_empty() && r.below(4) == 0 {
            WireEnd::Node(nodes[r.below(nodes.len())])
        } else {
            WireEnd::Pin(any_pin(r))
        }
    };
    match r.below(12) {
        0 | 1 if comps.len() < 14 => {
            let kinds = [
                ComponentKind::Switch { on: true },
                ComponentKind::Led,
                ComponentKind::TriState,
                ComponentKind::Gate { op: bw_core::GateOp::Xor, inputs: 2 },
                ComponentKind::Not,
            ];
            Edit::AddComponent {
                kind: kinds[r.below(kinds.len())],
                pos: GridPos::new(r.coord(), r.coord()),
                rotation: Rotation::R0,
            }
        }
        2..=4 if !comps.is_empty() => {
            let via = (0..r.below(3)).map(|_| GridPos::new(r.coord(), r.coord())).collect();
            Edit::AddWire { from: any_end(r), to: any_end(r), via }
        }
        5 if !segs.is_empty() => Edit::RemoveSegment(segs[r.below(segs.len())]),
        6 if !nodes.is_empty() => Edit::MoveNode(nodes[r.below(nodes.len())], GridPos::new(r.coord(), r.coord())),
        7 if !segs.is_empty() => Edit::DragSegment(segs[r.below(segs.len())], i32::try_from(r.below(9)).unwrap() - 4),
        8 | 9 if !comps.is_empty() => {
            Edit::MoveComponent(comps[r.below(comps.len())], GridPos::new(r.coord(), r.coord()))
        }
        10 if !comps.is_empty() => {
            let rots = [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270];
            Edit::SetRotation(comps[r.below(comps.len())], rots[r.below(4)])
        }
        11 if comps.len() > 6 => Edit::RemoveComponent(comps[r.below(comps.len())]),
        _ => Edit::AddComponent {
            kind: ComponentKind::Led,
            pos: GridPos::new(r.coord(), r.coord()),
            rotation: Rotation::R0,
        },
    }
}

/// Thousands of random wiring edits: every invariant holds after each one,
/// geometric edits never change the nets, and two copies replaying the same
/// edits (the UI and the simulator) stay identical.
#[test]
fn random_wiring_edits_keep_invariants_and_replay_identically() {
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    let mut a = Circuit::new();
    let mut b = Circuit::new();
    let mut ok = 0;
    for step in 0..4_000 {
        let e = random_edit(&a, &mut r);
        let before = (!e.affects_netlist()).then(|| partition(&a));
        let ra = a.apply(&e);
        let rb = b.apply(&e);
        assert_eq!(ra, rb, "step {step}: replay diverged on {e:?}");
        if ra.is_ok() {
            ok += 1;
        }
        a.check().unwrap_or_else(|err| panic!("step {step} after {e:?}: {err}"));
        if let Some(p) = before {
            assert_eq!(partition(&a), p, "step {step}: geometric edit {e:?} changed the nets");
        }
    }
    assert!(ok > 1_500, "only {ok} edits applied");
    let ta = format!("{:?}", a.segments().collect::<Vec<_>>());
    let tb = format!("{:?}", b.segments().collect::<Vec<_>>());
    assert_eq!(ta, tb);
    assert!(a.segment_count() > 0);
}
