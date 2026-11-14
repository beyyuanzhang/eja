//! A language-free continuity kernel. See docs/ARCHITECTURE.md for the contract.
//! Logical state is folded from the journal; effects execute only at typed ports.
pub mod adapters;
pub mod aja;
mod control;
mod domain;
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
