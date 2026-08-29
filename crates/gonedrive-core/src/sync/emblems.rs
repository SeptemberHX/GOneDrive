//! Folder emblems: the check / cloud badge a file manager shows on a folder.
//!
//! Split out of the engine because it is a self-contained rule — a folder is
//! "available on this device" only when everything inside it is — that only
//! reads item state and writes directory xattrs, touching none of the transfer
//! or reconciliation machinery.

use crate::db::{Account, ItemState};
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};

use super::engine::Engine;

impl Engine {
    /// Recompute a folder's badge from its direct children and return whether
    /// it changed.
    ///
    /// The rule the user expects, and Windows uses: a folder is "available on
    /// this device" (a check) only when *everything* inside is here — every
    /// file downloaded, every subfolder itself fully local — or when it was
    /// explicitly kept. A folder with some content still in the cloud shows the
    /// cloud, because opening it may still need the network. "Any one file is
    /// local" is not enough.
    pub(super) fn recompute_folder_mark(&self, account: &Account, folder_id: &str) -> Result<bool> {
        let Some(path) = self.store.local_path(account, folder_id)? else {
            return Ok(false);
        };
        let rooted = self.rooted(account)?;
        let folder = match rooted.open_read(&path) {
            Ok(file) if file.metadata()?.is_dir() => file,
            Ok(_) => return Ok(false),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        // An explicit "always keep on this device" wins outright.
        if self.store.explicitly_pinned(folder_id)? {
            let changed = Placeholder::state_file(&folder) != PlaceholderState::Hydrated;
            let _ = Placeholder::mark_dir_file(&folder, PlaceholderState::Hydrated, folder_id);
            let _ = Placeholder::set_pin_file(&folder, "pinned");
            return Ok(changed);
        }

        let mut all_local = true;
        for child in self.store.children(&self.account_id, folder_id)? {
            let local = if child.is_folder {
                // A subfolder counts as local only if it, too, is fully local.
                self.store
                    .local_path(account, &child.id)?
                    .and_then(|path| rooted.open_read(&path).ok())
                    .is_some_and(|file| {
                        Placeholder::state_file(&file) == PlaceholderState::Hydrated
                    })
            } else {
                child.state == ItemState::Synced && child.local_ino.is_some()
            };
            if !local {
                all_local = false;
                break;
            }
        }

        let target = if all_local {
            PlaceholderState::Hydrated
        } else {
            PlaceholderState::Dehydrated
        };
        let changed = Placeholder::state_file(&folder) != target;
        if changed {
            let _ = Placeholder::mark_dir_file(&folder, target, folder_id);
        }
        Ok(changed)
    }

    /// Recompute the badges of an item's ancestor folders after its state
    /// changed, stopping as soon as one is unchanged — nothing above a folder
    /// whose own badge did not move can have moved either.
    pub(super) fn refresh_folder_ancestors(&self, account: &Account, item_id: &str) -> Result<()> {
        let mut current = self.store.item(item_id)?.and_then(|i| i.parent_id);
        while let Some(id) = current {
            let Some(folder) = self.store.item(&id)? else {
                break;
            };
            if !folder.is_folder {
                break;
            }
            if !self.recompute_folder_mark(account, &id)? {
                break;
            }
            current = folder.parent_id;
        }
        Ok(())
    }
}
