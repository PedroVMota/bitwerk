//! `bw-sim` — event-driven logic simulation engine.
//!
//! * [`netlist`] compiles a [`bw_core::Circuit`] into a flat struct-of-arrays
//!   netlist and evaluates it with unit-delay delta cycles.
//! * [`engine`] runs that evaluator on a dedicated thread. The UI talks to it
//!   only through a command channel ([`Command`]) and reads immutable
//!   [`Snapshot`]s published through an `ArcSwap` — the UI never locks or waits.
//! * [`logic`] defines the four-state signal values (0, 1, Z, X) and the
//!   behaviour of gates, tri-state buffers and flip-flops.

#![forbid(unsafe_code)]

pub mod engine;
pub mod flatten;
pub mod logic;
pub mod netlist;

pub use engine::{Command, MemoryView, RunState, SimHandle, Snapshot, StepMode};
pub use flatten::{CompileStats, FlattenCache};
pub use netlist::{Hier, NO_NET, NetMap, Netlist, SimState, pack_levels, packed_level};

use bw_core::ComponentId;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SimError {
    /// The circuit keeps changing without settling (e.g. a NOT gate feeding
    /// itself). The simulation is halted. `path` is the instance path and
    /// id of a component on the oscillating loop, `place` the same as text
    /// (`root › Counter#3 › NAND#2`).
    #[error("race condition{}, process halted (net {net}{})", higher(path), at(place))]
    RaceCondition { net: u32, path: Vec<ComponentId>, place: String },
    /// Two or more tri-state outputs drive the same net at once (after the
    /// circuit settled). The simulation is halted. `drivers` are the paths
    /// of the conflicting components (instance path + id).
    #[error("bus contention{}, process halted (net {net}{})", higher_any(drivers), at(place))]
    BusContention { net: u32, drivers: Vec<Vec<ComponentId>>, place: String },
    #[error("circuit too large to simulate")]
    TooLarge,
}

/// " at higher level" when the problem is inside a macro.
fn higher(path: &[ComponentId]) -> &'static str {
    if path.len() > 1 { " at higher level" } else { "" }
}

fn higher_any(paths: &[Vec<ComponentId>]) -> &'static str {
    if paths.iter().any(|p| p.len() > 1) { " at higher level" } else { "" }
}

fn at(place: &str) -> String {
    if place.is_empty() { String::new() } else { format!(", at {place}") }
}
