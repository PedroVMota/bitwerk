//! Macro definitions of the W-128, from gates up to the 32-bit CPU slice
//! and the control unit.
//!
//! Hierarchy (depth 5 below the board):
//!
//! ```text
//! Slice32 ─┬─ RegFile32 ─┬─ Reg32 ── Reg8          (D flip-flops + load enable)
//!          │             ├─ RdMux                  (two 7:1 AND-OR read ports, one bit)
//!          │             └─ Dec3/7                 (register decoders)
//!          └─ ALU32 ───── ALU16 ─┬─ ALU4 ─┬─ ALU bit (operand mux, P/G, sum, result mux)
//!                                │        └─ CLA4    (4-bit carry lookahead)
//!                                └─ CLA4             (lookahead over 4 nibbles)
//! Control unit ─┬─ Reg32 (IR), Reg10 (MAR), Reg3 (flags), PC8, Dec3/7
//!               └─ decode / FSM gates
//! Latch16 (I/O display register), CLA4 (lookahead over the four slices)
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use bw_core::{Circuit, Color, ComponentKind, Edit, FfTrigger, FlipFlopType, GateOp, MacroDef, MacroId, NumFormat};

use crate::layout::{Body, H, LAST, Sig};

use GateOp::{And, Nor, Or, Xor};

/// The definitions table being built (in the board's root circuit).
pub struct Lib {
    pub c: Circuit,
    pub ids: BTreeMap<&'static str, MacroId>,
    /// Track-order conflicts summed over all bodies (0 = clean layout).
    pub conflicts: usize,
}

impl Default for Lib {
    fn default() -> Self {
        Self::new()
    }
}

impl Lib {
    #[must_use]
    pub fn new() -> Self {
        Self { c: Circuit::new(), ids: BTreeMap::new(), conflicts: 0 }
    }

    /// Lays `body` out and stores it as definition `key`.
    ///
    /// # Panics
    /// If the model refuses the definition (a generator bug).
    pub fn define(&mut self, key: &'static str, name: &str, body: Body, datasheet: &str) -> MacroId {
        let mut def = MacroDef::new(name);
        let lay = body.build_into(&mut def.body);
        self.conflicts += lay.conflicts;
        def.pins = lay.pins;
        def.datasheet = datasheet.trim().to_string();
        def.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        let id = self.c.macros().free_id().unwrap_or_else(|| panic!("no free macro id"));
        self.c.apply(&Edit::DefineMacro { id, def: Arc::new(def) }).unwrap_or_else(|e| panic!("{name}: {e}"));
        self.ids.insert(key, id);
        id
    }

    /// The component kind of an instance of `key`.
    ///
    /// # Panics
    /// If `key` was not defined yet.
    #[must_use]
    pub fn kind(&self, key: &str) -> ComponentKind {
        let id = self.ids.get(key).unwrap_or_else(|| panic!("macro {key} not defined"));
        ComponentKind::Macro(self.c.macros().get(*id).unwrap_or_else(|| panic!("macro {key}")).shape)
    }
}

/// Rising-edge D flip-flop without preset/clear.
pub const DFF: ComponentKind =
    ComponentKind::FlipFlop { ty: FlipFlopType::D, trigger: FfTrigger::RisingEdge, preset_clear: false };
/// Transparent D latch (C = 1: Q follows D).
pub const DLATCH: ComponentKind =
    ComponentKind::FlipFlop { ty: FlipFlopType::D, trigger: FfTrigger::Latch, preset_clear: false };

/// A 16-bit hexadecimal display.
pub const HEX16: ComponentKind = ComponentKind::Numeric { bits: 16, format: NumFormat::Hex, signed: false };

fn u(k: usize) -> u8 {
    u8::try_from(k).unwrap_or(u8::MAX)
}

/// Inputs named `prefix0..prefix{n-1}`.
fn inputs(b: &mut Body, prefix: &str, n: usize) -> Vec<Sig> {
    (0..n).map(|i| b.input(&format!("{prefix}{i}"))).collect()
}

/// An instance of `kind` in `col` with inputs fed from `ins`.
fn inst(b: &mut Body, kind: ComponentKind, col: usize, ins: &[Sig]) -> H {
    let h = b.add(kind, col);
    b.feed(h, 0, ins);
    h
}

// ---- small parts ----------------------------------------------------------

