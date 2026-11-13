#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::struct_field_names
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bw_core::{
    Circuit, ComponentId, ComponentKind, Edit, FfTrigger, FlipFlopType, GateOp, GridPos, MemoryType, PinRef, Rotation,
    SegmentId, WireEnd,
};
use bw_sim::logic::{L0, L1, X, Z};
use bw_sim::{Command, Netlist, RunState, SimError, SimHandle, SimState, StepMode};

fn pin(component: ComponentId, pin: u8) -> PinRef {
    PinRef { component, pin }
}

fn add(c: &mut Circuit, kind: ComponentKind) -> ComponentId {
    c.add_component(kind, GridPos::default(), Rotation::R0).unwrap()
}

/// Wires output `k` of `from` to input `to_pin` of `to`.
fn wire(c: &mut Circuit, from: ComponentId, k: u8, to: ComponentId, to_pin: u8) {
    let ins = c.component(from).unwrap().kind.input_count();
    c.connect(pin(from, ins + k), pin(to, to_pin), vec![]).unwrap();
}

fn sim(c: &Circuit) -> SimState {
    let mut s = SimState::new(Netlist::compile(c).unwrap(), c);
    s.reset().unwrap();
    s
}

fn out(s: &SimState, id: ComponentId, k: u8) -> u8 {
    s.output(id, k).unwrap()
}

fn set(s: &mut SimState, sw: ComponentId, v: u8) {
    s.set_switch(sw, v == L1).unwrap();
}

/// Two switches → gate → LED; returns (circuit, a, b, gate).
fn two_input(op: GateOp) -> (Circuit, ComponentId, ComponentId, ComponentId) {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: false });
    let b = add(&mut c, ComponentKind::Switch { on: false });
    let g = add(&mut c, ComponentKind::Gate { op, inputs: 2 });
    let led = add(&mut c, ComponentKind::Led);
    wire(&mut c, a, 0, g, 0);
    wire(&mut c, b, 0, g, 1);
    wire(&mut c, g, 0, led, 0);
    (c, a, b, g)
}

// ---- combinational, four-state --------------------------------------------

#[test]
fn gate_truth_tables_through_netlist() {
    for op in GateOp::ALL {
        let (c, a, b, g) = two_input(op);
        let mut s = sim(&c);
        for (va, vb) in [(false, false), (false, true), (true, false), (true, true)] {
            s.set_switch(a, va).unwrap();
            s.set_switch(b, vb).unwrap();
            assert_eq!(out(&s, g, 0), u8::from(op.eval([va, vb])), "{op:?}({va},{vb})");
        }
    }
}

/// An unconnected input floats (Z) and reads as X: AND with a 0 is still 0,
/// AND with a 1 is X, a NOT of it is X.
#[test]
fn floating_inputs_read_as_unknown() {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: false });
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::And, inputs: 2 });
    let o = add(&mut c, ComponentKind::Gate { op: GateOp::Or, inputs: 2 });
    let n = add(&mut c, ComponentKind::Not);
    wire(&mut c, a, 0, g, 0);
    wire(&mut c, a, 0, o, 0);
    let mut s = sim(&c);
    assert_eq!(out(&s, g, 0), L0, "AND with any 0 is 0");
    assert_eq!(out(&s, o, 0), X, "OR(0, Z) is X");
    assert_eq!(out(&s, n, 0), X, "NOT of a floating input");
    set(&mut s, a, L1);
    assert_eq!(out(&s, g, 0), X);
    assert_eq!(out(&s, o, 0), L1, "OR with any 1 is 1");
}

#[test]
fn not_gate_and_three_input() {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: true });
    let n = add(&mut c, ComponentKind::Not);
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::And, inputs: 3 });
    wire(&mut c, a, 0, n, 0);
    for i in 0..3 {
        wire(&mut c, a, 0, g, i);
    }
    let s = sim(&c);
    assert_eq!(out(&s, n, 0), L0);
    assert_eq!(out(&s, g, 0), L1);
}

#[test]
fn self_feeding_not_is_a_race_condition() {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: false });
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::Nor, inputs: 2 });
    wire(&mut c, a, 0, g, 0);
    wire(&mut c, g, 0, g, 1);
    // NOR(0, own output) settles at X from floating; a fresh NOT loop with
    // a known start oscillates instead.
    let mut c2 = Circuit::new();
    let sw = add(&mut c2, ComponentKind::Switch { on: true });
    let nand = add(&mut c2, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 });
    wire(&mut c2, sw, 0, nand, 0);
    wire(&mut c2, nand, 0, nand, 1);
    let mut s = SimState::new(Netlist::compile(&c2).unwrap(), &c2);
    // Settles to X (unknown feedback) at reset; forcing a known value
    // through the switch starts the oscillation.
    s.reset().unwrap();
    assert!(matches!(
        s.set_switch(sw, false).and_then(|()| s.set_switch(sw, true)),
        Err(SimError::RaceCondition { .. })
    ));
}

#[test]
fn nand_with_feedback_oscillates() {
    // NAND(switch, own output): stable at 1 while the switch is off, then
    // oscillates once it is on.
    let mut c = Circuit::new();
    let sw = add(&mut c, ComponentKind::Switch { on: false });
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 });
    wire(&mut c, sw, 0, g, 0);
    wire(&mut c, g, 0, g, 1);
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    s.reset().unwrap();
    assert_eq!(out(&s, g, 0), L1);
    assert!(matches!(s.set_switch(sw, true), Err(SimError::RaceCondition { .. })));
}

#[test]
fn sr_latch_of_gates_settles() {
    // Cross-coupled NOR latch must not be reported as a race.
    let mut c = Circuit::new();
    let set_ = add(&mut c, ComponentKind::Switch { on: false });
    let rst = add(&mut c, ComponentKind::Switch { on: true });
    let q = add(&mut c, ComponentKind::Gate { op: GateOp::Nor, inputs: 2 });
    let nq = add(&mut c, ComponentKind::Gate { op: GateOp::Nor, inputs: 2 });
    wire(&mut c, rst, 0, q, 0);
    wire(&mut c, nq, 0, q, 1);
    wire(&mut c, set_, 0, nq, 0);
    wire(&mut c, q, 0, nq, 1);
    let mut s = sim(&c);
    s.set_switch(rst, false).unwrap();
    assert_eq!(out(&s, q, 0), L0);
    s.set_switch(set_, true).unwrap();
    s.set_switch(set_, false).unwrap();
    assert_eq!(out(&s, q, 0), L1);
}

