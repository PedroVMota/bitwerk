use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::component::{Component, ComponentId, ComponentKind, PinDir, PinRef, Rotation};
use crate::geom::GridPos;

/// Maximum number of intermediate vertices of a single wire.
pub const MAX_WIRE_VERTICES: usize = 256;

/// Stable identifier of a wire inside a [`Circuit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WireId(pub u32);

/// A point-to-point connection from an output pin to an input pin.
///
/// TODO: wire joints / solder points (wire-to-wire connections) — for now
/// fan-out is expressed as several wires leaving the same output pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wire {
    pub id: WireId,
    pub from: PinRef,
    pub to: PinRef,
    /// Intermediate routing vertices (purely visual).
    #[serde(default)]
    pub via: Vec<GridPos>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CircuitError {
    #[error("duplicate component id {0:?}")]
    DuplicateComponent(ComponentId),
    #[error("duplicate wire id {0:?}")]
    DuplicateWire(WireId),
    #[error("component {0:?} does not exist")]
    NoSuchComponent(ComponentId),
    #[error("wire {0:?} does not exist")]
    NoSuchWire(WireId),
    #[error("component {0:?} has invalid parameters")]
    InvalidComponent(ComponentId),
    #[error("pin {0:?} does not exist")]
    NoSuchPin(PinRef),
    #[error("wire must start at an output pin")]
    SourceNotOutput,
    #[error("unable to connect two outputs")]
    TwoOutputs,
    #[error("input pin {0:?} is already driven")]
    InputAlreadyDriven(PinRef),
    #[error("wire has too many vertices (max {MAX_WIRE_VERTICES})")]
    TooManyVertices,
    #[error("identifier space exhausted")]
    IdSpaceExhausted,
}

/// One atomic model change. Both the UI and the simulation thread apply the
/// same sequence of edits to their own copy of a [`Circuit`] (ids are
/// allocated deterministically, so both copies stay identical), which lets an
/// edit reach the simulator without cloning or sharing the whole circuit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    AddComponent { kind: ComponentKind, pos: GridPos, rotation: Rotation },
    RemoveComponent(ComponentId),
    MoveComponent(ComponentId, GridPos),
    SetRotation(ComponentId, Rotation),
    SetKind(ComponentId, ComponentKind),
    Connect { from: PinRef, to: PinRef, via: Vec<GridPos> },
    Disconnect(WireId),
}

/// What an applied [`Edit`] created or touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// The edit created, changed or removed this component (wires attached
    /// to a removed component are removed with it).
    Component(ComponentId),
    /// The edit created or removed this wire; `to` is the driven component.
    Wire { id: WireId, to: ComponentId },
}

/// A flat (single-level) circuit.
///
/// Fields are private so the invariants documented at crate level cannot be
/// broken from outside; deserialization goes through the same validation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(try_from = "CircuitRaw")]
pub struct Circuit {
    components: BTreeMap<ComponentId, Component>,
    wires: BTreeMap<WireId, Wire>,
    /// Input pins currently driven by a wire (derived index).
    driven: HashSet<PinRef>,
    next_component: u32,
    next_wire: u32,
}

/// On-disk shape of a circuit; converted to [`Circuit`] only after validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitRaw {
    pub components: Vec<Component>,
    pub wires: Vec<Wire>,
}

#[derive(Serialize)]
struct CircuitRef<'a> {
    components: Vec<&'a Component>,
    wires: Vec<&'a Wire>,
}

impl Serialize for Circuit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        CircuitRef { components: self.components.values().collect(), wires: self.wires.values().collect() }.serialize(s)
    }
}

impl TryFrom<CircuitRaw> for Circuit {
    type Error = CircuitError;

    fn try_from(raw: CircuitRaw) -> Result<Self, Self::Error> {
        let mut c = Self::default();
        for comp in raw.components {
            if !comp.kind.is_valid() {
                return Err(CircuitError::InvalidComponent(comp.id));
            }
            let next = comp.id.0.checked_add(1).ok_or(CircuitError::IdSpaceExhausted)?;
            c.next_component = c.next_component.max(next);
            if c.components.insert(comp.id, comp.clone()).is_some() {
                return Err(CircuitError::DuplicateComponent(comp.id));
            }
        }
        for wire in raw.wires {
            let next = wire.id.0.checked_add(1).ok_or(CircuitError::IdSpaceExhausted)?;
            c.next_wire = c.next_wire.max(next);
            if c.wires.contains_key(&wire.id) {
                return Err(CircuitError::DuplicateWire(wire.id));
            }
            c.check_connection(wire.from, wire.to, &wire.via)?;
            c.driven.insert(wire.to);
            c.wires.insert(wire.id, wire);
        }
        Ok(c)
    }
}

