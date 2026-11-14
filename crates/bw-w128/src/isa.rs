//! The Andorinha W-128 instruction set and a cycle-exact reference model.
//!
//! * Eight 128-bit registers `r0`…`r7`; `r0` always reads 0 (writes are
//!   dropped), so it doubles as "discard" for compares.
//! * Harvard memory: a 256 × 32-bit program ROM addressed by the 8-bit PC,
//!   and a 10-bit word-addressed data space of 128-bit words:
//!   `0x000`–`0x1FF` RAM (two ranks of 256 words), `0x200`–`0x3FF` I/O (a
//!   store there writes the 128-bit OUT display register; loads from it read
//!   a floating bus).
//! * Flags Z (result zero), C (carry out / shifted-out bit, 0 for logic
//!   operations) and N (bit 127 of the result), written by every ALU
//!   instruction and by nothing else.
//!
//! Encoding (32 bits): `op[31:27] rd[26:24] ra[23:21] rb[20:18] 0[17:16]
//! imm[15:0]`. The immediate is sign-extended to 128 bits; jump targets are
//! `imm[7:0]`.
//!
//! | op | class | meaning |
//! |---|---|---|
//! | `00000` | ALU reg, func 0 | `HALT` |
//! | `00fff` | ALU reg | `rd ← ra ⊕ rb`, f = 1 ADD, 2 SUB, 3 AND, 4 OR, 5 XOR, 6 SHL (`ra << 1`), 7 SHR (`ra >> 1`) |
//! | `01000` | ALU imm, func 0 | `NOP` |
//! | `01fff` | ALU imm | `rd ← ra ⊕ sext(imm)` (6, 7: shifts, `imm` ignored) |
//! | `10xx0` | memory | `LD rd, [ra + imm]` |
//! | `10xx1` | memory | `ST rb, [ra + imm]` |
//! | `11ccc` | jump | `PC ← imm[7:0]` if c = 0 always, 1 Z, 2 !Z, 3 C, 4 !C, 5 N, 6 !N, 7 never |
//!
//! Timing (one cycle = one clock period): every instruction takes a fetch
//! and an execute cycle; `LD` and `ST` take a third (memory) cycle. `HALT`
//! enters the halt state after its execute cycle and stays there.

use std::fmt;

/// Words of the program ROM.
pub const ROM_WORDS: usize = 256;
/// Words of data RAM (two ranks of 256).
pub const RAM_WORDS: usize = 512;
/// First I/O address (the OUT register).
pub const IO_BASE: u16 = 0x200;
/// Data address mask (10 address lines).
pub const ADDR_MASK: u16 = 0x3ff;

/// Operation codes (the 5-bit `op` field).
pub mod op {
    pub const HALT: u8 = 0x00;
    pub const ADD: u8 = 0x01;
    pub const SUB: u8 = 0x02;
    pub const AND: u8 = 0x03;
    pub const OR: u8 = 0x04;
    pub const XOR: u8 = 0x05;
    pub const SHL: u8 = 0x06;
    pub const SHR: u8 = 0x07;
    pub const NOP: u8 = 0x08;
    pub const ADDI: u8 = 0x09;
    pub const SUBI: u8 = 0x0a;
    pub const ANDI: u8 = 0x0b;
    pub const ORI: u8 = 0x0c;
    pub const XORI: u8 = 0x0d;
    pub const LD: u8 = 0x10;
    pub const ST: u8 = 0x11;
    pub const JMP: u8 = 0x18;
    pub const JZ: u8 = 0x19;
    pub const JNZ: u8 = 0x1a;
    pub const JC: u8 = 0x1b;
    pub const JNC: u8 = 0x1c;
    pub const JN: u8 = 0x1d;
    pub const JNN: u8 = 0x1e;
}

/// Encodes one instruction.
#[must_use]
pub fn encode(op: u8, rd: u8, ra: u8, rb: u8, imm: u16) -> u32 {
    (u32::from(op & 0x1f) << 27)
        | (u32::from(rd & 7) << 24)
        | (u32::from(ra & 7) << 21)
        | (u32::from(rb & 7) << 18)
        | u32::from(imm)
}

/// The fields of an instruction word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    pub op: u8,
    pub rd: usize,
    pub ra: usize,
    pub rb: usize,
    pub imm: u16,
}

#[must_use]
pub fn decode(word: u32) -> Fields {
    #[allow(clippy::cast_possible_truncation, reason = "masked fields")]
    Fields {
        op: (word >> 27) as u8 & 0x1f,
        rd: (word >> 24) as usize & 7,
        ra: (word >> 21) as usize & 7,
        rb: (word >> 18) as usize & 7,
        imm: word as u16,
    }
}

/// Sign-extends a 16-bit immediate to 128 bits.
#[must_use]
pub fn sext(imm: u16) -> u128 {
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss, reason = "two's complement sign extension")]
    {
        i128::from(imm as i16) as u128
    }
}

/// Result and carry of ALU function `f` (1..=7) on `a` and `b`.
#[must_use]
pub fn alu(f: u8, a: u128, b: u128) -> (u128, bool) {
    match f {
        1 => a.overflowing_add(b),
        // a + !b + 1: the carry out is "no borrow".
        2 => {
            let (s, c1) = a.overflowing_add(!b);
            let (s, c2) = s.overflowing_add(1);
            (s, c1 || c2)
        }
        3 => (a & b, false),
        4 => (a | b, false),
        5 => (a ^ b, false),
        6 => (a << 1, a >> 127 == 1),
        7 => (a >> 1, a & 1 == 1),
        _ => (0, false),
    }
}