#[test]
fn clock_toggles_every_half_period() {
    let mut c = Circuit::new();
    let clk = add(&mut c, ComponentKind::Clock { half_period: 3 });
    let mut s = sim(&c);
    let mut levels = vec![];
    for _ in 0..6 {
        s.tick().unwrap();
        levels.push(out(&s, clk, 0));
    }
    assert_eq!(levels, vec![0, 0, 1, 1, 1, 0]);
}

// ---- tri-state and buses ----------------------------------------------------

/// Two tri-state buffers on one bus read by a gate (via the bus net).
struct Bus {
    c: Circuit,
    a: ComponentId,
    b: ComponentId,
    ea: ComponentId,
    eb: ComponentId,
    t1: ComponentId,
    t2: ComponentId,
    buf: ComponentId,
}

fn bus() -> Bus {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: true });
    let b = add(&mut c, ComponentKind::Switch { on: false });
    let ea = add(&mut c, ComponentKind::Switch { on: false });
    let eb = add(&mut c, ComponentKind::Switch { on: false });
    let t1 = add(&mut c, ComponentKind::TriState);
    let t2 = add(&mut c, ComponentKind::TriState);
    let buf = add(&mut c, ComponentKind::Gate { op: GateOp::And, inputs: 2 });
    let led = add(&mut c, ComponentKind::Led);
    wire(&mut c, a, 0, t1, 0);
    wire(&mut c, ea, 0, t1, 1);
    wire(&mut c, b, 0, t2, 0);
    wire(&mut c, eb, 0, t2, 1);
    for t in [t1, t2] {
        wire(&mut c, t, 0, buf, 0);
        wire(&mut c, t, 0, buf, 1);
        wire(&mut c, t, 0, led, 0);
    }
    Bus { c, a, b, ea, eb, t1, t2, buf }
}

#[test]
fn tristate_floats_and_single_driver_wins() {
    let Bus { c, a, b, ea, eb, t1, t2, buf } = bus();
    let mut s = sim(&c);
    // Both disabled: the bus floats; a gate reading it sees X.
    assert_eq!(out(&s, t1, 0), Z, "bus floats");
    assert_eq!(out(&s, t2, 0), Z, "same net");
    assert_eq!(out(&s, buf, 0), X, "Z propagates as X through logic");
    // One driver: its value.
    set(&mut s, ea, L1);
    assert_eq!(out(&s, t1, 0), L1);
    assert_eq!(out(&s, buf, 0), L1);
    set(&mut s, a, L0);
    assert_eq!(out(&s, buf, 0), L0);
    // Hand the bus over to the other driver.
    set(&mut s, ea, L0);
    set(&mut s, b, L1);
    set(&mut s, eb, L1);
    assert_eq!(out(&s, t2, 0), L1);
    assert_eq!(out(&s, buf, 0), L1);
}

#[test]
fn bus_contention_halts_naming_net_and_drivers() {
    let Bus { c, ea, eb, t1, t2, .. } = bus();
    let mut s = sim(&c);
    set(&mut s, ea, L1);
    let err = s.set_switch(eb, true).unwrap_err();
    let SimError::BusContention { net, drivers } = &err else { panic!("{err:?}") };
    assert_eq!(*net, s.netlist().output_net(t1, 0).unwrap());
    assert_eq!(drivers, &vec![t1, t2]);
    let msg = err.to_string();
    assert!(msg.contains("bus contention") && msg.contains(&format!("#{}", t1.0)), "{msg}");
}

/// Enables switched through gates of different depth overlap for one delta;
/// that is not contention once the circuit settles.
#[test]
fn complementary_enables_do_not_report_transient_contention() {
    let mut c = Circuit::new();
    let sel = add(&mut c, ComponentKind::Switch { on: false });
    let not = add(&mut c, ComponentKind::Not);
    let a = add(&mut c, ComponentKind::Switch { on: true });
    let t1 = add(&mut c, ComponentKind::TriState);
    let t2 = add(&mut c, ComponentKind::TriState);
    let led = add(&mut c, ComponentKind::Led);
    wire(&mut c, sel, 0, not, 0);
    wire(&mut c, a, 0, t1, 0);
    wire(&mut c, a, 0, t2, 0);
    wire(&mut c, sel, 0, t1, 1);
    wire(&mut c, not, 0, t2, 1);
    wire(&mut c, t1, 0, led, 0);
    wire(&mut c, t2, 0, led, 0);
    let mut s = sim(&c);
    for _ in 0..4 {
        set(&mut s, sel, L1);
        assert_eq!(out(&s, t1, 0), L1);
        set(&mut s, sel, L0);
        assert_eq!(out(&s, t2, 0), L1);
    }
}

// ---- flip-flops -----------------------------------------------------------

struct Ff {
    s: SimState,
    ff: ComponentId,
    /// Data switches (D; S, R; J, K) and the clock switch.
    a: ComponentId,
    b: ComponentId,
    clk: ComponentId,
    pre: Option<ComponentId>,
    clr: Option<ComponentId>,
}

