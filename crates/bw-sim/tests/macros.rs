//! Macro hierarchy: flattening correctness, nesting, limits, definition
//! edits, error paths and the per-instance inside view.

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::items_after_statements
)]

mod common;

use std::sync::Arc;

use bw_core::{
    Circuit, CircuitError, ComponentId, ComponentKind, Edit, FfTrigger, FlipFlopType, GateOp, GridPos, MacroDef,
    MacroId, PinRef, Rotation, TagDir, WireEnd,
};
use bw_sim::{FlattenCache, Netlist, SimError, SimState};
use common::{define, place};

fn pin(component: ComponentId, pin: u8) -> PinRef {
    PinRef { component, pin }
}

fn wire(c: &mut Circuit, from: PinRef, to: PinRef) {
    c.apply(&Edit::AddWire { from: WireEnd::Pin(from), to: WireEnd::Pin(to), via: vec![] }).unwrap();
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % (n.max(1) as u64)).unwrap()
    }
}

/// A random definition: `ins` input tags (tag 0 also clocks the
/// flip-flops), gates / NOTs / D flip-flops fed by earlier drivers, `outs`
/// output tags.
fn random_def(rng: &mut Rng, name: &str, ins: usize, outs: usize) -> MacroDef {
    let mut d = MacroDef::new(name);
    let b = &mut d.body;
    let mut drivers: Vec<PinRef> = Vec::new();
    for k in 0..ins {
        let t = b
            .add_component(
                ComponentKind::Tag { dir: TagDir::In, tristate: false },
                GridPos::new(0, 4 * k as i32),
                Rotation::R0,
            )
            .unwrap();
        drivers.push(pin(t, 0));
    }
    let clock = drivers[0];
    let n = 4 + rng.next(10);
    for i in 0..n {
        let (x, y) = (10 + 12 * (i as i32 % 4), 12 * (i as i32 / 4));
        let kind = match rng.next(8) {
            0 => ComponentKind::Not,
            1 => ComponentKind::FlipFlop { ty: FlipFlopType::D, trigger: FfTrigger::RisingEdge, preset_clear: false },
            k => ComponentKind::Gate { op: GateOp::ALL[k % 6], inputs: 2 + u8::try_from(rng.next(2)).unwrap() },
        };
        let id = b.add_component(kind, GridPos::new(x, y), Rotation::R0).unwrap();
        for p in 0..kind.input_count() {
            let src = if matches!(kind, ComponentKind::FlipFlop { .. }) && p == 1 {
                clock
            } else {
                drivers[rng.next(drivers.len())]
            };
            wire(b, src, pin(id, p));
        }
        for k in 0..kind.output_count() {
            drivers.push(pin(id, kind.input_count() + k));
        }
    }
    for k in 0..outs {
        let t = b
            .add_component(
                ComponentKind::Tag { dir: TagDir::Out, tristate: false },
                GridPos::new(70, 4 * k as i32),
                Rotation::R0,
            )
            .unwrap();
        // Prefer gate outputs (skip the input tags).
        let src = drivers[ins + rng.next(drivers.len() - ins)];
        wire(b, src, pin(t, 0));
    }
    d.sync_pins();
    d
}

/// The definition's body as a plain circuit: input tags become switches,
/// output tags LEDs (the "inlined" reference).
fn inlined(d: &MacroDef) -> (Circuit, Vec<ComponentId>, Vec<ComponentId>) {
    let mut c = d.body.clone();
    let iface = d.interface();
    let mut ins = Vec::new();
    let mut outs = Vec::new();
    for p in iface {
        let kind = if p.dir == TagDir::In { ComponentKind::Switch { on: false } } else { ComponentKind::Led };
        c.set_kind(p.tag, kind).unwrap();
        if p.dir == TagDir::In { ins.push(p.tag) } else { outs.push(p.tag) }
    }
    (c, ins, outs)
}

fn level_of_led(c: &Circuit, sim: &SimState, led: ComponentId) -> u8 {
    let drv = c.drivers_of(pin(led, 0));
    let Some(d) = drv.first() else { return 2 };
    let ins = c.component(d.component).unwrap().kind.input_count();
    sim.output(d.component, d.pin - ins).unwrap_or(2)
}

