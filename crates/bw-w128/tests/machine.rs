//! The whole W-128 board against the reference model of the ISA.
//!
//! * The bundled example is exactly what the generator produces, and its
//!   demo runs to HALT with RAM, OUT register, registers, flags and cycle
//!   count bit-exact with the model.
//! * Random programs (random 128-bit operands, every ALU operation, loads and
//!   stores across both RAM ranks and the I/O space, taken and untaken
//!   jumps) run in lockstep with the model: PC, flags and all registers are
//!   compared after every instruction.

#![allow(
    clippy::unwrap_used,
    clippy::many_single_char_names,
    clippy::format_push_string,
    clippy::cast_possible_truncation
)]

mod common;

use bw_w128::asm::assemble;
use bw_w128::board::{board, demo_board, project_text};
use bw_w128::isa::{Machine, RAM_WORDS, encode, op};
use bw_w128::program::{DEMO, factorial, fibonacci};
use bw_w128::run::Board;
use common::Rng;

fn example_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/w128-computer.bitwerk")
}

/// `BITWERK_REGEN_W128=1 cargo test -p bw-w128 --test machine` rewrites the
/// example (as does `cargo run -p bw-w128 --bin w128-gen`).
#[test]
fn example_is_up_to_date_and_runs_the_demo() {
    let text = project_text().unwrap();
    let path = example_path();
    if std::env::var_os("BITWERK_REGEN_W128").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    assert!(std::fs::read_to_string(&path).unwrap_or_default() == text, "regenerate the example (w128-gen)");

    let prog = assemble(DEMO).unwrap();
    let mut m = Machine::new(&prog.words, &[]);
    m.run(100_000).unwrap();
    assert_eq!(m.ram[186], fibonacci(186));
    assert_eq!(m.ram[186], 332_825_110_087_067_562_321_196_029_789_634_457_848);
    assert_eq!(m.ram[0x100 + 34], factorial(34));
    assert_eq!(m.out, factorial(34));
    for i in 0..187 {
        assert_eq!(m.ram[i], fibonacci(i), "model F({i})");
    }

    // The file as the app opens it.
    let loaded = bw_io::load_from_bytes(text.as_bytes()).unwrap();
    let (_, ids, stats) = demo_board().unwrap();
    assert_eq!(stats.conflicts, 0, "layout without shared rows");
    let mut b = Board::new(loaded.circuit, ids).unwrap();
    assert!(b.run_to_halt(m.cycles + 4).unwrap(), "halts");
    assert_eq!(b.cycles, m.cycles, "cycle count");
    for a in 0..RAM_WORDS {
        assert_eq!(b.ram(a), m.ram[a], "RAM[{a:#05x}]");
    }
    assert_eq!(b.out_register(), Some(m.out), "OUT register");
    assert_eq!(b.flags(), Some((m.z, m.c, m.n)), "flags");
    for r in 1..8 {
        assert_eq!(b.reg(r), Some(m.regs[r]), "r{r}");
    }
    // Halted means halted: more clocks change nothing.
    for _ in 0..20 {
        b.tick().unwrap();
    }
    assert!(b.halted());
    assert_eq!(b.reg(7), Some(m.regs[7]));
}

/// A random straight-line program with forward jumps.
fn random_program(rng: &mut Rng, len: usize) -> Vec<u32> {
    let mut p = Vec::new();
    // Load a few operands first.
    for r in 1..8u8 {
        p.push(encode(op::LD, r, 0, 0, rng.below(32) as u16));
    }
    while p.len() < len {
        let rd = rng.below(8) as u8;
        let ra = rng.below(8) as u8;
        let rb = rng.below(8) as u8;
        let w = match rng.below(12) {
            0..=4 => encode(1 + rng.below(7) as u8, rd, ra, rb, 0),
            5..=6 => encode(op::ADDI + rng.below(5) as u8, rd, ra, 0, rng.next() as u16),
            7 => encode(op::LD, rd, 0, 0, rng.below(RAM_WORDS as u64) as u16),
            8 => {
                // Any base register: RAM in either rank, or the I/O space.
                encode(op::ST, 0, ra, rb, rng.next() as u16)
            }
            9 => encode(op::ST, 0, 0, rb, 0x200 + rng.below(0x200) as u16),
            10 if p.len() + 2 < len => {
                let target = (p.len() + 2) as u16;
                encode(op::JMP + rng.below(8) as u8, 0, 0, 0, target)
            }
            _ => encode(op::NOP, 0, 0, 0, 0),
        };
        p.push(w);
    }
    p.push(encode(op::HALT, 0, 0, 0, 0));
    p
}

