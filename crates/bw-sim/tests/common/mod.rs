//! Helpers shared by the macro / library tests.

#![allow(dead_code, clippy::missing_panics_doc, clippy::unwrap_used)]

use std::sync::Arc;

use bw_core::{
    Circuit, ComponentId, ComponentKind, Edit, GridPos, MacroDef, MacroId, MacroTable, PinRef, Rotation, WireEnd,
};
use bw_sim::{Netlist, SimState};

/// Imports definition `root` of `source` into `c` (with everything it
/// uses) and returns its id in `c`.
pub fn import(c: &mut Circuit, source: &MacroTable, root: MacroId) -> MacroId {
    let (defs, id) = c.macros().import_plan(source, root).unwrap();
    for (id, def) in defs {
        c.apply(&Edit::DefineMacro { id, def }).unwrap();
    }
    id
}

/// Stores `def` under a fresh id of `c`.
pub fn define(c: &mut Circuit, def: MacroDef) -> MacroId {
    let id = c.macros().free_id().unwrap();
    c.apply(&Edit::DefineMacro { id, def: Arc::new(def) }).unwrap();
    id
}

/// Places an instance of `id`.
pub fn place(c: &mut Circuit, id: MacroId, at: GridPos) -> ComponentId {
    let shape = c.macros().get(id).unwrap().shape;
    c.add_component(ComponentKind::Macro(shape), at, Rotation::R0).unwrap()
}

/// A test bench: one instance of a macro with a switch on every input.
pub struct Bench {
    pub circuit: Circuit,
    pub inst: ComponentId,
    pub switches: Vec<ComponentId>,
    pub sim: SimState,
    pub outputs: u8,
}

impl Bench {
    pub fn new(source: &MacroTable, root: MacroId) -> Self {
        let mut c = Circuit::new();
        let id = import(&mut c, source, root);
        let shape = c.macros().get(id).unwrap().shape;
        let inst = place(&mut c, id, GridPos::new(40, 0));
        let mut switches = Vec::new();
        for k in 0..shape.inputs {
            let s = c
                .add_component(
                    ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
                    GridPos::new(0, 4 * i32::from(k)),
                    Rotation::R0,
                )
                .unwrap();
            c.apply(&Edit::AddWire {
                from: WireEnd::Pin(PinRef { component: s, pin: 0 }),
                to: WireEnd::Pin(PinRef { component: inst, pin: k }),
                via: vec![],
            })
            .unwrap();
            switches.push(s);
        }
        let net = Netlist::compile(&c).unwrap();
        let mut sim = SimState::new(net, &c);
        sim.reset().unwrap();
        Self { circuit: c, inst, switches, sim, outputs: shape.outputs }
    }

    pub fn set(&mut self, input: usize, on: bool) {
        self.sim.set_switch(self.switches[input], on).unwrap();
    }

    /// Sets inputs `first..first + n` to the bits of `v` (LSB first).
    pub fn set_bits(&mut self, first: usize, n: usize, v: u32) {
        for k in 0..n {
            self.set(first + k, v >> k & 1 == 1);
        }
    }

    /// Rising then falling edge on input `clk`.
    pub fn pulse(&mut self, clk: usize) {
        self.set(clk, true);
        self.set(clk, false);
    }

    /// Level of output `k` (0, 1, 2 = Z, 3 = X).
    pub fn out(&self, k: u8) -> u8 {
        self.sim.output(self.inst, k).unwrap_or(2)
    }

    /// Outputs `first..first + n` as a number (LSB first); panics on Z/X.
    pub fn bits(&self, first: u8, n: u8) -> u32 {
        (0..n).fold(0, |acc, k| {
            let v = self.out(first + k);
            assert!(v <= 1, "output {} is {v} (not 0/1)", first + k);
            acc | u32::from(v) << k
        })
    }
}