impl Ff {
    fn new(ty: FlipFlopType, trigger: FfTrigger, preset_clear: bool, wire_async: bool) -> Self {
        let mut c = Circuit::new();
        let a = add(&mut c, ComponentKind::Switch { on: false });
        let b = add(&mut c, ComponentKind::Switch { on: false });
        let clk = add(&mut c, ComponentKind::Switch { on: false });
        let ff = add(&mut c, ComponentKind::FlipFlop { ty, trigger, preset_clear });
        wire(&mut c, a, 0, ff, 0);
        wire(&mut c, clk, 0, ff, 1);
        if ty != FlipFlopType::D {
            wire(&mut c, b, 0, ff, 2);
        }
        let (mut pre, mut clr) = (None, None);
        if preset_clear && wire_async {
            let sync = ty.sync_inputs();
            let p = add(&mut c, ComponentKind::Switch { on: false });
            let r = add(&mut c, ComponentKind::Switch { on: false });
            wire(&mut c, p, 0, ff, sync);
            wire(&mut c, r, 0, ff, sync + 1);
            (pre, clr) = (Some(p), Some(r));
        }
        Self { s: sim(&c), ff, a, b, clk, pre, clr }
    }
    fn q(&self) -> (u8, u8) {
        (out(&self.s, self.ff, 0), out(&self.s, self.ff, 1))
    }
    fn inputs(&mut self, a: u8, b: u8) {
        set(&mut self.s, self.a, a);
        set(&mut self.s, self.b, b);
    }
    fn clock(&mut self, v: u8) {
        set(&mut self.s, self.clk, v);
    }
    fn pulse(&mut self) {
        self.clock(L1);
        self.clock(L0);
    }
}

#[test]
fn d_flip_flop_rising_edge() {
    let mut f = Ff::new(FlipFlopType::D, FfTrigger::RisingEdge, false, false);
    assert_eq!(f.q(), (L0, L1), "power-on state");
    // D alone changes nothing.
    f.inputs(L1, L0);
    assert_eq!(f.q().0, L0);
    // Rising edge samples D (doc: D=1, clk → SET).
    f.clock(L1);
    assert_eq!(f.q(), (L1, L0));
    // Changing D while the clock is high: no change (edge, not level).
    f.inputs(L0, L0);
    assert_eq!(f.q().0, L1);
    // Falling edge: no change.
    f.clock(L0);
    assert_eq!(f.q().0, L1);
    // Next rising edge: RESET (D=0).
    f.clock(L1);
    assert_eq!(f.q(), (L0, L1));
}

#[test]
fn d_flip_flop_falling_edge() {
    let mut f = Ff::new(FlipFlopType::D, FfTrigger::FallingEdge, false, false);
    f.inputs(L1, L0);
    f.clock(L1);
    assert_eq!(f.q().0, L0, "rising edge ignored");
    f.clock(L0);
    assert_eq!(f.q().0, L1, "falling edge samples");
}

/// The bistable (gated latch): transparent while C = 1, holding while C = 0.
#[test]
fn d_latch_is_level_sensitive() {
    let mut f = Ff::new(FlipFlopType::D, FfTrigger::Latch, false, false);
    f.inputs(L1, L0);
    assert_eq!(f.q().0, L0, "closed latch holds");
    f.clock(L1);
    assert_eq!(f.q().0, L1, "open latch follows D");
    f.inputs(L0, L0);
    assert_eq!(f.q().0, L0, "...and keeps following");
    f.inputs(L1, L0);
    f.clock(L0);
    f.inputs(L0, L0);
    assert_eq!(f.q().0, L1, "holds the last value once closed");
}

/// Master-slave: the master follows D while C = 1, Q changes only on the
/// falling edge.
#[test]
fn d_master_slave() {
    let mut f = Ff::new(FlipFlopType::D, FfTrigger::MasterSlave, false, false);
    f.inputs(L1, L0);
    f.clock(L1);
    assert_eq!(f.q().0, L0, "output waits for the falling edge");
    f.inputs(L0, L0);
    f.inputs(L1, L0);
    f.clock(L0);
    assert_eq!(f.q().0, L1, "slave takes the master's last value");
    f.inputs(L0, L0);
    assert_eq!(f.q().0, L1, "C low: nothing moves");
}

/// RS truth table (doc 4.3) on the default latch, including the forbidden
/// S = R = 1 (indeterminate: Q = Q̄ = X) and recovery from it.
#[test]
fn rs_flip_flop_truth_table_and_indeterminate_state() {
    let mut f = Ff::new(FlipFlopType::Rs, FfTrigger::Latch, false, false);
    f.clock(L1);
    f.inputs(L1, L0);
    assert_eq!(f.q(), (L1, L0), "S=1 R=0: SET");
    f.inputs(L0, L0);
    assert_eq!(f.q(), (L1, L0), "S=0 R=0: no change");
    f.inputs(L0, L1);
    assert_eq!(f.q(), (L0, L1), "S=0 R=1: RESET");
    f.inputs(L1, L1);
    assert_eq!(f.q(), (X, X), "S=1 R=1: indeterminate");
    // Release both while closed (switches change one at a time), reopen.
    f.clock(L0);
    f.inputs(L0, L0);
    f.clock(L1);
    assert_eq!(f.q(), (X, X), "stays unknown");
    f.inputs(L1, L0);
    assert_eq!(f.q(), (L1, L0), "a SET makes it known again");
    // Closed latch ignores S/R.
    f.clock(L0);
    f.inputs(L0, L1);
    assert_eq!(f.q().0, L1);
}

#[test]
fn rs_edge_triggered_indeterminate() {
    let mut f = Ff::new(FlipFlopType::Rs, FfTrigger::RisingEdge, false, false);
    f.inputs(L1, L1);
    assert_eq!(f.q().0, L0, "no edge yet");
    f.clock(L1);
    assert_eq!(f.q(), (X, X));
}

/// JK truth table (doc 4.3) on the default master-slave, output on the
/// falling edge: 00 hold, 01 reset, 10 set, 11 toggle.
#[test]
fn jk_master_slave_truth_table() {
    let mut f = Ff::new(FlipFlopType::Jk, FfTrigger::MasterSlave, false, false);
    f.inputs(L1, L0);
    f.clock(L1);
    assert_eq!(f.q().0, L0, "nothing before the falling edge");
    f.clock(L0);
    assert_eq!(f.q(), (L1, L0), "J=1 K=0: SET");
    f.inputs(L0, L0);
    f.pulse();
    assert_eq!(f.q().0, L1, "J=0 K=0: no change");
    f.inputs(L0, L1);
    f.pulse();
    assert_eq!(f.q(), (L0, L1), "J=0 K=1: RESET");
    f.inputs(L1, L1);
    f.pulse();
    assert_eq!(f.q().0, L1, "J=1 K=1: toggle");
    f.pulse();
    assert_eq!(f.q().0, L0, "toggle again");
}