#[test]
fn random_programs_run_in_lockstep_with_the_model() {
    let mut rng = Rng(0x00c0_ffee_1234_5678);
    let (base, ids, _) = board(&[], &[]);
    for round in 0..3 {
        let prog = random_program(&mut rng, 150);
        let ram: Vec<u128> = (0..64).map(|_| rng.operand()).collect();
        let c = bw_w128::board::load_memories(base.clone(), &ids, &prog, &ram);
        let mut b = Board::new(c, ids.clone()).unwrap();
        let mut m = Machine::new(&prog, &ram);
        let mut n = 0;
        while !m.halted {
            m.step().unwrap();
            n += 1;
            while b.cycles < m.cycles {
                b.tick().unwrap();
            }
            let at = format!(
                "round {round}, instruction {n} ({})",
                bw_w128::asm::disassemble(prog[usize::from(m.pc.wrapping_sub(1))])
            );
            if !m.halted {
                assert_eq!(b.pc(), Some(m.pc), "PC, {at}");
            }
            assert_eq!(b.flags(), Some((m.z, m.c, m.n)), "flags, {at}");
            for r in 1..8 {
                assert_eq!(b.reg(r), Some(m.regs[r]), "r{r}, {at}");
            }
        }
        assert!(b.halted(), "round {round}: halted");
        for a in 0..RAM_WORDS {
            assert_eq!(b.ram(a), m.ram[a], "round {round}: RAM[{a:#05x}]");
        }
        assert_eq!(b.out_register(), Some(m.out), "round {round}: OUT");
        assert!(m.out_writes > 0, "the I/O path was exercised");
    }
}

#[test]
fn every_alu_operation_on_random_128_bit_operands() {
    // r1 = RAM[2k], r2 = RAM[2k+1]; r3 = r1 op r2; RAM[0x100 + 8k + op] = r3.
    let mut rng = Rng(42);
    let mut src = String::new();
    let pairs = 12;
    for k in 0..pairs {
        src.push_str(&format!("ld r1, [r0 + {}]\nld r2, [r0 + {}]\n", 2 * k, 2 * k + 1));
        for (i, o) in ["add", "sub", "and", "or", "xor"].iter().enumerate() {
            src.push_str(&format!("{o} r3, r1, r2\nst r3, [r0 + {}]\n", 0x100 + 8 * k + i));
        }
        src.push_str(&format!("shl r3, r1\nst r3, [r0 + {}]\n", 0x100 + 8 * k + 5));
        src.push_str(&format!("shr r3, r1\nst r3, [r0 + {}]\n", 0x100 + 8 * k + 6));
    }
    src.push_str("halt\n");
    let prog = assemble(&src).unwrap();
    let ram: Vec<u128> = (0..2 * pairs).map(|_| rng.operand()).collect();
    let (c, ids, _) = board(&prog.words, &ram);
    let mut m = Machine::new(&prog.words, &ram);
    m.run(100_000).unwrap();
    let mut b = Board::new(c, ids).unwrap();
    assert!(b.run_to_halt(m.cycles + 4).unwrap());
    for k in 0..pairs {
        let (x, y) = (ram[2 * k], ram[2 * k + 1]);
        let want = [x.wrapping_add(y), x.wrapping_sub(y), x & y, x | y, x ^ y, x << 1, x >> 1];
        for (i, w) in want.iter().enumerate() {
            assert_eq!(b.ram(0x100 + 8 * k + i), *w, "pair {k} op {i}: {x:#x}, {y:#x}");
        }
    }
}

#[test]
fn memory_module_decodes_ranks_and_io() {
    // The same low address in rank 0, rank 1 and the I/O space.
    let src = "
        ldi r1, 0x1111
        ldi r2, 0x2222
        ldi r3, 0x3333
        st  r1, [r0 + 0x05]
        st  r2, [r0 + 0x105]
        st  r3, [r0 + 0x205]
        ld  r4, [r0 + 0x05]
        ld  r5, [r0 + 0x105]
        halt";
    let prog = assemble(src).unwrap();
    let (c, ids, _) = board(&prog.words, &[]);
    let mut b = Board::new(c, ids).unwrap();
    assert!(b.run_to_halt(100).unwrap());
    assert_eq!((b.ram(0x05), b.ram(0x105)), (0x1111, 0x2222));
    assert_eq!(b.out_register(), Some(0x3333), "the I/O write went to OUT, not RAM");
    assert_eq!((b.reg(4), b.reg(5)), (Some(0x1111), Some(0x2222)));
    let written: Vec<usize> = (0..RAM_WORDS).filter(|&a| b.ram(a) != 0).collect();
    assert_eq!(written, vec![0x05, 0x105]);
}

#[test]
fn reset_restarts_the_program() {
    let src = "start: addi r1, r1, 1\n jmp start";
    let prog = assemble(src).unwrap();
    let (c, ids, _) = board(&prog.words, &[]);
    let reset = ids.reset;
    let mut b = Board::new(c, ids).unwrap();
    for _ in 0..40 {
        b.tick().unwrap();
    }
    assert_eq!(b.reg(1), Some(5), "five loop iterations (4 cycles each)");
    b.sim.set_switch(reset, true).unwrap();
    b.tick().unwrap();
    b.tick().unwrap();
    b.sim.set_switch(reset, false).unwrap();
    assert_eq!(b.pc(), Some(0), "PC back to 0");
}

#[test]
fn layout_has_no_false_connections() {
    // No two nets share a wire segment, and no wire node sits on another
    // net's wire: what you see connected is connected.
    let (c, _, _) = demo_board().unwrap();
    let mut problems = bw_w128::layout::overlaps(&c);
    for (_, e) in c.macros().iter() {
        problems.extend(bw_w128::layout::overlaps(&e.def.body).into_iter().map(|p| format!("{}: {p}", e.def.name)));
    }
    assert!(problems.is_empty(), "{} problems, first: {:?}", problems.len(), &problems[..problems.len().min(10)]);
}