/// CLA4: 4-bit carry lookahead (the 74182 idea).
fn cla4(lib: &mut Lib) {
    let mut b = Body::new();
    let ci = b.input("CI");
    let mut g = Vec::new();
    let mut p = Vec::new();
    for i in 0..4 {
        g.push(b.input(&format!("G{i}")));
        p.push(b.input(&format!("P{i}")));
    }
    // Carry into bit k+1 (and the group generate for k = 3 without CI).
    let carry = |b: &mut Body, k: usize, with_ci: bool| -> Sig {
        let mut terms = vec![g[k]];
        for j in (0..k).rev() {
            let mut t: Vec<Sig> = (j + 1..=k).map(|m| p[m]).collect();
            t.push(g[j]);
            terms.push(b.gate(And, &t, 1));
        }
        if with_ci {
            let mut t: Vec<Sig> = (0..=k).map(|m| p[m]).collect();
            t.push(ci);
            terms.push(b.gate(And, &t, 1));
        }
        b.gate(Or, &terms, 2)
    };
    let c1 = carry(&mut b, 0, true);
    let c2 = carry(&mut b, 1, true);
    let c3 = carry(&mut b, 2, true);
    let gg = carry(&mut b, 3, false);
    let pp = b.gate(And, &[p[0], p[1], p[2], p[3]], 2);
    b.output("C1", c1);
    b.output("C2", c2);
    b.output("C3", c3);
    b.output("G", gg);
    b.output("P", pp);
    lib.define(
        "cla4",
        "CLA4",
        b,
        "CLA4 - 4-bit carry lookahead unit\n\nFrom the generate (G) and propagate (P) signals of four bits or groups and the carry in CI:\n\n- C1 = G0 + P0.CI\n- C2 = G1 + P1.G0 + P1.P0.CI\n- C3 = G2 + P2.G1 + P2.P1.G0 + P2.P1.P0.CI\n- G = G3 + P3.G2 + P3.P2.G1 + P3.P2.P1.G0 (group generate)\n- P = P3.P2.P1.P0 (group propagate)\n\nTwo gate delays from any input to any output. Used for bits within a nibble, nibbles within 16 bits and the four 32-bit slices.",
    );
}

/// Names of the ALU control lines, in pin order.
pub const ALU_CTRL: [&str; 11] = ["SREG", "SIMM", "SMEM", "SUB", "OSUM", "OAND", "OOR", "OXOR", "OSHL", "OSHR", "OPB"];

/// One bit of the ALU: operand mux, P/G, sum and result mux.
fn alu_bit(lib: &mut Lib) {
    let mut b = Body::new();
    let a = b.input("A");
    let rb = b.input("RB");
    let imm = b.input("IMM");
    let din = b.input("DIN");
    let ci = b.input("CI");
    let sl = b.input("SL");
    let sr = b.input("SR");
    let ctl: Vec<Sig> = ALU_CTRL.iter().map(|n| b.input(n)).collect();
    let (sreg, simm, smem, sub) = (ctl[0], ctl[1], ctl[2], ctl[3]);
    let t0 = b.gate(And, &[rb, sreg], 1);
    let t1 = b.gate(And, &[imm, simm], 1);
    let t2 = b.gate(And, &[din, smem], 1);
    let bb = b.gate(Or, &[t0, t1, t2], 2);
    let bx = b.gate(Xor, &[bb, sub], 3);
    let g = b.gate(And, &[a, bx], 4);
    let p = b.gate(Xor, &[a, bx], 4);
    let o = b.gate(Or, &[a, bx], 4);
    let s = b.gate(Xor, &[p, ci], 5);
    let r: Vec<Sig> = [(s, 4), (g, 5), (o, 6), (p, 7), (sl, 8), (sr, 9), (bb, 10)]
        .iter()
        .map(|&(v, k)| b.gate(And, &[v, ctl[k]], 6))
        .collect();
    let y = b.gate(Or, &r, 7);
    b.output("Y", y);
    b.output("G", g);
    b.output("P", p);
    lib.define(
        "alu-bit",
        "ALU bit",
        b,
        "ALU bit - one bit of the W-128 ALU\n\nOperand B = RB.SREG + IMM.SIMM + DIN.SMEM (register, immediate or memory), inverted when SUB = 1.\n\n- G = A.B, P = A xor B (carry generate / propagate)\n- sum S = P xor CI\n- Y = S.OSUM + G.OAND + (A + B).OOR + P.OXOR + SL.OSHL + SR.OSHR + B.OPB\n\nSL is the next lower bit of A (shift left), SR the next higher (shift right). Exactly one O* line is 1.",
    );
}

/// The 11 ALU control signals as named inputs.
fn ctrl_inputs(b: &mut Body) -> Vec<Sig> {
    ALU_CTRL.iter().map(|n| b.input(n)).collect()
}

/// ALU4: four ALU bits and a CLA4.
fn alu4(lib: &mut Lib) {
    let kb = lib.kind("alu-bit");
    let kc = lib.kind("cla4");
    let mut b = Body::new();
    let a = inputs(&mut b, "A", 4);
    let rb = inputs(&mut b, "RB", 4);
    let imm = inputs(&mut b, "IMM", 4);
    let din = inputs(&mut b, "DIN", 4);
    let ci = b.input("CI");
    let sl = b.input("SL");
    let sr = b.input("SR");
    let ctl = ctrl_inputs(&mut b);
    let bits: Vec<H> = (0..4).map(|_| b.add(kb, 1)).collect();
    let cla = b.add(kc, 2);
    b.gap(cla, 6);
    for i in 0..4 {
        let l = if i == 0 { sl } else { a[i - 1] };
        let r = if i == 3 { sr } else { a[i + 1] };
        b.feed(bits[i], 0, &[a[i], rb[i], imm[i], din[i]]);
        b.feed(bits[i], 5, &[l, r]);
        b.feed(bits[i], 7, &ctl);
    }
    b.connect(ci, bits[0], 4);
    b.connect(ci, cla, 0);
    for i in 0..4 {
        let g = b.out(bits[i], 1);
        let p = b.out(bits[i], 2);
        b.connect(g, cla, u(1 + 2 * i));
        b.connect(p, cla, u(2 + 2 * i));
    }
    for i in 1..4 {
        let c = b.out(cla, u(i - 1));
        b.connect(c, bits[i], 4);
    }
    for (i, &h) in bits.iter().enumerate() {
        let y = b.out(h, 0);
        b.output(&format!("Y{i}"), y);
    }
    let (g, p) = (b.out(cla, 3), b.out(cla, 4));
    b.output("G", g);
    b.output("P", p);
    lib.define(
        "alu4",
        "ALU4",
        b,
        "ALU4 - 4-bit ALU nibble\n\nFour ALU bits with a CLA4 computing the carries into bits 1-3 and the nibble's group G and P. SL / SR are the A bits just below / above the nibble (for shifts).",
    );
}