/// Ones catching: a J glitch while the clock is high is remembered by the
/// master-slave JK, but ignored by an edge-triggered one.
#[test]
fn jk_master_slave_catches_ones_but_edge_triggered_does_not() {
    for (trigger, expect) in [(FfTrigger::MasterSlave, L1), (FfTrigger::FallingEdge, L0)] {
        let mut f = Ff::new(FlipFlopType::Jk, trigger, false, false);
        f.clock(L1);
        f.inputs(L1, L0);
        f.inputs(L0, L0);
        f.clock(L0);
        assert_eq!(f.q().0, expect, "{trigger:?}");
    }
}

#[test]
fn jk_rising_edge_toggles() {
    let mut f = Ff::new(FlipFlopType::Jk, FfTrigger::RisingEdge, false, false);
    f.inputs(L1, L1);
    let mut seen = vec![];
    for _ in 0..4 {
        f.clock(L1);
        seen.push(f.q().0);
        f.clock(L0);
    }
    assert_eq!(seen, vec![L1, L0, L1, L0]);
}

/// Preset / clear are asynchronous and active high (doc 4.3); both at
/// once is indeterminate; unconnected they are inactive.
#[test]
fn asynchronous_preset_and_clear() {
    for ty in [FlipFlopType::D, FlipFlopType::Jk] {
        let mut f = Ff::new(ty, ty.default_trigger(), true, true);
        let (pre, clr) = (f.pre.unwrap(), f.clr.unwrap());
        set(&mut f.s, pre, L1);
        assert_eq!(f.q(), (L1, L0), "{ty:?} preset without a clock");
        set(&mut f.s, pre, L0);
        assert_eq!(f.q().0, L1, "{ty:?} preset is remembered");
        set(&mut f.s, clr, L1);
        assert_eq!(f.q(), (L0, L1), "{ty:?} clear");
        // Clear dominates a clock edge while held.
        f.inputs(L1, L0);
        f.pulse();
        f.clock(L1);
        assert_eq!(f.q().0, L0, "{ty:?} clear holds against the clock");
        set(&mut f.s, pre, L1);
        assert_eq!(f.q(), (X, X), "{ty:?} preset and clear together");
        set(&mut f.s, pre, L0);
        set(&mut f.s, clr, L0);
    }
    // Pins present but unconnected: the flip-flop works normally.
    let mut f = Ff::new(FlipFlopType::D, FfTrigger::RisingEdge, true, false);
    f.inputs(L1, L0);
    f.clock(L1);
    assert_eq!(f.q().0, L1);
}

/// D flip-flop in toggle mode (Q̄ → D) clocked by a clock source: each edge
/// of the right polarity toggles once; the circuit does not race.
fn toggle_circuit(trigger: FfTrigger) -> (Circuit, ComponentId) {
    let mut c = Circuit::new();
    let clk = add(&mut c, ComponentKind::Clock { half_period: 1 });
    let ff = add(&mut c, ComponentKind::FlipFlop { ty: FlipFlopType::D, trigger, preset_clear: false });
    wire(&mut c, clk, 0, ff, 1);
    wire(&mut c, ff, 1, ff, 0);
    (c, ff)
}

#[test]
fn toggle_flip_flop_divides_the_clock() {
    let (c, ff) = toggle_circuit(FfTrigger::RisingEdge);
    let mut s = sim(&c);
    let mut seen = vec![];
    for _ in 0..8 {
        s.tick().unwrap();
        seen.push(out(&s, ff, 0));
    }
    assert_eq!(seen, vec![1, 1, 0, 0, 1, 1, 0, 0]);
}

/// A 4-bit ripple counter of JK flip-flops (J = K = 1, each clocked by the
/// previous Q on its falling edge) counts 0..15 and wraps.
#[test]
fn jk_ripple_counter_counts() {
    let mut c = Circuit::new();
    let one = add(&mut c, ComponentKind::Switch { on: true });
    let clk = add(&mut c, ComponentKind::Switch { on: false });
    let mut ffs = vec![];
    for i in 0..4 {
        let ff = add(&mut c, ComponentKind::flip_flop(FlipFlopType::Jk));
        wire(&mut c, one, 0, ff, 0);
        wire(&mut c, one, 0, ff, 2);
        if i == 0 {
            wire(&mut c, clk, 0, ff, 1);
        } else {
            wire(&mut c, ffs[i - 1], 0, ff, 1);
        }
        ffs.push(ff);
    }
    let mut s = sim(&c);
    let count = |s: &SimState| ffs.iter().enumerate().map(|(i, &f)| u32::from(out(s, f, 0)) << i).sum::<u32>();
    for n in 1..=20u32 {
        s.set_switch(clk, true).unwrap();
        s.set_switch(clk, false).unwrap();
        assert_eq!(count(&s), n % 16);
    }
}