/// Why the reference model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// A load from the I/O space (reads a floating bus in the hardware).
    IoRead { pc: u8, addr: u16 },
    /// Did not halt within the cycle budget.
    Timeout,
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoRead { pc, addr } => write!(f, "load from I/O address {addr:#05x} at pc {pc:#04x}"),
            Self::Timeout => write!(f, "did not halt"),
        }
    }
}

impl std::error::Error for ModelError {}

/// Architectural state of the machine, stepped one instruction at a time
/// with the hardware's cycle count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "the architectural flags")]
pub struct Machine {
    pub rom: Vec<u32>,
    pub regs: [u128; 8],
    pub pc: u8,
    pub z: bool,
    pub c: bool,
    pub n: bool,
    pub ram: Vec<u128>,
    /// The OUT display register and how many times it was written.
    pub out: u128,
    pub out_writes: u64,
    pub halted: bool,
    /// Clock cycles so far (rising edges of the hardware clock).
    pub cycles: u64,
    pub instructions: u64,
}

impl Machine {
    /// A machine after reset with `program` in ROM and `ram` as the initial
    /// RAM contents (zero-filled).
    #[must_use]
    pub fn new(program: &[u32], ram: &[u128]) -> Self {
        let mut rom = vec![0; ROM_WORDS];
        for (d, s) in rom.iter_mut().zip(program) {
            *d = *s;
        }
        let mut r = vec![0; RAM_WORDS];
        for (d, s) in r.iter_mut().zip(ram) {
            *d = *s;
        }
        Self {
            rom,
            regs: [0; 8],
            pc: 0,
            z: false,
            c: false,
            n: false,
            ram: r,
            out: 0,
            out_writes: 0,
            halted: false,
            cycles: 0,
            instructions: 0,
        }
    }

    fn write(&mut self, rd: usize, v: u128) {
        if rd != 0 {
            self.regs[rd] = v;
        }
    }

    /// Executes one instruction (no-op once halted).
    ///
    /// # Errors
    /// [`ModelError::IoRead`] for a load from the I/O space.
    pub fn step(&mut self) -> Result<(), ModelError> {
        if self.halted {
            return Ok(());
        }
        let pc = self.pc;
        let i = decode(self.rom[usize::from(pc)]);
        self.pc = pc.wrapping_add(1);
        self.cycles += 2;
        self.instructions += 1;
        let f = i.op & 7;
        let a = self.regs[i.ra];
        match i.op >> 3 {
            0 | 1 if f == 0 => {
                if i.op == op::HALT {
                    self.halted = true;
                }
            }
            0 | 1 => {
                let b = if i.op >> 3 == 0 { self.regs[i.rb] } else { sext(i.imm) };
                let (y, c) = alu(f, a, b);
                self.write(i.rd, y);
                self.z = y == 0;
                self.c = c;
                self.n = y >> 127 == 1;
            }
            2 => {
                self.cycles += 1;
                #[allow(clippy::cast_possible_truncation, reason = "the address register is 10 bits")]
                let addr = (a.wrapping_add(sext(i.imm)) as u16) & ADDR_MASK;
                if i.op & 1 == 0 {
                    if addr >= IO_BASE {
                        return Err(ModelError::IoRead { pc, addr });
                    }
                    let v = self.ram[usize::from(addr)];
                    self.write(i.rd, v);
                } else if addr >= IO_BASE {
                    self.out = self.regs[i.rb];
                    self.out_writes += 1;
                } else {
                    self.ram[usize::from(addr)] = self.regs[i.rb];
                }
            }
            _ => {
                let take = match f {
                    0 => true,
                    1 => self.z,
                    2 => !self.z,
                    3 => self.c,
                    4 => !self.c,
                    5 => self.n,
                    6 => !self.n,
                    _ => false,
                };
                if take {
                    #[allow(clippy::cast_possible_truncation, reason = "jump targets are imm[7:0]")]
                    {
                        self.pc = i.imm as u8;
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs until `HALT` (or `max_cycles`).
    ///
    /// # Errors
    /// See [`Self::step`]; [`ModelError::Timeout`] past `max_cycles`.
    pub fn run(&mut self, max_cycles: u64) -> Result<(), ModelError> {
        while !self.halted {
            if self.cycles > max_cycles {
                return Err(ModelError::Timeout);
            }
            self.step()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let w = encode(op::ST, 5, 3, 6, 0xbeef);
        assert_eq!(decode(w), Fields { op: op::ST, rd: 5, ra: 3, rb: 6, imm: 0xbeef });
        assert_eq!(w >> 16 & 3, 0);
    }

    #[test]
    fn alu_matches_u128() {
        assert_eq!(alu(1, u128::MAX, 1), (0, true));
        assert_eq!(alu(2, 5, 7), (5u128.wrapping_sub(7), false));
        assert_eq!(alu(2, 7, 5), (2, true));
        assert_eq!(alu(6, 1 << 127 | 1, 0), (2, true));
        assert_eq!(alu(7, 3, 0), (1, true));
        assert_eq!(sext(0xffff), u128::MAX);
        assert_eq!(sext(0x7fff), 0x7fff);
    }

    #[test]
    fn halt_and_cycles() {
        let p = [encode(op::ADDI, 1, 0, 0, 5), encode(op::ST, 0, 0, 1, 3), encode(op::HALT, 0, 0, 0, 0)];
        let mut m = Machine::new(&p, &[]);
        m.run(100).unwrap();
        assert_eq!(m.ram[3], 5);
        assert_eq!(m.cycles, 2 + 3 + 2);
        assert_eq!(m.instructions, 3);
    }
}