/// ALU16: four ALU4 nibbles and a CLA4 over them.
fn alu16(lib: &mut Lib) {
    let kn = lib.kind("alu4");
    let kc = lib.kind("cla4");
    let mut b = Body::new();
    let a = inputs(&mut b, "A", 16);
    let rb = inputs(&mut b, "RB", 16);
    let imm = inputs(&mut b, "IMM", 16);
    let din = inputs(&mut b, "DIN", 16);
    let ci = b.input("CI");
    let sl = b.input("SL");
    let sr = b.input("SR");
    let ctl = ctrl_inputs(&mut b);
    let nib: Vec<H> = (0..4).map(|_| b.add(kn, 1)).collect();
    let cla = b.add(kc, 2);
    for k in 0..4 {
        let r = 4 * k..4 * k + 4;
        b.feed(nib[k], 0, &a[r.clone()]);
        b.feed(nib[k], 4, &rb[r.clone()]);
        b.feed(nib[k], 8, &imm[r.clone()]);
        b.feed(nib[k], 12, &din[r]);
        let l = if k == 0 { sl } else { a[4 * k - 1] };
        let rr = if k == 3 { sr } else { a[4 * k + 4] };
        b.feed(nib[k], 17, &[l, rr]);
        b.feed(nib[k], 19, &ctl);
    }
    b.connect(ci, nib[0], 16);
    b.connect(ci, cla, 0);
    for k in 0..4 {
        let (g, p) = (b.out(nib[k], 4), b.out(nib[k], 5));
        b.connect(g, cla, u(1 + 2 * k));
        b.connect(p, cla, u(2 + 2 * k));
    }
    for k in 1..4 {
        let c = b.out(cla, u(k - 1));
        b.connect(c, nib[k], 16);
    }
    for k in 0..4 {
        for i in 0..4 {
            let y = b.out(nib[k], u(i));
            b.output(&format!("Y{}", 4 * k + i), y);
        }
    }
    let (g, p) = (b.out(cla, 3), b.out(cla, 4));
    b.output("G", g);
    b.output("P", p);
    lib.define(
        "alu16",
        "ALU16",
        b,
        "ALU16 - 16-bit ALU\n\nFour ALU4 nibbles; a CLA4 over their group G/P gives the carries into bits 4, 8 and 12 and the 16-bit group G and P (second lookahead level).",
    );
}

/// ALU32: two ALU16 halves, the carry into bit 16, the slice G/P and the
/// zero detector.
fn alu32(lib: &mut Lib) {
    let kh = lib.kind("alu16");
    let mut b = Body::new();
    let a = inputs(&mut b, "A", 32);
    let rb = inputs(&mut b, "RB", 32);
    let imm = inputs(&mut b, "IMM", 16);
    let immx = b.input("IMMX");
    let din = inputs(&mut b, "DIN", 32);
    let ci = b.input("CI");
    let sl = b.input("SL");
    let sr = b.input("SR");
    let ctl = ctrl_inputs(&mut b);
    let half: Vec<H> = (0..2).map(|_| b.add(kh, 1)).collect();
    b.gap(half[1], 8);
    for k in 0..2 {
        let r = 16 * k..16 * k + 16;
        b.feed(half[k], 0, &a[r.clone()]);
        b.feed(half[k], 16, &rb[r.clone()]);
        if k == 0 {
            b.feed(half[k], 32, &imm);
        } else {
            b.feed(half[k], 32, &[immx; 16]);
        }
        b.feed(half[k], 48, &din[r]);
        let l = if k == 0 { sl } else { a[15] };
        let rr = if k == 1 { sr } else { a[16] };
        b.feed(half[k], 65, &[l, rr]);
        b.feed(half[k], 67, &ctl);
    }
    b.connect(ci, half[0], 64);
    let (g0, p0) = (b.out(half[0], 16), b.out(half[0], 17));
    let (g1, p1) = (b.out(half[1], 16), b.out(half[1], 17));
    let t = b.gate(And, &[p0, ci], 2);
    let c16 = b.gate(Or, &[g0, t], 3);
    b.connect(c16, half[1], 64);
    let t = b.gate(And, &[p1, g0], 2);
    let g = b.gate(Or, &[g1, t], 3);
    let p = b.gate(And, &[p1, p0], 2);
    let ys: Vec<Sig> = (0..32).map(|i| b.out(half[i / 16], u(i % 16))).collect();
    let ors: Vec<Sig> = (0..4).map(|k| b.gate(Or, &ys[8 * k..8 * k + 8], 2)).collect();
    let z = b.gate(Nor, &ors, 3);
    for (i, &y) in ys.iter().enumerate() {
        b.output(&format!("Y{i}"), y);
    }
    b.output("G", g);
    b.output("P", p);
    b.output("Z", z);
    lib.define(
        "alu32",
        "ALU32",
        b,
        "ALU32 - 32-bit ALU of one CPU slice\n\nTwo ALU16 halves. C16 = G(lo) + P(lo).CI; the slice generate G = G(hi) + P(hi).G(lo) and propagate P = P(hi).P(lo) go to the board's CLA4, which returns the slice's carry in. Z = 1 when all 32 result bits are 0. IMMX feeds bits 16-31 of the immediate (the sign extension).",
    );
}

