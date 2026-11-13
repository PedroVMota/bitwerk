//! The built-in macro library: its generator and the behaviour of every
//! part.
//!
//! The built-in parts are `.bwmacro` files in `crates/bw-io/library/`,
//! embedded in the binary. They are *generated* by the code below (original
//! circuits built from bitwerk primitives and laid out by a small maze
//! router), so they are reproducible: `files_are_up_to_date` fails when a
//! file differs from what the generator produces. To rewrite them:
//!
//! ```sh
//! BITWERK_REGEN_LIBRARY=1 cargo test -p bw-sim --test library
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::needless_range_loop,
    clippy::unnecessary_cast
)]

mod common;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;

use bw_core::{
    Circuit, ComponentId, ComponentKind, Edit, FfTrigger, FlipFlopType, GateOp, GridPos, MacroDef, MacroId, MacroPin,
    Package, PinRef, PinSide, Rotation, TagDir, WireEnd,
};
use common::Bench;

// ---- layout: a small grid maze router ------------------------------------

type Pt = (i32, i32);
const DIRS: [Pt; 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

fn add(p: Pt, d: usize) -> Pt {
    (p.0 + DIRS[d].0, p.1 + DIRS[d].1)
}

fn reverse(d: usize) -> usize {
    d ^ 1
}

fn horizontal(d: usize) -> bool {
    d < 2
}

/// Router state: which net uses which grid edges and points.
#[derive(Default)]
struct Grid {
    blocked: HashSet<Pt>,
    /// Unit edges (lower point, horizontal?) → net.
    edges: HashMap<(Pt, bool), usize>,
    /// Point → (net, flags: 1 horizontal edge, 2 vertical edge, 4 node).
    points: HashMap<Pt, Vec<(usize, u8)>>,
    lo: Pt,
    hi: Pt,
}

impl Grid {
    fn edge_key(p: Pt, q: Pt) -> (Pt, bool) {
        (if p < q { p } else { q }, p.1 == q.1)
    }

    fn mark(&mut self, p: Pt, net: usize, flag: u8) {
        let v = self.points.entry(p).or_default();
        if let Some(e) = v.iter_mut().find(|e| e.0 == net) {
            e.1 |= flag;
        } else {
            v.push((net, flag));
        }
    }

    /// Other nets at `p` that a wire moving in direction `d` could cross
    /// (straight through): `Some(crossing?)`, or `None` if it may not enter.
    fn enter(&self, p: Pt, net: usize, d: usize) -> Option<bool> {
        let Some(v) = self.points.get(&p) else { return Some(false) };
        let mut crossing = false;
        for &(n, f) in v {
            if n == net {
                return None;
            }
            let same = if horizontal(d) { f & 1 != 0 } else { f & 2 != 0 };
            if f & 4 != 0 || same {
                return None;
            }
            crossing = true;
        }
        Some(crossing)
    }
}

fn outward(c: &Circuit, pin: PinRef) -> usize {
    let comp = c.component(pin.component).unwrap();
    match comp.kind.pin_side(pin.pin).unwrap() {
        PinSide::Left => 1,
        PinSide::Right => 0,
        PinSide::Top => 3,
        PinSide::Bottom => 2,
    }
}

fn pos(c: &Circuit, pin: PinRef) -> Pt {
    let p = c.component(pin.component).unwrap().pin_pos(pin.pin).unwrap();
    (p.x, p.y)
}

/// Routes `nets` (driver pin, reader pins) on body `c`, adding the wires.
/// A net that cannot be routed is moved to the front and everything is
/// routed again (rip-up and retry).
fn route_all(c: &mut Circuit, nets: &[(PinRef, Vec<PinRef>)], name: &str) {
    let original = c.clone();
    let mut order: Vec<usize> = (0..nets.len()).collect();
    for _attempt in 0..64 {
        *c = original.clone();
        match try_route(c, nets, &order) {
            Ok(()) => return,
            Err(net) => {
                let i = order.iter().position(|&n| n == net).unwrap();
                assert!(i > 0, "{name}: net {net} cannot be routed even first");
                order.remove(i);
                order.insert(0, net);
            }
        }
    }
    panic!("{name}: routing did not converge");
}

fn try_route(c: &mut Circuit, nets: &[(PinRef, Vec<PinRef>)], order: &[usize]) -> Result<(), usize> {
    let mut g = Grid { lo: (i32::MAX, i32::MAX), hi: (i32::MIN, i32::MIN), ..Grid::default() };
    for comp in c.components() {
        let b = comp.bounds();
        for x in b.min.x..=b.max.x {
            for y in b.min.y..=b.max.y {
                g.blocked.insert((x, y));
            }
        }
        g.lo = (g.lo.0.min(b.min.x - 8), g.lo.1.min(b.min.y - 8));
        g.hi = (g.hi.0.max(b.max.x + 8), g.hi.1.max(b.max.y + 8));
    }
    for &net in order {
        let (drv, readers) = &nets[net];
        let dp = pos(c, *drv);
        g.mark(dp, net, 4);
        let mut sinks = readers.clone();
        sinks.sort_by_key(|r| {
            let p = pos(c, *r);
            ((p.0 - dp.0).abs() + (p.1 - dp.1).abs(), p)
        });
        for r in sinks {
            if !route_one(c, &mut g, net, *drv, r) {
                return Err(net);
            }
        }
    }
    Ok(())
}

fn route_one(c: &mut Circuit, g: &mut Grid, net: usize, drv: PinRef, target: PinRef) -> bool {
    let tp = pos(c, target);
    let t_in = reverse(outward(c, target));
    let dp = pos(c, drv);
    // Start states: the driver pin (outward only) and every point of the
    // net's tree (any free direction).
    let mut starts: Vec<(Pt, usize)> = vec![(dp, outward(c, drv))];
    let tree: Vec<Pt> =
        g.points.iter().filter(|(p, v)| **p != dp && v.iter().any(|e| e.0 == net)).map(|(p, _)| *p).collect();
    for p in tree {
        if g.blocked.contains(&p) {
            continue;
        }
        for d in 0..4 {
            starts.push((p, d));
        }
    }
    let mut dist: HashMap<(Pt, usize), u32> = HashMap::new();
    let mut prev: HashMap<(Pt, usize), Option<(Pt, usize)>> = HashMap::new();
    let mut heap = BinaryHeap::new();
    let mut seen_start = HashSet::new();
    for (s, d) in starts {
        let q = add(s, d);
        if g.edges.contains_key(&Grid::edge_key(s, q)) {
            continue;
        }
        let ok = if q == tp { d == t_in } else { !g.blocked.contains(&q) && g.enter(q, net, d).is_some() };
        if !ok || !seen_start.insert((q, d)) {
            continue;
        }
        dist.insert((q, d), 1);
        prev.insert((q, d), None);
        heap.push(Reverse((1u32, q, d, s)));
    }
    let mut start_of: HashMap<(Pt, usize), Pt> = HashMap::new();
    for Reverse((_, q, d, s)) in &heap {
        start_of.insert((*q, *d), *s);
    }
    let mut found = None;
    while let Some(Reverse((cost, p, d, _))) = heap.pop() {
        if dist.get(&(p, d)).is_some_and(|&c| c < cost) {
            continue;
        }
        if p == tp {
            found = Some((p, d));
            break;
        }
        let crossing = g.enter(p, net, d) == Some(true);
        for nd in 0..4 {
            if nd == reverse(d) || (crossing && nd != d) {
                continue;
            }
            let q = add(p, nd);
            if q.0 < g.lo.0 || q.1 < g.lo.1 || q.0 > g.hi.0 || q.1 > g.hi.1 {
                continue;
            }
            if g.edges.contains_key(&Grid::edge_key(p, q)) {
                continue;
            }
            let extra = if q == tp {
                if nd != t_in {
                    continue;
                }
                0
            } else {
                if g.blocked.contains(&q) {
                    continue;
                }
                match g.enter(q, net, nd) {
                    None => continue,
                    Some(true) => 2,
                    Some(false) => 0,
                }
            };
            let nc = cost + 1 + extra + if nd == d { 0 } else { 6 };
            if dist.get(&(q, nd)).is_none_or(|&c| nc < c) {
                dist.insert((q, nd), nc);
                prev.insert((q, nd), Some((p, d)));
                heap.push(Reverse((nc, q, nd, (0, 0))));
            }
        }
    }
    let Some(end) = found else { return false };
    // Path back to the start.
    let mut pts = vec![end.0];
    let mut cur = end;
    let mut first = end;
    while let Some(Some(p)) = prev.get(&cur) {
        pts.push(p.0);
        cur = *p;
        first = cur;
    }
    let start = start_of[&first];
    pts.push(start);
    pts.reverse();
    // Corners.
    let mut via = Vec::new();
    for w in pts.windows(3) {
        let (a, b, cc) = (w[0], w[1], w[2]);
        if (a.0 == b.0) != (b.0 == cc.0) {
            via.push(GridPos::new(b.0, b.1));
        }
    }
    let from = if start == dp && c.pin_node(drv).is_none() {
        WireEnd::Pin(drv)
    } else if let Some((n, _)) = c.nodes().find(|(_, w)| (w.pos.x, w.pos.y) == start) {
        WireEnd::Node(n)
    } else {
        let s = c
            .segments()
            .map(|(s, _)| s)
            .find(|&s| {
                let (a, b) = c.segment_ends(s).unwrap();
                let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
                let (y0, y1) = (a.y.min(b.y), a.y.max(b.y));
                start.0 >= x0 && start.0 <= x1 && start.1 >= y0 && start.1 <= y1
            })
            .unwrap();
        WireEnd::OnSegment(s, GridPos::new(start.0, start.1))
    };
    c.apply(&Edit::AddWire { from, to: WireEnd::Pin(target), via: via.clone() }).unwrap();
    for w in pts.windows(2) {
        let (p, q) = (w[0], w[1]);
        g.edges.insert(Grid::edge_key(p, q), net);
        let f = if p.1 == q.1 { 1 } else { 2 };
        g.mark(p, net, f);
        g.mark(q, net, f);
    }
    for p in [start, tp].into_iter().chain(via.iter().map(|v| (v.x, v.y))) {
        g.mark(p, net, 4);
    }
    true
}

// ---- definition builder ----------------------------------------------------

struct Lib {
    c: Circuit,
    ids: HashMap<&'static str, MacroId>,
}

impl Lib {
    fn add(&mut self, key: &'static str, def: MacroDef) {
        let id = self.c.macros().free_id().unwrap();
        self.c.apply(&Edit::DefineMacro { id, def: Arc::new(def) }).unwrap();
        self.ids.insert(key, id);
    }
}

struct B<'a> {
    lib: &'a Lib,
    def: MacroDef,
    nets: Vec<(PinRef, Vec<PinRef>)>,
}