impl Circuit {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn component_count(&self) -> usize {
        self.components.len()
    }

    #[must_use]
    pub fn wire_count(&self) -> usize {
        self.wires.len()
    }

    /// Components in ascending id order (deterministic).
    #[must_use]
    pub fn components(&self) -> impl ExactSizeIterator<Item = &Component> + Clone {
        self.components.values()
    }

    /// Wires in ascending id order (deterministic).
    #[must_use]
    pub fn wires(&self) -> impl ExactSizeIterator<Item = &Wire> + Clone {
        self.wires.values()
    }

    #[must_use]
    pub fn component(&self, id: ComponentId) -> Option<&Component> {
        self.components.get(&id)
    }

    #[must_use]
    pub fn wire(&self, id: WireId) -> Option<&Wire> {
        self.wires.get(&id)
    }

    /// Adds a component and returns its new id.
    ///
    /// # Errors
    /// Fails if the kind parameters are out of range or ids are exhausted.
    pub fn add_component(
        &mut self,
        kind: ComponentKind,
        pos: GridPos,
        rotation: Rotation,
    ) -> Result<ComponentId, CircuitError> {
        let id = ComponentId(self.next_component);
        if !kind.is_valid() {
            return Err(CircuitError::InvalidComponent(id));
        }
        self.next_component = self.next_component.checked_add(1).ok_or(CircuitError::IdSpaceExhausted)?;
        self.components.insert(id, Component { id, kind, pos, rotation });
        Ok(id)
    }