/// `RegN`: `n` D flip-flops with a load enable (D = WE ? D : Q).
fn reg_n(lib: &mut Lib, key: &'static str, name: &str, n: usize) {
    let mut b = Body::new();
    let d = inputs(&mut b, "D", n);
    let we = b.input("WE");
    let clk = b.input("CLK");
    let nwe = b.not(we, 1);
    let ffs: Vec<H> = (0..n).map(|_| b.add(DFF, 4)).collect();
    for i in 0..n {
        let q = b.out(ffs[i], 0);
        let t0 = b.gate(And, &[d[i], we], 2);
        let t1 = b.gate(And, &[q, nwe], 2);
        let dd = b.gate(Or, &[t0, t1], 3);
        b.feed(ffs[i], 0, &[dd, clk]);
    }
    for (i, &f) in ffs.iter().enumerate() {
        let q = b.out(f, 0);
        b.output(&format!("Q{i}"), q);
    }
    lib.define(
        key,
        name,
        b,
        &format!(
            "{name} - {n}-bit register\n\nRising-edge D flip-flops with a load enable: on the rising edge of CLK each Q takes D when WE = 1 and keeps its value when WE = 0 (a 2:1 AND-OR mux in front of each flip-flop)."
        ),
    );
}

/// Reg32: four Reg8.
fn reg32(lib: &mut Lib) {
    let k8 = lib.kind("reg8");
    let mut b = Body::new();
    let d = inputs(&mut b, "D", 32);
    let we = b.input("WE");
    let clk = b.input("CLK");
    let parts: Vec<H> = (0..4).map(|_| b.add(k8, 1)).collect();
    for k in 0..4 {
        b.feed(parts[k], 0, &d[8 * k..8 * k + 8]);
        b.feed(parts[k], 8, &[we, clk]);
    }
    for k in 0..4 {
        for i in 0..8 {
            let q = b.out(parts[k], u(i));
            b.output(&format!("Q{}", 8 * k + i), q);
        }
    }
    lib.define(
        "reg32",
        "Reg32",
        b,
        "Reg32 - 32-bit register\n\nFour Reg8 sharing WE and CLK: Q0-Q31 load D0-D31 on the rising edge of CLK when WE = 1.",
    );
}

/// `Dec3/7`: 3-to-8 decoder with enable, output 0 omitted.
fn dec3(lib: &mut Lib) {
    let mut b = Body::new();
    let a = inputs(&mut b, "A", 3);
    let en = b.input("EN");
    let na: Vec<Sig> = a.iter().map(|&s| b.not(s, 1)).collect();
    for k in 1..8usize {
        let bits: Vec<Sig> = (0..3).map(|i| if k >> i & 1 == 1 { a[i] } else { na[i] }).collect();
        let y = b.gate(And, &[bits[0], bits[1], bits[2], en], 2);
        b.output(&format!("Y{k}"), y);
    }
    lib.define(
        "dec3",
        "Dec3/7",
        b,
        "Dec3/7 - 3-to-8 decoder with enable\n\nYk = 1 when A2 A1 A0 = k and EN = 1, for k = 1..7. There is no Y0: register r0 is the constant 0, so nothing selects it.",
    );
}

/// `RdMux`: one bit of both register-file read ports (7:1 AND-OR each).
fn rdmux(lib: &mut Lib) {
    let mut b = Body::new();
    let q: Vec<Sig> = (1..8).map(|k| b.input(&format!("Q{k}"))).collect();
    let sa: Vec<Sig> = (1..8).map(|k| b.input(&format!("SA{k}"))).collect();
    let sb: Vec<Sig> = (1..8).map(|k| b.input(&format!("SB{k}"))).collect();
    let ta: Vec<Sig> = (0..7).map(|k| b.gate(And, &[q[k], sa[k]], 1)).collect();
    let tb: Vec<Sig> = (0..7).map(|k| b.gate(And, &[q[k], sb[k]], 1)).collect();
    let ya = b.gate(Or, &ta, 2);
    let yb = b.gate(Or, &tb, 2);
    b.output("A", ya);
    b.output("B", yb);
    lib.define(
        "rdmux",
        "RdMux",
        b,
        "RdMux - one bit of the two register-file read ports\n\nA = OR of Qk.SAk, B = OR of Qk.SBk for registers k = 1..7 (one-hot selects from the read decoders; no select = register r0 = 0).",
    );
}