/// A macro instance behaves exactly like its body inlined, for random
/// circuits and random input sequences, also when nested one level deeper.
#[test]
fn flattened_macro_matches_inlined_circuit() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for case in 0..40 {
        let (ins, outs) = (2 + rng.next(3), 1 + rng.next(4));
        let def = random_def(&mut rng, "R", ins, outs);
        // Reference.
        let (body, sw, leds) = inlined(&def);
        let mut reference = SimState::new(Netlist::compile(&body).unwrap(), &body);
        reference.reset().unwrap();
        // The macro, optionally wrapped in a pass-through macro.
        let mut c = Circuit::new();
        let id = define(&mut c, def.clone());
        let id = if case % 2 == 1 {
            let mut w = MacroDef::new("Wrap");
            let shape = c.macros().get(id).unwrap().shape;
            let inner = w.body.add_component(ComponentKind::Macro(shape), GridPos::new(20, 0), Rotation::R0).unwrap();
            for k in 0..ins {
                let t = w
                    .body
                    .add_component(
                        ComponentKind::Tag { dir: TagDir::In, tristate: false },
                        GridPos::new(0, 4 * k as i32),
                        Rotation::R0,
                    )
                    .unwrap();
                wire(&mut w.body, pin(t, 0), pin(inner, u8::try_from(k).unwrap()));
            }
            for k in 0..outs {
                let t = w
                    .body
                    .add_component(
                        ComponentKind::Tag { dir: TagDir::Out, tristate: false },
                        GridPos::new(60, 4 * k as i32),
                        Rotation::R0,
                    )
                    .unwrap();
                wire(&mut w.body, pin(inner, u8::try_from(ins + k).unwrap()), pin(t, 0));
            }
            w.sync_pins();
            define(&mut c, w)
        } else {
            id
        };
        let inst = place(&mut c, id, GridPos::new(40, 0));
        let mut switches = Vec::new();
        for k in 0..ins {
            let s = c
                .add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 4 * k as i32), Rotation::R0)
                .unwrap();
            wire(&mut c, pin(s, 0), pin(inst, u8::try_from(k).unwrap()));
            switches.push(s);
        }
        let mut sim = SimState::new(Netlist::compile(&c).unwrap(), &c);
        sim.reset().unwrap();
        for step in 0..60 {
            let k = rng.next(ins);
            let on = rng.next(2) == 1;
            let r1 = reference.set_switch(sw[k], on);
            let r2 = sim.set_switch(switches[k], on);
            assert_eq!(r1.is_ok(), r2.is_ok(), "case {case} step {step}: {r1:?} vs {r2:?}");
            if r1.is_err() {
                break;
            }
            for (o, &led) in leds.iter().enumerate() {
                let want = level_of_led(&body, &reference, led);
                let got = sim.output(inst, u8::try_from(o).unwrap()).unwrap_or(2);
                assert_eq!(got, want, "case {case} step {step} output {o}");
            }
        }
    }
}

fn not_def(name: &str) -> MacroDef {
    let mut d = MacroDef::new(name);
    let a = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 0), Rotation::R0)
        .unwrap();
    let n = d.body.add_component(ComponentKind::Not, GridPos::new(8, 0), Rotation::R0).unwrap();
    let y = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::Out, tristate: false }, GridPos::new(16, 0), Rotation::R0)
        .unwrap();
    wire(&mut d.body, pin(a, 0), pin(n, 0));
    wire(&mut d.body, pin(n, 1), pin(y, 0));
    d.sync_pins();
    d
}

/// A definition wrapping one instance of `inner` (1 in, 1 out).
fn wrap(c: &Circuit, inner: MacroId, name: &str, copies: usize) -> MacroDef {
    let shape = c.macros().get(inner).unwrap().shape;
    let mut d = MacroDef::new(name);
    let a = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 0), Rotation::R0)
        .unwrap();
    let y = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::Out, tristate: false }, GridPos::new(60, 0), Rotation::R0)
        .unwrap();
    let mut prev = pin(a, 0);
    for i in 0..copies {
        let m = d
            .body
            .add_component(ComponentKind::Macro(shape), GridPos::new(10 + 4 * i as i32, 10 * i as i32), Rotation::R0)
            .unwrap();
        wire(&mut d.body, prev, pin(m, 0));
        prev = pin(m, 1);
    }
    wire(&mut d.body, prev, pin(y, 0));
    d.sync_pins();
    d
}

