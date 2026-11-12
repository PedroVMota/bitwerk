use serde::{Deserialize, Serialize};

use crate::geom::{GridPos, GridRect};

/// Minimum number of inputs of a multi-input gate.
pub const MIN_GATE_INPUTS: u8 = 2;
/// Maximum number of inputs of a multi-input gate.
pub const MAX_GATE_INPUTS: u8 = 8;
/// Maximum clock half-period, in simulation ticks.
pub const MAX_CLOCK_HALF_PERIOD: u32 = 1_000_000;

/// Stable identifier of a component inside a [`crate::Circuit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ComponentId(pub u32);

/// Boolean operation of a combinational gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GateOp {
    And,
    Or,
    Nand,
    Nor,
    Xor,
    Xnor,
}

impl GateOp {
    pub const ALL: [Self; 6] = [Self::And, Self::Or, Self::Nand, Self::Nor, Self::Xor, Self::Xnor];

    /// Evaluates the gate over an iterator of input levels.
    #[must_use]
    pub fn eval(self, inputs: impl IntoIterator<Item = bool>) -> bool {
        let mut it = inputs.into_iter();
        match self {
            Self::And => it.all(|b| b),
            Self::Nand => !it.all(|b| b),
            Self::Or => it.any(|b| b),
            Self::Nor => !it.any(|b| b),
            Self::Xor => it.fold(false, |acc, b| acc ^ b),
            Self::Xnor => !it.fold(false, |acc, b| acc ^ b),
        }
    }

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
            Self::Nand => "NAND",
            Self::Nor => "NOR",
            Self::Xor => "XOR",
            Self::Xnor => "XNOR",
        }
    }
}

/// The kind (and kind-specific parameters) of a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ComponentKind {
    /// Multi-input gate with `inputs` in `MIN_GATE_INPUTS..=MAX_GATE_INPUTS`.
    Gate { op: GateOp, inputs: u8 },
    /// Inverter (one input, one output).
    Not,
    /// Manual toggle switch (one output).
    Switch { on: bool },
    /// Square-wave clock (one output); toggles every `half_period` ticks.
    Clock { half_period: u32 },
    /// Light-emitting diode (one input).
    Led,
}

impl ComponentKind {
    #[must_use]
    pub const fn input_count(&self) -> u8 {
        match self {
            Self::Gate { inputs, .. } => *inputs,
            Self::Not | Self::Led => 1,
            Self::Switch { .. } | Self::Clock { .. } => 0,
        }
    }

    #[must_use]
    pub const fn output_count(&self) -> u8 {
        match self {
            Self::Led => 0,
            _ => 1,
        }
    }

    #[must_use]
    pub const fn pin_count(&self) -> u8 {
        self.input_count() + self.output_count()
    }

    /// Body size in grid units (width, height), unrotated.
    #[must_use]
    pub const fn size(&self) -> (i32, i32) {
        match self {
            Self::Gate { inputs, .. } => (4, 2 * (*inputs as i32)),
            Self::Not => (4, 2),
            Self::Switch { .. } | Self::Clock { .. } | Self::Led => (2, 2),
        }
    }

    /// Checks kind-specific parameter limits.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        match self {
            Self::Gate { inputs, .. } => *inputs >= MIN_GATE_INPUTS && *inputs <= MAX_GATE_INPUTS,
            Self::Clock { half_period } => *half_period >= 1 && *half_period <= MAX_CLOCK_HALF_PERIOD,
            Self::Not | Self::Switch { .. } | Self::Led => true,
        }
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Gate { op, .. } => op.name(),
            Self::Not => "NOT",
            Self::Switch { .. } => "Switch",
            Self::Clock { .. } => "Clock",
            Self::Led => "LED",
        }
    }
}

/// Rotation of a component body, in 90° steps clockwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    /// Next rotation, 90° clockwise.
    #[must_use]
    pub const fn rotated_cw(self) -> Self {
        match self {
            Self::R0 => Self::R90,
            Self::R90 => Self::R180,
            Self::R180 => Self::R270,
            Self::R270 => Self::R0,
        }
    }
}

/// Direction of a pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinDir {
    Input,
    Output,
}

/// Reference to one pin of one component. Pins are numbered inputs first,
/// then outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PinRef {
    pub component: ComponentId,
    pub pin: u8,
}

/// A component placed on the grid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    pub id: ComponentId,
    pub kind: ComponentKind,
    /// Top-left corner of the (unrotated) body.
    pub pos: GridPos,
    #[serde(default)]
    pub rotation: Rotation,
}

impl Component {
    #[must_use]
    pub fn pin_dir(&self, pin: u8) -> Option<PinDir> {
        let ins = self.kind.input_count();
        if pin < ins {
            Some(PinDir::Input)
        } else if pin < self.kind.pin_count() {
            Some(PinDir::Output)
        } else {
            None
        }
    }

    /// Local (unrotated) offset of a pin from `pos`.
    #[must_use]
    pub fn pin_local_offset(&self, pin: u8) -> Option<(i32, i32)> {
        let (w, h) = self.kind.size();
        match self.pin_dir(pin)? {
            PinDir::Input => Some((0, 2 * i32::from(pin) + 1)),
            PinDir::Output => Some((w, h / 2)),
        }
    }

    /// Absolute grid position of a pin, taking rotation into account.
    #[must_use]
    pub fn pin_pos(&self, pin: u8) -> Option<GridPos> {
        let (lx, ly) = self.pin_local_offset(pin)?;
        let (w, h) = self.kind.size();
        let (rx, ry) = match self.rotation {
            Rotation::R0 => (lx, ly),
            Rotation::R90 => (h - ly, lx),
            Rotation::R180 => (w - lx, h - ly),
            Rotation::R270 => (ly, w - lx),
        };
        Some(self.pos.offset(rx, ry))
    }

    /// Bounding rectangle of the body on the grid.
    #[must_use]
    pub fn bounds(&self) -> GridRect {
        let (w, h) = self.kind.size();
        let (w, h) = match self.rotation {
            Rotation::R0 | Rotation::R180 => (w, h),
            Rotation::R90 | Rotation::R270 => (h, w),
        };
        GridRect { min: self.pos, max: self.pos.offset(w, h) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_truth_tables() {
        let cases = [(false, false), (false, true), (true, false), (true, true)];
        let expect = |op: GateOp| -> [bool; 4] {
            match op {
                GateOp::And => [false, false, false, true],
                GateOp::Or => [false, true, true, true],
                GateOp::Nand => [true, true, true, false],
                GateOp::Nor => [true, false, false, false],
                GateOp::Xor => [false, true, true, false],
                GateOp::Xnor => [true, false, false, true],
            }
        };
        for op in GateOp::ALL {
            for (i, (a, b)) in cases.iter().enumerate() {
                assert_eq!(op.eval([*a, *b]), expect(op)[i], "{op:?} {a} {b}");
            }
        }
    }

    #[test]
    fn pins_and_bounds() {
        let c = Component {
            id: ComponentId(1),
            kind: ComponentKind::Gate { op: GateOp::And, inputs: 2 },
            pos: GridPos::new(10, 10),
            rotation: Rotation::R0,
        };
        assert_eq!(c.pin_pos(0), Some(GridPos::new(10, 11)));
        assert_eq!(c.pin_pos(1), Some(GridPos::new(10, 13)));
        assert_eq!(c.pin_pos(2), Some(GridPos::new(14, 12)));
        assert_eq!(c.pin_pos(3), None);
        assert_eq!(c.bounds().max, GridPos::new(14, 14));
    }
}