fn pin(component: ComponentId, pin: u8) -> PinRef {
    PinRef { component, pin }
}

fn gate(op: GateOp, inputs: u8) -> ComponentKind {
    ComponentKind::Gate { op, inputs }
}

impl<'a> B<'a> {
    fn new(lib: &'a Lib, name: &str) -> Self {
        Self { lib, def: MacroDef::new(name), nets: Vec::new() }
    }

    fn comp(&mut self, kind: ComponentKind, x: i32, y: i32) -> ComponentId {
        self.def.body.add_component(kind, GridPos::new(x, y), Rotation::R0).unwrap()
    }

    fn inst(&mut self, key: &str, x: i32, y: i32) -> ComponentId {
        let shape = self.lib.c.macros().get(self.lib.ids[key]).unwrap().shape;
        self.comp(ComponentKind::Macro(shape), x, y)
    }

    fn tag(&mut self, name: &str, dir: TagDir, x: i32, y: i32, dil: u8) -> PinRef {
        let id = self.comp(ComponentKind::Tag { dir, tristate: false }, x, y);
        self.def.pins.push(MacroPin { tag: id, name: name.into(), dil });
        pin(id, 0)
    }

    fn input(&mut self, name: &str, x: i32, y: i32) -> PinRef {
        self.tag(name, TagDir::In, x, y, 0)
    }

    fn output(&mut self, name: &str, x: i32, y: i32) -> PinRef {
        self.tag(name, TagDir::Out, x, y, 0)
    }

    /// Connects driver `from` to reader `to`.
    fn wire(&mut self, from: PinRef, to: PinRef) {
        match self.nets.iter_mut().find(|n| n.0 == from) {
            Some(n) => n.1.push(to),
            None => self.nets.push((from, vec![to])),
        }
    }

    fn finish(mut self, package: Package, datasheet: &str) -> MacroDef {
        let name = self.def.name.clone();
        route_all(&mut self.def.body, &self.nets, &name);
        self.def.package = package;
        self.def.datasheet = datasheet.trim().to_string();
        self.def.body.check().unwrap();
        self.def.validate().unwrap();
        self.def
    }
}

fn out_of(kind: ComponentKind, id: ComponentId, k: u8) -> PinRef {
    pin(id, kind.input_count() + k)
}

// ---- the parts ---------------------------------------------------------------

