//! Four-state signal values and the evaluation rules of every component.
//!
//! A net carries one of four levels, stored as a `u8` (2 bits used):
//!
//! | value | meaning |
//! |---|---|
//! | [`L0`] = 0 | logic 0 |
//! | [`L1`] = 1 | logic 1 |
//! | [`Z`] = 2 | floating / high impedance (no active driver, unconnected) |
//! | [`X`] = 3 | unknown or conflicting |
//!
//! Inputs read Z as X: a floating input is an unknown value. Gates follow
//! the usual dominance rules (AND with any 0 is 0, OR with any 1 is 1);
//! otherwise any unknown input makes the output unknown. Sequential parts
//! are exact under X: an unknown input yields X only if the possible values
//! disagree (see [`resolve_unknown`]).

use bw_core::{FfTrigger, FlipFlopType, GateOp};

pub const L0: u8 = 0;
pub const L1: u8 = 1;
pub const Z: u8 = 2;
pub const X: u8 = 3;

/// Value of an input as seen by logic: Z reads as X.
#[inline]
#[must_use]
pub const fn input(v: u8) -> u8 {
    // 0 → 0, 1 → 1, Z (2) → X (3), X → X, without a branch.
    v | (v >> 1)
}

/// Logical NOT (unknowns stay unknown).
#[inline]
#[must_use]
pub const fn not(v: u8) -> u8 {
    if v < 2 { v ^ 1 } else { X }
}

/// Evaluates a gate over four-state inputs.
#[allow(clippy::inline_always, reason = "measured: ~10% of the tick cost in the 100k stress circuit")]
#[inline(always)]
pub fn gate(op: GateOp, inputs: impl Iterator<Item = u8>) -> u8 {
    match op {
        GateOp::And | GateOp::Nand => {
            let mut r = L1;
            for v in inputs {
                if v == L0 {
                    r = L0;
                    break;
                }
                if v != L1 {
                    r = X;
                }
            }
            if op == GateOp::Nand { not(r) } else { r }
        }
        GateOp::Or | GateOp::Nor => {
            let mut r = L0;
            for v in inputs {
                if v == L1 {
                    r = L1;
                    break;
                }
                if v != L0 {
                    r = X;
                }
            }
            if op == GateOp::Nor { not(r) } else { r }
        }
        GateOp::Xor | GateOp::Xnor => {
            let mut r = L0;
            for v in inputs {
                if v > L1 {
                    return X;
                }
                r ^= v;
            }
            if op == GateOp::Xnor { not(r) } else { r }
        }
    }
}

/// Tri-state buffer: follows IN while EN = 1, floats while EN = 0, unknown
/// otherwise.
#[inline]
#[must_use]
pub const fn tristate(input_v: u8, enable: u8) -> u8 {
    match enable {
        L1 => {
            if input_v < 2 {
                input_v
            } else {
                X
            }
        }
        L0 => Z,
        _ => X,
    }
}

/// Evaluates `f` for every binary assignment of the unknown arguments:
/// the common result if all agree, else X. `f` returns `None` for an
/// indeterminate outcome (RS = 11), which is X.
#[must_use]
pub fn resolve_unknown<const N: usize>(args: [u8; N], f: impl Fn([bool; N]) -> Option<bool>) -> u8 {
    let mut known = [false; N];
    let mut mask = 0u32;
    for i in 0..N {
        known[i] = args[i] == L1;
        if args[i] > L1 {
            mask |= 1 << i;
        }
    }
    if mask == 0 {
        return f(known).map_or(X, u8::from);
    }
    // Enumerate the subsets of `mask` (no allocation: this is the hot path).
    let mut result: Option<bool> = None;
    let mut sub = 0u32;
    loop {
        let mut a = known;
        for (i, v) in a.iter_mut().enumerate() {
            if mask >> i & 1 == 1 {
                *v = sub >> i & 1 == 1;
            }
        }
        match (f(a), result) {
            (None, _) => return X,
            (Some(v), None) => result = Some(v),
            (Some(v), Some(r)) if v != r => return X,
            _ => {}
        }
        if sub == mask {
            break;
        }
        sub = sub.wrapping_sub(mask) & mask;
    }
    result.map_or(X, u8::from)
}