fn wait_for(h: &SimHandle, pred: impl Fn(&bw_sim::Snapshot) -> bool) -> Arc<bw_sim::Snapshot> {
    let start = Instant::now();
    loop {
        let s = h.snapshot();
        if pred(&s) {
            return s;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "timeout waiting for snapshot");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Step by half and full clock cycles (doc 6.4) advances a rising-edge
/// toggle flip-flop by one clock edge per half cycle.
#[test]
fn step_half_and_full_clock_cycles() {
    let (c, ff) = toggle_circuit(FfTrigger::RisingEdge);
    let net = Netlist::compile(&c).unwrap().output_net(ff, 0).unwrap() as usize;
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    h.send(Command::Load { circuit: Arc::new(c), revision: 1 });
    let mut tick = wait_for(&h, |s| s.revision == 1 && s.net_count > 0).tick;
    let mut q = vec![];
    for mode in [StepMode::HalfCycle, StepMode::HalfCycle, StepMode::FullCycle, StepMode::FullCycle] {
        h.send(Command::Step(mode));
        let edges = if mode == StepMode::HalfCycle { 1 } else { 2 };
        let s = wait_for(&h, |s| s.tick == tick + edges);
        tick = s.tick;
        q.push(s.level(net));
    }
    // Half: rising edge (Q 0→1); half: falling (no change); full: rise +
    // fall (Q 1→0); full: rise + fall (Q 0→1).
    assert_eq!(q, vec![L1, L1, L0, L1]);
}

// ---- memory -----------------------------------------------------------------

struct Mem {
    s: SimState,
    m: ComponentId,
    dw: u8,
    addr: Vec<ComponentId>,
    data: Vec<ComponentId>,
    rw: Option<ComponentId>,
    cs: ComponentId,
}

impl Mem {
    fn new(ty: MemoryType, addr_bits: u8, data_bits: u8, contents: Option<Vec<u16>>) -> Self {
        let mut c = Circuit::new();
        let kind = ComponentKind::Memory { ty, addr_bits, data_bits };
        let m = add(&mut c, kind);
        if let Some(w) = contents {
            c.set_memory(m, &w).unwrap();
        }
        let mut addr = vec![];
        for i in 0..addr_bits {
            let sw = add(&mut c, ComponentKind::Switch { on: false });
            wire(&mut c, sw, 0, m, i);
            addr.push(sw);
        }
        let mut data = vec![];
        let mut rw = None;
        let mut next = addr_bits;
        if ty == MemoryType::Ram {
            for i in 0..data_bits {
                let sw = add(&mut c, ComponentKind::Switch { on: false });
                wire(&mut c, sw, 0, m, addr_bits + i);
                data.push(sw);
            }
            let r = add(&mut c, ComponentKind::Switch { on: true });
            wire(&mut c, r, 0, m, addr_bits + data_bits);
            rw = Some(r);
            next += data_bits + 1;
        }
        let cs = add(&mut c, ComponentKind::Switch { on: false });
        wire(&mut c, cs, 0, m, next);
        Self { s: sim(&c), m, dw: data_bits, addr, data, rw, cs }
    }
    fn set_addr(&mut self, a: usize) {
        for (i, &sw) in self.addr.iter().enumerate() {
            self.s.set_switch(sw, a >> i & 1 == 1).unwrap();
        }
    }
    fn set_data(&mut self, v: u16) {
        for (i, &sw) in self.data.iter().enumerate() {
            self.s.set_switch(sw, v >> i & 1 == 1).unwrap();
        }
    }
    fn outputs(&self) -> Vec<u8> {
        (0..self.dw).map(|k| out(&self.s, self.m, k)).collect()
    }
    fn word(&self) -> Option<u16> {
        let o = self.outputs();
        o.iter().all(|&v| v <= L1).then(|| o.iter().enumerate().map(|(k, &v)| u16::from(v) << k).sum())
    }
}

#[test]
fn ram_write_then_read_cycles() {
    let mut m = Mem::new(MemoryType::Ram, 4, 8, None);
    let rw = m.rw.unwrap();
    for a in [0usize, 5, 15] {
        m.set_addr(a);
        m.set_data(u16::try_from(a * 16 + 3).unwrap());
        m.s.set_switch(rw, false).unwrap(); // write
        m.s.set_switch(rw, true).unwrap(); // back to read
    }
    for a in [0usize, 5, 15] {
        m.set_addr(a);
        assert_eq!(m.word(), Some(u16::try_from(a * 16 + 3).unwrap()), "address {a}");
    }
    m.set_addr(7);
    assert_eq!(m.word(), Some(0), "never written");
    assert_eq!(m.s.memory(m.m).unwrap().0[5], 0x53);
}

/// C̄S̄ high deselects (outputs Z, no write); while writing (R/W̄ = 0) the
/// outputs float too, i.e. the outputs are enabled only for a selected read.
#[test]
fn ram_chip_select_and_output_enable_semantics() {
    let mut m = Mem::new(MemoryType::Ram, 4, 4, None);
    let rw = m.rw.unwrap();
    m.set_addr(2);
    m.set_data(0b1010);
    m.s.set_switch(rw, false).unwrap();
    assert!(m.outputs().iter().all(|&v| v == Z), "outputs float while writing");
    m.s.set_switch(rw, true).unwrap();
    assert_eq!(m.word(), Some(0b1010));
    // Deselected: outputs float and writes are ignored.
    m.s.set_switch(m.cs, true).unwrap();
    assert!(m.outputs().iter().all(|&v| v == Z), "deselected outputs float");
    m.set_data(0b0101);
    m.s.set_switch(rw, false).unwrap();
    m.s.set_switch(rw, true).unwrap();
    m.s.set_switch(m.cs, false).unwrap();
    assert_eq!(m.word(), Some(0b1010), "no write while deselected");
}

#[test]
fn unknown_address_reads_unknown() {
    let mut c = Circuit::new();
    let rom = add(&mut c, ComponentKind::Memory { ty: MemoryType::Rom, addr_bits: 4, data_bits: 1 });
    let s = sim(&c);
    assert_eq!(out(&s, rom, 0), X, "floating address, chip select defaulting to selected");
}

/// ROM contents come from the circuit; the smallest and largest address
/// widths both work at their extreme addresses.
#[test]
fn rom_reads_contents_at_address_width_bounds() {
    for bits in [bw_core::MIN_ADDR_BITS, bw_core::MAX_ADDR_BITS] {
        let words = 1usize << bits;
        let contents: Vec<u16> = (0..words).map(|i| u16::try_from(i % 65_521).unwrap()).collect();
        let mut m = Mem::new(MemoryType::Rom, bits, 16, Some(contents.clone()));
        for a in [0, 1, words / 2, words - 1] {
            m.set_addr(a);
            let o: Vec<u8> = (0..16).map(|k| out(&m.s, m.m, k)).collect();
            let w: u16 = o.iter().enumerate().map(|(k, &v)| u16::from(v) << k).sum();
            assert_eq!(w, contents[a], "{bits} bits, address {a}");
        }
    }
}

#[test]
fn live_memory_edit_propagates() {
    let mut m = Mem::new(MemoryType::Rom, 4, 4, None);
    m.set_addr(3);
    assert_eq!(m.outputs(), vec![L0; 4]);
    m.s.write_memory_word(m.m, 3, 0b1001).unwrap();
    assert_eq!(m.outputs(), vec![L1, L0, L0, L1]);
}

// ---- engine -------------------------------------------------------------------

#[test]
fn engine_thread_publishes_snapshots_and_halts_on_race() {
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    let (c, a, b, g) = two_input(GateOp::And);
    let gi = Netlist::compile(&c).unwrap().output_net(g, 0).unwrap() as usize;
    h.send(Command::Load { circuit: Arc::new(c), revision: 1 });
    h.send(Command::SetSwitch(a, true));
    h.send(Command::SetSwitch(b, true));
    wait_for(&h, |s| s.revision == 1 && s.level(gi) == L1);

    let mut osc = Circuit::new();
    let sw = add(&mut osc, ComponentKind::Switch { on: true });
    let n = add(&mut osc, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 });
    wire(&mut osc, sw, 0, n, 0);
    wire(&mut osc, n, 0, n, 1);
    h.send(Command::Load { circuit: Arc::new(osc), revision: 2 });
    h.send(Command::SetSwitch(sw, false));
    h.send(Command::SetSwitch(sw, true));
    let s = wait_for(&h, |s| s.revision == 2 && s.state == RunState::Halted);
    assert!(matches!(s.error, Some(SimError::RaceCondition { .. })));
}

#[test]
fn engine_halts_on_bus_contention() {
    let Bus { c, ea, eb, t1, t2, .. } = bus();
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    h.send(Command::Load { circuit: Arc::new(c), revision: 1 });
    h.send(Command::SetSwitch(ea, true));
    h.send(Command::SetSwitch(eb, true));
    let s = wait_for(&h, |s| s.state == RunState::Halted);
    match &s.error {
        Some(SimError::BusContention { drivers, .. }) => assert_eq!(drivers, &vec![t1, t2]),
        e => panic!("{e:?}"),
    }
}

#[test]
fn engine_applies_incremental_edits() {
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    let (c, a, b, g) = two_input(GateOp::And);
    h.send(Command::Load { circuit: Arc::new(c), revision: 1 });
    h.send(Command::SetSwitch(a, true));
    // Replace input b by a new switch that is on: AND becomes 1.
    h.send(Command::Edit { edit: Edit::RemoveComponent(b), revision: 2 });
    let add_on = Edit::AddComponent {
        kind: ComponentKind::Switch { on: true },
        pos: GridPos::default(),
        rotation: Rotation::R0,
    };
    h.send(Command::Edit { edit: add_on, revision: 3 });
    // Ids are allocated deterministically: the new switch gets id 4.
    let new_sw = ComponentId(4);
    let connect = Edit::AddWire { from: WireEnd::Pin(pin(new_sw, 0)), to: WireEnd::Pin(pin(g, 1)), via: vec![] };
    h.send(Command::Edit { edit: connect, revision: 4 });
    let s = wait_for(&h, |s| s.revision == 4);
    let gi = (0..s.net_count).find(|&n| {
        let (st, e) = (s.net_map.start[n] as usize, s.net_map.start[n + 1] as usize);
        s.net_map.pins[st..e].contains(&(g, 0))
    });
    let gi = gi.unwrap();
    wait_for(&h, |s| s.level(gi) == L1);
    assert!(!s.net_map.pins.iter().any(|&(id, _)| id == b));
}

/// Editing a running circuit (recompilation) keeps flip-flop states and
/// RAM contents; moving a part does not even recompile. Stop resets both.
#[test]
fn edits_keep_sequential_state_and_stop_resets_it() {
    let mut c = Circuit::new();
    let d = add(&mut c, ComponentKind::Switch { on: true });
    let clk = add(&mut c, ComponentKind::Switch { on: false });
    let ff = add(&mut c, ComponentKind::flip_flop(FlipFlopType::D));
    wire(&mut c, d, 0, ff, 0);
    wire(&mut c, clk, 0, ff, 1);
    let ram = add(&mut c, ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 4, data_bits: 4 });
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    h.send(Command::Load { circuit: Arc::new(c), revision: 1 });
    h.send(Command::SetSwitch(clk, true)); // Q = 1
    h.send(Command::Edit { edit: Edit::SetMemoryWord { id: ram, addr: 2, value: 9 }, revision: 2 });
    h.send(Command::Edit { edit: Edit::MoveComponent(ff, GridPos::new(5, 5)), revision: 3 });
    let s = wait_for(&h, |s| s.revision == 3);
    let compile_id = s.compile_id;
    let q_net = |s: &bw_sim::Snapshot| {
        (0..s.net_count)
            .find(|&n| s.net_map.pins[s.net_map.start[n] as usize..s.net_map.start[n + 1] as usize].contains(&(ff, 0)))
    };
    let qn = q_net(&s).unwrap();
    wait_for(&h, |s| s.level(qn) == L1);
    assert_eq!(h.snapshot().compile_id, compile_id, "a move does not recompile");
    // A structural edit recompiles; the state survives.
    let led = Edit::AddComponent { kind: ComponentKind::Led, pos: GridPos::default(), rotation: Rotation::R0 };
    h.send(Command::Edit { edit: led, revision: 4 });
    h.send(Command::WatchMemory(Some(ram)));
    let s = wait_for(&h, |s| s.revision == 4 && s.compile_id > compile_id);
    assert_eq!(s.level(q_net(&s).unwrap()), L1, "flip-flop state kept across recompilation");
    let start = Instant::now();
    while h.memory_view().is_none_or(|v| v.words.get(2) != Some(&9)) {
        assert!(start.elapsed() < Duration::from_secs(5), "memory view not published");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Stop: Q back to 0; the memory keeps its design contents (the edit).
    h.send(Command::Stop);
    let g = s.generation;
    let s = wait_for(&h, |s| s.generation > g && s.state == RunState::Stopped);
    assert_eq!(s.level(q_net(&s).unwrap()), L0);
    assert_eq!(h.memory_view().unwrap().words[2], 9);
}

/// `cargo test --release -p bw-sim -- --ignored --nocapture sim_bench`:
/// ticks per second on a large circuit with flip-flops and memories (512
/// 4-bit JK counters, each addressing a 16 x 8 RAM whose outputs feed an
/// XOR tree, all clocked from one clock).
#[test]
#[ignore = "benchmark; run manually"]
fn sim_bench_flip_flops_and_memory() {
    let mut c = Circuit::new();
    let clk = add(&mut c, ComponentKind::Clock { half_period: 1 });
    let one = add(&mut c, ComponentKind::Switch { on: true });
    let mut rams = vec![];
    for _ in 0..512 {
        let mut prev = clk;
        let mut qs = vec![];
        for _ in 0..4 {
            let ff = add(&mut c, ComponentKind::flip_flop(FlipFlopType::Jk));
            wire(&mut c, one, 0, ff, 0);
            wire(&mut c, prev, 0, ff, 1);
            wire(&mut c, one, 0, ff, 2);
            qs.push(ff);
            prev = ff;
        }
        let ram = add(&mut c, ComponentKind::Memory { ty: MemoryType::Ram, addr_bits: 4, data_bits: 8 });
        c.set_memory(ram, &(0..16).map(|i| i * 37 % 256).collect::<Vec<u16>>()).unwrap();
        for (i, &q) in qs.iter().enumerate() {
            wire(&mut c, q, 0, ram, u8::try_from(i).unwrap());
        }
        let mut level = (0..8).collect::<Vec<u8>>();
        let mut srcs: Vec<(ComponentId, u8)> = level.iter().map(|&k| (ram, k)).collect();
        while srcs.len() > 1 {
            let mut next = vec![];
            for pair in srcs.chunks(2) {
                let x = add(&mut c, ComponentKind::Gate { op: GateOp::Xor, inputs: 2 });
                wire(&mut c, pair[0].0, pair[0].1, x, 0);
                wire(&mut c, pair[1].0, pair[1].1, x, 1);
                next.push((x, 0));
            }
            srcs = next;
        }
        let led = add(&mut c, ComponentKind::Led);
        wire(&mut c, srcs[0].0, srcs[0].1, led, 0);
        level.clear();
        rams.push(ram);
    }
    let t = Instant::now();
    let mut s = sim(&c);
    println!("{} components, {} nets: compile + reset {:?}", c.component_count(), s.netlist().net_count(), t.elapsed());
    let n = 20_000u32;
    let t = Instant::now();
    for _ in 0..n {
        s.tick().unwrap();
    }
    let dt = t.elapsed();
    println!("{n} ticks in {dt:?}: {:.0} ticks/s ({:?} per tick)", f64::from(n) / dt.as_secs_f64(), dt / n);
    assert!(s.memory(rams[0]).is_some());
}

// ---- wire graph: junctions, buses, geometry-only edits ----------------------

/// The segment of `c` whose interior contains `p`.
fn seg_at(c: &Circuit, p: GridPos) -> SegmentId {
    c.segments()
        .map(|(id, _)| id)
        .find(|&id| {
            let (a, b) = c.segment_ends(id).unwrap();
            (a.x == b.x && p.x == a.x && p.y > a.y.min(b.y) && p.y < a.y.max(b.y))
                || (a.y == b.y && p.y == a.y && p.x > a.x.min(b.x) && p.x < a.x.max(b.x))
        })
        .expect("no segment at point")
}

/// One switch fans out to two gates through a junction: both gates see it.
#[test]
fn fanout_through_a_junction_drives_every_branch() {
    let mut c = Circuit::new();
    let sw = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 0), Rotation::R0).unwrap();
    let n1 = c.add_component(ComponentKind::Not, GridPos::new(12, 0), Rotation::R0).unwrap();
    let n2 = c.add_component(ComponentKind::Not, GridPos::new(12, 8), Rotation::R0).unwrap();
    c.connect(pin(sw, 0), pin(n1, 0), vec![]).unwrap();
    let s = seg_at(&c, GridPos::new(6, 1));
    c.apply(&Edit::AddWire {
        from: WireEnd::OnSegment(s, GridPos::new(6, 1)),
        to: WireEnd::Pin(pin(n2, 0)),
        via: vec![GridPos::new(6, 9)],
    })
    .unwrap();
    assert_eq!(c.junction_count(), 1);
    let mut st = sim(&c);
    assert_eq!((out(&st, n1, 0), out(&st, n2, 0)), (L1, L1));
    set(&mut st, sw, L1);
    assert_eq!((out(&st, n1, 0), out(&st, n2, 0)), (L0, L0));
    // Deleting the branch's vertical segment cuts the second NOT off: its
    // input floats (reads X).
    let v = seg_at(&c, GridPos::new(6, 5));
    c.apply(&Edit::RemoveSegment(v)).unwrap();
    let st = sim(&c);
    assert_eq!((out(&st, n1, 0), out(&st, n2, 0)), (L1, X));
}

