//! `bw-sim` — event-driven logic simulation engine.
//!
//! * [`netlist`] compiles a [`bw_core::Circuit`] into a flat struct-of-arrays
//!   netlist and evaluates it with unit-delay delta cycles.
//! * [`engine`] runs that evaluator on a dedicated thread. The UI talks to it
//!   only through a command channel ([`Command`]) and reads immutable
//!   [`Snapshot`]s published through an `ArcSwap` — the UI never locks or waits.

#![forbid(unsafe_code)]

pub mod engine;
pub mod netlist;

pub use engine::{Command, RunState, SimHandle, Snapshot, StepMode};
pub use netlist::{NO_NET, Netlist, SimState, net_numbering};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SimError {
    /// The circuit keeps changing without settling (e.g. a NOT gate feeding
    /// itself). The simulation is halted.
    #[error("race condition, process halted (net {net})")]
    RaceCondition { net: u32 },
    #[error("circuit too large to simulate")]
    TooLarge,
}
