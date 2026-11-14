//! Statistics of the generated project and a timed run of the demo.

use std::fmt::Write as _;
use std::time::Instant;

use bw_core::{Circuit, ComponentKind};

use crate::isa::Machine;
use crate::run::Board;

fn walk(c: &Circuit, body: &Circuit, segs: &mut u64, comps: &mut u64, mult: u64) {
    *segs += mult * body.segment_count() as u64;
    *comps += mult * body.component_count() as u64;
    for comp in body.components() {
        if let ComponentKind::Macro(s) = comp.kind {
            if let Some(e) = c.macros().get(s.def) {
                walk(c, &e.def.body, segs, comps, mult);
            }
        }
    }
}

fn count(c: &Circuit, body: &Circuit, kinds: &mut std::collections::BTreeMap<&'static str, u64>) {
    for comp in body.components() {
        match comp.kind {
            ComponentKind::Macro(s) => {
                if let Some(e) = c.macros().get(s.def) {
                    count(c, &e.def.body, kinds);
                }
            }
            k if k.is_primitive() => *kinds.entry(k.label()).or_default() += 1,
            _ => {}
        }
    }
}

/// Counts and (with `run`) a simulated run of the demo, as text.
///
/// # Errors
/// If the project does not load, compile or match the reference model.
pub fn report(bytes: &[u8], run: bool) -> Result<String, Box<dyn std::error::Error>> {
    let mut out = String::new();
    let t0 = Instant::now();
    let p = bw_io::load_from_bytes(bytes)?;
    let load_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let c = p.circuit;
    let t = c.macros();
    let prims = c.flat_size();
    let depth = t.iter().map(|(_, e)| e.depth).max().unwrap_or(0);
    let (mut segs, mut comps) = (0u64, 0u64);
    walk(&c, &c, &mut segs, &mut comps, 1);
    let mut kinds: std::collections::BTreeMap<&'static str, u64> = std::collections::BTreeMap::new();
    count(&c, &c, &mut kinds);
    writeln!(out, "load: {load_ms:.1} ms ({} bytes)", bytes.len())?;
    writeln!(out, "macro definitions: {}, max depth below the board: {depth}", t.len())?;
    writeln!(out, "root: {} components, {} wire segments", c.component_count(), c.segment_count())?;
    writeln!(out, "flattened primitives: {prims}")?;
    writeln!(out, "flattened components incl. tags/instances/notes: {comps}, wire segments (all instances): {segs}")?;
    writeln!(
        out,
        "definition bodies: {} wire segments in all",
        t.iter().map(|(_, e)| e.def.body.segment_count()).sum::<usize>()
    )?;
    let list: Vec<String> = kinds.iter().map(|(k, n)| format!("{k} {n}")).collect();
    writeln!(out, "primitives by kind: {}", list.join(", "))?;
    let (_, ids, stats) = crate::board::demo_board()?;
    let lanes = |rank: usize| ids.ram[rank].iter().map(|c| c.0.to_string()).collect::<Vec<_>>().join(" ");
    writeln!(
        out,
        "board ids: CU {}, slices 0-3 {} {} {} {}, ROM lo/hi {} {}, RAM rank 0 lanes 0-7: {}, rank 1: {}, OUT latches from {}",
        ids.cu.0,
        ids.slices[0].0,
        ids.slices[1].0,
        ids.slices[2].0,
        ids.slices[3].0,
        ids.rom_lo.0,
        ids.rom_hi.0,
        lanes(0),
        lanes(1),
        ids.out_latches[0].0
    )?;
    writeln!(out, "layout: {} track-order conflicts", stats.conflicts)?;
    let t1 = Instant::now();
    let net = bw_sim::Netlist::compile(&c)?;
    writeln!(
        out,
        "netlist: {} nodes, {} nets, compiled in {:.1} ms",
        net.node_count(),
        net.net_count(),
        t1.elapsed().as_secs_f64() * 1000.0
    )?;
    if run {
        let prog = crate::asm::assemble(crate::program::DEMO)?;
        let mut m = Machine::new(&prog.words, &[]);
        m.run(1_000_000)?;
        let mut b = Board::new(c, ids)?;
        let t2 = Instant::now();
        let halted = b.run_to_halt(m.cycles + 10)?;
        let dt = t2.elapsed().as_secs_f64();
        writeln!(
            out,
            "demo: {} instructions, {} cycles (model), halted = {halted} after {} cycles / {} ticks in {:.2} s ({:.1} us/tick, {:.0} ticks/s)",
            m.instructions,
            m.cycles,
            b.cycles,
            b.ticks,
            dt,
            dt * 1e6 / f64::from(u32::try_from(b.ticks).unwrap_or(u32::MAX)),
            f64::from(u32::try_from(b.ticks).unwrap_or(u32::MAX)) / dt
        )?;
        let ram_ok = (0..crate::isa::RAM_WORDS).all(|a| b.ram(a) == m.ram[a]);
        writeln!(
            out,
            "check: RAM {}, OUT {:#034x} ({}), cycles {}",
            if ram_ok { "matches" } else { "DIFFERS" },
            b.out_register().unwrap_or(0),
            if b.out_register() == Some(m.out) { "matches" } else { "DIFFERS" },
            if b.cycles == m.cycles { "match" } else { "DIFFER" }
        )?;
    }
    Ok(out)
}
