//! Synthetic large circuits for stress tests and benchmarks (debug / test
//! use; shared by the app's stress test and the renderer benchmark).

use crate::{Circuit, CircuitError, ComponentId, ComponentKind, GateOp, GridPos, PinRef, Rotation};

/// Builds a grid of gate chains with roughly `target` components.
///
/// Each row starts with a clock (or a switch) followed by a chain of gates;
/// every gate is fed by its predecessor (and, for two-input gates, by the one
/// before that), so clock edges ripple visibly along the rows.
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
            ComponentKind::Switch { on: row % 4 == 1 }
        };
        let mut prev2 = c.add_component(source, GridPos::new(0, y), Rotation::R0)?;
        let mut prev = prev2;
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
            let o1 = PinRef { component: prev, pin: c_out_pin(&c, prev) };
            let mid_y = y + 1;
            c.connect(o1, PinRef { component: id, pin: 0 }, vec![GridPos::new(x - 2, mid_y)])?;
            if kind.input_count() == 2 {
                let o2 = PinRef { component: prev2, pin: c_out_pin(&c, prev2) };
                c.connect(o2, PinRef { component: id, pin: 1 }, vec![GridPos::new(x - 1, y + 3)])?;
            }
            if kind.output_count() == 0 {
                break;
            }
            prev2 = prev;
            prev = id;
        }
    }
    Ok(c)
}

fn c_out_pin(c: &Circuit, id: ComponentId) -> u8 {
    c.component(id).map_or(0, |comp| comp.kind.input_count())
}

#[cfg(test)]
mod tests {
    #[test]
    fn generates_requested_size() {
        let c = super::generate(5_000).unwrap();
        assert!(c.component_count() >= 5_000);
        assert!(c.wire_count() > c.component_count());
    }
}