/// `RdMux8`: eight bits of both read ports (eight `RdMux` bits).
fn rdmux8(lib: &mut Lib) {
    let km = lib.kind("rdmux");
    let mut b = Body::new();
    // Register-major inputs: r1 bits 0-7, r2 bits 0-7, ...
    let q: Vec<Vec<Sig>> = (1..8).map(|r| (0..8).map(|i| b.input(&format!("R{r}.{i}"))).collect()).collect();
    let sa: Vec<Sig> = (1..8).map(|k| b.input(&format!("SA{k}"))).collect();
    let sb: Vec<Sig> = (1..8).map(|k| b.input(&format!("SB{k}"))).collect();
    let bits: Vec<H> = (0..8).map(|_| b.add(km, 1)).collect();
    for (i, &m) in bits.iter().enumerate() {
        let qs: Vec<Sig> = q.iter().map(|r| r[i]).collect();
        b.feed(m, 0, &qs);
        b.feed(m, 7, &sa);
        b.feed(m, 14, &sb);
    }
    for (i, &m) in bits.iter().enumerate() {
        let y = b.out(m, 0);
        b.output(&format!("A{i}"), y);
    }
    for (i, &m) in bits.iter().enumerate() {
        let y = b.out(m, 1);
        b.output(&format!("B{i}"), y);
    }
    lib.define(
        "rdmux8",
        "RdMux8",
        b,
        "RdMux8 - eight bits of the two register-file read ports\n\nEight RdMux bits sharing the one-hot selects SA1-SA7 (port A) and SB1-SB7 (port B). Inputs Rk.i are bit i of register rk.",
    );
}

/// `RegFile32`: 32 bits of the eight registers (r0 = 0), one write port and
/// two read ports, with hex displays of every register.
fn regfile32(lib: &mut Lib) {
    let kr = lib.kind("reg32");
    let km = lib.kind("rdmux8");
    let kd = lib.kind("dec3");
    let mut b = Body::new();
    b.title("REGISTER FILE (32-bit lane of r1-r7)");
    let d = inputs(&mut b, "D", 32);
    let we = b.input("WE");
    let clk = b.input("CLK");
    let rd = inputs(&mut b, "RD", 3);
    let ra = inputs(&mut b, "RA", 3);
    let rbs = inputs(&mut b, "RB", 3);
    let vcc = b.add(ComponentKind::Const { high: true }, 1);
    let vcc = b.out(vcc, 0);
    let wdec = inst(&mut b, kd, 1, &[rd[0], rd[1], rd[2], we]);
    b.label(wdec, "WRITE", 12, Color::Yellow);
    let adec = inst(&mut b, kd, 1, &[ra[0], ra[1], ra[2], vcc]);
    b.label(adec, "READ A", 12, Color::Yellow);
    let bdec = inst(&mut b, kd, 1, &[rbs[0], rbs[1], rbs[2], vcc]);
    b.label(bdec, "READ B", 12, Color::Yellow);
    let regs: Vec<H> = (0..7).map(|_| b.add(kr, 2)).collect();
    for (k, &r) in regs.iter().enumerate() {
        b.feed(r, 0, &d);
        let w = b.out(wdec, u(k));
        b.feed(r, 32, &[w, clk]);
        b.label(r, &format!("r{}", k + 1), 20, Color::Cyan);
    }
    // Displays: high and low halves of each register lane.
    for (k, &r) in regs.iter().enumerate() {
        for half in [1usize, 0] {
            let h = b.add(HEX16, 3);
            b.color(h, Color::Cyan);
            let qs: Vec<Sig> = (0..16).map(|i| b.out(r, u(16 * half + i))).collect();
            b.feed(h, 0, &qs);
            if half == 1 {
                b.label(h, &format!("r{} [31:16]  [15:0]", k + 1), 12, Color::Cyan);
            }
        }
    }
    let muxes: Vec<H> = (0..4).map(|_| b.add(km, 3)).collect();
    b.label(muxes[0], "READ PORTS A / B", 14, Color::Yellow);
    b.gap(muxes[0], 10);
    for (j, &m) in muxes.iter().enumerate() {
        for (k, &r) in regs.iter().enumerate() {
            let qs: Vec<Sig> = (0..8).map(|i| b.out(r, u(8 * j + i))).collect();
            b.feed(m, u(8 * k), &qs);
        }
        let sa: Vec<Sig> = (0..7).map(|k| b.out(adec, u(k))).collect();
        let sb: Vec<Sig> = (0..7).map(|k| b.out(bdec, u(k))).collect();
        b.feed(m, 56, &sa);
        b.feed(m, 63, &sb);
    }
    for i in 0..32 {
        let y = b.out(muxes[i / 8], u(i % 8));
        b.output(&format!("A{i}"), y);
    }
    for i in 0..32 {
        let y = b.out(muxes[i / 8], u(8 + i % 8));
        b.output(&format!("B{i}"), y);
    }
    lib.define(
        "regfile32",
        "RegFile32",
        b,
        "RegFile32 - one 32-bit lane of the register file\n\nRegisters r1-r7 (Reg32), a write decoder (RD, WE) and two read decoders (RA, RB) driving four RdMux8. Register r0 has no storage: reading it gives 0, writing it does nothing. The hex displays show every register's 32 bits live (enter a running slice to see them).",
    );
}

