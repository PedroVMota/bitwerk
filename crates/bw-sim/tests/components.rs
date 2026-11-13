//! Behaviour of the format 5 components (power / ground, sequence
//! generators, push buttons, pulse inputs, 7-segment and numeric displays,
//! text annotations) and of the Digital Works compatible evaluation mode.

#![allow(clippy::unwrap_used, clippy::missing_panics_doc, clippy::many_single_char_names)]

use bw_core::{
    Circuit, ComponentId, ComponentKind, EvalMode, GateOp, GridPos, Note, NumFormat, PinRef, Rotation, SwitchMode,
};
use bw_sim::{Netlist, SimError, SimState};

fn add(c: &mut Circuit, kind: ComponentKind, x: i32, y: i32) -> ComponentId {
    c.add_component(kind, GridPos::new(x, y), Rotation::R0).unwrap()
}

fn wire(c: &mut Circuit, a: ComponentId, ap: u8, b: ComponentId, bp: u8) {
    c.connect(PinRef { component: a, pin: ap }, PinRef { component: b, pin: bp }, vec![]).unwrap();
}

fn run(c: &Circuit) -> SimState {
    let mut s = SimState::new(Netlist::compile(c).unwrap(), c);
    s.reset().unwrap();
    s
}

#[test]
fn power_and_ground_are_constant() {
    let mut c = Circuit::new();
    let vcc = add(&mut c, ComponentKind::Const { high: true }, 0, 0);
    let gnd = add(&mut c, ComponentKind::Const { high: false }, 0, 4);
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 }, 6, 0);
    wire(&mut c, vcc, 0, g, 0);
    wire(&mut c, gnd, 0, g, 1);
    let mut s = run(&c);
    assert_eq!((s.output(vcc, 0), s.output(gnd, 0), s.output(g, 0)), (Some(1), Some(0), Some(1)));
    // A constant ignores SetSwitch and time.
    s.set_switch(vcc, false).unwrap();
    for _ in 0..50 {
        s.tick().unwrap();
    }
    assert_eq!(s.output(vcc, 0), Some(1));
}

#[test]
fn sequence_generator_steps_through_its_pattern() {
    let mut c = Circuit::new();
    // 0, 1, 1, 0, 1 repeating, one step every 2 ticks.
    let seq = add(&mut c, ComponentKind::SeqGen { pattern: 0b10110, len: 5, half_period: 2 }, 0, 0);
    let mut s = run(&c);
    let mut seen = vec![s.output(seq, 0).unwrap()];
    let mut edges = 0;
    for _ in 0..19 {
        if s.tick().unwrap() {
            edges += 1;
            seen.push(s.output(seq, 0).unwrap());
        }
    }
    assert_eq!(seen, vec![0, 1, 1, 0, 1, 0, 1, 1, 0, 1]);
    assert_eq!(edges, 9, "every step is a step boundary, even without a level change");
    // Reset starts the pattern over.
    s.reset().unwrap();
    assert_eq!(s.output(seq, 0), Some(0));
    // A clock is the pattern 01.
    let mut c2 = Circuit::new();
    let clk = add(&mut c2, ComponentKind::Clock { half_period: 3 }, 0, 0);
    let seq2 = add(&mut c2, ComponentKind::SeqGen { pattern: 0b10, len: 2, half_period: 3 }, 0, 4);
    let mut s2 = run(&c2);
    for _ in 0..20 {
        s2.tick().unwrap();
        assert_eq!(s2.output(clk, 0), s2.output(seq2, 0));
    }
}

#[test]
fn push_button_and_pulse_inputs() {
    let mut c = Circuit::new();
    let push = add(&mut c, ComponentKind::Switch { on: false, mode: SwitchMode::Momentary }, 0, 0);
    let pulse = add(&mut c, ComponentKind::Switch { on: true, mode: SwitchMode::Pulse }, 0, 4);
    let mut s = run(&c);
    assert_eq!(s.output(push, 0), Some(0), "rest level of push-to-make");
    s.set_switch(push, true).unwrap();
    assert_eq!(s.output(push, 0), Some(1), "held");
    s.set_switch(push, false).unwrap();
    assert_eq!(s.output(push, 0), Some(0), "released");
    // The pulse input starts at its `on` level and alternates at the
    // default clock speed.
    let mut levels = vec![s.output(pulse, 0).unwrap()];
    for _ in 0..(4 * bw_core::PULSE_HALF_PERIOD) {
        if s.tick().unwrap() {
            levels.push(s.output(pulse, 0).unwrap());
        }
    }
    assert_eq!(levels, vec![1, 0, 1, 0, 1]);
    // The hand sets the phase.
    let now = s.output(pulse, 0).unwrap();
    s.set_switch(pulse, now == 0).unwrap();
    assert_eq!(s.output(pulse, 0), Some(u8::from(now == 0)));
    for _ in 0..bw_core::PULSE_HALF_PERIOD {
        s.tick().unwrap();
    }
    assert_eq!(s.output(pulse, 0), Some(now), "keeps alternating from the new phase");
}