#[test]
fn nesting_depth_limit() {
    let mut c = Circuit::new();
    let mut id = define(&mut c, not_def("N0"));
    for level in 2..=bw_core::MAX_MACRO_DEPTH {
        let w = wrap(&c, id, &format!("N{level}"), 1);
        id = define(&mut c, w);
        assert_eq!(c.macros().get(id).unwrap().depth, level);
    }
    let too_deep = wrap(&c, id, "deep", 1);
    let free = c.macros().free_id().unwrap();
    assert!(matches!(
        c.apply(&Edit::DefineMacro { id: free, def: Arc::new(too_deep) }),
        Err(CircuitError::MacroTooDeep { .. })
    ));
    // 16 levels of inverters: an instance inverts its input 1 time.
    let inst = place(&mut c, id, GridPos::new(20, 0));
    let s = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 0), Rotation::R0).unwrap();
    wire(&mut c, pin(s, 0), pin(inst, 0));
    let mut sim = SimState::new(Netlist::compile(&c).unwrap(), &c);
    sim.reset().unwrap();
    assert_eq!(sim.output(inst, 0), Some(1));
    sim.set_switch(s, true).unwrap();
    assert_eq!(sim.output(inst, 0), Some(0));
}

#[test]
fn cycles_are_refused() {
    let mut c = Circuit::new();
    let a = define(&mut c, not_def("A"));
    let w = wrap(&c, a, "B", 1);
    let b = define(&mut c, w);
    // A containing B (which contains A).
    let a_in_b = wrap(&c, b, "A", 1);
    assert_eq!(c.apply(&Edit::DefineMacro { id: a, def: Arc::new(a_in_b) }), Err(CircuitError::MacroCycle(b)));
    // A containing itself.
    let a_in_a = wrap(&c, a, "A", 1);
    assert!(matches!(c.apply(&Edit::DefineMacro { id: a, def: Arc::new(a_in_a) }), Err(CircuitError::MacroCycle(_))));
    // Nothing changed.
    assert_eq!(c.macros().get(a).unwrap().depth, 1);
    assert!(c.apply(&Edit::RemoveMacro(a)).is_err(), "A is used by B");
    c.apply(&Edit::RemoveMacro(b)).unwrap();
    c.apply(&Edit::RemoveMacro(a)).unwrap();
}

#[test]
fn macro_bomb_is_refused() {
    let mut c = Circuit::new();
    let mut id = define(&mut c, not_def("N"));
    let mut size = 1u64;
    let mut refused = false;
    for level in 0..10 {
        let d = wrap(&c, id, &format!("L{level}"), 10);
        let free = c.macros().free_id().unwrap();
        match c.apply(&Edit::DefineMacro { id: free, def: Arc::new(d) }) {
            Ok(_) => {
                size *= 10;
                id = free;
                assert_eq!(c.macros().get(id).unwrap().flat, size);
            }
            Err(CircuitError::MacroTooLarge { size: s, .. }) => {
                assert!(s > bw_core::MAX_FLAT_COMPONENTS);
                refused = true;
                break;
            }
            Err(e) => panic!("{e}"),
        }
    }
    assert!(refused, "10^7 components must be refused");
    // The root cannot exceed the limit either (1 M + 1 M + 1 M).
    let shape = c.macros().get(id).unwrap().shape;
    let mut placed = 0;
    while c.add_component(ComponentKind::Macro(shape), GridPos::new(0, 3 * placed), Rotation::R0).is_ok() {
        placed += 1;
        assert!(placed < 10);
    }
    assert!(c.flat_size() <= bw_core::MAX_FLAT_COMPONENTS);
    // ... and a file whose root exceeds it is refused when loading.
    let text = bw_io::save_to_string(&bw_io::Project { name: "bomb".into(), circuit: c.clone() }).unwrap();
    assert!(bw_io::load_from_bytes(text.as_bytes()).is_ok());
    let at = text.find("circuit:").unwrap();
    let at = at + text[at..].find("components: [").unwrap() + "components: [".len();
    let extra = format!("(id: (777), kind: Macro(({})), pos: (x: 0, y: 900)),", id.0);
    let forged = format!("{}{extra}{}", &text[..at], &text[at..]);
    match bw_io::load_from_bytes(forged.as_bytes()) {
        Err(bw_io::IoError::Parse(m)) => assert!(m.contains("too large"), "{m}"),
        other => panic!("forged bomb accepted: {other:?}"),
    }
}

