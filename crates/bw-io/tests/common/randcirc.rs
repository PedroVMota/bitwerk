//! Random circuits over every component kind the model has (shared by the
//! Digital Works export tests of `bw-io` and `bw-sim`).

#![allow(dead_code, clippy::missing_panics_doc, clippy::unwrap_used, clippy::match_same_arms)]

use std::sync::Arc;

use bw_core::{
    Circuit, ComponentId, ComponentKind, DATA_WIDTHS, Edit, FfTrigger, FlipFlopType, GateOp, GridPos, MacroDef,
    MemoryType, Note, NumFormat, PinRef, Rotation, SwitchMode, TagDir,
};

/// Deterministic xorshift64* generator.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n.max(1) as u64).unwrap()
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// Options of a random circuit.
#[derive(Clone, Copy)]
pub struct Opts {
    /// Components besides the sources.
    pub parts: usize,
    /// Any rotation (else unrotated).
    pub rotate: bool,
    /// Only kinds whose behaviour the export keeps exactly (no push
    /// buttons / pulse inputs, clocks at the default speed).
    pub exact: bool,
    /// A definition body: sources are input tags, every free output gets
    /// an output tag.
    pub body: bool,
}

/// A random kind of part (no sources, no macros).
pub fn random_part(r: &mut Rng, o: Opts) -> ComponentKind {
    let ops = GateOp::ALL;
    match r.below(12) {
        0..=2 => ComponentKind::Gate { op: ops[r.below(6)], inputs: 2 + u8::try_from(r.below(7)).unwrap() },
        3 => ComponentKind::Not,
        4 => ComponentKind::TriState,
        5 | 6 => loop {
            let ty = FlipFlopType::ALL[r.below(3)];
            let trigger = FfTrigger::ALL[r.below(4)];
            let k = ComponentKind::FlipFlop { ty, trigger, preset_clear: r.chance(50) };
            if k.is_valid() {
                break k;
            }
        },
        7 => {
            let ty = if r.chance(50) { MemoryType::Ram } else { MemoryType::Rom };
            ComponentKind::Memory {
                ty,
                addr_bits: 4 + u8::try_from(r.below(2)).unwrap(),
                data_bits: DATA_WIDTHS[r.below(4)],
            }
        }
        8 => ComponentKind::SevenSeg { common_anode: r.chance(50) },
        9 => ComponentKind::Numeric {
            bits: 2 + u8::try_from(r.below(15)).unwrap(),
            format: NumFormat::ALL[r.below(3)],
            signed: r.chance(30),
        },
        10 => ComponentKind::Led,
        _ => {
            if o.exact {
                ComponentKind::Led
            } else {
                ComponentKind::Text { w: 1, h: 1 }
            }
        }
    }
}

/// A clock or asynchronous preset / clear input, any input of a latch,
/// a RAM or a macro.
fn timing_pin(kind: ComponentKind, pin: u8) -> bool {
    match kind {
        // A transparent latch follows hazards (glitches) of its data,
        // whose order depends on the evaluation order.
        ComponentKind::FlipFlop { trigger: FfTrigger::Latch, .. } => true,
        ComponentKind::FlipFlop { ty, .. } => pin == FlipFlopType::CLOCK_INPUT || pin >= ty.sync_inputs(),
        // A RAM written while its address or data change races too.
        ComponentKind::Memory { ty: MemoryType::Ram, .. } => true,
        // Inside a macro any input may be one of these.
        ComponentKind::Macro(_) => true,
        _ => false,
    }
}

fn source(r: &mut Rng, o: Opts) -> ComponentKind {
    if o.body {
        return ComponentKind::Tag { dir: TagDir::In, tristate: false };
    }
    match r.below(if o.exact { 5 } else { 7 }) {
        0 | 1 => ComponentKind::Switch { on: r.chance(50), mode: SwitchMode::Toggle },
        2 => ComponentKind::Clock { half_period: if o.exact { 10 } else { 1 + u32::try_from(r.below(30)).unwrap() } },
        3 => {
            let len = 1 + u8::try_from(r.below(12)).unwrap();
            ComponentKind::SeqGen {
                pattern: r.next() & ((1u64 << len) - 1),
                len,
                half_period: if o.exact { 10 } else { 1 + u32::try_from(r.below(30)).unwrap() },
            }
        }
        4 => ComponentKind::Const { high: r.chance(50) },
        5 => ComponentKind::Switch { on: r.chance(50), mode: SwitchMode::Momentary },
        _ => ComponentKind::Switch { on: r.chance(50), mode: SwitchMode::Pulse },
    }
}

fn rotation(r: &mut Rng, o: Opts) -> Rotation {
    if o.rotate { [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270][r.below(4)] } else { Rotation::R0 }
}

/// Places a component in cell `k` of a loose grid.
fn place(c: &mut Circuit, kind: ComponentKind, k: usize, rot: Rotation) -> Option<ComponentId> {
    let x = i32::try_from(k % 8).unwrap() * 40;
    let y = i32::try_from(k / 8).unwrap() * 40;
    c.add_component(kind, GridPos::new(x, y), rot).ok()
}

/// A random feed-forward circuit: sources, then parts whose inputs are
/// wired to earlier outputs (so it never oscillates), with optional
/// macro instances of `defs` (definition id, inputs, outputs).
pub fn random_circuit(r: &mut Rng, o: Opts, defs: &[(bw_core::MacroId, ComponentKind)]) -> Circuit {
    let mut c = if o.body { Circuit::new_body() } else { Circuit::new() };
    fill(r, &mut c, o, defs);
    c
}

