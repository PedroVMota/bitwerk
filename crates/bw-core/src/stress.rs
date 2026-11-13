//! Synthetic large circuits for stress tests and benchmarks (debug / test
//! use; shared by the app's stress test and the renderer benchmark).

#![allow(clippy::many_single_char_names, reason = "grid coordinates")]

use crate::wiring::{SegmentId, WireEnd};
use crate::{Circuit, CircuitError, ComponentId, ComponentKind, Edit, GateOp, GridPos, PinRef, Rotation};

/// Builds a grid of gate chains with roughly `target` components.
///
/// Each row starts with a clock (or a switch) followed by a chain of gates;
/// every gate's first input is fed by its predecessor and, for two-input
/// gates, the second by the one before that, so clock edges ripple visibly
/// along the rows. All wiring is orthogonal: the first input gets an L (or
/// a straight wire), and the second branches off the predecessor's input
/// wire at a junction (a fan-out of two on every net), running below the
/// row.
///
/// # Errors
/// Only if the model rejects a generated component (should not happen).
pub fn generate(target: usize) -> Result<Circuit, CircuitError> {
    let per_row = 320usize;
    let rows = target.div_ceil(per_row);
    let mut c = Circuit::new();
    let ops = [GateOp::Xor, GateOp::Nand, GateOp::Or, GateOp::Xnor, GateOp::And, GateOp::Nor];
    for row in 0..rows {
        let y = i32::try_from(row * 10).unwrap_or(i32::MAX);
        let source = if row % 2 == 0 {
            ComponentKind::Clock { half_period: 2 + u32::try_from(row % 7).unwrap_or(0) }
        } else {
            ComponentKind::Switch { on: row % 4 == 1, mode: crate::SwitchMode::Toggle }
        };
        let src = c.add_component(source, GridPos::new(0, y), Rotation::R0)?;
        // (component, segments of the wire feeding its first input, the
        // bend / junction point of that wire on the driver's net).
        let mut prev2: Option<(Vec<SegmentId>, GridPos)> = None;
        let mut prev = src;
        for col in 1..per_row {
            let x = i32::try_from(col * 8).unwrap_or(i32::MAX);
            let kind = if col % 5 == 0 {
                ComponentKind::Not
            } else if col == per_row - 1 {
                ComponentKind::Led
            } else {
                ComponentKind::Gate { op: ops[(row + col) % ops.len()], inputs: 2 }
            };
            let id = c.add_component(kind, GridPos::new(x, y), Rotation::R0)?;
            let out = out_pin(&c, prev);
            let o = c.component(prev).and_then(|p| p.pin_pos(out.pin)).unwrap_or_default();
            let bend = GridPos::new(x - 2, o.y);
            let a = c.apply(&Edit::AddWire {
                from: WireEnd::Pin(out),
                to: WireEnd::Pin(PinRef { component: id, pin: 0 }),
                via: vec![bend, GridPos::new(x - 2, y + 1)],
            })?;
            let segs: Vec<SegmentId> = a.segments.into_iter().filter(|&s| c.segment(s).is_some()).collect();
            if kind.input_count() == 2 {
                // Second input: branch off the wire that feeds the
                // predecessor (the net of the gate two places back).
                let lane = y + 6 + i32::try_from(col % 2).unwrap_or(0);
                let to = WireEnd::Pin(PinRef { component: id, pin: 1 });
                let via = vec![GridPos::new(x - 1, lane), GridPos::new(x - 1, y + 3)];
                if let Some((psegs, at)) = &prev2 {
                    let from = on_wire(&c, psegs, *at).unwrap_or(WireEnd::Pin(out));
                    let mut v = vec![GridPos::new(at.x, lane)];
                    v.extend(via);
                    c.apply(&Edit::AddWire { from, to, via: v })?;
                } else {
                    // First gate of the row: both inputs from the source.
                    let from = on_wire(&c, &segs, bend).unwrap_or(WireEnd::Pin(out));
                    c.apply(&Edit::AddWire { from, to, via: vec![GridPos::new(bend.x, y + 3)] })?;
                }
            }
            if kind.output_count() == 0 {
                break;
            }
            prev2 = Some((segs, bend));
            prev = id;
        }
    }
    Ok(c)
}

/// The wire end at `p` on one of `segs` (a node or a split point).
fn on_wire(c: &Circuit, segs: &[SegmentId], p: GridPos) -> Option<WireEnd> {
    segs.iter().copied().find_map(|s| {
        let (a, b) = c.segment_ends(s)?;
        let inside = if a.x == b.x {
            p.x == a.x && p.y >= a.y.min(b.y) && p.y <= a.y.max(b.y)
        } else {
            p.y == a.y && p.x >= a.x.min(b.x) && p.x <= a.x.max(b.x)
        };
        inside.then_some(WireEnd::OnSegment(s, p))
    })
}

fn out_pin(c: &Circuit, id: ComponentId) -> PinRef {
    PinRef { component: id, pin: c.component(id).map_or(0, |comp| comp.kind.input_count()) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generates_requested_size() {
        let c = super::generate(5_000).unwrap();
        assert!(c.component_count() >= 5_000);
        assert!(c.segment_count() > 2 * c.component_count());
        assert!(c.junction_count() > c.component_count() / 2, "fan-out through junctions");
        c.check().unwrap();
    }
}