/// Two tri-state buffers joined by a wire between their output wires form
/// one bus net, resolved like before.
#[test]
fn tristate_bus_through_junctions() {
    let mut c = Circuit::new();
    let (d1, d2, e1, e2) = (
        c.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, 0), Rotation::R0).unwrap(),
        c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 10), Rotation::R0).unwrap(),
        c.add_component(ComponentKind::Switch { on: false }, GridPos::new(4, -6), Rotation::R0).unwrap(),
        c.add_component(ComponentKind::Switch { on: false }, GridPos::new(4, 4), Rotation::R0).unwrap(),
    );
    let t1 = c.add_component(ComponentKind::TriState, GridPos::new(6, 0), Rotation::R0).unwrap();
    let t2 = c.add_component(ComponentKind::TriState, GridPos::new(6, 10), Rotation::R0).unwrap();
    let l1 = c.add_component(ComponentKind::Led, GridPos::new(20, 1), Rotation::R0).unwrap();
    let l2 = c.add_component(ComponentKind::Led, GridPos::new(20, 11), Rotation::R0).unwrap();
    for (d, e, t, l) in [(d1, e1, t1, l1), (d2, e2, t2, l2)] {
        c.connect(pin(d, 0), pin(t, 0), vec![]).unwrap();
        c.connect(pin(e, 0), pin(t, 1), vec![]).unwrap();
        c.connect(pin(t, 2), pin(l, 0), vec![]).unwrap();
    }
    let (a, b) = (seg_at(&c, GridPos::new(15, 2)), seg_at(&c, GridPos::new(15, 12)));
    c.apply(&Edit::AddWire {
        from: WireEnd::OnSegment(a, GridPos::new(15, 2)),
        to: WireEnd::OnSegment(b, GridPos::new(15, 12)),
        via: vec![],
    })
    .unwrap();
    let net = Netlist::compile(&c).unwrap();
    assert_eq!(net.output_net(t1, 0), net.output_net(t2, 0), "one bus net");
    let mut st = SimState::new(net, &c);
    st.reset().unwrap();
    let bus = |st: &SimState| st.values()[st.netlist().output_net(t1, 0).unwrap() as usize];
    assert_eq!(bus(&st), Z, "nobody drives the bus");
    st.set_switch(e1, true).unwrap();
    assert_eq!(bus(&st), L1, "t1 drives 1");
    st.set_switch(e1, false).unwrap();
    st.set_switch(e2, true).unwrap();
    assert_eq!(bus(&st), L0, "t2 drives 0");
    assert!(matches!(st.set_switch(e1, true), Err(SimError::BusContention { .. })));
}

