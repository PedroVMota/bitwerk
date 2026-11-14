//! A small two-pass assembler for the W-128 (see [`crate::isa`]).
//!
//! ```text
//! ; comment (also #)
//!         .equ  IO, 0x200        ; constant
//! loop:   addi  r4, r4, 1        ; label on the same line or alone
//!         st    r1, [r4 + 3]     ; memory operand: [reg], [reg ± expr], [expr]
//!         jnz   loop
//! ```
//!
//! Expressions are sums and differences of numbers (decimal, `0x` hex,
//! `0b` binary), labels and `.equ` names. Immediates must fit a signed
//! 16-bit field (the hardware sign-extends them); jump targets are ROM
//! addresses (0–255).
//!
//! Pseudo-instructions: `ldi rd, imm` (`addi rd, r0, imm`), `mov rd, ra`
//! (`add rd, ra, r0`), `not rd, ra` (`xori rd, ra, -1`), `cmp ra, rb`
//! (`sub r0, ra, rb`), `cmpi ra, imm` (`subi r0, ra, imm`), `tst ra`
//! (`add r0, ra, r0`), `out rb` (`st rb, [r0 + 0x200]`).

use std::collections::HashMap;
use std::fmt;

use crate::isa::{IO_BASE, ROM_WORDS, encode, op};

/// An assembly error with its (1-based) line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsmError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for AsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for AsmError {}

/// The assembled program and its symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub words: Vec<u32>,
    pub labels: HashMap<String, i64>,
    /// Source line of each word (1-based), for listings.
    pub lines: Vec<usize>,
}

struct Stmt<'a> {
    line: usize,
    mnemonic: String,
    args: Vec<&'a str>,
}

fn err<T>(line: usize, msg: impl Into<String>) -> Result<T, AsmError> {
    Err(AsmError { line, msg: msg.into() })
}

fn is_ident(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic() || f == '_') && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

/// Splits operands at top-level commas (not inside brackets).
fn split_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last);
    }
    out
}

fn number(t: &str) -> Option<i64> {
    let t = t.replace('_', "");
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        i64::from_str_radix(b, 2).ok()
    } else {
        t.parse().ok()
    }
}

fn eval(expr: &str, syms: &HashMap<String, i64>, line: usize) -> Result<i64, AsmError> {
    let e = expr.trim();
    if e.is_empty() {
        return err(line, "missing expression");
    }
    let mut total: i64 = 0;
    let mut sign = 1i64;
    let mut term = String::new();
    let mut expect_term = true;
    let flush = |term: &mut String, sign: i64, total: &mut i64| -> Result<(), AsmError> {
        let t = term.trim();
        let v = if let Some(n) = number(t) {
            n
        } else if is_ident(t) {
            match syms.get(&t.to_ascii_lowercase()) {
                Some(&v) => v,
                None => return err(line, format!("unknown symbol `{t}`")),
            }
        } else {
            return err(line, format!("bad expression term `{t}`"));
        };
        *total = total.checked_add(sign * v).ok_or(AsmError { line, msg: "expression overflow".into() })?;
        term.clear();
        Ok(())
    };
    for ch in e.chars() {
        match ch {
            '+' | '-' if expect_term && term.trim().is_empty() => {
                if ch == '-' {
                    sign = -sign;
                }
            }
            '+' | '-' => {
                flush(&mut term, sign, &mut total)?;
                sign = if ch == '-' { -1 } else { 1 };
                expect_term = true;
            }
            c if c.is_whitespace() => term.push(c),
            c => {
                term.push(c);
                expect_term = false;
            }
        }
    }
    if term.trim().is_empty() {
        return err(line, "expression ends with an operator");
    }
    flush(&mut term, sign, &mut total)?;
    Ok(total)
}

fn reg(t: &str, line: usize) -> Result<u8, AsmError> {
    let t = t.trim().to_ascii_lowercase();
    match t.strip_prefix('r').and_then(|n| n.parse::<u8>().ok()) {
        Some(n) if n < 8 && t.len() == 2 => Ok(n),
        _ => err(line, format!("expected a register r0-r7, found `{t}`")),
    }
}