fn half_adder(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "Half adder");
    let a = b.input("A", 0, 0);
    let bb = b.input("B", 0, 6);
    let x = b.comp(gate(GateOp::Xor, 2), 14, 0);
    let n = b.comp(gate(GateOp::And, 2), 14, 7);
    let s = b.output("S", 28, 1);
    let co = b.output("C", 28, 8);
    b.wire(a, pin(x, 0));
    b.wire(a, pin(n, 0));
    b.wire(bb, pin(x, 1));
    b.wire(bb, pin(n, 1));
    b.wire(pin(x, 2), s);
    b.wire(pin(n, 2), co);
    b.finish(
        Package::Auto,
        "Half adder\n\nAdds two bits.\n\n- S = A xor B (sum)\n- C = A and B (carry)\n\n| A | B | S | C |\n|---|---|---|---|\n| 0 | 0 | 0 | 0 |\n| 0 | 1 | 1 | 0 |\n| 1 | 0 | 1 | 0 |\n| 1 | 1 | 0 | 1 |",
    )
}

fn full_adder(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "Full adder");
    let a = b.input("A", 0, 0);
    let bb = b.input("B", 0, 4);
    let ci = b.input("CI", 0, 10);
    let h1 = b.inst("half-adder", 14, 0);
    let h2 = b.inst("half-adder", 34, 6);
    let or = b.comp(gate(GateOp::Or, 2), 54, 10);
    let s = b.output("S", 66, 7);
    let co = b.output("CO", 66, 12);
    b.wire(a, pin(h1, 0));
    b.wire(bb, pin(h1, 1));
    b.wire(pin(h1, 2), pin(h2, 0));
    b.wire(ci, pin(h2, 1));
    b.wire(pin(h2, 2), s);
    b.wire(pin(h1, 3), pin(or, 0));
    b.wire(pin(h2, 3), pin(or, 1));
    b.wire(pin(or, 2), co);
    b.finish(
        Package::Auto,
        "Full adder\n\nAdds two bits and a carry in, built from two half adders and an OR gate.\n\n- S = A xor B xor CI\n- CO = majority(A, B, CI)",
    )
}

fn adder_4(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "4-bit adder");
    let a: Vec<PinRef> = (0..4).map(|i| b.input(&format!("A{i}"), 0, 4 * i)).collect();
    let bb: Vec<PinRef> = (0..4).map(|i| b.input(&format!("B{i}"), 0, 16 + 4 * i)).collect();
    let ci = b.input("CI", 0, 34);
    let fa: Vec<ComponentId> = (0..4).map(|i| b.inst("full-adder", 24 + 22 * i, 2 + 10 * i)).collect();
    let s: Vec<PinRef> = (0..4).map(|i| b.output(&format!("S{i}"), 130, 3 + 10 * i)).collect();
    let co = b.output("CO", 130, 42);
    for i in 0..4 {
        b.wire(a[i], pin(fa[i], 0));
        b.wire(bb[i], pin(fa[i], 1));
        b.wire(pin(fa[i], 3), s[i]);
        if i == 0 {
            b.wire(ci, pin(fa[i], 2));
        } else {
            b.wire(pin(fa[i - 1], 4), pin(fa[i], 2));
        }
    }
    b.wire(pin(fa[3], 4), co);
    b.finish(
        Package::Auto,
        "4-bit adder\n\nRipple-carry adder made of four full adders: S = A + B + CI, with A, B and S as 4-bit numbers (bit 0 first) and CO the carry out of bit 3.",
    )
}

fn mux_2(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "2:1 mux");
    let d0 = b.input("D0", 0, 0);
    let d1 = b.input("D1", 0, 6);
    let s = b.input("S", 0, 12);
    let n = b.comp(ComponentKind::Not, 12, 12);
    let a0 = b.comp(gate(GateOp::And, 2), 24, 0);
    let a1 = b.comp(gate(GateOp::And, 2), 24, 7);
    let or = b.comp(gate(GateOp::Or, 2), 36, 3);
    let y = b.output("Y", 48, 4);
    b.wire(d0, pin(a0, 0));
    b.wire(s, pin(n, 0));
    b.wire(pin(n, 1), pin(a0, 1));
    b.wire(d1, pin(a1, 0));
    b.wire(s, pin(a1, 1));
    b.wire(pin(a0, 2), pin(or, 0));
    b.wire(pin(a1, 2), pin(or, 1));
    b.wire(pin(or, 2), y);
    b.finish(Package::Auto, "2:1 multiplexer\n\nY = D0 while S = 0, Y = D1 while S = 1.")
}

fn mux_4(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "4:1 mux");
    let d: Vec<PinRef> = (0..4).map(|i| b.input(&format!("D{i}"), 0, 4 * i)).collect();
    let s0 = b.input("S0", 0, 18);
    let s1 = b.input("S1", 0, 24);
    let ma = b.inst("mux-2", 18, 0);
    let mb = b.inst("mux-2", 18, 12);
    let mc = b.inst("mux-2", 40, 5);
    let y = b.output("Y", 58, 5);
    b.wire(d[0], pin(ma, 0));
    b.wire(d[1], pin(ma, 1));
    b.wire(s0, pin(ma, 2));
    b.wire(d[2], pin(mb, 0));
    b.wire(d[3], pin(mb, 1));
    b.wire(s0, pin(mb, 2));
    b.wire(pin(ma, 3), pin(mc, 0));
    b.wire(pin(mb, 3), pin(mc, 1));
    b.wire(s1, pin(mc, 2));
    b.wire(pin(mc, 3), y);
    b.finish(
        Package::Auto,
        "4:1 multiplexer\n\nY = D[S], where S = S1 S0 selects one of D0..D3. Built from three 2:1 multiplexers.",
    )
}

fn decoder_3to8(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "3-to-8 decoder");
    let a: Vec<PinRef> = (0..3).map(|i| b.input(&format!("A{i}"), 0, 4 * i)).collect();
    let en = b.input("EN", 0, 16);
    let nots: Vec<ComponentId> = (0..3).map(|i| b.comp(ComponentKind::Not, 14, 4 * i)).collect();
    let ands: Vec<ComponentId> = (0..8).map(|k| b.comp(gate(GateOp::And, 4), 40, 10 * k)).collect();
    let ys: Vec<PinRef> = (0..8).map(|k| b.output(&format!("Y{k}"), 56, 10 * k + 3)).collect();
    for i in 0..3 {
        b.wire(a[i], pin(nots[i], 0));
    }
    for k in 0..8u8 {
        for i in 0..3u8 {
            let src = if k >> i & 1 == 1 { a[usize::from(i)] } else { pin(nots[usize::from(i)], 1) };
            b.wire(src, pin(ands[usize::from(k)], i));
        }
        b.wire(en, pin(ands[usize::from(k)], 3));
        b.wire(pin(ands[usize::from(k)], 4), ys[usize::from(k)]);
    }
    b.finish(
        Package::Auto,
        "3-to-8 decoder\n\nWhile EN = 1, output Y[n] is 1 for n = A2 A1 A0 and every other output is 0. While EN = 0 all outputs are 0.",
    )
}