/// Geometry-only wiring edits (moving a bend or junction, dragging a
/// segment) never recompile the simulator; a connectivity edit does.
#[test]
fn moving_a_bend_does_not_recompile() {
    let mut c = Circuit::new();
    let sw = c.add_component(ComponentKind::Switch { on: true }, GridPos::new(0, 0), Rotation::R0).unwrap();
    let led = c.add_component(ComponentKind::Led, GridPos::new(12, 6), Rotation::R0).unwrap();
    c.connect(pin(sw, 0), pin(led, 0), vec![GridPos::new(6, 1), GridPos::new(6, 7)]).unwrap();
    let bend = c.nodes().find(|(_, n)| n.is_free()).unwrap().0;
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    h.send(Command::Load { circuit: Arc::new(c.clone()), revision: 1 });
    let s = wait_for(&h, |s| s.revision == 1 && s.net_count > 0);
    let compile_id = s.compile_id;
    let mut rev = 1;
    for e in [
        Edit::MoveNode(bend, GridPos::new(8, 1)),
        Edit::DragSegment(seg_at(&c, GridPos::new(10, 7)), 0),
        Edit::MoveNode(bend, GridPos::new(9, 1)),
    ] {
        c.apply(&e).unwrap();
        rev += 1;
        h.send(Command::Edit { edit: e, revision: rev });
    }
    let vertical = c.segments().map(|(id, _)| id).find(|&id| {
        let (a, b) = c.segment_ends(id).unwrap();
        a.x == b.x
    });
    let e = Edit::DragSegment(vertical.unwrap(), 2);
    c.apply(&e).unwrap();
    rev += 1;
    h.send(Command::Edit { edit: e, revision: rev });
    let s = wait_for(&h, |s| s.revision == rev);
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(h.snapshot().compile_id, compile_id, "geometry edits do not recompile");
    assert_eq!(s.compile_id, compile_id);
    // Deleting a segment changes connectivity: recompiles.
    let any = c.segments().next().unwrap().0;
    h.send(Command::Edit { edit: Edit::RemoveSegment(any), revision: rev + 1 });
    wait_for(&h, |s| s.revision == rev + 1 && s.compile_id > compile_id);
}

