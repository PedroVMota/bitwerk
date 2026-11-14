//! **Andorinha W-128** — a 128-bit computer built from bitwerk gates.
//!
//! This crate generates `examples/w128-computer.bitwerk` deterministically
//! and holds everything needed to check it:
//!
//! * [`isa`]: the instruction set and a cycle-exact reference model;
//! * [`asm`]: a two-pass assembler for the program ROM;
//! * [`program`]: the demo program (128-bit Fibonacci and factorials);
//! * [`layout`]: column placement and channel routing of a netlist into a
//!   bitwerk circuit (macro bodies and the board);
//! * [`blocks`]: the macro definitions (registers, ALU, register file,
//!   32-bit CPU slices, control unit, …);
//! * [`board`]: the mainboard (CPU, program ROM, RAM module, I/O card,
//!   displays) and the project.
//!
//! Run `cargo run --release -p bw-w128 --bin w128-gen` to rewrite the
//! example (it also prints the statistics in the README).

#![forbid(unsafe_code)]
#![allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::needless_range_loop,
    reason = "netlist generator: bit indices and short signal names (a, b, g, p, …) mirror the logic equations"
)]

pub mod asm;
pub mod blocks;
pub mod board;
pub mod isa;
pub mod layout;
pub mod program;
pub mod report;
pub mod run;