fn dff(pc: bool) -> ComponentKind {
    ComponentKind::FlipFlop { ty: FlipFlopType::D, trigger: FfTrigger::RisingEdge, preset_clear: pc }
}

fn jkff() -> ComponentKind {
    ComponentKind::FlipFlop { ty: FlipFlopType::Jk, trigger: FfTrigger::RisingEdge, preset_clear: true }
}

fn register_4(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "4-bit register");
    let d: Vec<PinRef> = (0..4).map(|i| b.input(&format!("D{i}"), 0, 4 * i)).collect();
    let ld = b.input("LD", 0, 18);
    let clk = b.input("CLK", 0, 22);
    let clr = b.input("CLR", 0, 26);
    for i in 0..4 {
        let y = 12 * i as i32;
        let m = b.inst("mux-2", 20, y);
        let f = b.comp(dff(true), 40, y);
        let q = b.output(&format!("Q{i}"), 60, y);
        b.wire(pin(f, 4), pin(m, 0));
        b.wire(d[i], pin(m, 1));
        b.wire(ld, pin(m, 2));
        b.wire(pin(m, 3), pin(f, 0));
        b.wire(clk, pin(f, 1));
        b.wire(clr, pin(f, 3));
        b.wire(pin(f, 4), q);
    }
    b.finish(
        Package::Auto,
        "4-bit register\n\nOn the rising edge of CLK, Q takes D while LD = 1 and keeps its value while LD = 0. CLR = 1 clears Q to 0 at once (asynchronous, active high).",
    )
}

fn shift_register_4(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "4-bit shift register");
    let d: Vec<PinRef> = (0..4).map(|i| b.input(&format!("D{i}"), 0, 4 * i)).collect();
    let si = b.input("SI", 0, 18);
    let ld = b.input("LD", 0, 22);
    let clk = b.input("CLK", 0, 26);
    let clr = b.input("CLR", 0, 30);
    let mut prev = si;
    for i in 0..4 {
        let y = 12 * i as i32;
        let m = b.inst("mux-2", 20, y);
        let f = b.comp(dff(true), 40, y);
        let q = b.output(&format!("Q{i}"), 60, y);
        b.wire(prev, pin(m, 0));
        b.wire(d[i], pin(m, 1));
        b.wire(ld, pin(m, 2));
        b.wire(pin(m, 3), pin(f, 0));
        b.wire(clk, pin(f, 1));
        b.wire(clr, pin(f, 3));
        b.wire(pin(f, 4), q);
        prev = pin(f, 4);
    }
    b.finish(
        Package::Auto,
        "4-bit shift register with parallel load\n\nOn the rising edge of CLK: with LD = 1 the register loads D0..D3; with LD = 0 it shifts towards Q3 (Q0 takes SI, Q1 takes the old Q0, and so on). CLR = 1 clears it at once.",
    )
}

fn counter_4(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "4-bit counter");
    let en = b.input("EN", 0, 0);
    let clk = b.input("CLK", 0, 6);
    let clr = b.input("CLR", 0, 12);
    let ffs: Vec<ComponentId> = (0..4).map(|i| b.comp(jkff(), 44, 12 * i)).collect();
    let ands: Vec<ComponentId> = (0..4).map(|i| b.comp(gate(GateOp::And, 2), 24, 12 * i + 8)).collect();
    let qs: Vec<PinRef> = (0..4).map(|i| b.output(&format!("Q{i}"), 64, 12 * i as i32)).collect();
    let co = b.output("CO", 64, 50);
    // T_i: EN for bit 0, then EN · Q0 · … chained through AND gates.
    let mut t = en;
    for i in 0..4 {
        b.wire(t, pin(ffs[i], 0));
        b.wire(t, pin(ffs[i], 2));
        b.wire(clk, pin(ffs[i], 1));
        b.wire(clr, pin(ffs[i], 4));
        b.wire(pin(ffs[i], 5), qs[i]);
        b.wire(t, pin(ands[i], 0));
        b.wire(pin(ffs[i], 5), pin(ands[i], 1));
        t = pin(ands[i], 2);
    }
    b.wire(t, co);
    b.finish(
        Package::Auto,
        "4-bit synchronous counter\n\nCounts up on the rising edge of CLK while EN = 1 (0, 1, … 15, 0). Q0 is the least significant bit. CO = 1 while EN = 1 and the count is 15 (cascade it into the EN of the next counter). CLR = 1 clears the count at once.",
    )
}

fn decade_counter(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "Decade counter");
    let en = b.input("EN", 0, 0);
    let clk = b.input("CLK", 0, 6);
    let clr = b.input("CLR", 0, 12);
    let ffs: Vec<ComponentId> = (0..4).map(|i| b.comp(jkff(), 56, 12 * i)).collect();
    let qs: Vec<PinRef> = (0..4).map(|i| b.output(&format!("Q{i}"), 76, 12 * i as i32)).collect();
    let co = b.output("CO", 76, 50);
    let q = |i: usize| pin(ffs[i], 5);
    let qn = |i: usize| pin(ffs[i], 6);
    let t1 = b.comp(gate(GateOp::And, 3), 20, 18);
    let t2 = b.comp(gate(GateOp::And, 3), 20, 28);
    let t3a = b.comp(gate(GateOp::And, 4), 20, 38);
    let t3b = b.comp(gate(GateOp::And, 3), 20, 50);
    let t3 = b.comp(gate(GateOp::Or, 2), 38, 42);
    for (g, srcs) in [
        (t1, vec![en, q(0), qn(3)]),
        (t2, vec![en, q(0), q(1)]),
        (t3a, vec![en, q(0), q(1), q(2)]),
        (t3b, vec![en, q(0), q(3)]),
    ] {
        for (k, s) in srcs.into_iter().enumerate() {
            b.wire(s, pin(g, u8::try_from(k).unwrap()));
        }
    }
    b.wire(pin(t3a, 4), pin(t3, 0));
    b.wire(pin(t3b, 3), pin(t3, 1));
    let ts = [en, pin(t1, 3), pin(t2, 3), pin(t3, 2)];
    for i in 0..4 {
        b.wire(ts[i], pin(ffs[i], 0));
        b.wire(ts[i], pin(ffs[i], 2));
        b.wire(clk, pin(ffs[i], 1));
        b.wire(clr, pin(ffs[i], 4));
        b.wire(q(i), qs[i]);
    }
    b.wire(pin(t3b, 3), co);
    b.finish(
        Package::Auto,
        "Decade (BCD) counter\n\nCounts 0 … 9, 0 on the rising edge of CLK while EN = 1; Q3..Q0 is the BCD digit. CO = 1 while EN = 1 and the count is 9 (cascade it into the EN of the next digit). CLR = 1 clears it at once.",
    )
}

