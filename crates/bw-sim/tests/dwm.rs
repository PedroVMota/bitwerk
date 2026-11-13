//! Behaviour of imported Digital Works circuits (the official samples in
//! `crates/bw-io/tests/fixtures/dwm`, see the NOTICE there).
//!
//! The four circuits checked by the format research (`docs/dwm`) must
//! behave exactly as described there; a few 74-series parts are checked
//! against their datasheet truth tables.

#![allow(clippy::unwrap_used, clippy::missing_panics_doc, clippy::many_single_char_names)]

mod common;

use bw_core::{Circuit, ComponentId, ComponentKind, EvalMode, GridPos, PinRef, Rotation, SwitchMode};
use bw_io::dwm;
use bw_sim::{Netlist, SimError, SimState};

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bw-io/tests/fixtures/dwm").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn import(name: &str) -> Circuit {
    let (p, report) = dwm::import_project(&fixture(name), name).unwrap();
    assert_eq!(p.circuit.eval_mode(), EvalMode::Sequential, "imports use the DW compatible mode");
    assert!(report.counts.iter().any(|(_, n)| *n > 0));
    p.circuit
}

/// The fixture imported, or (`exported`) imported, exported as a `.dwm`
/// again and that file imported: the round trip must keep the behaviour.
fn load(name: &str, exported: bool) -> Circuit {
    let c = import(name);
    if !exported {
        return c;
    }
    let project = bw_io::Project { name: name.into(), circuit: c };
    let (bytes, _) = dwm::export_project(&project, name).unwrap();
    let (p, _) = dwm::import_project(&bytes, name).unwrap();
    p.circuit
}

fn sim(c: &Circuit) -> SimState {
    let mut s = SimState::new(Netlist::compile(c).unwrap(), c);
    s.reset().unwrap();
    s
}

/// Root components of a kind, sorted by `key`.
fn find(c: &Circuit, pred: impl Fn(&ComponentKind) -> bool, key: impl Fn(GridPos) -> (i32, i32)) -> Vec<ComponentId> {
    let mut v: Vec<(GridPos, ComponentId)> = c.components().filter(|k| pred(&k.kind)).map(|k| (k.pos, k.id)).collect();
    v.sort_by_key(|&(p, id)| (key(p), id));
    v.into_iter().map(|(_, id)| id).collect()
}

fn switches(c: &Circuit, key: impl Fn(GridPos) -> (i32, i32)) -> Vec<ComponentId> {
    find(c, |k| matches!(k, ComponentKind::Switch { .. }), key)
}

fn leds(c: &Circuit, key: impl Fn(GridPos) -> (i32, i32)) -> Vec<ComponentId> {
    find(c, |k| matches!(k, ComponentKind::Led), key)
}

/// Level at an input pin (through the net's driver).
fn input_level(c: &Circuit, s: &SimState, pin: PinRef) -> u8 {
    let Some(d) = c.drivers_of(pin).first().copied() else { return 2 };
    let comp = c.component(d.component).unwrap();
    s.output(d.component, d.pin - comp.kind.input_count()).unwrap_or(2)
}

fn led(c: &Circuit, s: &SimState, id: ComponentId) -> u8 {
    input_level(c, s, PinRef { component: id, pin: 0 })
}

#[test]
fn xor_from_nand_gates() {
    xor_from_nand_gates_in(false);
}

#[test]
fn xor_from_nand_gates_after_export() {
    xor_from_nand_gates_in(true);
}

fn xor_from_nand_gates_in(exported: bool) {
    let c = load("xor-nand.dwm", exported);
    let ins = switches(&c, |p| (p.y, p.x));
    let outs = leds(&c, |p| (p.y, p.x));
    assert_eq!((ins.len(), outs.len()), (4, 2));
    let mut s = sim(&c);
    for a in [false, true] {
        for b in [false, true] {
            for (&id, v) in ins.iter().zip([a, b, a, b]) {
                s.set_switch(id, v).unwrap();
            }
            let want = u8::from(a ^ b);
            assert_eq!([led(&c, &s, outs[0]), led(&c, &s, outs[1])], [want, want], "A={a} B={b}");
        }
    }
}

