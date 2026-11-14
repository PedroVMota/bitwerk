//! Shared helpers: a bench with a switch on every input of one macro, and a
//! small deterministic random generator.

#![allow(dead_code, clippy::missing_panics_doc, clippy::unwrap_used)]

use bw_core::{ComponentId, ComponentKind, Edit, GridPos, PinRef, Rotation, SwitchMode, WireEnd};
use bw_sim::{Netlist, SimState};

/// xorshift64* (deterministic, no dependencies).
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn u128(&mut self) -> u128 {
        u128::from(self.next()) << 64 | u128::from(self.next())
    }

    /// Mostly uniform, sometimes an edge case (0, all ones, carries).
    pub fn operand(&mut self) -> u128 {
        match self.next() % 8 {
            0 => 0,
            1 => u128::MAX,
            2 => u128::MAX >> (self.next() % 128),
            3 => 1u128 << (self.next() % 128),
            _ => self.u128(),
        }
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One instance of a W-128 macro with a switch on every input.
pub struct Bench {
    pub sim: SimState,
    pub inst: ComponentId,
    pub switches: Vec<ComponentId>,
    pub inputs: u8,
    pub outputs: u8,
}

impl Bench {
    pub fn new(key: &str) -> Self {
        let mut lib = bw_w128::blocks::library();
        let kind = lib.kind(key);
        let c = &mut lib.c;
        let inst = c.add_component(kind, GridPos::new(40, 0), Rotation::R0).unwrap();
        let (inputs, outputs) = (kind.input_count(), kind.output_count());
        let mut switches = Vec::new();
        for k in 0..inputs {
            let s = c
                .add_component(
                    ComponentKind::Switch { on: false, mode: SwitchMode::Toggle },
                    GridPos::new(0, 2 * i32::from(k)),
                    Rotation::R0,
                )
                .unwrap();
            c.apply(&Edit::AddWire {
                from: WireEnd::Pin(PinRef { component: s, pin: 0 }),
                to: WireEnd::Pin(PinRef { component: inst, pin: k }),
                via: vec![GridPos::new(20 + i32::from(k % 16), 2 * i32::from(k) + 1)],
            })
            .unwrap();
            switches.push(s);
        }
        let net = Netlist::compile(c).unwrap();
        let mut sim = SimState::new(net, c);
        sim.reset().unwrap();
        Self { sim, inst, switches, inputs, outputs }
    }

    pub fn set(&mut self, input: usize, on: bool) {
        self.sim.set_switch(self.switches[input], on).unwrap();
    }

    /// Inputs `first..first + n` = bits of `v` (LSB first).
    pub fn set_bits(&mut self, first: usize, n: usize, v: u128) {
        for k in 0..n {
            self.set(first + k, v >> k & 1 == 1);
        }
    }

    pub fn pulse(&mut self, clk: usize) {
        self.set(clk, true);
        self.set(clk, false);
    }

    /// Level of output `k` (0, 1, 2 = Z, 3 = X).
    pub fn out(&self, k: u8) -> u8 {
        self.sim.output(self.inst, k).unwrap_or(2)
    }

    /// Outputs `first..first + n` as a number; panics on Z / X.
    pub fn bits(&self, first: u8, n: u8) -> u128 {
        (0..n).fold(0, |acc, k| {
            let v = self.out(first + k);
            assert!(v <= 1, "output {} is {v}", first + k);
            acc | u128::from(v) << k
        })
    }
}