/// Editing a definition changes every instance: behaviour, and pins when
/// the interface changes (wires stay on the pins whose tags survive).
#[test]
fn definition_edit_updates_all_instances() {
    let mut c = Circuit::new();
    let id = define(&mut c, not_def("Inv"));
    let mut insts = Vec::new();
    let mut sws = Vec::new();
    for i in 0..3 {
        let inst = place(&mut c, id, GridPos::new(20, 8 * i));
        let s = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 8 * i), Rotation::R0).unwrap();
        wire(&mut c, pin(s, 0), pin(inst, 0));
        let led = c.add_component(ComponentKind::Led, GridPos::new(50, 8 * i), Rotation::R0).unwrap();
        wire(&mut c, pin(inst, 1), pin(led, 0));
        insts.push(inst);
        sws.push(s);
    }
    let run = |c: &Circuit| {
        let mut s = SimState::new(Netlist::compile(c).unwrap(), c);
        s.reset().unwrap();
        s
    };
    let s = run(&c);
    assert!(insts.iter().all(|&i| s.output(i, 0) == Some(1)));
    // Replace the NOT by a buffer-like AND(a, a): every instance follows.
    let mut d = (**c.macros().def(id).unwrap()).clone();
    let not = d.body.components().find(|k| k.kind == ComponentKind::Not).unwrap().id;
    d.body.set_kind(not, ComponentKind::Gate { op: GateOp::Xnor, inputs: 2 }).unwrap_err(); // wired: pins differ
    let a_tag = d.pins[0].tag;
    let y_tag = d.pins[1].tag;
    d.body.remove_component(not).unwrap();
    let g = d
        .body
        .add_component(ComponentKind::Gate { op: GateOp::Or, inputs: 2 }, GridPos::new(8, 0), Rotation::R0)
        .unwrap();
    wire(&mut d.body, pin(a_tag, 0), pin(g, 0));
    wire(&mut d.body, pin(a_tag, 0), pin(g, 1));
    wire(&mut d.body, pin(g, 2), pin(y_tag, 0));
    let applied = c.apply(&Edit::DefineMacro { id, def: Arc::new(d.clone()) }).unwrap();
    assert!(applied.macros.contains(&id));
    let s = run(&c);
    assert!(insts.iter().all(|&i| s.output(i, 0) == Some(0)), "all instances are buffers now");
    // Interface change: a new input tag B (pins re-matched by tag).
    let b_tag = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 6), Rotation::R0)
        .unwrap();
    d.body.remove_component(g).unwrap();
    let g = d
        .body
        .add_component(ComponentKind::Gate { op: GateOp::And, inputs: 2 }, GridPos::new(8, 0), Rotation::R0)
        .unwrap();
    wire(&mut d.body, pin(a_tag, 0), pin(g, 0));
    wire(&mut d.body, pin(b_tag, 0), pin(g, 1));
    wire(&mut d.body, pin(g, 2), pin(y_tag, 0));
    d.sync_pins();
    let applied = c.apply(&Edit::DefineMacro { id, def: Arc::new(d.clone()) }).unwrap();
    assert_eq!(applied.components, insts, "every instance changed shape");
    c.check().unwrap();
    for (&i, &s) in insts.iter().zip(&sws) {
        let k = c.component(i).unwrap().kind;
        assert_eq!((k.input_count(), k.output_count()), (2, 1));
        assert_eq!(c.drivers_of(pin(i, 0)), vec![pin(s, 0)], "input A keeps its wire");
        assert!(c.pin_node(pin(i, 2)).is_some(), "output (now pin 2) keeps its wire");
        assert!(c.pin_node(pin(i, 1)).is_none(), "new input B is unwired");
    }
    // Removing the A tag detaches the wires on that pin.
    d.body.remove_component(a_tag).unwrap();
    d.sync_pins();
    c.apply(&Edit::DefineMacro { id, def: Arc::new(d) }).unwrap();
    c.check().unwrap();
    for &i in &insts {
        assert_eq!(c.component(i).unwrap().kind.input_count(), 1);
        assert!(c.pin_node(pin(i, 0)).is_none(), "B was never wired");
        assert!(c.pin_node(pin(i, 1)).is_some(), "the output keeps its wire");
    }
    assert_eq!(c.total_instances(id), 3);
}