#[test]
fn four_bit_equality_for_all_inputs() {
    four_bit_equality_for_all_inputs_in(false);
}

#[test]
fn four_bit_equality_for_all_inputs_after_export() {
    four_bit_equality_for_all_inputs_in(true);
}

fn four_bit_equality_for_all_inputs_in(exported: bool) {
    let c = load("equality-xor.dwm", exported);
    let ins = switches(&c, |p| (p.x, p.y));
    let out = leds(&c, |p| (p.x, p.y));
    assert_eq!((ins.len(), out.len()), (8, 1));
    let mut s = sim(&c);
    for a in 0..16u32 {
        for b in 0..16u32 {
            let bits = (0..4).map(|i| a >> (3 - i) & 1 == 1).chain((0..4).map(|i| b >> (3 - i) & 1 == 1));
            for (&id, v) in ins.iter().zip(bits) {
                s.set_switch(id, v).unwrap();
            }
            assert_eq!(led(&c, &s, out[0]), u8::from(a == b), "A={a} B={b}");
        }
    }
}

/// Advances to the next clock edge (the imported clock has a half period
/// of 10 ticks).
fn half_cycle(s: &mut SimState) {
    loop {
        if s.tick().unwrap() {
            return;
        }
    }
}

#[test]
fn master_slave_jk_from_gates() {
    master_slave_jk_from_gates_in(false);
}

#[test]
fn master_slave_jk_from_gates_after_export() {
    master_slave_jk_from_gates_in(true);
}

fn master_slave_jk_from_gates_in(exported: bool) {
    let c = load("ms-jk.dwm", exported);
    let jk = switches(&c, |p| (p.y, p.x));
    let l = leds(&c, |p| (p.x, p.y));
    assert_eq!((jk.len(), l.len()), (2, 4), "J, K; master Q/Q̄, slave Q/Q̄");
    let mut s = sim(&c);
    let q = |s: &SimState| (led(&c, s, l[2]), led(&c, s, l[3]));
    // Reset first: J = 0, K = 1, one clock cycle.
    s.set_switch(jk[0], false).unwrap();
    s.set_switch(jk[1], true).unwrap();
    half_cycle(&mut s);
    half_cycle(&mut s);
    let mut now = q(&s).0;
    assert_eq!(q(&s), (0, 1));
    for (j, k) in [(1, 0), (0, 0), (1, 1), (1, 1), (1, 1), (0, 1), (1, 1)] {
        s.set_switch(jk[0], j == 1).unwrap();
        s.set_switch(jk[1], k == 1).unwrap();
        half_cycle(&mut s); // clock rises: the master takes the inputs
        assert_eq!(q(&s).0, now, "J{j}K{k}: the slave holds while the clock is high");
        half_cycle(&mut s); // clock falls: the slave follows
        let want = match (j, k) {
            (0, 0) => now,
            (0, 1) => 0,
            (1, 0) => 1,
            _ => 1 - now,
        };
        assert_eq!(q(&s), (want, 1 - want), "J{j}K{k}: Q {now} -> {want}");
        now = want;
    }
}

/// The same flip-flop in unit-delay mode does not work: from the
/// floating start its cross-coupled NAND latches stay unknown (X), and when
/// the inputs of a latch rise together the two gates flip in lock-step and
/// never settle (a race). This is why imports use the sequential mode.
#[test]
fn master_slave_jk_fails_with_unit_delay() {
    let mut c = import("ms-jk.dwm");
    c.set_eval_mode(EvalMode::UnitDelay);
    let jk = switches(&c, |p| (p.y, p.x));
    let l = leds(&c, |p| (p.x, p.y));
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    let mut r = s.reset();
    let _ = s.set_switch(jk[0], false);
    let _ = s.set_switch(jk[1], true);
    for _ in 0..4 {
        if r.is_ok() {
            r = (0..10).try_for_each(|_| s.tick().map(|_| ()));
        }
    }
    let q = led(&c, &s, l[2]);
    assert!(
        matches!(r, Err(SimError::RaceCondition { .. })) || q > 1,
        "unit delay unexpectedly resolved the latch: {r:?}, Q = {q}"
    );
}

