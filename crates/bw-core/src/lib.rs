//! `bw-core` — the bitwerk circuit model.
//!
//! Pure data: components, pins and wires placed on an integer grid. Everything
//! here is serde-serializable and contains no I/O, threading or rendering.
//!
//! Invariants (enforced by [`Circuit`] on construction *and* on deserialization):
//! * every wire goes from an existing component's **output** pin to an existing
//!   component's **input** pin;
//! * every input pin is driven by at most one wire (two outputs can never be
//!   shorted together);
//! * component parameters are within the documented limits.

#![forbid(unsafe_code)]

mod circuit;
mod component;
mod geom;
pub mod stress;

pub use circuit::{Applied, Circuit, CircuitError, CircuitRaw, Edit, MAX_WIRE_VERTICES, Wire, WireId};
pub use component::{
    Component, ComponentId, ComponentKind, GateOp, MAX_CLOCK_HALF_PERIOD, MAX_GATE_INPUTS, MIN_GATE_INPUTS, PinDir,
    PinRef, Rotation,
};
pub use geom::{GridPos, GridRect};