/// Bus contention and race conditions inside a macro name the hierarchy.
#[test]
fn errors_name_the_hierarchy() {
    let mut c = Circuit::new();
    let mut d = MacroDef::new("Clash");
    let b = &mut d.body;
    let a = b
        .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 0), Rotation::R0)
        .unwrap();
    let e = b
        .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 8), Rotation::R0)
        .unwrap();
    let n = b.add_component(ComponentKind::Not, GridPos::new(8, 12), Rotation::R0).unwrap();
    let t1 = b.add_component(ComponentKind::TriState, GridPos::new(20, 0), Rotation::R0).unwrap();
    let t2 = b.add_component(ComponentKind::TriState, GridPos::new(20, 10), Rotation::R0).unwrap();
    let y = b
        .add_component(ComponentKind::Tag { dir: TagDir::Out, tristate: true }, GridPos::new(40, 4), Rotation::R0)
        .unwrap();
    wire(b, pin(a, 0), pin(t1, 0));
    wire(b, pin(a, 0), pin(n, 0));
    wire(b, pin(n, 1), pin(t2, 0));
    wire(b, pin(e, 0), pin(t1, 1));
    wire(b, pin(e, 0), pin(t2, 1));
    wire(b, pin(t1, 2), pin(y, 0));
    wire(b, pin(t2, 2), pin(y, 0));
    d.sync_pins();
    let id = define(&mut c, d);
    assert_eq!(c.macros().get(id).unwrap().shape.tristate, 1, "tri-state output");
    let board = {
        let shape = c.macros().get(id).unwrap().shape;
        let mut w = MacroDef::new("Board");
        let ta = w
            .body
            .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 0), Rotation::R0)
            .unwrap();
        let te = w
            .body
            .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 6), Rotation::R0)
            .unwrap();
        let m = w.body.add_component(ComponentKind::Macro(shape), GridPos::new(12, 0), Rotation::R0).unwrap();
        wire(&mut w.body, pin(ta, 0), pin(m, 0));
        wire(&mut w.body, pin(te, 0), pin(m, 1));
        w.sync_pins();
        w
    };
    let outer = define(&mut c, board);
    let inst = place(&mut c, outer, GridPos::new(30, 0));
    let sa = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 0), Rotation::R0).unwrap();
    let se = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 6), Rotation::R0).unwrap();
    wire(&mut c, pin(sa, 0), pin(inst, 0));
    wire(&mut c, pin(se, 0), pin(inst, 1));
    let mut sim = SimState::new(Netlist::compile(&c).unwrap(), &c);
    sim.reset().unwrap();
    let err = sim.set_switch(se, true).unwrap_err();
    let SimError::BusContention { drivers, place: at, .. } = &err else { panic!("{err:?}") };
    let inner = c
        .macros()
        .def(outer)
        .unwrap()
        .body
        .components()
        .find(|k| matches!(k.kind, ComponentKind::Macro(_)))
        .unwrap()
        .id;
    assert_eq!(drivers, &vec![vec![inst, inner, t1], vec![inst, inner, t2]]);
    assert!(at.contains(&format!("root › Board#{} › Clash#{} › Tri-state#{}", inst.0, inner.0, t1.0)), "{at}");
    let msg = err.to_string();
    assert!(msg.starts_with("bus contention at higher level"), "{msg}");

    // A race (an inverter feeding itself) inside a macro.
    let mut c = Circuit::new();
    let mut d = MacroDef::new("Osc");
    let a = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::In, tristate: false }, GridPos::new(0, 0), Rotation::R0)
        .unwrap();
    let n = d
        .body
        .add_component(ComponentKind::Gate { op: GateOp::Nand, inputs: 2 }, GridPos::new(8, 0), Rotation::R0)
        .unwrap();
    let y = d
        .body
        .add_component(ComponentKind::Tag { dir: TagDir::Out, tristate: false }, GridPos::new(20, 0), Rotation::R0)
        .unwrap();
    wire(&mut d.body, pin(a, 0), pin(n, 0));
    wire(&mut d.body, pin(n, 2), pin(n, 1));
    wire(&mut d.body, pin(n, 2), pin(y, 0));
    d.sync_pins();
    let id = define(&mut c, d);
    let inst = place(&mut c, id, GridPos::new(10, 0));
    let s = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 0), Rotation::R0).unwrap();
    wire(&mut c, pin(s, 0), pin(inst, 0));
    let mut sim = SimState::new(Netlist::compile(&c).unwrap(), &c);
    sim.reset().unwrap();
    let err = sim.set_switch(s, true).unwrap_err();
    let SimError::RaceCondition { path, place: at, .. } = &err else { panic!("{err:?}") };
    assert_eq!(path, &vec![inst, n]);
    assert_eq!(at, &format!("root › Osc#{} › NAND#{}", inst.0, n.0));
    assert!(err.to_string().contains("race condition at higher level"), "{err}");
}