/// Segments a…g (active high) to a digit.
fn digit(seg: [u8; 7]) -> Option<u32> {
    let pats: [(u32, [u8; 7]); 12] = [
        (0, [1, 1, 1, 1, 1, 1, 0]),
        (1, [0, 1, 1, 0, 0, 0, 0]),
        (2, [1, 1, 0, 1, 1, 0, 1]),
        (3, [1, 1, 1, 1, 0, 0, 1]),
        (4, [0, 1, 1, 0, 0, 1, 1]),
        (5, [1, 0, 1, 1, 0, 1, 1]),
        (6, [1, 0, 1, 1, 1, 1, 1]),
        (6, [0, 0, 1, 1, 1, 1, 1]),
        (7, [1, 1, 1, 0, 0, 0, 0]),
        (8, [1, 1, 1, 1, 1, 1, 1]),
        (9, [1, 1, 1, 1, 0, 1, 1]),
        (9, [1, 1, 1, 0, 0, 1, 1]),
    ];
    pats.iter().find(|(_, p)| *p == seg).map(|(d, _)| *d)
}

/// The number shown on a row of 7-segment displays (blank digits = 0).
fn shown(c: &Circuit, s: &SimState, displays: &[ComponentId]) -> u32 {
    displays.iter().fold(0, |acc, &id| {
        let ComponentKind::SevenSeg { common_anode } = c.component(id).unwrap().kind else { unreachable!() };
        let seg: [u8; 7] = std::array::from_fn(|k| {
            let v = s.output(id, u8::try_from(k).unwrap()).unwrap();
            assert!(v <= 1, "segment {k} is {v}");
            if common_anode { 1 - v } else { v }
        });
        let d = if seg == [0; 7] { 0 } else { digit(seg).unwrap_or_else(|| panic!("segments {seg:?}")) };
        acc * 10 + d
    })
}

#[test]
fn counter_counts_on_the_seven_segment_displays() {
    counter_counts_on_the_seven_segment_displays_in(false);
}

#[test]
fn counter_counts_on_the_seven_segment_displays_after_export() {
    counter_counts_on_the_seven_segment_displays_in(true);
}

fn counter_counts_on_the_seven_segment_displays_in(exported: bool) {
    let c = load("counter.dwm", exported);
    let displays = find(&c, |k| matches!(k, ComponentKind::SevenSeg { .. }), |p| (p.x, p.y));
    assert_eq!(displays.len(), 2);
    let mut s = sim(&c);
    let mut seen = Vec::new();
    for _ in 0..14 {
        half_cycle(&mut s);
        half_cycle(&mut s);
        seen.push(shown(&c, &s, &displays));
    }
    assert_eq!(seen, (1..=14).collect::<Vec<u32>>());
}