fn imm16(v: i64, line: usize) -> Result<u16, AsmError> {
    if !(-32768..=32767).contains(&v) {
        return err(line, format!("immediate {v} does not fit a signed 16-bit field"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "range checked above")]
    Ok(v as u16)
}

/// `[ra]`, `[ra + e]`, `[ra - e]` or `[e]` → (ra, offset).
fn mem(t: &str, syms: &HashMap<String, i64>, line: usize) -> Result<(u8, u16), AsmError> {
    let Some(inner) = t.trim().strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return err(line, format!("expected a memory operand like [r1 + 4], found `{t}`"));
    };
    let inner = inner.trim();
    let head: String = inner.chars().take_while(char::is_ascii_alphanumeric).collect();
    if reg(&head, line).is_ok() {
        let rest = inner[head.len()..].trim();
        let off = if rest.is_empty() { 0 } else { eval(rest, syms, line)? };
        Ok((reg(&head, line)?, imm16(off, line)?))
    } else {
        Ok((0, imm16(eval(inner, syms, line)?, line)?))
    }
}

fn want(s: &Stmt<'_>, n: usize) -> Result<(), AsmError> {
    if s.args.len() == n {
        Ok(())
    } else {
        err(s.line, format!("`{}` takes {n} operand(s), found {}", s.mnemonic, s.args.len()))
    }
}

fn alu_reg(name: &str) -> Option<u8> {
    Some(match name {
        "add" => op::ADD,
        "sub" => op::SUB,
        "and" => op::AND,
        "or" => op::OR,
        "xor" => op::XOR,
        _ => return None,
    })
}

fn alu_imm(name: &str) -> Option<u8> {
    Some(match name {
        "addi" => op::ADDI,
        "subi" => op::SUBI,
        "andi" => op::ANDI,
        "ori" => op::ORI,
        "xori" => op::XORI,
        _ => return None,
    })
}

fn jump(name: &str) -> Option<u8> {
    Some(match name {
        "jmp" => op::JMP,
        "jz" => op::JZ,
        "jnz" => op::JNZ,
        "jc" => op::JC,
        "jnc" => op::JNC,
        "jn" => op::JN,
        "jnn" => op::JNN,
        _ => return None,
    })
}

fn emit(s: &Stmt<'_>, syms: &HashMap<String, i64>) -> Result<u32, AsmError> {
    let a = &s.args;
    let l = s.line;
    let m = s.mnemonic.as_str();
    if let Some(o) = alu_reg(m) {
        want(s, 3)?;
        return Ok(encode(o, reg(a[0], l)?, reg(a[1], l)?, reg(a[2], l)?, 0));
    }
    if let Some(o) = alu_imm(m) {
        want(s, 3)?;
        return Ok(encode(o, reg(a[0], l)?, reg(a[1], l)?, 0, imm16(eval(a[2], syms, l)?, l)?));
    }
    if let Some(o) = jump(m) {
        want(s, 1)?;
        let t = eval(a[0], syms, l)?;
        if !(0..i64::try_from(ROM_WORDS).unwrap_or(256)).contains(&t) {
            return err(l, format!("jump target {t} outside the ROM (0-255)"));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "range checked above")]
        return Ok(encode(o, 0, 0, 0, t as u16));
    }
    Ok(match m {
        "halt" => {
            want(s, 0)?;
            encode(op::HALT, 0, 0, 0, 0)
        }
        "nop" => {
            want(s, 0)?;
            encode(op::NOP, 0, 0, 0, 0)
        }
        "shl" | "shr" => {
            want(s, 2)?;
            encode(if m == "shl" { op::SHL } else { op::SHR }, reg(a[0], l)?, reg(a[1], l)?, 0, 0)
        }
        "ld" => {
            want(s, 2)?;
            let (ra, off) = mem(a[1], syms, l)?;
            encode(op::LD, reg(a[0], l)?, ra, 0, off)
        }
        "st" => {
            want(s, 2)?;
            let (ra, off) = mem(a[1], syms, l)?;
            encode(op::ST, 0, ra, reg(a[0], l)?, off)
        }
        "ldi" => {
            want(s, 2)?;
            encode(op::ADDI, reg(a[0], l)?, 0, 0, imm16(eval(a[1], syms, l)?, l)?)
        }
        "mov" => {
            want(s, 2)?;
            encode(op::ADD, reg(a[0], l)?, reg(a[1], l)?, 0, 0)
        }
        "not" => {
            want(s, 2)?;
            encode(op::XORI, reg(a[0], l)?, reg(a[1], l)?, 0, 0xffff)
        }
        "cmp" => {
            want(s, 2)?;
            encode(op::SUB, 0, reg(a[0], l)?, reg(a[1], l)?, 0)
        }
        "cmpi" => {
            want(s, 2)?;
            encode(op::SUBI, 0, reg(a[0], l)?, 0, imm16(eval(a[1], syms, l)?, l)?)
        }
        "tst" => {
            want(s, 1)?;
            encode(op::ADD, 0, reg(a[0], l)?, 0, 0)
        }
        "out" => {
            want(s, 1)?;
            encode(op::ST, 0, 0, reg(a[0], l)?, IO_BASE)
        }
        _ => return err(l, format!("unknown instruction `{}`", s.mnemonic)),
    })
}

