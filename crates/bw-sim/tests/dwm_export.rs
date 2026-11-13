//! Digital Works export keeps the behaviour: every fixture and every
//! bundled example is exported, imported again, and both circuits are
//! simulated side by side (Digital Works compatible mode) with the same
//! switch changes; every LED, 7-segment and numeric input must agree at
//! every step (where the original is Z or X, the two-valued export may
//! show either level). Components are matched by the position of their first pin,
//! which the export keeps.

#![allow(
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::many_single_char_names,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bw_core::{Circuit, ComponentId, ComponentKind, EvalMode, GridPos, PinRef};
use bw_io::dwm;
use bw_sim::{Netlist, SimState};

fn files(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .collect();
    v.sort();
    v
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Components of a kind keyed by the position of pin 0.
fn by_pin(c: &Circuit, pred: impl Fn(&ComponentKind) -> bool) -> BTreeMap<(i32, i32), ComponentId> {
    c.components().filter(|k| pred(&k.kind)).filter_map(|k| k.pin_pos(0).map(|p: GridPos| ((p.x, p.y), k.id))).collect()
}

fn level(c: &Circuit, s: &SimState, pin: PinRef) -> u8 {
    let Some(d) = c.drivers_of(pin).first().copied() else { return 0 };
    let comp = c.component(d.component).unwrap();
    s.output(d.component, d.pin - comp.kind.input_count()).unwrap_or(2)
}

fn is_display(k: &ComponentKind) -> bool {
    matches!(k, ComponentKind::Led | ComponentKind::SevenSeg { .. } | ComponentKind::Numeric { .. })
}

/// Levels at every display input, by display position.
fn observe(c: &Circuit, s: &SimState) -> BTreeMap<(i32, i32), Vec<u8>> {
    by_pin(c, is_display)
        .into_iter()
        .map(|(k, id)| {
            let n = c.component(id).unwrap().kind.input_count();
            (k, (0..n).map(|pin| level(c, s, PinRef { component: id, pin })).collect())
        })
        .collect()
}

/// Simulates both circuits with the same deterministic switch changes.
fn same_behaviour(name: &str, a: &Circuit, b: &Circuit, steps: usize) {
    let sa = by_pin(a, |k| matches!(k, ComponentKind::Switch { .. }));
    let sb = by_pin(b, |k| matches!(k, ComponentKind::Switch { .. }));
    assert_eq!(sa.keys().collect::<Vec<_>>(), sb.keys().collect::<Vec<_>>(), "{name}: switch positions");
    assert_eq!(by_pin(a, is_display).len(), by_pin(b, is_display).len(), "{name}: displays");
    let mut x = SimState::new(Netlist::compile(a).unwrap(), a);
    let mut y = SimState::new(Netlist::compile(b).unwrap(), b);
    let (rx, ry) = (x.reset(), y.reset());
    assert_eq!(rx.is_ok(), ry.is_ok(), "{name}: reset {rx:?} / {ry:?}");
    if rx.is_err() {
        return;
    }
    let keys: Vec<(i32, i32)> = sa.keys().copied().collect();
    let mut on: BTreeMap<(i32, i32), bool> = BTreeMap::new();
    let mut rng = 0x2545_f491_u32;
    for step in 0..steps {
        if !keys.is_empty() {
            rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let k = keys[(rng >> 16) as usize % keys.len()];
            let v = !on
                .get(&k)
                .copied()
                .unwrap_or_else(|| matches!(a.component(sa[&k]).unwrap().kind, ComponentKind::Switch { on: true, .. }));
            on.insert(k, v);
            let (r1, r2) = (x.set_switch(sa[&k], v), y.set_switch(sb[&k], v));
            assert_eq!(r1.is_ok(), r2.is_ok(), "{name} step {step}: {r1:?} / {r2:?}");
            if r1.is_err() {
                return;
            }
        }
        for _ in 0..7 {
            let (r1, r2) = (x.tick(), y.tick());
            assert_eq!(r1.is_ok(), r2.is_ok(), "{name} step {step}: {r1:?} / {r2:?}");
            if r1.is_err() {
                return;
            }
        }
        // Where the original floats or is unknown (Z / X), Digital Works,
        // being two-valued, may show anything (e.g. a RAM's data bus
        // carries the written value while writing).
        let (oa, ob) = (observe(a, &x), observe(b, &y));
        assert_eq!(oa.keys().collect::<Vec<_>>(), ob.keys().collect::<Vec<_>>(), "{name}: displays");
        for (k, va) in &oa {
            let vb = &ob[k];
            let same = va.len() == vb.len() && va.iter().zip(vb).all(|(p, q)| *p > 1 || p == q);
            assert!(same, "{name}: step {step}: display at {k:?}: {va:?} vs {vb:?}\n{oa:?}\n{ob:?}");
        }
    }
}

fn exported(name: &str, p: &bw_io::Project) -> Circuit {
    let (bytes, _) = dwm::export_project(p, name).unwrap();
    dwm::import_project(&bytes, name).unwrap().0.circuit
}

#[test]
fn fixtures_behave_the_same_after_export() {
    for path in files(&root().join("crates/bw-io/tests/fixtures/dwm"), "dwm") {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let (a, _) = dwm::import_project(&std::fs::read(&path).unwrap(), &name).unwrap();
        let b = exported(&name, &a);
        same_behaviour(&name, &a.circuit, &b, 120);
    }
}

#[test]
fn examples_behave_the_same_after_export() {
    for path in files(&root().join("examples"), "bitwerk") {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut a = bw_io::load_from_path(&path).unwrap();
        // Digital Works semantics on both sides.
        a.circuit.set_eval_mode(EvalMode::Sequential);
        let b = exported(&name, &a);
        same_behaviour(&name, &a.circuit, &b, 120);
    }
}

#[path = "../../bw-io/tests/common/randcirc.rs"]
mod randcirc;

/// Random feed-forward circuits over every kind (wide gates, every
/// flip-flop trigger, latches, RAM / ROM, displays, nested macros, any
/// rotation) behave the same after export and re-import.
#[test]
fn random_circuits_behave_the_same_after_export() {
    let mut failures = Vec::new();
    for seed in 0..80u64 {
        let o = randcirc::Opts { parts: 8 + (seed as usize % 20), rotate: seed % 2 == 1, exact: true, body: false };
        let c = randcirc::random_project(seed, o);
        let p = bw_io::Project { name: format!("random {seed}"), circuit: c };
        let b = exported("r.dwm", &p);
        let name = format!("seed {seed}");
        if let Err(e) = std::panic::catch_unwind(|| same_behaviour(&name, &p.circuit, &b, 60)) {
            failures.push(e.downcast_ref::<String>().cloned().unwrap_or_default());
        }
    }
    for f in &failures {
        eprintln!("{f}\n");
    }
    assert!(failures.is_empty(), "{} of 80 random circuits differ", failures.len());
}

/// A bench around one definition: switches on the inputs, LEDs on the
/// outputs.
fn bench(table: &bw_core::MacroTable, id: bw_core::MacroId) -> Circuit {
    use bw_core::{Edit, Rotation};
    let mut c = Circuit::new();
    c.set_eval_mode(EvalMode::Sequential);
    let (defs, nid) = c.macros().import_plan(table, id).unwrap();
    for (i, d) in defs {
        c.apply(&Edit::DefineMacro { id: i, def: d }).unwrap();
    }
    let shape = c.macros().get(nid).unwrap().shape;
    let inst = c.add_component(ComponentKind::Macro(shape), GridPos::new(40, 0), Rotation::R0).unwrap();
    for k in 0..shape.inputs + shape.outputs {
        let y = 6 * i32::from(k);
        if k < shape.inputs {
            let s = c
                .add_component(
                    ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
                    GridPos::new(0, y),
                    Rotation::R0,
                )
                .unwrap();
            c.connect(PinRef { component: s, pin: 0 }, PinRef { component: inst, pin: k }, vec![]).unwrap();
        } else {
            let l = c.add_component(ComponentKind::Led, GridPos::new(120, y), Rotation::R0).unwrap();
            c.connect(PinRef { component: inst, pin: k }, PinRef { component: l, pin: 0 }, vec![]).unwrap();
        }
    }
    c
}

/// Every kind (every flip-flop variant, RAM / ROM, wide gates, …), in
/// every rotation, alone on a bench of switches and LEDs, behaves the
/// same after export.
#[test]
fn every_kind_behaves_the_same_after_export() {
    use bw_core::{FfTrigger, FlipFlopType, GateOp, MemoryType, Rotation};
    let mut kinds = vec![ComponentKind::Not, ComponentKind::TriState];
    for op in GateOp::ALL {
        for inputs in 2..=8 {
            kinds.push(ComponentKind::Gate { op, inputs });
        }
    }
    for ty in FlipFlopType::ALL {
        for trigger in FfTrigger::ALL {
            for preset_clear in [false, true] {
                let k = ComponentKind::FlipFlop { ty, trigger, preset_clear };
                if k.is_valid() {
                    kinds.push(k);
                }
            }
        }
    }
    for ty in [MemoryType::Ram, MemoryType::Rom] {
        kinds.push(ComponentKind::Memory { ty, addr_bits: 4, data_bits: 2 });
    }
    let mut failures = Vec::new();
    for kind in kinds {
        for rot in [Rotation::R0, Rotation::R90, Rotation::R180, Rotation::R270] {
            let mut c = Circuit::new();
            c.set_eval_mode(EvalMode::Sequential);
            let id = c.add_component(kind, GridPos::new(30, 30), rot).unwrap();
            for k in 0..kind.pin_count() {
                let y = 6 * i32::from(k);
                if k < kind.input_count() {
                    let s = c
                        .add_component(
                            ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle },
                            GridPos::new(0, y),
                            Rotation::R0,
                        )
                        .unwrap();
                    c.connect(PinRef { component: s, pin: 0 }, PinRef { component: id, pin: k }, vec![]).unwrap();
                } else {
                    let l = c.add_component(ComponentKind::Led, GridPos::new(80, y), Rotation::R0).unwrap();
                    c.connect(PinRef { component: id, pin: k }, PinRef { component: l, pin: 0 }, vec![]).unwrap();
                }
            }
            let p = bw_io::Project { name: "k".into(), circuit: c };
            let b = exported("k.dwm", &p);
            let name = format!("{kind:?} {rot:?}");
            if let Err(e) = std::panic::catch_unwind(|| same_behaviour(&name, &p.circuit, &b, 150)) {
                failures.push(e.downcast_ref::<String>().cloned().unwrap_or_default());
            }
        }
    }
    for f in &failures {
        eprintln!("{}\n", f.lines().next().unwrap_or(""));
    }
    assert!(failures.is_empty(), "{} kinds differ", failures.len());
}

/// Random (nested) macro definitions exported as Digital Works macros and
/// imported back behave the same on a bench of switches and LEDs.
#[test]
fn random_macros_behave_the_same_after_macro_export() {
    let mut n = 0;
    for seed in 0..40u64 {
        let o = randcirc::Opts { parts: 4, rotate: seed % 2 == 1, exact: true, body: false };
        let c = randcirc::random_project(seed, o);
        for (id, e) in c.macros().iter() {
            let (bytes, _) = dwm::export_macro(c.macros(), id, "m.dwm").unwrap();
            let (m, _) = dwm::import_macro(&bytes, "m.dwm").unwrap();
            let a = bench(c.macros(), id);
            let b = bench(&m.table, m.root);
            same_behaviour(&format!("seed {seed} macro {}", e.def.name), &a, &b, 40);
            n += 1;
        }
    }
    assert!(n > 20, "{n} macros");
}
