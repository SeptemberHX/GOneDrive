//! The sync engine.

pub mod engine;
pub mod plan;
pub mod selection;
pub mod transfer;

pub use engine::{Direction, Engine, FreeSpaceOutcome, SyncEvent, SyncSummary};
pub use selection::{Listing, PinOutcome, Selection};
pub use plan::{
    decide_local, decide_remote, order_parents_first, partition_delta, LocalAction, LocalStat,
    RemoteAction, SkipReason,
};
