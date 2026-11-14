//! Each W-128 building block on its own, against a Rust model: exhaustive
//! where small (CLA4, decoder), random vectors where wide (ALU32 against
//! u32 arithmetic, register file).

#![allow(
    clippy::unwrap_used,
    clippy::many_single_char_names,
    clippy::verbose_bit_mask,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)]

mod common;

use common::{Bench, Rng};

#[test]
fn cla4_is_exhaustively_correct() {
    let mut b = Bench::new("cla4");
    for v in 0u32..512 {
        b.set_bits(0, 9, u128::from(v));
        let ci = v & 1;
        let g: Vec<u32> = (0..4).map(|i| v >> (1 + 2 * i) & 1).collect();
        let p: Vec<u32> = (0..4).map(|i| v >> (2 + 2 * i) & 1).collect();
        let mut c = vec![ci];
        for i in 0..4 {
            c.push(g[i] | (p[i] & c[i]));
        }
        let gg = g[3] | p[3] & g[2] | p[3] & p[2] & g[1] | p[3] & p[2] & p[1] & g[0];
        let pp = p[0] & p[1] & p[2] & p[3];
        let want = c[1] | c[2] << 1 | c[3] << 2 | gg << 3 | pp << 4;
        assert_eq!(b.bits(0, 5), u128::from(want), "inputs {v:09b}");
    }
}

/// ALU32 pin numbers.
const A: usize = 0;
const RB: usize = 32;
const IMM: usize = 64;
const IMMX: usize = 80;
const DIN: usize = 81;
const CI: usize = 113;
const SL: usize = 114;
const SR: usize = 115;
const CTL: usize = 116;

#[test]
fn alu32_matches_u32_arithmetic_on_random_vectors() {
    let mut b = Bench::new("alu32");
    assert_eq!((b.inputs, b.outputs), (127, 35));
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for n in 0..300 {
        let a = rng.next() as u32;
        let rb = rng.next() as u32;
        let imm = rng.next() as u16;
        let immx = rng.next() & 1 == 1;
        let din = rng.next() as u32;
        let ci = rng.next() & 1;
        let sl = rng.next() & 1;
        let sr = rng.next() & 1;
        let src = rng.below(3) as usize;
        let sub = rng.next() & 3 == 0;
        let op = rng.below(7) as usize;
        b.set_bits(A, 32, u128::from(a));
        b.set_bits(RB, 32, u128::from(rb));
        b.set_bits(IMM, 16, u128::from(imm));
        b.set(IMMX, immx);
        b.set_bits(DIN, 32, u128::from(din));
        b.set_bits(CI, 1, u128::from(ci));
        b.set_bits(SL, 1, u128::from(sl));
        b.set_bits(SR, 1, u128::from(sr));
        for k in 0..3 {
            b.set(CTL + k, k == src);
        }
        b.set(CTL + 3, sub);
        for k in 0..7 {
            b.set(CTL + 4 + k, k == op);
        }
        let bsrc = match src {
            0 => rb,
            1 => u32::from(imm) | if immx { 0xffff_0000 } else { 0 },
            _ => din,
        };
        let bx = if sub { !bsrc } else { bsrc };
        let y = match op {
            0 => a.wrapping_add(bx).wrapping_add(ci as u32),
            1 => a & bx,
            2 => a | bx,
            3 => a ^ bx,
            4 => a << 1 | sl as u32,
            5 => a >> 1 | (sr as u32) << 31,
            _ => bsrc,
        };
        let g = (u64::from(a) + u64::from(bx)) >> 32 == 1;
        let p = a ^ bx == u32::MAX;
        let ctx = format!("vector {n}: a={a:#x} b={bsrc:#x} src={src} sub={sub} op={op} ci={ci}");
        assert_eq!(b.bits(0, 32), u128::from(y), "Y, {ctx}");
        assert_eq!(b.out(32), u8::from(g), "G, {ctx}");
        assert_eq!(b.out(33), u8::from(p), "P, {ctx}");
        assert_eq!(b.out(34), u8::from(y == 0), "Z, {ctx}");
    }
}

#[test]
fn alu32_carry_chain_extremes() {
    // All propagate: a carry in ripples through the lookahead tree.
    let mut b = Bench::new("alu32");
    b.set(CTL, true); // SREG
    b.set(CTL + 4, true); // OSUM
    b.set_bits(A, 32, 0xffff_ffff);
    b.set_bits(RB, 32, 0);
    assert_eq!(b.bits(0, 32), 0xffff_ffff);
    assert_eq!((b.out(32), b.out(33)), (0, 1), "G = 0, P = 1");
    b.set(CI, true);
    assert_eq!(b.bits(0, 32), 0);
    assert_eq!(b.out(34), 1, "Z");
    b.set_bits(RB, 32, 1);
    assert_eq!((b.out(32), b.out(33)), (1, 0), "G = 1, P = 0");
}