/// Adds random parts to `c` (see [`random_circuit`]).
pub fn fill(r: &mut Rng, c: &mut Circuit, o: Opts, defs: &[(bw_core::MacroId, ComponentKind)]) {
    if !o.body {
        c.set_eval_mode(bw_core::EvalMode::Sequential);
    }
    let mut outs: Vec<PinRef> = Vec::new();
    let mut cell = 0;
    for _ in 0..3 + r.below(4) {
        let kind = source(r, o);
        if let Some(id) = place(c, kind, cell, rotation(r, o)) {
            outs.push(PinRef { component: id, pin: kind.input_count() });
        }
        cell += 1;
    }
    for _ in 0..o.parts {
        let kind = if !defs.is_empty() && r.chance(20) { defs[r.below(defs.len())].1 } else { random_part(r, o) };
        // Digital Works cannot rotate a 7-segment display (its pins move on
        // export): in exact mode keep it upright so it can be matched.
        let rot = if o.exact && matches!(kind, ComponentKind::SevenSeg { .. }) { Rotation::R0 } else { rotation(r, o) };
        let Some(id) = place(c, kind, cell, rot) else { continue };
        cell += 1;
        if let ComponentKind::Text { .. } = kind {
            let _ = c.set_note(id, Note::new(["A", "Bus\nline 2", "Grüße €", "x̄ ✓"][r.below(4)]));
            continue;
        }
        if let ComponentKind::Memory { data_bits, .. } = kind {
            let n = kind.memory_words();
            let mask = bw_core::memenc::width_mask(data_bits);
            let words: Vec<u16> = (0..n).map(|_| u16::try_from(r.next() & 0xffff).unwrap() & mask).collect();
            c.set_memory(id, &words).unwrap();
        }
        for pin in 0..kind.input_count() {
            if o.exact && timing_pin(kind, pin) {
                // Zero-delay simulation races when a clock (or a RAM's
                // write strobe) changes together with the data: in exact
                // mode such pins get a source of their own.
                let src = if o.body {
                    ComponentKind::Tag { dir: TagDir::In, tristate: false }
                } else {
                    ComponentKind::Switch { on: false, mode: SwitchMode::Toggle }
                };
                if let Some(s) = place(c, src, 2000 + cell * 4 + usize::from(pin), Rotation::R0) {
                    let _ = c.connect(PinRef { component: s, pin: 0 }, PinRef { component: id, pin }, vec![]);
                }
                continue;
            }
            if outs.is_empty() || (!o.exact && r.chance(10)) {
                continue;
            }
            let from = outs[r.below(outs.len())];
            let _ = c.connect(from, PinRef { component: id, pin }, vec![]);
        }
        // Floating (Z) and unknown levels propagate differently through
        // the helper gates of an export (Digital Works is two-valued; a
        // RAM's data bus carries the written value while writing): in
        // exact mode tri-state outputs are only observed, and every input
        // is driven.
        let observe_only = o.exact && kind.tristate_mask() != 0;
        for k in 0..kind.output_count() {
            let p = PinRef { component: id, pin: kind.input_count() + k };
            if observe_only {
                if let Some(l) = place(c, ComponentKind::Led, 1000 + cell * 20 + usize::from(k), Rotation::R0) {
                    let _ = c.connect(p, PinRef { component: l, pin: 0 }, vec![]);
                }
            } else {
                outs.push(p);
            }
        }
    }
    // Observe every output (an LED, or an output tag in a body).
    for (k, &p) in outs.clone().iter().enumerate() {
        if c.net_pins(p).len() > 1 && r.chance(50) {
            continue;
        }
        let kind = if o.body { ComponentKind::Tag { dir: TagDir::Out, tristate: false } } else { ComponentKind::Led };
        if let Some(id) = place(c, kind, cell + k, Rotation::R0) {
            let _ = c.connect(p, PinRef { component: id, pin: 0 }, vec![]);
        }
    }
}

/// Defines a random macro in `c` (nested in the earlier ones) and returns
/// its id and instance kind.
pub fn random_macro(
    r: &mut Rng,
    c: &mut Circuit,
    o: Opts,
    name: &str,
    defs: &[(bw_core::MacroId, ComponentKind)],
) -> Option<(bw_core::MacroId, ComponentKind)> {
    let parts = 6 + r.below(8);
    let body = random_circuit(r, Opts { body: true, rotate: o.rotate, parts, exact: o.exact }, defs);
    let mut def = MacroDef::new(name);
    def.body = body;
    def.sync_pins();
    if r.chance(30) {
        def.package = bw_core::Package::Dil(28);
    }
    if def.validate().is_err() {
        def.package = bw_core::Package::Auto;
    }
    def.datasheet = "Random test macro.".into();
    let id = c.macros().free_id()?;
    c.apply(&Edit::DefineMacro { id, def: Arc::new(def) }).ok()?;
    let shape = c.macros().get(id)?.shape;
    Some((id, ComponentKind::Macro(shape)))
}

/// A random project circuit with a few (nested) macros.
pub fn random_project(seed: u64, o: Opts) -> Circuit {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut c = Circuit::new();
    let mut defs = Vec::new();
    for k in 0..r.below(4) {
        if let Some(d) = random_macro(&mut r, &mut c, o, &format!("M{k}"), &defs) {
            defs.push(d);
        }
    }
    fill(&mut r, &mut c, o, &defs);
    c
}