/// Every sample fixture imports and settles in the compatible mode.
#[test]
fn every_fixture_imports_and_settles() {
    for name in [
        "xor-nand.dwm",
        "equality-xor.dwm",
        "ms-jk.dwm",
        "counter.dwm",
        "traffic-lights.dwm",
        "numbers.dwm",
        "memory.dwm",
        "rtx.dwm",
        "logic-history.dwm",
        "comparator.dwm",
        "clocked-d-latch.dwm",
    ] {
        for exported in [false, true] {
            let c = load(name, exported);
            let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
            s.reset().unwrap_or_else(|e| panic!("{name}: {e}"));
            for _ in 0..200 {
                s.tick().unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        }
    }
    // The bus contention sample: two tri-state drivers on a bus, each
    // enabled by a switch; enabling both halts with bus contention.
    for exported in [false, true] {
        let c = load("bus-contention.dwm", exported);
        let sws = switches(&c, |p| (p.y, p.x));
        let mut found = false;
        for &a in &sws {
            for &b in &sws {
                let mut s = sim(&c);
                let r = s.set_switch(a, true).and_then(|()| s.set_switch(b, true));
                let r = r.and_then(|()| (0..40).try_for_each(|_| s.tick().map(|_| ())));
                if matches!(r, Err(SimError::BusContention { .. })) {
                    found = true;
                }
            }
        }
        eprintln!("bus contention switches: {}", sws.len());
        assert!(found, "some switch setting makes the bus contention sample halt");
    }
}

// ---------------------------------------------------------------- 74xx

/// A bench around one imported IC: a switch on every input pin, by DIL
/// pin number.
struct Ic {
    c: Circuit,
    inst: ComponentId,
    sw: std::collections::HashMap<u8, ComponentId>,
    out: std::collections::HashMap<u8, u8>,
    s: SimState,
}

impl Ic {
    fn new(name: &str, exported: bool) -> Self {
        let (mut file, _) = dwm::import_macro(&fixture(name), name).unwrap();
        if exported {
            // As a Digital Works macro and back.
            let (bytes, _) = dwm::export_macro(&file.table, file.root, name).unwrap();
            file = dwm::import_macro(&bytes, name).unwrap().0;
        }
        let mut c = Circuit::new();
        c.set_eval_mode(EvalMode::Sequential);
        let id = common::import(&mut c, &file.table, file.root);
        let shape = c.macros().get(id).unwrap().shape;
        assert!(shape.dil > 0, "{name} is a DIL package");
        let inst = common::place(&mut c, id, GridPos::new(40, 0));
        let mut sw = std::collections::HashMap::new();
        let mut out = std::collections::HashMap::new();
        for k in 0..shape.inputs + shape.outputs {
            let dil = shape.dil_pins[usize::from(k)];
            if k < shape.inputs {
                let s = c
                    .add_component(
                        ComponentKind::Switch { on: false, mode: SwitchMode::Toggle },
                        GridPos::new(0, 4 * i32::from(k)),
                        Rotation::R0,
                    )
                    .unwrap();
                c.connect(PinRef { component: s, pin: 0 }, PinRef { component: inst, pin: k }, vec![]).unwrap();
                sw.insert(dil, s);
            } else {
                out.insert(dil, k - shape.inputs);
            }
        }
        let s = sim(&c);
        Self { c, inst, sw, out, s }
    }

    fn set(&mut self, pin: u8, v: bool) {
        let id = *self.sw.get(&pin).unwrap_or_else(|| panic!("pin {pin} is not an input"));
        self.s.set_switch(id, v).unwrap();
    }

    fn get(&self, pin: u8) -> u8 {
        let k = *self.out.get(&pin).unwrap_or_else(|| panic!("pin {pin} is not an output"));
        self.s.output(self.inst, k).unwrap()
    }
}

#[test]
fn ic_74hc00_quad_nand() {
    ic_74hc00_quad_nand_in(false);
}

#[test]
fn ic_74hc00_quad_nand_after_export() {
    ic_74hc00_quad_nand_in(true);
}

fn ic_74hc00_quad_nand_in(exported: bool) {
    let mut ic = Ic::new("74hc00.dwm", exported);
    let _ = &ic.c;
    for (a, b, y) in [(1, 2, 3), (4, 5, 6), (9, 10, 8), (12, 13, 11)] {
        for (va, vb) in [(false, false), (false, true), (true, false), (true, true)] {
            ic.set(a, va);
            ic.set(b, vb);
            assert_eq!(ic.get(y), u8::from(!(va && vb)), "pins {a},{b} -> {y}");
        }
    }
}

#[test]
fn ic_74hc125_tristate_buffers() {
    ic_74hc125_tristate_buffers_in(false);
}

#[test]
fn ic_74hc125_tristate_buffers_after_export() {
    ic_74hc125_tristate_buffers_in(true);
}

fn ic_74hc125_tristate_buffers_in(exported: bool) {
    let mut ic = Ic::new("74hc125.dwm", exported);
    for (oe, a, y) in [(1, 2, 3), (4, 5, 6), (10, 9, 8), (13, 12, 11)] {
        ic.set(oe, true);
        assert_eq!(ic.get(y), 2, "disabled output floats");
        ic.set(oe, false);
        for v in [false, true] {
            ic.set(a, v);
            assert_eq!(ic.get(y), u8::from(v));
        }
        ic.set(oe, true);
    }
}

#[test]
fn ic_74hc74_dual_d_flip_flop() {
    ic_74hc74_dual_d_flip_flop_in(false);
}

#[test]
fn ic_74hc74_dual_d_flip_flop_after_export() {
    ic_74hc74_dual_d_flip_flop_in(true);
}

fn ic_74hc74_dual_d_flip_flop_in(exported: bool) {
    let mut ic = Ic::new("74hc74.dwm", exported);
    // (CLR̄, D, CP, PR̄, Q, Q̄)
    for (clr, d, cp, pr, q, qn) in [(1, 2, 3, 4, 5, 6), (13, 12, 11, 10, 9, 8)] {
        ic.set(clr, true);
        ic.set(pr, true);
        for v in [true, false, true] {
            ic.set(d, v);
            ic.set(cp, true);
            assert_eq!((ic.get(q), ic.get(qn)), (u8::from(v), u8::from(!v)), "D={v} on the rising edge");
            ic.set(d, !v);
            assert_eq!(ic.get(q), u8::from(v), "holds while the clock is high");
            ic.set(cp, false);
        }
        ic.set(clr, false);
        assert_eq!((ic.get(q), ic.get(qn)), (0, 1), "asynchronous clear (active low)");
        ic.set(clr, true);
        ic.set(pr, false);
        assert_eq!((ic.get(q), ic.get(qn)), (1, 0), "asynchronous preset (active low)");
        ic.set(pr, true);
    }
}

#[test]
fn ic_74hc76_dual_jk_flip_flop() {
    ic_74hc76_dual_jk_flip_flop_in(false);
}

#[test]
fn ic_74hc76_dual_jk_flip_flop_after_export() {
    ic_74hc76_dual_jk_flip_flop_in(true);
}

fn ic_74hc76_dual_jk_flip_flop_in(exported: bool) {
    let mut ic = Ic::new("74hc76.dwm", exported);
    // (CP̄, SD̄, RD̄, J, K, Q, Q̄)
    for (cp, sd, rd, j, k, q, qn) in [(1, 2, 3, 4, 16, 15, 14), (6, 7, 8, 9, 12, 11, 10)] {
        ic.set(sd, true);
        ic.set(rd, false);
        assert_eq!((ic.get(q), ic.get(qn)), (0, 1), "reset (active low)");
        ic.set(rd, true);
        let mut now = 0u8;
        for (vj, vk) in [(true, false), (false, false), (true, true), (true, true), (false, true)] {
            ic.set(j, vj);
            ic.set(k, vk);
            ic.set(cp, true);
            assert_eq!(ic.get(q), now, "no change before the falling edge");
            ic.set(cp, false);
            now = match (vj, vk) {
                (false, false) => now,
                (false, true) => 0,
                (true, false) => 1,
                (true, true) => 1 - now,
            };
            assert_eq!((ic.get(q), ic.get(qn)), (now, 1 - now), "J={vj} K={vk}");
        }
        ic.set(sd, false);
        assert_eq!(ic.get(q), 1, "set (active low)");
        ic.set(sd, true);
    }
}
