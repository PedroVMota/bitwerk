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
pub mod logic;
pub mod netlist;

pub use engine::{Command, MemoryView, RunState, SimHandle, Snapshot, StepMode};
pub use netlist::{NO_NET, NetMap, Netlist, SimState, pack_levels, packed_level};

use bw_core::ComponentId;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SimError {
    /// The circuit keeps changing without settling (e.g. a NOT gate feeding
    /// itself). The simulation is halted.
    #[error("race condition, process halted (net {net})")]
    RaceCondition { net: u32 },
    /// Two or more tri-state outputs drive the same net at once (after the
    /// circuit settled). The simulation is halted.
    #[error("bus contention, process halted (net {net}, driven by {})", list(drivers))]
    BusContention { net: u32, drivers: Vec<ComponentId> },
    #[error("circuit too large to simulate")]
    TooLarge,
}

fn list(ids: &[ComponentId]) -> String {
    ids.iter().map(|id| format!("#{}", id.0)).collect::<Vec<_>>().join(", ")
}