    /// Removes a component and every wire attached to it.
    ///
    /// # Errors
    /// Fails if the component does not exist.
    pub fn remove_component(&mut self, id: ComponentId) -> Result<Component, CircuitError> {
        let comp = self.components.remove(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        let driven = &mut self.driven;
        self.wires.retain(|_, w| {
            let touches = w.from.component == id || w.to.component == id;
            if touches {
                driven.remove(&w.to);
            }
            !touches
        });
        Ok(comp)
    }

    /// Moves a component to a new position.
    ///
    /// # Errors
    /// Fails if the component does not exist.
    pub fn move_component(&mut self, id: ComponentId, pos: GridPos) -> Result<(), CircuitError> {
        let c = self.components.get_mut(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        c.pos = pos;
        Ok(())
    }

    /// Sets a component's rotation (wires follow the pins automatically).
    ///
    /// # Errors
    /// Fails if the component does not exist.
    pub fn set_rotation(&mut self, id: ComponentId, rotation: Rotation) -> Result<(), CircuitError> {
        let c = self.components.get_mut(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        c.rotation = rotation;
        Ok(())
    }

    /// Replaces a component's kind (e.g. to toggle a switch or change a
    /// parameter). The pin count must not change while wires are attached.
    ///
    /// # Errors
    /// Fails if the component does not exist or the kind is invalid.
    pub fn set_kind(&mut self, id: ComponentId, kind: ComponentKind) -> Result<(), CircuitError> {
        if !kind.is_valid() {
            return Err(CircuitError::InvalidComponent(id));
        }
        let c = self.components.get_mut(&id).ok_or(CircuitError::NoSuchComponent(id))?;
        if c.kind.input_count() != kind.input_count() || c.kind.output_count() != kind.output_count() {
            let attached = self.wires.values().any(|w| w.from.component == id || w.to.component == id);
            if attached {
                return Err(CircuitError::InvalidComponent(id));
            }
        }
        c.kind = kind;
        Ok(())
    }

    fn pin_dir(&self, pin: PinRef) -> Result<PinDir, CircuitError> {
        let comp = self.components.get(&pin.component).ok_or(CircuitError::NoSuchComponent(pin.component))?;
        comp.pin_dir(pin.pin).ok_or(CircuitError::NoSuchPin(pin))
    }

    fn check_connection(&self, from: PinRef, to: PinRef, via: &[GridPos]) -> Result<(), CircuitError> {
        if via.len() > MAX_WIRE_VERTICES {
            return Err(CircuitError::TooManyVertices);
        }
        if self.pin_dir(from)? != PinDir::Output {
            return Err(CircuitError::SourceNotOutput);
        }
        if self.pin_dir(to)? != PinDir::Input {
            return Err(CircuitError::TwoOutputs);
        }
        if self.driven.contains(&to) {
            return Err(CircuitError::InputAlreadyDriven(to));
        }
        Ok(())
    }

    /// Connects an output pin to an input pin.
    ///
    /// # Errors
    /// Fails if the pins do not exist, have the wrong direction, the input is
    /// already driven, or ids are exhausted.
    pub fn connect(&mut self, from: PinRef, to: PinRef, via: Vec<GridPos>) -> Result<WireId, CircuitError> {
        self.check_connection(from, to, &via)?;
        let id = WireId(self.next_wire);
        self.next_wire = self.next_wire.checked_add(1).ok_or(CircuitError::IdSpaceExhausted)?;
        self.driven.insert(to);
        self.wires.insert(id, Wire { id, from, to, via });
        Ok(id)
    }

    /// Removes a wire.
    ///
    /// # Errors
    /// Fails if the wire does not exist.
    pub fn disconnect(&mut self, id: WireId) -> Result<Wire, CircuitError> {
        let w = self.wires.remove(&id).ok_or(CircuitError::NoSuchWire(id))?;
        self.driven.remove(&w.to);
        Ok(w)
    }

    /// Applies one [`Edit`].
    ///
    /// # Errors
    /// Same as the corresponding individual operation; on error the circuit
    /// is unchanged.
    pub fn apply(&mut self, edit: &Edit) -> Result<Applied, CircuitError> {
        match edit {
            Edit::AddComponent { kind, pos, rotation } => {
                self.add_component(*kind, *pos, *rotation).map(Applied::Component)
            }
            Edit::RemoveComponent(id) => self.remove_component(*id).map(|_| Applied::Component(*id)),
            Edit::MoveComponent(id, pos) => self.move_component(*id, *pos).map(|()| Applied::Component(*id)),
            Edit::SetRotation(id, rot) => self.set_rotation(*id, *rot).map(|()| Applied::Component(*id)),
            Edit::SetKind(id, kind) => self.set_kind(*id, *kind).map(|()| Applied::Component(*id)),
            Edit::Connect { from, to, via } => {
                self.connect(*from, *to, via.clone()).map(|id| Applied::Wire { id, to: to.component })
            }
            Edit::Disconnect(id) => self.disconnect(*id).map(|w| Applied::Wire { id: *id, to: w.to.component }),
        }
    }

    /// Returns the wire driving a given input pin, if any.
    #[must_use]
    pub fn driver_of(&self, input: PinRef) -> Option<&Wire> {
        if !self.driven.contains(&input) {
            return None;
        }
        self.wires.values().find(|w| w.to == input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::GateOp;

    fn and2() -> ComponentKind {
        ComponentKind::Gate { op: GateOp::And, inputs: 2 }
    }

    #[test]
    fn connect_rules() {
        let mut c = Circuit::new();
        let s = c.add_component(ComponentKind::Switch { on: false }, GridPos::new(0, 0), Rotation::R0).unwrap();
        let g = c.add_component(and2(), GridPos::new(4, 0), Rotation::R0).unwrap();
        let out = PinRef { component: s, pin: 0 };
        let in0 = PinRef { component: g, pin: 0 };
        let gout = PinRef { component: g, pin: 2 };
        c.connect(out, in0, vec![]).unwrap();
        assert_eq!(c.connect(out, in0, vec![]), Err(CircuitError::InputAlreadyDriven(in0)));
        assert_eq!(c.connect(out, gout, vec![]), Err(CircuitError::TwoOutputs));
        assert_eq!(c.connect(in0, gout, vec![]), Err(CircuitError::SourceNotOutput));
        c.remove_component(s).unwrap();
        assert_eq!(c.wire_count(), 0);
    }

    #[test]
    fn edits_replay_identically() {
        let edits = [
            Edit::AddComponent {
                kind: ComponentKind::Switch { on: true },
                pos: GridPos::new(0, 0),
                rotation: Rotation::R0,
            },
            Edit::AddComponent { kind: and2(), pos: GridPos::new(4, 0), rotation: Rotation::R0 },
            Edit::Connect {
                from: PinRef { component: ComponentId(0), pin: 0 },
                to: PinRef { component: ComponentId(1), pin: 0 },
                via: vec![GridPos::new(2, 1)],
            },
            Edit::MoveComponent(ComponentId(1), GridPos::new(8, 2)),
            Edit::SetRotation(ComponentId(1), Rotation::R90),
            Edit::Disconnect(WireId(0)),
            Edit::RemoveComponent(ComponentId(0)),
        ];
        let mut a = Circuit::new();
        let mut b = Circuit::new();
        for e in &edits {
            let ra = a.apply(e).unwrap();
            assert_eq!(b.apply(e).unwrap(), ra);
        }
        assert_eq!(a.component_count(), 1);
        assert_eq!(a.components().collect::<Vec<_>>(), b.components().collect::<Vec<_>>());
        assert!(a.apply(&Edit::RemoveComponent(ComponentId(0))).is_err());
    }

    #[test]
    fn invalid_gate_rejected() {
        let mut c = Circuit::new();
        let bad = ComponentKind::Gate { op: GateOp::Or, inputs: 200 };
        assert!(c.add_component(bad, GridPos::default(), Rotation::R0).is_err());
    }
}