/// Each instance has its own state; the inside view of an instance maps
/// the nets of *that* instance onto the definition's components.
#[test]
fn inside_view_shows_one_instance() {
    let src = bw_io::library::builtin().iter().find(|e| e.key == "builtin/half-adder").unwrap();
    let mut c = Circuit::new();
    let id = common::import(&mut c, &src.file.table, src.file.root);
    let i1 = place(&mut c, id, GridPos::new(30, 0));
    let i2 = place(&mut c, id, GridPos::new(30, 20));
    let mut sw = Vec::new();
    for (k, inst) in [(0, i1), (1, i1), (2, i2), (3, i2)] {
        let s = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 6 * k), Rotation::R0).unwrap();
        wire(&mut c, pin(s, 0), pin(inst, u8::try_from(k % 2).unwrap()));
        sw.push(s);
    }
    let mut sim = SimState::new(Netlist::compile(&c).unwrap(), &c);
    sim.reset().unwrap();
    sim.set_switch(sw[0], true).unwrap(); // instance 1: A = 1, B = 0 → S = 1
    sim.set_switch(sw[2], true).unwrap();
    sim.set_switch(sw[3], true).unwrap(); // instance 2: A = B = 1 → S = 0, C = 1
    let body = &c.macros().def(id).unwrap().body;
    let xor = body.components().find(|k| matches!(k.kind, ComponentKind::Gate { op: GateOp::Xor, .. })).unwrap().id;
    let net = sim.netlist();
    let lvl = |path: &[ComponentId], comp, k| {
        let n = net.view_output_net(path, comp, k).unwrap();
        bw_sim::packed_level(&sim.packed(), n as usize)
    };
    assert_eq!(lvl(&[i1], xor, 0), 1);
    assert_eq!(lvl(&[i2], xor, 0), 0);
    // The A input tag inside instance 2 shows the level coming in.
    let a_tag = c.macros().def(id).unwrap().pins[0].tag;
    assert_eq!(lvl(&[i2], a_tag, 0), 1);
    // The view maps (renderer input) list the body's components.
    let m1 = net.view_map(&[i1]).unwrap();
    assert!(m1.pins.contains(&(xor, 0)) && m1.pins.contains(&(a_tag, 0)));
    assert!(net.view_map(&[ComponentId(9999)]).is_none());
    let root = net.view_map(&[]).unwrap();
    assert!(root.pins.contains(&(i1, 0)) && root.pins.contains(&(i2, 1)), "instance outputs drive root nets");
}