#[test]
fn displays_mirror_their_inputs() {
    let mut c = Circuit::new();
    let seg = add(&mut c, ComponentKind::SevenSeg { common_anode: false }, 20, 0);
    let num = add(&mut c, ComponentKind::Numeric { bits: 4, format: NumFormat::Hex, signed: false }, 40, 0);
    let mut sw = Vec::new();
    for k in 0..4u8 {
        let s = add(&mut c, ComponentKind::Switch { on: k % 2 == 0, mode: SwitchMode::Toggle }, 0, 3 * i32::from(k));
        wire(&mut c, s, 0, num, k);
        wire(&mut c, s, 0, seg, k);
        sw.push(s);
    }
    let mut s = run(&c);
    let shown = |s: &SimState, id, n| (0..n).map(|k| s.output(id, k).unwrap()).collect::<Vec<u8>>();
    assert_eq!(shown(&s, num, 4), vec![1, 0, 1, 0]);
    // Unconnected segments are floating (Z = 2).
    assert_eq!(shown(&s, seg, 7), vec![1, 0, 1, 0, 2, 2, 2]);
    s.set_switch(sw[1], true).unwrap();
    assert_eq!(shown(&s, num, 4), vec![1, 1, 1, 0]);
    // The snapshot net map lists the display signals after the outputs.
    let map = s.netlist().net_map();
    let listed = |id: ComponentId| map.pins.iter().filter(|(c, _)| *c == id).count();
    assert_eq!((listed(seg), listed(num)), (7, 4));
}

#[test]
fn text_annotations_are_not_simulated() {
    let mut c = Circuit::new();
    let t = add(&mut c, ComponentKind::Text { w: 1, h: 1 }, 0, 0);
    c.apply(&bw_core::Edit::SetNote { id: t, note: Note::new("hello") }).unwrap();
    let s = run(&c);
    assert_eq!(s.netlist().node_count(), 0);
    assert_eq!(s.output(t, 0), None);
}

/// An RS latch of two NANDs whose inputs return to 1 together: unit delay
/// flips both gates in lock-step forever (a race), the sequential mode
/// settles into a definite state.
#[test]
fn sequential_mode_settles_a_symmetric_latch() {
    for mode in [EvalMode::UnitDelay, EvalMode::Sequential] {
        let mut c = Circuit::new();
        c.set_eval_mode(mode);
        let s_in = add(&mut c, ComponentKind::Switch { on: false, mode: SwitchMode::Toggle }, 0, 0);
        let a = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 }, 10, 0);
        let b = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 }, 10, 10);
        wire(&mut c, s_in, 0, a, 0);
        wire(&mut c, s_in, 0, b, 1);
        wire(&mut c, a, 2, b, 0);
        wire(&mut c, b, 2, a, 1);
        let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
        s.reset().unwrap();
        assert_eq!((s.output(a, 0), s.output(b, 0)), (Some(1), Some(1)), "{mode:?}: both inputs 0");
        let r = s.set_switch(s_in, true);
        match mode {
            EvalMode::UnitDelay => assert!(matches!(r, Err(SimError::RaceCondition { .. })), "{r:?}"),
            EvalMode::Sequential => {
                r.unwrap();
                let (qa, qb) = (s.output(a, 0).unwrap(), s.output(b, 0).unwrap());
                assert!(qa <= 1 && qb <= 1 && qa != qb, "settled to a stored state: {qa} {qb}");
            }
        }
    }
}

/// A true oscillation is still a race in the sequential mode.
#[test]
fn sequential_mode_still_detects_oscillation() {
    let mut c = Circuit::new();
    c.set_eval_mode(EvalMode::Sequential);
    let en = add(&mut c, ComponentKind::Switch { on: false, mode: SwitchMode::Toggle }, 0, 0);
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 }, 6, 0);
    wire(&mut c, en, 0, g, 0);
    wire(&mut c, g, 2, g, 1);
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    s.reset().unwrap();
    assert!(matches!(s.set_switch(en, true), Err(SimError::RaceCondition { .. })));
}

/// In the compatible mode floating inputs read as 0 (as in Digital Works);
/// in unit-delay mode as X.
#[test]
fn floating_inputs_by_mode() {
    for (mode, want) in [(EvalMode::UnitDelay, 3), (EvalMode::Sequential, 1)] {
        let mut c = Circuit::new();
        c.set_eval_mode(mode);
        let g = add(&mut c, ComponentKind::Not, 0, 0);
        let s = run(&c);
        assert_eq!(s.output(g, 0), Some(want), "{mode:?}");
    }
}