fn t_flip_flop(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "T flip-flop");
    let t = b.input("T", 0, 0);
    let c = b.input("C", 0, 4);
    let clr = b.input("CLR", 0, 10);
    let f = b.comp(jkff(), 18, 0);
    let q = b.output("Q", 36, 0);
    let qn = b.output("QN", 36, 4);
    b.wire(t, pin(f, 0));
    b.wire(t, pin(f, 2));
    b.wire(c, pin(f, 1));
    b.wire(clr, pin(f, 4));
    b.wire(pin(f, 5), q);
    b.wire(pin(f, 6), qn);
    b.finish(
        Package::Auto,
        "T (toggle) flip-flop\n\nOn the rising edge of C, Q toggles while T = 1 and holds while T = 0. QN is the complement of Q. CLR = 1 clears Q at once.",
    )
}

fn bcd_7seg(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "BCD to 7-segment");
    let d: Vec<PinRef> = (0..4).map(|i| b.input(&format!("D{i}"), 0, 5 * i)).collect();
    // A = D3, B = D2, C = D1, D = D0.
    let (a, bb, c, dd) = (d[3], d[2], d[1], d[0]);
    let nb = b.comp(ComponentKind::Not, 14, 22);
    let nc = b.comp(ComponentKind::Not, 14, 26);
    let nd = b.comp(ComponentKind::Not, 14, 30);
    b.wire(bb, pin(nb, 0));
    b.wire(c, pin(nc, 0));
    b.wire(dd, pin(nd, 0));
    let (nb, nc, nd) = (pin(nb, 1), pin(nc, 1), pin(nd, 1));
    let term = |b: &mut B, srcs: &[PinRef], y: i32| {
        let n = u8::try_from(srcs.len()).unwrap();
        let g = b.comp(gate(GateOp::And, n), 34, y);
        for (k, s) in srcs.iter().enumerate() {
            b.wire(*s, pin(g, u8::try_from(k).unwrap()));
        }
        pin(g, n)
    };
    let bd = term(&mut b, &[bb, dd], 0);
    let nbnd = term(&mut b, &[nb, nd], 7);
    let ncnd = term(&mut b, &[nc, nd], 14);
    let cd = term(&mut b, &[c, dd], 21);
    let nbc = term(&mut b, &[nb, c], 28);
    let cnd = term(&mut b, &[c, nd], 35);
    let bncd = term(&mut b, &[bb, nc, dd], 42);
    let bnc = term(&mut b, &[bb, nc], 51);
    let bnd = term(&mut b, &[bb, nd], 58);
    let segs: [(&str, Vec<PinRef>); 7] = [
        ("a", vec![a, c, bd, nbnd]),
        ("b", vec![nb, ncnd, cd]),
        ("c", vec![bb, nc, dd]),
        ("d", vec![a, nbnd, nbc, cnd, bncd]),
        ("e", vec![nbnd, cnd]),
        ("f", vec![a, ncnd, bnc, bnd]),
        ("g", vec![a, nbc, bnc, cnd]),
    ];
    let mut y = 0;
    for (name, srcs) in segs {
        let n = u8::try_from(srcs.len()).unwrap();
        let g = b.comp(gate(GateOp::Or, n), 70, y);
        for (k, s) in srcs.into_iter().enumerate() {
            b.wire(s, pin(g, u8::try_from(k).unwrap()));
        }
        let o = b.output(name, 86, y + i32::from(n) - 1);
        b.wire(pin(g, n), o);
        y += 2 * i32::from(n) + 4;
    }
    b.finish(
        Package::Auto,
        "BCD to 7-segment driver\n\nTurns a BCD digit D3..D0 into the segments a..g of a common display (1 = segment lit):\n\n```\n  aaa\n f   b\n  ggg\n e   c\n  ddd\n```\n\nInputs 10–15 are not BCD digits; they light other, unspecified patterns.",
    )
}

fn wide_gate(lib: &Lib, op: GateOp, n: usize) -> MacroDef {
    let name = format!("{}-{n}", op.name());
    let mut b = B::new(lib, &name);
    let names = ["A", "B", "C", "D", "E", "F"];
    let ins: Vec<PinRef> = (0..n).map(|i| b.input(names[i], 0, 4 * i as i32)).collect();
    let first = b.comp(gate(op, 3), 16, 0);
    let rest = n - 3;
    let second = b.comp(gate(op, u8::try_from(rest).unwrap()), 16, 12);
    let last = b.comp(gate(op, 2), 32, 6);
    let y = b.output("Y", 44, 6);
    for (i, p) in ins.iter().enumerate() {
        if i < 3 {
            b.wire(*p, pin(first, u8::try_from(i).unwrap()));
        } else {
            b.wire(*p, pin(second, u8::try_from(i - 3).unwrap()));
        }
    }
    b.wire(pin(first, 3), pin(last, 0));
    b.wire(pin(second, u8::try_from(rest).unwrap()), pin(last, 1));
    b.wire(pin(last, 2), y);
    let f = if op == GateOp::And { "1 only while every input is 1" } else { "1 while any input is 1" };
    b.finish(
        Package::Auto,
        &format!("{n}-input {}\n\nY is {f}. Built as a two-level tree of smaller gates.", op.name()),
    )
}

/// A DIL part: `units` copies of a gate; `layout[k]` = (unit, pin of the
/// gate or `None` for its output, DIL number, name).
fn dil_gates(lib: &Lib, name: &str, kind: ComponentKind, units: usize, numbers: &[(&str, u8)], text: &str) -> MacroDef {
    let mut b = B::new(lib, name);
    let ins = usize::from(kind.input_count());
    let mut gates = Vec::new();
    for u in 0..units {
        gates.push(b.comp(kind, 20, 10 * u as i32));
    }
    let mut row_in = 0;
    let mut outs = Vec::new();
    for (u, g) in gates.iter().enumerate() {
        for k in 0..ins {
            let (nm, num) = numbers[u * (ins + 1) + k];
            let t = b.tag(nm, TagDir::In, 0, 4 * row_in, num);
            row_in += 1;
            b.wire(t, pin(*g, u8::try_from(k).unwrap()));
        }
        outs.push((u, *g));
    }
    for (u, g) in outs {
        let (nm, num) = numbers[u * (ins + 1) + ins];
        let t = b.tag(nm, TagDir::Out, 36, 10 * u as i32, num);
        b.wire(out_of(kind, g, 0), t);
    }
    b.finish(Package::Dil(14), text)
}