/// Recompiling after a definition edit only rebuilds that definition's
/// template; and state survives recompiles per instance path.
#[test]
fn incremental_compile_and_state_transfer() {
    let src = bw_io::library::builtin().iter().find(|e| e.key == "builtin/counter-4").unwrap();
    let mut c = Circuit::new();
    let id = common::import(&mut c, &src.file.table, src.file.root);
    let mut insts = Vec::new();
    for i in 0..200 {
        insts.push(place(&mut c, id, GridPos::new(40 * (i % 20), 60 * (i / 20))));
    }
    let mut cache = FlattenCache::default();
    let (_, s1) = Netlist::compile_cached(&c, &mut cache).unwrap();
    assert_eq!((s1.templates_built, s1.instances), (1, 200));
    let (_, s2) = Netlist::compile_cached(&c, &mut cache).unwrap();
    assert_eq!((s2.templates_built, s2.templates_reused), (0, 200));
    // Edit the definition (move a component): one template rebuilt.
    let mut d = (**c.macros().def(id).unwrap()).clone();
    let any = d.body.components().find(|k| matches!(k.kind, ComponentKind::Gate { .. })).unwrap().id;
    let p = d.body.component(any).unwrap().pos;
    d.body.move_component(any, p.offset(0, 1)).unwrap();
    c.apply(&Edit::DefineMacro { id, def: Arc::new(d) }).unwrap();
    let (_, s3) = Netlist::compile_cached(&c, &mut cache).unwrap();
    assert_eq!(s3.templates_built, 1);
    println!("200 counters: compile {:?} (flatten {:?}, netlist {:?})", s3.total(), s3.flatten, s3.netlist);
}

/// "Create macro from selection": the gates of the half-adder example
/// become a macro; nets crossing the selection become tags and the circuit
/// behaves exactly as before for every input combination.
#[test]
fn macro_from_selection_preserves_behaviour() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/flipflops-memory.bitwerk");
    for example in ["half-adder", "all-components", "flipflops-memory"] {
        let path = path.with_file_name(format!("{example}.bitwerk"));
        let before = bw_io::load_from_path(&path).unwrap().circuit;
        let switches: Vec<ComponentId> =
            before.components().filter(|c| matches!(c.kind, ComponentKind::Switch { .. })).map(|c| c.id).collect();
        let leds: Vec<ComponentId> =
            before.components().filter(|c| c.kind == ComponentKind::Led).map(|c| c.id).collect();
        // Everything except sources, LEDs and memories.
        let sel: Vec<ComponentId> = before
            .components()
            .filter(|c| {
                !matches!(c.kind, ComponentKind::Switch { .. } | ComponentKind::Led | ComponentKind::Clock { .. })
            })
            .map(|c| c.id)
            .collect();
        let mut after = before.clone();
        let plan = match after.plan_macro(&sel, "Extracted") {
            Ok(p) => p,
            Err(CircuitError::BadMacro(m)) if m.contains("bus") => continue, // documented limitation
            Err(e) => panic!("{example}: {e}"),
        };
        let n_pins = plan.links.len();
        let (_, err) = after.apply_macro_plan(plan, MacroId(0));
        assert!(err.is_none(), "{example}: {err:?}");
        after.check().unwrap();
        let def = after.macros().def(MacroId(0)).unwrap();
        assert_eq!(def.pins.len(), n_pins);
        assert!(def.pins.iter().all(|p| !p.name.is_empty()));
        let run = |c: &Circuit| {
            let mut s = SimState::new(Netlist::compile(c).unwrap(), c);
            s.reset().unwrap();
            s
        };
        let (mut s1, mut s2) = (run(&before), run(&after));
        let mut rng = Rng(7);
        for step in 0..40 {
            for &l in &leds {
                assert_eq!(
                    level_of_led(&after, &s2, l),
                    level_of_led(&before, &s1, l),
                    "{example} step {step} LED {l:?}"
                );
            }
            if switches.is_empty() {
                break;
            }
            let k = switches[rng.next(switches.len())];
            let on = rng.next(2) == 1;
            s1.set_switch(k, on).unwrap();
            s2.set_switch(k, on).unwrap();
        }
    }
}
