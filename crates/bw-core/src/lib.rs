//! `bw-core` — the bitwerk circuit model.
//!
//! Pure data: components, pins and wires placed on an integer grid. Everything
//! here is serde-serializable and contains no I/O, threading or rendering.
//!
//! Wiring is a graph of orthogonal segments between nodes (pins, junctions
//! and bends); a net is a connected component of that graph (see
//! [`wiring`]).
//!
//! Invariants (enforced by [`Circuit`] on construction *and* on deserialization):
//! * every segment is horizontal or vertical and joins two nodes; free nodes
//!   (junctions, bends) have two or more segments, pin nodes sit on their pin;
//! * every net has at most one driver (two outputs can never be shorted
//!   together), except that several **tri-state** outputs may share a net (a
//!   bus; the simulator resolves it and halts on bus contention);
//! * component parameters are within the documented limits, and memory
//!   contents match their memory's size and data width.

#![forbid(unsafe_code)]

mod circuit;
mod component;
mod geom;
pub mod memenc;
pub mod stress;
pub mod wiring;

pub use circuit::{Applied, Circuit, CircuitError, CircuitRaw, Edit, MAX_WIRE_VERTICES, MemoryImage, MemoryRecord};
pub use component::{
    Component, ComponentId, ComponentKind, DATA_WIDTHS, FfTrigger, FlipFlopType, GateOp, MAX_ADDR_BITS,
    MAX_CLOCK_HALF_PERIOD, MAX_GATE_INPUTS, MAX_MEMORY_WORDS, MIN_ADDR_BITS, MIN_GATE_INPUTS, MemoryType, PinDir,
    PinRef, PinSide, Rotation, data_width_index,
};
pub use geom::{GridPos, GridRect};
pub use wiring::{Nets, NodeAt, NodeId, NodeRecord, Segment, SegmentId, SegmentRecord, WireEnd, WireNode};