/// Assembles `source` into ROM words.
///
/// # Errors
/// The first error found, with its line.
pub fn assemble(source: &str) -> Result<Program, AsmError> {
    let mut syms: HashMap<String, i64> = HashMap::new();
    let mut stmts: Vec<Stmt<'_>> = Vec::new();
    // Pass 1: labels, constants and statements.
    for (i, raw) in source.lines().enumerate() {
        let line = i + 1;
        let mut text = raw.split([';', '#']).next().unwrap_or("").trim();
        while let Some(colon) = text.find(':') {
            let label = text[..colon].trim();
            if !is_ident(label) {
                break;
            }
            let key = label.to_ascii_lowercase();
            if syms.contains_key(&key) {
                return err(line, format!("`{label}` defined twice"));
            }
            syms.insert(key, i64::try_from(stmts.len()).unwrap_or(i64::MAX));
            text = text[colon + 1..].trim();
        }
        if text.is_empty() {
            continue;
        }
        let (mnemonic, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let mnemonic = mnemonic.to_ascii_lowercase();
        let args = split_args(rest);
        if mnemonic == ".equ" {
            if args.len() != 2 || !is_ident(args[0]) {
                return err(line, "usage: .equ NAME, value");
            }
            let key = args[0].to_ascii_lowercase();
            if syms.contains_key(&key) {
                return err(line, format!("`{}` defined twice", args[0]));
            }
            let v = eval(args[1], &syms, line)?;
            syms.insert(key, v);
            continue;
        }
        stmts.push(Stmt { line, mnemonic, args });
    }
    if stmts.len() > ROM_WORDS {
        return err(stmts.last().map_or(0, |s| s.line), format!("program longer than the ROM ({ROM_WORDS} words)"));
    }
    // Pass 2: encode.
    let mut words = Vec::with_capacity(stmts.len());
    let mut lines = Vec::with_capacity(stmts.len());
    for s in &stmts {
        words.push(emit(s, &syms)?);
        lines.push(s.line);
    }
    Ok(Program { words, labels: syms, lines })
}

/// A human-readable listing of one instruction word.
#[must_use]
pub fn disassemble(word: u32) -> String {
    let i = crate::isa::decode(word);
    let imm = crate::isa::sext(i.imm);
    #[allow(clippy::cast_possible_wrap, reason = "display of a signed immediate")]
    let simm = imm as i128;
    let f = i.op & 7;
    let names = ["", "add", "sub", "and", "or", "xor", "shl", "shr"];
    match i.op >> 3 {
        0 if f == 0 => "halt".into(),
        1 if f == 0 => "nop".into(),
        0 | 1 if f >= 6 => format!("{} r{}, r{}", names[usize::from(f)], i.rd, i.ra),
        0 => format!("{} r{}, r{}, r{}", names[usize::from(f)], i.rd, i.ra, i.rb),
        1 => format!("{}i r{}, r{}, {simm}", names[usize::from(f)], i.rd, i.ra),
        2 if i.op & 1 == 0 => format!("ld r{}, [r{} + {simm}]", i.rd, i.ra),
        2 => format!("st r{}, [r{} + {simm}]", i.rb, i.ra),
        _ => {
            let c = ["jmp", "jz", "jnz", "jc", "jnc", "jn", "jnn", "jnever"][usize::from(f)];
            format!("{c} {}", i.imm & 0xff)
        }
    }
}
