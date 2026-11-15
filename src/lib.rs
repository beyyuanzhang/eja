//! A language-free continuity kernel. See docs/ARCHITECTURE.md for the contract.
//! Logical state is folded from the journal; effects execute only at typed ports.

// The execution contract needs what the kernel uses: advisory file locks for the
// journal, process groups for owned executions, signals for interruption. A build
// without them would run without those guarantees, so it is refused.
#[cfg(not(unix))]
compile_error!(
    "eja requires a unix host: journal locks, process groups and signals are part of its contract"
);
pub mod adapters;
pub mod aja;
mod control;
mod domain;
pub mod fold;
mod state;
mod store;
mod turn;
pub use control::{Control, ControlError, INTERRUPTED};
pub use domain::*;
pub use state::{
    active_identity, current_goal, open_commitments, project, project_with_evidence, relations,
    tasks,
};
pub use store::{format_utc, now_utc, Store};
pub use turn::{recover, turn, turn_at, turn_at_with_options};
