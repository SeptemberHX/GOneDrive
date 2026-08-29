//! Choosing which parts of a drive are kept on this device.
//!
//! Separated from the sync engine because browsing and changing a selection
//! must work without the network: a user deciding what to download should not
//! need a working token to look at their own folder list.

use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use crate::db::{Account, ItemRecord, ItemState, OpKind, PinState, Store};
use crate::error::{Error, Result};

use super::rooted_fs::RootedFs;

/// One row of a browsable listing.
#[derive(Debug, Clone)]
pub struct Listing {
    pub item: ItemRecord,
    /// Pin after inheritance is resolved.
    pub pin: PinState,
    /// Whether the pin was set on this item rather than inherited.
    pub explicit: bool,
    /// Total bytes of the subtree (or the file's own size).
    pub bytes: u64,
    pub files: u64,
}

/// What a pin is about to cost.
#[derive(Debug, Clone, Default)]
pub struct PinOutcome {
    pub files: u64,
    pub bytes: u64,
    /// Queued downloads withdrawn because the path was excluded.
    pub cancelled: u64,
    /// Free space on the sync root's filesystem, when it could be determined.
    pub free_bytes: Option<u64>,
}

impl PinOutcome {
    /// Whether this selection would run the disk below a safe floor.
    pub fn exceeds_free_space(&self, min_free: u64) -> bool {
        self.free_bytes
            .is_some_and(|free| self.bytes.saturating_add(min_free) > free)
    }
}

pub struct Selection {
    store: Store,
    config: Arc<Config>,
    account_id: String,
}

impl Selection {
    pub fn new(store: Store, config: Arc<Config>, account_id: String) -> Self {
        Self {
            store,
            config,
            account_id,
        }
    }

    fn account(&self) -> Result<Account> {
        self.store
            .account(&self.account_id)?
            .ok_or_else(|| Error::other(format!("unknown account {}", self.account_id)))
    }

    /// One entry of a browsable listing.
    ///
    /// Carries the *effective* pin and the subtree's total size so the caller
    /// can show what a choice would cost before it is made.
    pub fn list(&self, relative: &Path) -> Result<Vec<Listing>> {
        let account = self.account()?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let parent = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such folder: {}", relative.display())))?;

        let default = self.config.download_new.default_pin();
        let mut out = Vec::new();
        for child in self.store.children(&self.account_id, &parent.id)? {
            let (bytes, files) = if child.is_folder {
                self.store.subtree_size(&child.id)?
            } else {
                (child.size, 1)
            };
            out.push(Listing {
                pin: self.store.effective_pin(&child.id, default)?,
                explicit: child.pin_state != PinState::Inherit,
                bytes,
                files,
                item: child,
            });
        }
        Ok(out)
    }

    /// What pinning a path would cost, without changing anything.
    ///
    /// Separate from [`Selection::set_pin`] so the caller can refuse a
    /// selection that does not fit *before* a single byte is queued.
    pub fn preview(&self, relative: &Path) -> Result<PinOutcome> {
        let account = self.account()?;
        let target = self.resolve(&account, relative)?;

        let (bytes, files) = if target.is_folder {
            self.store.subtree_size(&target.id)?
        } else {
            (target.size, 1)
        };
        // Anything already on disk is not going to be fetched again.
        let already: u64 = if target.is_folder {
            self.store
                .files_in_subtree(&target.id)?
                .into_iter()
                .filter(|f| f.state == ItemState::Synced && f.local_ino.is_some())
                .map(|f| f.size)
                .sum()
        } else {
            0
        };

        Ok(PinOutcome {
            files,
            bytes: bytes.saturating_sub(already),
            cancelled: 0,
            free_bytes: available_bytes(&account.root_local_path),
        })
    }

    /// Resolve a path relative to the sync root to the item it names.
    fn resolve(&self, account: &Account, relative: &Path) -> Result<ItemRecord> {
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        self.store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))
    }

    /// Choose whether a path's content is kept on this device.
    ///
    /// Pinning queues everything already enumerated beneath it; unpinning stops
    /// future downloads but never deletes what is already there, because
    /// "I don't need new copies of this" and "delete my files" are different
    /// requests and only one of them was made.
    pub fn set_pin(&self, relative: &Path, pin: PinState) -> Result<PinOutcome> {
        let account = self.account()?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        // The directory itself first: a pinned but empty folder should still
        // appear, and downloads below it need somewhere to land.
        if pin.wants_content() && target.is_folder {
            if let Some(dir) = self.store.local_path(&account, &target.id)? {
                let rooted = RootedFs::open(&account.root_local_path)?;
                rooted.ensure_dir(&dir)?;
                // The engine re-stamps directories after the transfers land;
                // this only gets the empty-folder case right.
                if let Some(mtime) = target.remote_mtime {
                    let times = std::fs::FileTimes::new()
                        .set_modified(crate::sync::plan::to_filetime(mtime));
                    let _ = rooted.set_times(&dir, times);
                }
            }
        }

        // Persist the preference only after all filesystem preconditions have
        // succeeded, so a failed pin cannot be reported while remaining set.
        self.store.set_pin_state(&target.id, pin)?;

        let mut outcome = PinOutcome::default();
        if !pin.wants_content() {
            // Withdraw anything already queued beneath here. Without this,
            // excluding a folder appears to do nothing until the downloads it
            // was meant to cancel have finished.
            outcome.cancelled =
                self.store
                    .cancel_ops_in_subtree(&self.account_id, &target.id, OpKind::Download)?
                    as u64;
            return Ok(outcome);
        }

        let files = if target.is_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target.clone()]
        };

        let default = self.config.download_new.default_pin();
        for file in files {
            // Already on disk and current: nothing to fetch.
            if file.state == ItemState::Synced && file.local_ino.is_some() {
                continue;
            }
            // A nearer explicit exclusion beats the folder we just pinned.
            if !self.store.effective_pin(&file.id, default)?.wants_content() {
                continue;
            }
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            self.store.enqueue(
                &self.account_id,
                OpKind::Download,
                Some(&file.id),
                Some(&path),
                None,
            )?;
            self.store
                .set_item_state(&file.id, ItemState::DirtyRemote)?;
            outcome.files += 1;
            outcome.bytes += file.size;
        }

        outcome.free_bytes = available_bytes(&account.root_local_path);
        Ok(outcome)
    }
}

/// Bytes available to an unprivileged user on the filesystem holding `path`.
///
/// `f_bavail` rather than `f_bfree`: the difference is the reserved blocks only
/// root may use, and counting those would let a sync fill the disk to the point
/// where the desktop session itself cannot write.
pub fn available_bytes(path: &Path) -> Option<u64> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string and `stat` is a valid
    // writable statvfs for the duration of the call.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
