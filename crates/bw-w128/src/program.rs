//! The demo program in the example's ROM.
//!
//! 1. Fibonacci: `RAM[i] = F(i)` for i = 0…186 (F(186) is the largest
//!    Fibonacci number below 2^128) and each value is shown on the OUT
//!    display as it is computed.
//! 2. Factorials by shift-and-add multiplication: `RAM[0x100 + n] = n!` for
//!    n = 1…34 (34! is the largest factorial below 2^128), each shown on the
//!    OUT display.
//! 3. Reads F(186) back from RAM (a load), dumps r1…r7 to `RAM[0x1F1…0x1F7]`
//!    and halts with 34! on the OUT display.

/// Assembly source of the demo.
pub const DEMO: &str = r"
; Andorinha W-128 demo program
        .equ IO,    0x200       ; OUT display register (I/O space)
        .equ FTAB,  0x100       ; factorial table (RAM rank 1)
        .equ NFIB,  187         ; F(0) ... F(186)
        .equ DUMP,  0x1f0       ; register dump r1..r7 at 0x1f1..0x1f7

start:  ldi   r1, 0             ; a = F(i)
        ldi   r2, 1             ; b = F(i+1)
        ldi   r4, 0             ; i
        ldi   r5, NFIB          ; numbers left
fib:    st    r1, [r4]          ; RAM[i] = F(i)
        out   r1                ; show it
        add   r3, r1, r2        ; F(i+2), 128-bit add
        mov   r1, r2
        mov   r2, r3
        addi  r4, r4, 1
        subi  r5, r5, 1
        jnz   fib

        ldi   r6, 1             ; r6 = n!
        ldi   r7, 1             ; r7 = n
fact:   mov   r1, r6            ; multiplicand (shifted left)
        mov   r2, r7            ; multiplier (shifted right)
        ldi   r3, 0             ; product
mul:    shr   r2, r2            ; C = next multiplier bit
        jnc   skip
        add   r3, r3, r1
skip:   shl   r1, r1
        tst   r2
        jnz   mul
        mov   r6, r3            ; n! = (n-1)! * n
        st    r6, [r7 + FTAB]   ; RAM[0x100 + n] = n!
        out   r6
        addi  r7, r7, 1
        cmpi  r7, 35
        jnz   fact

        ld    r1, [r0 + 186]    ; F(186) back from RAM
        ld    r2, [r0 + FTAB + 34]
        cmp   r1, r2            ; F(186) > 34!: C = 1 (no borrow), Z = 0
        st    r1, [r0 + DUMP + 1]
        st    r2, [r0 + DUMP + 2]
        st    r3, [r0 + DUMP + 3]
        st    r4, [r0 + DUMP + 4]
        st    r5, [r0 + DUMP + 5]
        st    r6, [r0 + DUMP + 6]
        st    r7, [r0 + DUMP + 7]
        halt
";

/// F(n) for n ≤ 186.
#[must_use]
pub fn fibonacci(n: usize) -> u128 {
    let (mut a, mut b) = (0u128, 1u128);
    for _ in 0..n {
        let c = a.wrapping_add(b);
        a = b;
        b = c;
    }
    a
}

/// n! for n ≤ 34.
#[must_use]
pub fn factorial(n: u32) -> u128 {
    (1..=u128::from(n)).product()
}