fn dual_dff(lib: &Lib) -> MacroDef {
    let mut b = B::new(lib, "Dual D flip-flop");
    let nums =
        [[("1D", 2u8), ("1CLK", 3), ("1PRE", 4), ("1CLR", 1)], [("2D", 12), ("2CLK", 11), ("2PRE", 10), ("2CLR", 13)]];
    let qn = [[("1Q", 5u8), ("1QN", 6)], [("2Q", 9), ("2QN", 8)]];
    let mut ffs = Vec::new();
    for u in 0..2usize {
        let f = b.comp(dff(true), 22, 16 * u as i32);
        for (k, (nm, num)) in nums[u].iter().enumerate() {
            let t = b.tag(nm, TagDir::In, 0, 16 * u as i32 + 4 * k as i32, *num);
            b.wire(t, pin(f, u8::try_from(k).unwrap()));
        }
        ffs.push(f);
    }
    for u in 0..2usize {
        for (k, (nm, num)) in qn[u].iter().enumerate() {
            let t = b.tag(nm, TagDir::Out, 42, 16 * u as i32 + 4 * k as i32, *num);
            b.wire(pin(ffs[u], 4 + u8::try_from(k).unwrap()), t);
        }
    }
    b.finish(
        Package::Dil(14),
        "Dual D flip-flop (DIL-14)\n\nTwo independent rising-edge D flip-flops. PRE and CLR are asynchronous and active high (unlike many real parts); leave them at 0 for normal use.\n\nPins: 1 1CLR, 2 1D, 3 1CLK, 4 1PRE, 5 1Q, 6 1QN, 7 GND (not modelled), 8 2QN, 9 2Q, 10 2PRE, 11 2CLK, 12 2D, 13 2CLR, 14 VCC (not modelled).",
    )
}

/// Every built-in part: (file stem, definition key in the container).
fn build() -> (Lib, Vec<&'static str>) {
    let mut lib = Lib { c: Circuit::new(), ids: HashMap::new() };
    let mut order = Vec::new();
    macro_rules! part {
        ($key:literal, $e:expr) => {{
            let d = $e;
            lib.add($key, d);
            order.push($key);
        }};
    }
    part!("half-adder", half_adder(&lib));
    part!("full-adder", full_adder(&lib));
    part!("adder-4", adder_4(&lib));
    part!("mux-2", mux_2(&lib));
    part!("mux-4", mux_4(&lib));
    part!("decoder-3to8", decoder_3to8(&lib));
    part!("register-4", register_4(&lib));
    part!("shift-register-4", shift_register_4(&lib));
    part!("counter-4", counter_4(&lib));
    part!("decade-counter", decade_counter(&lib));
    part!("t-flip-flop", t_flip_flop(&lib));
    part!("bcd-7seg", bcd_7seg(&lib));
    part!("and-5", wide_gate(&lib, GateOp::And, 5));
    part!("or-5", wide_gate(&lib, GateOp::Or, 5));
    part!("and-6", wide_gate(&lib, GateOp::And, 6));
    part!("or-6", wide_gate(&lib, GateOp::Or, 6));
    let gate_pins =
        |names: [&'static str; 3], nums: [u8; 3]| [(names[0], nums[0]), (names[1], nums[1]), (names[2], nums[2])];
    let quad: Vec<(&str, u8)> = [
        gate_pins(["1A", "1B", "1Y"], [1, 2, 3]),
        gate_pins(["2A", "2B", "2Y"], [4, 5, 6]),
        gate_pins(["3A", "3B", "3Y"], [9, 10, 8]),
        gate_pins(["4A", "4B", "4Y"], [12, 13, 11]),
    ]
    .concat();
    let quad_text = |op: &str| {
        format!(
            "Quad 2-input {op} (DIL-14)\n\nFour independent 2-input {op} gates in a 14-pin dual-in-line package (74-series style pinout).\n\nPins: 1 1A, 2 1B, 3 1Y, 4 2A, 5 2B, 6 2Y, 7 GND (not modelled), 8 3Y, 9 3A, 10 3B, 11 4Y, 12 4A, 13 4B, 14 VCC (not modelled)."
        )
    };
    part!("quad-nand2", dil_gates(&lib, "Quad NAND", gate(GateOp::Nand, 2), 4, &quad, &quad_text("NAND")));
    part!("quad-and2", dil_gates(&lib, "Quad AND", gate(GateOp::And, 2), 4, &quad, &quad_text("AND")));
    let hex: Vec<(&str, u8)> = vec![
        ("1A", 1),
        ("1Y", 2),
        ("2A", 3),
        ("2Y", 4),
        ("3A", 5),
        ("3Y", 6),
        ("4A", 9),
        ("4Y", 8),
        ("5A", 11),
        ("5Y", 10),
        ("6A", 13),
        ("6Y", 12),
    ];
    part!(
        "hex-inverter",
        dil_gates(
            &lib,
            "Hex inverter",
            ComponentKind::Not,
            6,
            &hex,
            "Hex inverter (DIL-14)\n\nSix independent inverters, Y = not A, in a 14-pin dual-in-line package (74-series style pinout).\n\nPins: 1 1A, 2 1Y, 3 2A, 4 2Y, 5 3A, 6 3Y, 7 GND (not modelled), 8 4Y, 9 4A, 10 5Y, 11 5A, 12 6Y, 13 6A, 14 VCC (not modelled)."
        )
    );
    part!("dual-dff", dual_dff(&lib));
    (lib, order)
}

fn library_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bw-io/library")
}

/// The embedded files are exactly what the generator produces (set
/// `BITWERK_REGEN_LIBRARY=1` to rewrite them).
#[test]
fn files_are_up_to_date() {
    let (lib, order) = build();
    let regen = std::env::var_os("BITWERK_REGEN_LIBRARY").is_some();
    let mut stale = Vec::new();
    for key in order {
        let text = bw_io::bwmacro::save_to_string(lib.c.macros(), lib.ids[key]).unwrap();
        let path = library_dir().join(format!("{key}.bwmacro"));
        if regen {
            std::fs::write(&path, &text).unwrap();
        } else if std::fs::read_to_string(&path).unwrap_or_default() != text {
            stale.push(key);
        }
    }
    assert!(stale.is_empty(), "library files out of date (regenerate them): {stale:?}");
    assert_eq!(bw_io::library::builtin().len(), bw_io::library::BUILTIN.len(), "every built-in part parses");
}

// ---- behaviour ---------------------------------------------------------------