#[test]
fn registers_load_and_hold() {
    for (key, n) in [("reg8", 8usize), ("reg3", 3), ("reg10", 10), ("reg32", 32)] {
        let mut b = Bench::new(key);
        let (we, clk) = (n, n + 1);
        let mut rng = Rng(7 + n as u64);
        let mut want = 0u128;
        assert_eq!(b.bits(0, n as u8), 0, "{key} starts at 0");
        for _ in 0..40 {
            let v = rng.u128() & ((1u128 << n) - 1);
            let load = rng.next() & 1 == 1;
            b.set_bits(0, n, v);
            b.set(we, load);
            b.pulse(clk);
            if load {
                want = v;
            }
            assert_eq!(b.bits(0, n as u8), want, "{key}");
            // Changing D without a clock edge changes nothing.
            b.set_bits(0, n, !v & ((1u128 << n) - 1));
            assert_eq!(b.bits(0, n as u8), want, "{key} holds between edges");
        }
    }
}

#[test]
fn decoder_is_exhaustively_correct() {
    let mut b = Bench::new("dec3");
    for v in 0u32..16 {
        b.set_bits(0, 4, u128::from(v));
        let (a, en) = (v & 7, v >> 3 == 1);
        let want = if en && a != 0 { 1u128 << (a - 1) } else { 0 };
        assert_eq!(b.bits(0, 7), want, "A={a} EN={en}");
    }
}

#[test]
fn read_mux_selects_one_register() {
    let mut b = Bench::new("rdmux8");
    let mut rng = Rng(99);
    // Inputs: Q of r1..r7 (8 bits each, register-major), SA1-7, SB1-7.
    let regs: Vec<u128> = (0..7).map(|_| u128::from(rng.next() as u8)).collect();
    for (k, &r) in regs.iter().enumerate() {
        b.set_bits(8 * k, 8, r);
    }
    for sa in 0..8usize {
        for sb in 0..8usize {
            for k in 1..8 {
                b.set(56 + k - 1, k == sa);
                b.set(63 + k - 1, k == sb);
            }
            let val = |s: usize| if s == 0 { 0 } else { regs[s - 1] };
            assert_eq!(b.bits(0, 8), val(sa), "port A selects r{sa}");
            assert_eq!(b.bits(8, 8), val(sb), "port B selects r{sb}");
        }
    }
}

#[test]
fn register_file_writes_and_reads_both_ports() {
    let mut b = Bench::new("regfile32");
    // Inputs: D0-31, WE, CLK, RD0-2, RA0-2, RB0-2. Outputs: A0-31, B0-31.
    let (we, clk, rd, ra, rb) = (32, 33, 34, 37, 40);
    let mut rng = Rng(5);
    let mut regs = [0u128; 8];
    for step in 0..120 {
        let v = u128::from(rng.next() as u32);
        let r = rng.below(8) as usize;
        let write = rng.below(4) != 0;
        b.set_bits(0, 32, v);
        b.set_bits(rd, 3, r as u128);
        b.set(we, write);
        b.pulse(clk);
        if write && r != 0 {
            regs[r] = v;
        }
        let (x, y) = (rng.below(8) as usize, rng.below(8) as usize);
        b.set_bits(ra, 3, x as u128);
        b.set_bits(rb, 3, y as u128);
        assert_eq!(b.bits(0, 32), regs[x], "step {step}: port A r{x}");
        assert_eq!(b.bits(32, 32), regs[y], "step {step}: port B r{y}");
    }
}

#[test]
fn program_counter_counts_jumps_and_resets() {
    let mut b = Bench::new("pc8");
    // Inputs: CLK, RST, INC, LD, T0-7.
    let (clk, rst, inc, ld, t) = (0, 1, 2, 3, 4);
    b.set(inc, true);
    for i in 1..=300u32 {
        b.pulse(clk);
        assert_eq!(b.bits(0, 8), u128::from(i % 256), "increments and wraps");
    }
    b.set(inc, false);
    b.pulse(clk);
    assert_eq!(b.bits(0, 8), u128::from(300u32 % 256), "holds without INC");
    b.set_bits(t, 8, 0xa5);
    b.set(ld, true);
    b.pulse(clk);
    assert_eq!(b.bits(0, 8), 0xa5, "jump");
    b.set(ld, false);
    b.set(rst, true);
    b.pulse(clk);
    assert_eq!(b.bits(0, 8), 0, "synchronous reset");
}

#[test]
fn latch_is_transparent_while_enabled() {
    let mut b = Bench::new("latch16");
    b.set_bits(0, 16, 0x1234);
    assert_eq!(b.bits(0, 16), 0, "closed");
    b.set(16, true);
    assert_eq!(b.bits(0, 16), 0x1234, "open: follows D");
    b.set_bits(0, 16, 0xbeef);
    assert_eq!(b.bits(0, 16), 0xbeef);
    b.set(16, false);
    b.set_bits(0, 16, 0);
    assert_eq!(b.bits(0, 16), 0xbeef, "keeps the last value");
}
