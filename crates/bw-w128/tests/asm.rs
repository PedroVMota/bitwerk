//! The W-128 assembler: encodings, operands, symbols, pseudo-instructions
//! and errors.

#![allow(clippy::unwrap_used)]

use bw_w128::asm::{assemble, disassemble};
use bw_w128::isa::{encode, op};

fn one(src: &str) -> u32 {
    let p = assemble(src).unwrap();
    assert_eq!(p.words.len(), 1, "{src}");
    p.words[0]
}

#[test]
fn encodes_every_instruction() {
    assert_eq!(one("halt"), 0);
    assert_eq!(one("nop"), encode(op::NOP, 0, 0, 0, 0));
    assert_eq!(one("add r1, r2, r3"), encode(op::ADD, 1, 2, 3, 0));
    assert_eq!(one("SUB R7, R6, R5"), encode(op::SUB, 7, 6, 5, 0));
    assert_eq!(one("and r1, r1, r1"), encode(op::AND, 1, 1, 1, 0));
    assert_eq!(one("or r2, r3, r4"), encode(op::OR, 2, 3, 4, 0));
    assert_eq!(one("xor r2, r3, r4"), encode(op::XOR, 2, 3, 4, 0));
    assert_eq!(one("shl r1, r2"), encode(op::SHL, 1, 2, 0, 0));
    assert_eq!(one("shr r1, r2"), encode(op::SHR, 1, 2, 0, 0));
    assert_eq!(one("addi r1, r2, -1"), encode(op::ADDI, 1, 2, 0, 0xffff));
    assert_eq!(one("subi r1, r2, 0x7fff"), encode(op::SUBI, 1, 2, 0, 0x7fff));
    assert_eq!(one("andi r1, r2, 0b1010"), encode(op::ANDI, 1, 2, 0, 10));
    assert_eq!(one("ori r1, r2, 1_000"), encode(op::ORI, 1, 2, 0, 1000));
    assert_eq!(one("xori r1, r2, -32768"), encode(op::XORI, 1, 2, 0, 0x8000));
    assert_eq!(one("ld r4, [r5 + 3]"), encode(op::LD, 4, 5, 0, 3));
    assert_eq!(one("ld r4, [r5]"), encode(op::LD, 4, 5, 0, 0));
    assert_eq!(one("ld r4, [r5 - 2]"), encode(op::LD, 4, 5, 0, 0xfffe));
    assert_eq!(one("ld r4, [0x1ff]"), encode(op::LD, 4, 0, 0, 0x1ff));
    assert_eq!(one("st r6, [r5 + 1]"), encode(op::ST, 0, 5, 6, 1));
    for (m, o) in [("jmp", op::JMP), ("jz", op::JZ), ("jnz", op::JNZ), ("jc", op::JC), ("jnc", op::JNC)] {
        assert_eq!(one(&format!("{m} 200")), encode(o, 0, 0, 0, 200));
    }
    assert_eq!(one("jn 1"), encode(op::JN, 0, 0, 0, 1));
    assert_eq!(one("jnn 2"), encode(op::JNN, 0, 0, 0, 2));
}

#[test]
fn pseudo_instructions() {
    assert_eq!(one("ldi r3, 42"), encode(op::ADDI, 3, 0, 0, 42));
    assert_eq!(one("mov r3, r4"), encode(op::ADD, 3, 4, 0, 0));
    assert_eq!(one("not r3, r4"), encode(op::XORI, 3, 4, 0, 0xffff));
    assert_eq!(one("cmp r3, r4"), encode(op::SUB, 0, 3, 4, 0));
    assert_eq!(one("cmpi r3, 35"), encode(op::SUBI, 0, 3, 0, 35));
    assert_eq!(one("tst r3"), encode(op::ADD, 0, 3, 0, 0));
    assert_eq!(one("out r6"), encode(op::ST, 0, 0, 6, 0x200));
}

#[test]
fn labels_constants_and_expressions() {
    let p = assemble(
        "
        .equ BASE, 0x100   ; comment
        .equ TOP, BASE + 0x0f - 1
start:  ldi r1, TOP        # other comment
loop:   st r1, [r2 + BASE + 2]
        jnz loop
        jmp start
end:    halt",
    )
    .unwrap();
    assert_eq!(p.words[0], encode(op::ADDI, 1, 0, 0, 0x10e));
    assert_eq!(p.words[1], encode(op::ST, 0, 2, 1, 0x102));
    assert_eq!(p.words[2], encode(op::JNZ, 0, 0, 0, 1));
    assert_eq!(p.words[3], encode(op::JMP, 0, 0, 0, 0));
    assert_eq!(p.labels["end"], 4);
    assert_eq!(p.lines, vec![4, 5, 6, 7, 8]);
}

#[test]
fn errors_name_the_line() {
    let cases = [
        ("foo r1", 1, "unknown instruction"),
        ("\nadd r1, r2", 2, "takes 3 operand"),
        ("add r8, r1, r2", 1, "register"),
        ("addi r1, r2, 40000", 1, "16-bit"),
        ("jmp 300", 1, "outside the ROM"),
        ("jmp nowhere", 1, "unknown symbol"),
        ("a: nop\na: nop", 2, "defined twice"),
        ("ld r1, r2", 1, "memory operand"),
        ("ldi r1, 3 +", 1, "operator"),
    ];
    for (src, line, msg) in cases {
        let e = assemble(src).unwrap_err();
        assert_eq!(e.line, line, "{src}");
        assert!(e.msg.contains(msg), "{src}: {}", e.msg);
    }
    let long: String = (0..257).map(|_| "nop\n").collect();
    assert!(assemble(&long).unwrap_err().msg.contains("longer than the ROM"));
}

#[test]
fn disassembly_round_trips() {
    let src = "add r1, r2, r3\nsubi r4, r5, -7\nshl r1, r2\nld r1, [r2 + 5]\nst r3, [r4 + -1]\njnz 7\nhalt\nnop";
    let p = assemble(src).unwrap();
    let text: Vec<String> = p.words.iter().map(|&w| disassemble(w)).collect();
    let again = assemble(&text.join("\n")).unwrap();
    assert_eq!(again.words, p.words, "{text:?}");
}

#[test]
fn the_demo_assembles() {
    let p = assemble(bw_w128::program::DEMO).unwrap();
    assert!(p.words.len() > 30 && p.words.len() < 256);
    assert_eq!(*p.words.last().unwrap(), 0, "ends with halt");
}
