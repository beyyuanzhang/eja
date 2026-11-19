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
mod fold;
mod measure;
mod state;
mod store;
mod turn;

/// The one report a recovery writes when only the interruption is known. Old
/// journals without the `recovered` marker still decode as unknown.
pub(crate) const INTERRUPTED_REPORT: &str = "worker session interrupted; state unknown";

pub use control::{Control, ControlError, INTERRUPTED};
pub use domain::*;
pub use state::{OpenProposal, State};
pub use store::{
    format_utc, inspect_journal, now_utc, read_journal, JournalLease, JournalScan, Store,
};
pub use turn::{
    recover, turn, turn_at, turn_at_with_options, validate_turn_input, TurnOutcome, TurnRun,
};
