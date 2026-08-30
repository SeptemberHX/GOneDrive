//! Files-on-demand hydration, lifecycle, and space-management policy.

mod lifecycle;
mod policy;
mod staging;

pub use lifecycle::{lock_unwatched_placeholder_for_account, materialise_placeholder_for_account};
pub use policy::{
    dehydrated_paths_for_account, placeholder_inventory_for_account, PlaceholderInventory,
};
