#![allow(clippy::unwrap_used, clippy::expect_used, clippy::many_single_char_names)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bw_core::{Circuit, ComponentId, ComponentKind, GateOp, GridPos, PinRef, Rotation};
use bw_sim::{Command, Netlist, RunState, SimError, SimHandle, SimState, net_numbering};

fn pin(component: ComponentId, pin: u8) -> PinRef {
    PinRef { component, pin }
}

fn add(c: &mut Circuit, kind: ComponentKind) -> ComponentId {
    c.add_component(kind, GridPos::default(), Rotation::R0).unwrap()
}

/// Two switches → gate → LED; returns (circuit, a, b, gate).
fn two_input(op: GateOp) -> (Circuit, ComponentId, ComponentId, ComponentId) {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: false });
    let b = add(&mut c, ComponentKind::Switch { on: false });
    let g = add(&mut c, ComponentKind::Gate { op, inputs: 2 });
    let led = add(&mut c, ComponentKind::Led);
    c.connect(pin(a, 0), pin(g, 0), vec![]).unwrap();
    c.connect(pin(b, 0), pin(g, 1), vec![]).unwrap();
    c.connect(pin(g, 2), pin(led, 0), vec![]).unwrap();
    (c, a, b, g)
}

#[test]
fn gate_truth_tables_through_netlist() {
    for op in GateOp::ALL {
        let (c, a, b, g) = two_input(op);
        let nets = net_numbering(&c);
        let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
        s.reset().unwrap();
        for (va, vb) in [(false, false), (false, true), (true, false), (true, true)] {
            s.set_switch(a, va).unwrap();
            s.set_switch(b, vb).unwrap();
            let out = s.values[nets[&g] as usize] != 0;
            assert_eq!(out, op.eval([va, vb]), "{op:?}({va},{vb})");
        }
    }
}

#[test]
fn not_gate_and_three_input() {
    let mut c = Circuit::new();
    let a = add(&mut c, ComponentKind::Switch { on: true });
    let n = add(&mut c, ComponentKind::Not);
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::And, inputs: 3 });
    c.connect(pin(a, 0), pin(n, 0), vec![]).unwrap();
    for i in 0..3 {
        c.connect(pin(a, 0), pin(g, i), vec![]).unwrap();
    }
    let nets = net_numbering(&c);
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    s.reset().unwrap();
    assert_eq!(s.values[nets[&n] as usize], 0);
    assert_eq!(s.values[nets[&g] as usize], 1);
}

#[test]
fn self_feeding_not_is_a_race_condition() {
    let mut c = Circuit::new();
    let n = add(&mut c, ComponentKind::Not);
    c.connect(pin(n, 1), pin(n, 0), vec![]).unwrap();
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    assert!(matches!(s.reset(), Err(SimError::RaceCondition { .. })));
}

#[test]
fn nand_with_feedback_oscillates() {
    // NAND(switch, own output): oscillates once the switch is on.
    let mut c = Circuit::new();
    let sw = add(&mut c, ComponentKind::Switch { on: false });
    let g = add(&mut c, ComponentKind::Gate { op: GateOp::Nand, inputs: 2 });
    c.connect(pin(sw, 0), pin(g, 0), vec![]).unwrap();
    c.connect(pin(g, 2), pin(g, 1), vec![]).unwrap();
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    s.reset().unwrap(); // switch off → output stuck at 1, stable
    assert!(matches!(s.set_switch(sw, true), Err(SimError::RaceCondition { .. })));
}

#[test]
fn sr_latch_settles() {
    // Cross-coupled NOR latch must not be reported as a race.
    let mut c = Circuit::new();
    let set = add(&mut c, ComponentKind::Switch { on: false });
    let rst = add(&mut c, ComponentKind::Switch { on: true });
    let q = add(&mut c, ComponentKind::Gate { op: GateOp::Nor, inputs: 2 });
    let nq = add(&mut c, ComponentKind::Gate { op: GateOp::Nor, inputs: 2 });
    c.connect(pin(rst, 0), pin(q, 0), vec![]).unwrap();
    c.connect(pin(nq, 2), pin(q, 1), vec![]).unwrap();
    c.connect(pin(set, 0), pin(nq, 0), vec![]).unwrap();
    c.connect(pin(q, 2), pin(nq, 1), vec![]).unwrap();
    let nets = net_numbering(&c);
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    s.reset().unwrap();
    s.set_switch(rst, false).unwrap();
    assert_eq!(s.values[nets[&q] as usize], 0);
    s.set_switch(set, true).unwrap();
    s.set_switch(set, false).unwrap();
    assert_eq!(s.values[nets[&q] as usize], 1);
}

#[test]
fn clock_toggles_every_half_period() {
    let mut c = Circuit::new();
    let clk = add(&mut c, ComponentKind::Clock { half_period: 3 });
    let nets = net_numbering(&c);
    let mut s = SimState::new(Netlist::compile(&c).unwrap(), &c);
    s.reset().unwrap();
    let mut levels = vec![];
    for _ in 0..6 {
        s.tick().unwrap();
        levels.push(s.values[nets[&clk] as usize]);
    }
    assert_eq!(levels, vec![0, 0, 1, 1, 1, 0]);
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

#[test]
fn engine_thread_publishes_snapshots_and_halts_on_race() {
    let h = SimHandle::spawn(Box::new(|| {})).unwrap();
    let (c, a, b, g) = two_input(GateOp::And);
    let nets = net_numbering(&c);
    h.send(Command::Load { circuit: Arc::new(c), revision: 1 });
    h.send(Command::SetSwitch(a, true));
    h.send(Command::SetSwitch(b, true));
    let gi = nets[&g] as usize;
    wait_for(&h, |s| s.revision == 1 && s.values.get(gi) == Some(&1));

    let mut osc = Circuit::new();
    let n = add(&mut osc, ComponentKind::Not);
    osc.connect(pin(n, 1), pin(n, 0), vec![]).unwrap();
    h.send(Command::Load { circuit: Arc::new(osc), revision: 2 });
    h.send(Command::Run);
    let s = wait_for(&h, |s| s.revision == 2 && s.state == RunState::Halted);
    assert!(matches!(s.error, Some(SimError::RaceCondition { .. })));
}

#[test]
fn engine_applies_incremental_edits() {
    use bw_core::Edit;
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
    h.send(Command::Edit { edit: Edit::Connect { from: pin(new_sw, 0), to: pin(g, 1), via: vec![] }, revision: 4 });
    let s = wait_for(&h, |s| s.revision == 4);
    let gi = s.net_components.iter().position(|&id| id == g).unwrap();
    let s = wait_for(&h, |s| s.values.get(gi) == Some(&1));
    assert_eq!(s.net_components.len(), s.values.len());
    assert!(!s.net_components.contains(&b));
}