fn bench(key: &str) -> Bench {
    let e = bw_io::library::builtin().iter().find(|e| e.key == format!("builtin/{key}")).unwrap();
    Bench::new(&e.file.table, e.file.root)
}

#[test]
fn every_part_has_a_datasheet_and_named_pins() {
    for e in bw_io::library::builtin() {
        let d = e.file.def().unwrap();
        assert!(d.datasheet.len() > 40, "{} needs a datasheet", e.key);
        assert!(!d.pins.is_empty(), "{}", e.key);
        d.validate().unwrap();
    }
}

#[test]
fn adders() {
    let mut b = bench("half-adder");
    for v in 0..4u32 {
        b.set_bits(0, 2, v);
        let (x, y) = (v & 1, v >> 1);
        assert_eq!(b.bits(0, 2), (x ^ y) | (x & y) << 1, "half adder {v}");
    }
    let mut b = bench("full-adder");
    for v in 0..8u32 {
        b.set_bits(0, 3, v);
        let sum = (v & 1) + (v >> 1 & 1) + (v >> 2);
        assert_eq!(b.bits(0, 2), sum, "full adder {v}");
    }
    let mut b = bench("adder-4");
    for a in 0..16u32 {
        for x in 0..16u32 {
            for ci in 0..2u32 {
                b.set_bits(0, 4, a);
                b.set_bits(4, 4, x);
                b.set(8, ci == 1);
                assert_eq!(b.bits(0, 5), a + x + ci, "{a} + {x} + {ci}");
            }
        }
    }
}

#[test]
fn multiplexers_and_decoder() {
    let mut b = bench("mux-2");
    for v in 0..8u32 {
        b.set_bits(0, 3, v);
        let expect = if v >> 2 == 1 { v >> 1 & 1 } else { v & 1 };
        assert_eq!(b.bits(0, 1), expect, "mux-2 {v:03b}");
    }
    let mut b = bench("mux-4");
    for v in 0..64u32 {
        b.set_bits(0, 6, v);
        let sel = v >> 4;
        assert_eq!(b.bits(0, 1), v >> sel & 1, "mux-4 {v:06b}");
    }
    let mut b = bench("decoder-3to8");
    for v in 0..16u32 {
        b.set_bits(0, 4, v);
        let expect = if v >> 3 == 1 { 1 << (v & 7) } else { 0 };
        assert_eq!(b.bits(0, 8), expect, "decoder {v:04b}");
    }
}

#[test]
fn wide_gates() {
    for (key, n, and) in [("and-5", 5, true), ("or-5", 5, false), ("and-6", 6, true), ("or-6", 6, false)] {
        let mut b = bench(key);
        let all = (1u32 << n) - 1;
        for v in 0..=all {
            b.set_bits(0, n, v);
            let expect = u32::from(if and { v == all } else { v != 0 });
            assert_eq!(b.bits(0, 1), expect, "{key} {v:b}");
        }
    }
}

#[test]
fn bcd_to_seven_segments() {
    // Segments a..g (bit 0 = a) of the digits 0..9.
    let digits = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
    let mut b = bench("bcd-7seg");
    for (v, &seg) in digits.iter().enumerate() {
        b.set_bits(0, 4, u32::try_from(v).unwrap());
        assert_eq!(b.bits(0, 7), seg, "digit {v}");
    }
    for v in 10..16 {
        b.set_bits(0, 4, v);
        let _ = b.bits(0, 7); // defined 0/1 levels, unspecified pattern
    }
}

#[test]
fn t_flip_flop_toggles() {
    let mut b = bench("t-flip-flop");
    // T, C, CLR → Q, QN
    assert_eq!(b.bits(0, 2), 0b10);
    b.set(0, true);
    for k in 1..6u32 {
        b.pulse(1);
        let q = k & 1;
        assert_eq!(b.bits(0, 2), q | (1 - q) << 1, "after {k} pulses");
    }
    b.set(0, false);
    let before = b.bits(0, 1);
    b.pulse(1);
    assert_eq!(b.bits(0, 1), before, "T = 0 holds");
    b.set(2, true);
    assert_eq!(b.bits(0, 1), 0, "CLR");
}

#[test]
fn counters_count() {
    let mut b = bench("counter-4");
    // EN, CLK, CLR → Q0..Q3, CO
    b.set(0, true);
    for k in 1..=40u32 {
        b.pulse(1);
        assert_eq!(b.bits(0, 4), k % 16, "count after {k}");
        assert_eq!(b.bits(4, 1), u32::from(k % 16 == 15), "carry at {k}");
    }
    b.set(0, false);
    b.pulse(1);
    assert_eq!(b.bits(0, 4), 40 % 16, "EN = 0 holds");
    b.set(2, true);
    assert_eq!(b.bits(0, 4), 0, "CLR");
    let mut b = bench("decade-counter");
    b.set(0, true);
    for k in 1..=25u32 {
        b.pulse(1);
        assert_eq!(b.bits(0, 4), k % 10, "decade count after {k}");
        assert_eq!(b.bits(4, 1), u32::from(k % 10 == 9), "decade carry at {k}");
    }
}

#[test]
fn registers_load_and_shift() {
    let mut b = bench("register-4");
    // D0..D3, LD, CLK, CLR
    b.set_bits(0, 4, 0b1011);
    b.pulse(5);
    assert_eq!(b.bits(0, 4), 0, "LD = 0 holds");
    b.set(4, true);
    b.pulse(5);
    assert_eq!(b.bits(0, 4), 0b1011);
    b.set(4, false);
    b.set_bits(0, 4, 0b0100);
    b.pulse(5);
    assert_eq!(b.bits(0, 4), 0b1011, "holds while LD = 0");
    b.set(6, true);
    assert_eq!(b.bits(0, 4), 0, "CLR");
    let mut b = bench("shift-register-4");
    // D0..D3, SI, LD, CLK, CLR
    b.set_bits(0, 4, 0b0110);
    b.set(5, true);
    b.pulse(6);
    assert_eq!(b.bits(0, 4), 0b0110, "parallel load");
    b.set(5, false);
    b.set(4, true);
    b.pulse(6);
    assert_eq!(b.bits(0, 4), 0b1101, "shift with SI = 1");
    b.set(4, false);
    b.pulse(6);
    assert_eq!(b.bits(0, 4), 0b1010);
}