/// The bundled `flipflops-memory` example: the clock fans out through
/// junctions to four JK flip-flops forming a synchronous counter, which
/// counts 0, 1, 2, … on falling clock edges; the tri-state bus shows the
/// selected buffer.
#[test]
fn bundled_counter_example_counts_through_its_junctions() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/flipflops-memory.bitwerk");
    let c = bw_io::load_from_path(&path).unwrap().circuit;
    let ffs: Vec<ComponentId> =
        c.components().filter(|k| matches!(k.kind, ComponentKind::FlipFlop { .. })).map(|k| k.id).collect();
    assert_eq!(ffs.len(), 4);
    let clock = c.components().find(|k| matches!(k.kind, ComponentKind::Clock { .. })).unwrap();
    // The clock net reaches every flip-flop's C input (pin 1).
    let net = c.net_pins(pin(clock.id, 0));
    assert!(ffs.iter().all(|&f| net.contains(&pin(f, 1))), "{net:?}");
    let ComponentKind::Clock { half_period } = clock.kind else { unreachable!() };
    let mut s = sim(&c);
    let value = |s: &SimState| ffs.iter().enumerate().map(|(i, &f)| u32::from(out(s, f, 0) == L1) << i).sum::<u32>();
    let mut seen = vec![value(&s)];
    for _ in 0..(2 * half_period * 20) {
        s.tick().unwrap();
        let v = value(&s);
        if seen.last() != Some(&v) {
            seen.push(v);
        }
    }
    assert_eq!(&seen[..18], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 1], "{seen:?}");
    // Bus: select = 0 enables the second buffer (through the NOT), whose
    // input switch is off.
    let led = c.components().find(|k| k.kind == ComponentKind::Led && k.pos == GridPos::new(24, 23)).unwrap();
    let drivers = c.drivers_of(pin(led.id, 0));
    assert_eq!(drivers.len(), 2);
    let bus = s.netlist().output_net(drivers[0].component, 0).unwrap();
    assert_eq!(s.values()[bus as usize], L0);
}