/// Next state of a flip-flop's (slave) output when it is clocked: `a`, `b`
/// are the data inputs (D: `a`; RS: S = `a`, R = `b`; JK: J = `a`, K = `b`).
#[must_use]
pub fn ff_next(ty: FlipFlopType, a: u8, b: u8, q: u8) -> u8 {
    resolve_unknown([a, b, q], |[a, b, q]| match ty {
        FlipFlopType::D => Some(a),
        FlipFlopType::Rs => match (a, b) {
            (false, false) => Some(q),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (true, true) => None,
        },
        FlipFlopType::Jk => Some(match (a, b) {
            (false, false) => q,
            (true, false) => true,
            (false, true) => false,
            (true, true) => !q,
        }),
    })
}

/// Next state of the master latch of a master-slave flip-flop while the
/// clock is high. The JK master only sets when Q = 0 and only resets when
/// Q = 1 (feedback from the slave), so it changes at most once per pulse
/// ("ones catching").
#[must_use]
pub fn ms_master_next(ty: FlipFlopType, a: u8, b: u8, q: u8, m: u8) -> u8 {
    resolve_unknown([a, b, q, m], |[a, b, q, m]| match ty {
        FlipFlopType::D => Some(a),
        FlipFlopType::Rs => match (a, b) {
            (false, false) => Some(m),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (true, true) => None,
        },
        FlipFlopType::Jk => Some(if a && !q {
            true
        } else if b && q {
            false
        } else {
            m
        }),
    })
}

/// State after the asynchronous preset / clear inputs (active high; both
/// active is indeterminate).
#[must_use]
pub fn ff_async(pre: u8, clr: u8, q: u8) -> u8 {
    if pre == L0 && clr == L0 {
        return q;
    }
    resolve_unknown([pre, clr, q], |[p, c, q]| match (p, c) {
        (false, false) => Some(q),
        (true, false) => Some(true),
        (false, true) => Some(false),
        (true, true) => None,
    })
}

/// Clock transition classes for edge detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    None,
    /// A definite active edge.
    Yes,
    /// A transition through X that may or may not be an active edge.
    Maybe,
}

/// Classifies the clock transition `prev → now` for an edge from `from` to `to`.
#[must_use]
pub const fn edge(prev: u8, now: u8, from: u8, to: u8) -> Edge {
    if prev == from && now == to {
        Edge::Yes
    } else if (prev == from && now == X) || (prev == X && now == to) {
        Edge::Maybe
    } else {
        Edge::None
    }
}

/// Persistent state of one flip-flop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FfState {
    /// Output Q (Q̄ is its complement).
    pub q: u8,
    /// Master latch (master-slave only).
    pub m: u8,
    /// Clock level at the previous evaluation.
    pub clk: u8,
}

impl FfState {
    /// Power-on state: Q = 0.
    pub const RESET: Self = Self { q: L0, m: L0, clk: X };
}

