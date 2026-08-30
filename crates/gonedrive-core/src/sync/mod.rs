//! The sync engine.

pub mod emblems;
pub mod engine;
mod hydration;
mod local;
pub mod plan;
mod queue;
mod remote;
pub(crate) mod rooted_fs;
pub mod selection;
pub mod transfer;

pub use engine::{Direction, Engine, FreeSpaceOutcome, SyncEvent, SyncSummary};
pub use hydration::{
    dehydrated_paths_for_account, lock_unwatched_placeholder_for_account,
    materialise_placeholder_for_account, placeholder_inventory_for_account, PlaceholderInventory,
};
pub use plan::{
    decide_local, decide_remote, order_parents_first, partition_delta, LocalAction, LocalStat,
    RemoteAction, SkipReason,
};
pub use selection::{Listing, PinOutcome, Selection};