/// Slice32: one 32-bit lane of the CPU datapath (register file + ALU).
fn slice32(lib: &mut Lib) {
    let kf = lib.kind("regfile32");
    let ka = lib.kind("alu32");
    let mut b = Body::new();
    b.title("W-128 CPU SLICE (32 bits)");
    let clk = b.input("CLK");
    let ra = inputs(&mut b, "RA", 3);
    let rbs = inputs(&mut b, "RB", 3);
    let rd = inputs(&mut b, "RD", 3);
    let we = b.input("WE");
    let ctl = ctrl_inputs(&mut b);
    let cin = b.input("CIN");
    let sli = b.input("SLI");
    let sri = b.input("SRI");
    let imm = inputs(&mut b, "IMM", 16);
    let immx = b.input("IMMX");
    let din = inputs(&mut b, "DIN", 32);
    let rf = b.add(kf, 1);
    let alu = b.add(ka, 2);
    // RegFile inputs: D0-31, WE, CLK, RD0-2, RA0-2, RB0-2.
    b.feed(rf, 32, &[we, clk]);
    b.feed(rf, 34, &rd);
    b.feed(rf, 37, &ra);
    b.feed(rf, 40, &rbs);
    // ALU inputs: A0-31, RB0-31, IMM0-15, IMMX, DIN0-31, CI, SL, SR, ctl.
    let av: Vec<Sig> = (0..32).map(|i| b.out(rf, u(i))).collect();
    let bv: Vec<Sig> = (0..32).map(|i| b.out(rf, u(32 + i))).collect();
    b.feed(alu, 0, &av);
    b.feed(alu, 32, &bv);
    b.feed(alu, 64, &imm);
    b.feed(alu, 80, &[immx]);
    b.feed(alu, 81, &din);
    b.feed(alu, 113, &[cin, sli, sri]);
    b.feed(alu, 116, &ctl);
    let ys: Vec<Sig> = (0..32).map(|i| b.out(alu, u(i))).collect();
    b.feed(rf, 0, &ys);
    // Live displays of this slice's operands and result.
    for (name, bus, col, color) in [("A", &av, 2, Color::Yellow), ("B", &bv, 2, Color::Orange)] {
        for half in [1usize, 0] {
            let h = b.add(HEX16, col);
            b.color(h, color);
            b.feed(h, 0, &bus[16 * half..16 * half + 16]);
            if half == 1 {
                b.label(h, &format!("{name} [31:16] [15:0]"), 14, color);
            }
        }
    }
    for (i, &y) in ys.iter().enumerate() {
        b.output(&format!("Y{i}"), y);
    }
    for (i, &v) in bv.iter().enumerate() {
        b.output(&format!("DOUT{i}"), v);
    }
    let (g, p, z) = (b.out(alu, 32), b.out(alu, 33), b.out(alu, 34));
    b.output("G", g);
    b.output("P", p);
    b.output("Z", z);
    b.output("A0", av[0]);
    b.output("A31", av[31]);
    for half in [1usize, 0] {
        let h = b.add(HEX16, LAST);
        b.color(h, Color::Cyan);
        b.feed(h, 0, &ys[16 * half..16 * half + 16]);
        if half == 1 {
            b.label(h, "Y [31:16] [15:0]", 14, Color::Cyan);
        }
    }
    lib.define(
        "slice32",
        "W128 SLICE32",
        b,
        "W128 SLICE32 - 32-bit CPU slice\n\nFour slices side by side make the 128-bit datapath (a bit-slice design, like the bit-slice processors of the 1970s). Each holds bits 32k..32k+31 of all registers (RegFile32) and of the ALU (ALU32).\n\n- Y: ALU result (written back to register RD when WE = 1)\n- DOUT: register RB (store data)\n- G / P: slice carry generate / propagate, to the board's CLA4; CIN comes back from it\n- A0 / A31: lowest / highest bit of operand A, the shift-in of the neighbouring slices (SLI / SRI)\n- Z: the 32 result bits are all 0",
    );
}

/// Latch16: 16 transparent D latches (the OUT display register).
fn latch16(lib: &mut Lib) {
    let mut b = Body::new();
    let d = inputs(&mut b, "D", 16);
    let le = b.input("LE");
    for (i, &di) in d.iter().enumerate() {
        let h = b.add(DLATCH, 1);
        b.feed(h, 0, &[di, le]);
        let q = b.out(h, 0);
        b.output(&format!("Q{i}"), q);
    }
    lib.define(
        "latch16",
        "Latch16",
        b,
        "Latch16 - 16-bit transparent latch\n\nWhile LE = 1 each Q follows D; when LE falls Q keeps the last value. Eight of them form the 128-bit OUT register of the I/O card.",
    );
}