#[test]
fn dil_parts() {
    let mut b = bench("quad-nand2");
    for v in 0..256u32 {
        b.set_bits(0, 8, v);
        let expect = (0..4).fold(0, |acc, u| acc | u32::from((v >> (2 * u) & 3) != 3) << u);
        assert_eq!(b.bits(0, 4), expect);
    }
    let mut b = bench("quad-and2");
    for v in 0..256u32 {
        b.set_bits(0, 8, v);
        let expect = (0..4).fold(0, |acc, u| acc | u32::from((v >> (2 * u) & 3) == 3) << u);
        assert_eq!(b.bits(0, 4), expect);
    }
    let mut b = bench("hex-inverter");
    for v in 0..64u32 {
        b.set_bits(0, 6, v);
        assert_eq!(b.bits(0, 6), !v & 63);
    }
    let mut b = bench("dual-dff");
    // 1D 1CLK 1PRE 1CLR 2D 2CLK 2PRE 2CLR → 1Q 1QN 2Q 2QN
    b.set(0, true);
    b.pulse(1);
    assert_eq!(b.bits(0, 4), 0b1001, "1Q = 1, 2Q = 0");
    b.set(4, true);
    b.pulse(5);
    assert_eq!(b.bits(0, 4), 0b0101);
    b.set(3, true);
    assert_eq!(b.bits(0, 2), 0b10, "1CLR");
    // The DIL template gives every pin its package number.
    let e = bw_io::library::builtin().iter().find(|e| e.key == "builtin/dual-dff").unwrap();
    let shape = e.file.table.get(e.file.root).unwrap().shape;
    assert_eq!(shape.dil, 14);
    assert_eq!(&shape.dil_pins[..12], &[2, 3, 4, 1, 12, 11, 10, 13, 5, 6, 9, 8]);
}

// ---- the bundled example using macros ------------------------------------------

fn import_builtin(c: &mut Circuit, key: &str) -> MacroId {
    let e = bw_io::library::builtin().iter().find(|e| e.key == key).unwrap();
    let (defs, root) = c.macros().import_plan(&e.file.table, e.file.root).unwrap();
    for (id, def) in defs {
        let mut def = (*def).clone();
        if id == root {
            def.origin = Some(bw_core::LibraryRef { key: e.key.clone(), hash: e.file.hash() });
        }
        c.apply(&Edit::DefineMacro { id, def: Arc::new(def) }).unwrap();
    }
    root
}

/// `examples/macros-demo.bitwerk`: a decade counter driving a BCD to
/// 7-segment driver whose outputs light seven LEDs laid out as a digit,
/// and a 4-bit adder with switches and LEDs.
fn demo_project() -> bw_io::Project {
    let mut c = Circuit::new();
    let dec = import_builtin(&mut c, "builtin/decade-counter");
    let nand = import_builtin(&mut c, "builtin/quad-nand2");
    let seg = import_builtin(&mut c, "builtin/bcd-7seg");
    let add = import_builtin(&mut c, "builtin/adder-4");
    let mac = |c: &Circuit, id| ComponentKind::Macro(c.macros().get(id).unwrap().shape);
    let put = |c: &mut Circuit, k, x, y| c.add_component(k, GridPos::new(x, y), Rotation::R0).unwrap();
    let mut nets: Vec<(PinRef, Vec<PinRef>)> = Vec::new();
    let clk = put(&mut c, ComponentKind::Clock { half_period: 10 }, 0, 2);
    let en = put(&mut c, ComponentKind::Switch { on: true, mode: bw_core::SwitchMode::Toggle }, 0, 7);
    let clr = put(&mut c, ComponentKind::Switch { on: false, mode: bw_core::SwitchMode::Toggle }, 0, 12);
    let k = mac(&c, dec);
    let counter = put(&mut c, k, 12, 2);
    let k = mac(&c, seg);
    let driver = put(&mut c, k, 42, 0);
    nets.push((pin(en, 0), vec![pin(counter, 0)]));
    nets.push((pin(clk, 0), vec![pin(counter, 1)]));
    nets.push((pin(clr, 0), vec![pin(counter, 2)]));
    for i in 0..4 {
        nets.push((pin(counter, 3 + i), vec![pin(driver, i)]));
    }
    // Seven LEDs as a digit: a b c d e f g.
    let spots = [(80, 0), (86, 4), (86, 11), (80, 15), (74, 11), (74, 4), (80, 7)];
    for (i, (x, y)) in spots.iter().enumerate() {
        let led = put(&mut c, ComponentKind::Led, *x, *y);
        nets.push((pin(driver, 4 + u8::try_from(i).unwrap()), vec![pin(led, 0)]));
    }
    // The adder: A0..A3, B0..B3, CI on switches; S0..S3, CO on LEDs.
    let k = mac(&c, add);
    let adder = put(&mut c, k, 42, 34);
    // A DIL part: gate 1 of a quad NAND fed by A0 and B0.
    let k = mac(&c, nand);
    let dil = put(&mut c, k, 100, 30);
    let nled = put(&mut c, ComponentKind::Led, 120, 33);
    for i in 0..9u8 {
        let s = put(
            &mut c,
            ComponentKind::Switch { on: i == 0 || i == 5, mode: bw_core::SwitchMode::Toggle },
            0,
            26 + 4 * i32::from(i),
        );
        let mut readers = vec![pin(adder, i)];
        if i == 0 || i == 4 {
            readers.push(pin(dil, u8::from(i != 0)));
        }
        nets.push((pin(s, 0), readers));
    }
    nets.push((pin(dil, 8), vec![pin(nled, 0)]));
    for i in 0..5u8 {
        let led = put(&mut c, ComponentKind::Led, 74, 34 + 4 * i32::from(i));
        nets.push((pin(adder, 9 + i), vec![pin(led, 0)]));
    }
    route_all(&mut c, &nets, "demo");
    c.check().unwrap();
    bw_io::Project { name: "Macros demo".into(), circuit: c }
}

#[test]
fn demo_example_is_up_to_date() {
    let text = bw_io::save_to_string(&demo_project()).unwrap();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/macros-demo.bitwerk");
    if std::env::var_os("BITWERK_REGEN_LIBRARY").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap_or_default(), text, "regenerate the example");
    // It counts: after n clock edges the driver shows digit n % 10.
    let p = bw_io::load_from_bytes(text.as_bytes()).unwrap();
    let c = p.circuit;
    let mut sim = bw_sim::SimState::new(bw_sim::Netlist::compile(&c).unwrap(), &c);
    sim.reset().unwrap();
    let driver = c.components().filter(|k| matches!(k.kind, ComponentKind::Macro(_))).nth(1).unwrap().id;
    let digits = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
    let seg = |sim: &bw_sim::SimState| (0..7u8).fold(0u32, |a, k| a | u32::from(sim.output(driver, k) == Some(1)) << k);
    assert_eq!(seg(&sim), digits[0]);
    let mut edges = 0;
    for _ in 0..400 {
        if sim.tick().unwrap() {
            edges += 1;
        }
    }
    assert_eq!(seg(&sim), digits[(edges / 2) % 10], "after {edges} clock edges");
}