/// One evaluation of a flip-flop. `edges` is false while the circuit is
/// being initialised (time zero has no clock edges). Returns Q.
#[allow(clippy::too_many_arguments, reason = "flat inputs of the hot path")]
pub fn flip_flop(
    st: &mut FfState,
    ty: FlipFlopType,
    trigger: FfTrigger,
    a: u8,
    b: u8,
    clk: u8,
    async_pins: Option<(u8, u8)>,
    edges: bool,
) -> u8 {
    let prev = st.clk;
    st.clk = clk;
    let pessimistic = |q: u8, n: u8| if n == q { q } else { X };
    // While initialising, nets are still settling from "floating": an
    // unknown clock or preset/clear is not yet a value and must not poison
    // the stored state (and there are no edges at time zero).
    let settled = |v: u8| edges || v <= L1;
    match trigger {
        FfTrigger::Latch => match clk {
            L1 => st.q = ff_next(ty, a, b, st.q),
            L0 => {}
            _ if settled(clk) => st.q = pessimistic(st.q, ff_next(ty, a, b, st.q)),
            _ => {}
        },
        FfTrigger::RisingEdge | FfTrigger::FallingEdge => {
            let (from, to) = if trigger == FfTrigger::RisingEdge { (L0, L1) } else { (L1, L0) };
            if edges {
                match edge(prev, clk, from, to) {
                    Edge::Yes => st.q = ff_next(ty, a, b, st.q),
                    Edge::Maybe => st.q = pessimistic(st.q, ff_next(ty, a, b, st.q)),
                    Edge::None => {}
                }
            }
        }
        FfTrigger::MasterSlave => {
            if edges {
                match edge(prev, clk, L1, L0) {
                    Edge::Yes => st.q = st.m,
                    Edge::Maybe => st.q = pessimistic(st.q, st.m),
                    Edge::None => {}
                }
            }
            match clk {
                L1 => st.m = ms_master_next(ty, a, b, st.q, st.m),
                L0 => {}
                _ if settled(clk) => st.m = pessimistic(st.m, ms_master_next(ty, a, b, st.q, st.m)),
                _ => {}
            }
        }
    }
    if let Some((pre, clr)) = async_pins {
        if settled(pre) && settled(clr) {
            st.q = ff_async(pre, clr, st.q);
            st.m = ff_async(pre, clr, st.m);
        }
    }
    st.q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gates_with_unknowns() {
        let and = |v: &[u8]| gate(GateOp::And, v.iter().copied());
        let or = |v: &[u8]| gate(GateOp::Or, v.iter().copied());
        assert_eq!(and(&[L0, X]), L0, "AND with a 0 is 0");
        assert_eq!(and(&[L1, X]), X);
        assert_eq!(or(&[L1, X]), L1, "OR with a 1 is 1");
        assert_eq!(or(&[L0, X]), X);
        assert_eq!(gate(GateOp::Nand, [L0, X].into_iter()), L1);
        assert_eq!(gate(GateOp::Nor, [L1, X].into_iter()), L0);
        assert_eq!(gate(GateOp::Xor, [L1, X].into_iter()), X);
        assert_eq!(gate(GateOp::Xnor, [L1, L1].into_iter()), L1);
        assert_eq!(not(X), X);
        assert_eq!(input(Z), X);
    }

    #[test]
    fn tristate_truth_table() {
        assert_eq!(tristate(L0, L1), L0);
        assert_eq!(tristate(L1, L1), L1);
        assert_eq!(tristate(L0, L0), Z);
        assert_eq!(tristate(L1, L0), Z);
        assert_eq!(tristate(X, L1), X);
        assert_eq!(tristate(L1, X), X);
    }

    #[test]
    fn resolve_unknown_is_exact() {
        // D = X: unknown. JK with J = 1, K = 0: 1 whatever Q is.
        assert_eq!(ff_next(FlipFlopType::D, X, L0, L0), X);
        assert_eq!(ff_next(FlipFlopType::Jk, L1, L0, X), L1);
        assert_eq!(ff_next(FlipFlopType::Jk, L1, L1, X), X);
        // RS: S = X, R = 0 with Q = 1 stays 1; S = 1, R = 1 is X.
        assert_eq!(ff_next(FlipFlopType::Rs, X, L0, L1), L1);
        assert_eq!(ff_next(FlipFlopType::Rs, L1, L1, L0), X);
        assert_eq!(ff_async(L0, L0, X), X);
        assert_eq!(ff_async(L1, X, L1), X);
        assert_eq!(ff_async(X, L0, L1), L1);
    }
}