/// PC8: the program counter (PC + INC, or T when LD; 0 on RST).
fn pc8(lib: &mut Lib) {
    let mut b = Body::new();
    let clk = b.input("CLK");
    let rst = b.input("RST");
    let inc = b.input("INC");
    let ld = b.input("LD");
    let t = inputs(&mut b, "T", 8);
    let nrst = b.not(rst, 1);
    let nld = b.not(ld, 1);
    let ffs: Vec<H> = (0..8).map(|_| b.add(DFF, 7)).collect();
    let mut carry = inc;
    for i in 0..8 {
        let q = b.out(ffs[i], 0);
        let sum = b.gate(Xor, &[q, carry], 2);
        if i < 7 {
            carry = b.gate(And, &[q, carry], 2);
        }
        let t0 = b.gate(And, &[sum, nld], 3);
        let t1 = b.gate(And, &[t[i], ld], 3);
        let m = b.gate(Or, &[t0, t1], 4);
        let dd = b.gate(And, &[m, nrst], 5);
        b.feed(ffs[i], 0, &[dd, clk]);
    }
    for (i, &f) in ffs.iter().enumerate() {
        let q = b.out(f, 0);
        b.output(&format!("PC{i}"), q);
    }
    lib.define(
        "pc8",
        "PC8",
        b,
        "PC8 - 8-bit program counter\n\nOn the rising edge of CLK: PC = 0 if RST, else T if LD (jump), else PC + INC (an incrementer whose carry in is INC).",
    );
}

/// The control unit: IR, PC, MAR, flags, state machine and decoder.
#[allow(clippy::too_many_lines, reason = "the decoder equations, one per control line")]
fn control_unit(lib: &mut Lib) {
    let k32 = lib.kind("reg32");
    let k10 = lib.kind("reg10");
    let k3 = lib.kind("reg3");
    let kpc = lib.kind("pc8");
    let kd = lib.kind("dec3");
    let mut b = Body::new();
    b.title("W-128 CONTROL UNIT");
    let clk = b.input("CLK");
    let rst = b.input("RST");
    let ins = inputs(&mut b, "I", 32);
    let yin = inputs(&mut b, "Y", 10);
    let zs = inputs(&mut b, "Z", 4);
    let c128 = b.input("C128");
    let a0 = b.input("A0");
    let a127 = b.input("A127");
    let nin = b.input("N");

    // Registers in column 1: IR, state, flags, MAR, PC.
    let ir = b.add(k32, 1);
    b.label(ir, "IR", 20, Color::Cyan);
    b.feed(ir, 0, &ins);
    let s0 = b.add(DFF, 1);
    b.label(s0, "STATE", 14, Color::Yellow);
    let s1 = b.add(DFF, 1);
    let flags = b.add(k3, 1);
    b.label(flags, "FLAGS Z C N", 14, Color::Yellow);
    let mar = b.add(k10, 1);
    b.label(mar, "MAR", 20, Color::Cyan);
    b.feed(mar, 0, &yin);
    let pc = b.add(kpc, 1);
    b.label(pc, "PC", 20, Color::Cyan);
    let irq: Vec<Sig> = (0..32).map(|i| b.out(ir, u(i))).collect();
    let (q0, nq0) = (b.out(s0, 0), b.out(s0, 1));
    let (q1, nq1) = (b.out(s1, 0), b.out(s1, 1));
    let (fz, fc, fnn) = (b.out(flags, 0), b.out(flags, 1), b.out(flags, 2));
    let (o0, o1, o2, o3, o4) = (irq[27], irq[28], irq[29], irq[30], irq[31]);

    // Decode (levelized from column 2).
    let is_f = b.gate(And, &[nq1, nq0], 2);
    let is_e = b.gate(And, &[nq1, q0], 2);
    let is_m = b.gate(And, &[q1, nq0], 2);
    let is_h = b.gate(And, &[q1, q0], 2);
    let no4 = b.not(o4, 2);
    let no3 = b.not(o3, 2);
    let no0 = b.not(o0, 2);
    let nrst = b.not(rst, 2);
    let nclk = b.not(clk, 2);
    let nfz = b.not(fz, 2);
    let nfc = b.not(fc, 2);
    let nfn = b.not(fnn, 2);
    let vcc = b.add(ComponentKind::Const { high: true }, 1);
    let vcc = b.out(vcc, 0);
    let fdec = inst(&mut b, kd, 2, &[o0, o1, o2, vcc]);
    let d: Vec<Sig> = (0..7).map(|k| b.out(fdec, u(k))).collect(); // d[k-1] = func k
    let fzero = b.gate(Nor, &[o2, o1, o0], 2);
    let nfzero = b.gate(Or, &[o2, o1, o0], 2);
    let cls0 = b.gate(Nor, &[o4, o3], 3);
    let cls2 = b.gate(And, &[o4, no3], 3);
    let cls3 = b.gate(And, &[o4, o3], 3);
    let cls_imm_mem = b.gate(Xor, &[o4, o3], 3);
    let aluop = b.gate(And, &[no4, nfzero], 3);
    let is_halt = b.gate(And, &[cls0, fzero], 4);
    let is_ld = b.gate(And, &[cls2, no0], 4);
    let is_st = b.gate(And, &[cls2, o0], 4);
    // Next state: F → E; E → H (HALT), M (LD/ST) or F; M → F; H → H.
    let e_halt = b.g(And, &[is_e, is_halt]);
    let ns0 = b.g(Or, &[is_f, e_halt, is_h]);
    let ns0 = b.g(And, &[ns0, nrst]);
    let mh = b.g(Or, &[cls2, is_halt]);
    let e_mh = b.g(And, &[is_e, mh]);
    let ns1 = b.g(Or, &[e_mh, is_h]);
    let ns1 = b.g(And, &[ns1, nrst]);
    b.feed(s0, 0, &[ns0, clk]);
    b.feed(s1, 0, &[ns1, clk]);
    b.feed(ir, 32, &[is_f, clk]);
    // Jumps: condition from the flags.
    let c1 = b.gate(And, &[d[0], fz], 3);
    let c2 = b.gate(And, &[d[1], nfz], 3);
    let c3 = b.gate(And, &[d[2], fc], 3);
    let c4 = b.gate(And, &[d[3], nfc], 3);
    let c5 = b.gate(And, &[d[4], fnn], 3);
    let c6 = b.gate(And, &[d[5], nfn], 3);
    let cond = b.gate(Or, &[fzero, c1, c2, c3, c4, c5, c6], 4);
    let jump = b.g(And, &[is_e, cls3, cond]);
    b.feed(pc, 0, &[clk, rst, is_f, jump]);
    b.feed(pc, 4, &irq[0..8]);
    // Datapath control.
    let we_alu = b.g(And, &[is_e, aluop]);
    let smem = b.g(And, &[is_m, is_ld]);
    let we = b.g(Or, &[we_alu, smem]);
    let simm = b.g(And, &[is_e, cls_imm_mem]);
    let sreg = b.g(Nor, &[simm, smem]);
    let sub = b.g(And, &[is_e, no4, d[1]]);
    let addsub = b.gate(Or, &[d[0], d[1]], 3);
    let arith = b.g(And, &[no4, addsub]);
    let sum_or_mem = b.g(Or, &[arith, cls2]);
    let osum = b.g(And, &[is_e, sum_or_mem]);
    let ologic: Vec<Sig> = (2..7).map(|k| b.g(And, &[is_e, no4, d[k]])).collect();
    // Flags.
    let tc = b.g(And, &[c128, addsub]);
    let tl = b.g(And, &[a127, d[5]]);
    let tr = b.g(And, &[a0, d[6]]);
    let cnew = b.g(Or, &[tc, tl, tr]);
    let znew = b.gate(And, &zs, 1);
    b.feed(flags, 0, &[znew, cnew, nin, we_alu, clk]);
    // MAR and the write strobe.
    let mle = b.g(And, &[is_e, cls2]);
    b.feed(mar, 10, &[mle, clk]);
    let wstb = b.g(And, &[is_m, is_st, nclk]);

    let pcq: Vec<Sig> = (0..8).map(|i| b.out(pc, u(i))).collect();
    for (i, &s) in pcq.iter().enumerate() {
        b.output(&format!("PC{i}"), s);
    }
    for i in 0..10 {
        let s = b.out(mar, u(i));
        b.output(&format!("MAR{i}"), s);
    }
    for (name, base) in [("RA", 21), ("RB", 18), ("RD", 24)] {
        for i in 0..3 {
            b.output(&format!("{name}{i}"), irq[base + i]);
        }
    }
    b.output("WE", we);
    let ctl = [sreg, simm, smem, sub, osum, ologic[0], ologic[1], ologic[2], ologic[3], ologic[4], smem];
    for (name, &s) in ALU_CTRL.iter().zip(&ctl) {
        b.output(name, s);
    }
    b.output("WSTB", wstb);
    for (i, &s) in irq.iter().enumerate() {
        b.output(&format!("IR{i}"), s);
    }
    b.output("FETCH", is_f);
    b.output("EXEC", is_e);
    b.output("MEM", is_m);
    b.output("HALT", is_h);
    b.output("FZ", fz);
    b.output("FC", fc);
    b.output("FN", fnn);
    lib.define(
        "cu",
        "W128 CONTROL UNIT",
        b,
        "W128 CONTROL UNIT - hardwired control\n\nState machine (two flip-flops): FETCH (IR = ROM[PC], PC = PC + 1) -> EXEC -> (LD/ST: MEM) -> FETCH; HALT -> HALT. RST (synchronous) returns to FETCH at PC 0.\n\nEXEC: ALU instructions write RD and the flags; LD/ST compute ra + imm into MAR; jumps load PC with imm[7:0] when the condition holds. MEM: LD writes the memory word into RD; ST pulses WSTB during the low half of the clock.\n\nOutputs: register selects (RA, RB, RD from IR), WE, the ALU controls (operand select SREG/SIMM/SMEM, SUB, one-hot operation OSUM..OPB), PC, MAR, WSTB, IR and the state / flag lines for the LEDs.",
    );
}

/// Builds every definition into a fresh table.
#[must_use]
pub fn library() -> Lib {
    let mut lib = Lib::new();
    cla4(&mut lib);
    alu_bit(&mut lib);
    alu4(&mut lib);
    alu16(&mut lib);
    alu32(&mut lib);
    reg_n(&mut lib, "reg8", "Reg8", 8);
    reg_n(&mut lib, "reg10", "Reg10", 10);
    reg_n(&mut lib, "reg3", "Reg3", 3);
    reg32(&mut lib);
    dec3(&mut lib);
    rdmux(&mut lib);
    rdmux8(&mut lib);
    regfile32(&mut lib);
    slice32(&mut lib);
    latch16(&mut lib);
    pc8(&mut lib);
    control_unit(&mut lib);
    lib
}
