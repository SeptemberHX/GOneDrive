//! Placeholder state transitions for hydration and dehydration.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::db::{Account, ItemState, PinState, Store};
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::sync::plan::LocalStat;

use super::super::engine::{Engine, SyncEvent};
use super::super::rooted_fs::RootedFs;

/// Create, or recognise, one exact placeholder using only durable local state.
/// This is intentionally independent of Graph authentication so the HSM can
/// repair a placeholder removed after a prior mark failure even while the
/// token endpoint or login keyring is unavailable.
pub fn materialise_placeholder_for_account(
    store: &Store,
    account: &Account,
    item_id: &str,
) -> Result<PathBuf> {
    let record = store
        .item(item_id)?
        .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;
    if record.account_id != account.id || record.is_folder {
        return Err(Error::other(format!(
            "item {item_id} is not a file in account {}",
            account.id
        )));
    }
    let path = store
        .local_path(account, item_id)?
        .ok_or_else(|| Error::other(format!("no local path for {item_id}")))?;
    let rooted = RootedFs::open(&account.root_local_path)?;

    let file = match rooted.open_read(&path) {
        Ok(file)
            if Placeholder::item_id_file(&file).as_deref() == Some(item_id)
                && Placeholder::state_file(&file) == PlaceholderState::Dehydrated =>
        {
            file
        }
        Ok(_) => {
            return Err(Error::other(format!(
                "{} is occupied by local content",
                path.display()
            )))
        }
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = rooted.create_new_file(&path)?;
            Placeholder::create_file(
                &file,
                record.size,
                item_id,
                record.ctag.as_deref(),
                record.remote_mtime,
            )?;
            file
        }
        Err(error) => return Err(error),
    };

    let local = LocalStat::from_metadata(&file.metadata()?);
    store.set_item_state(item_id, ItemState::Dehydrated)?;
    store.set_local_stat(
        item_id,
        OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
        Some(local.size),
        Some(local.ino),
    )?;
    Ok(path)
}

/// Fail closed without making a remote file disappear from the local tree.
/// The helper recognises the saved mode and restores it only after the inode is
/// protected by a successful fanotify mark.
pub fn lock_unwatched_placeholder_for_account(
    store: &Store,
    account: &Account,
    path: &Path,
) -> Result<()> {
    let rooted = RootedFs::open(&account.root_local_path)?;
    let file = match rooted.open_read(path) {
        Ok(file) => file,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            use std::os::unix::fs::MetadataExt;
            let path_file = rooted.open_path(path)?;
            if path_file.metadata()?.mode() & 0o7777 == 0 {
                return Ok(());
            }
            return Err(Error::Io(error));
        }
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let item_id = Placeholder::item_id_file(&file)
        .ok_or_else(|| Error::other(format!("{} is not a placeholder", path.display())))?;
    let record = store
        .item(&item_id)?
        .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;
    if record.account_id != account.id
        || store.local_path(account, &item_id)?.as_deref() != Some(path)
        || Placeholder::state_file(&file) != PlaceholderState::Dehydrated
    {
        return Err(Error::other(format!(
            "{} is not an owned dehydrated placeholder",
            path.display()
        )));
    }
    Placeholder::lock_unprotected_file(&file)
}

impl Engine {
    /// Record that a placeholder the helper just filled is now complete.
    pub fn finish_hydration(&self, path: &Path) -> Result<()> {
        let (account, record, file) = self.open_owned_placeholder(path, true)?;
        let item_id = record.id.clone();

        // A fanotify worker may emit a second completion for the same inode
        // when a final range read discovers that the whole file is already
        // materialised.  A synced database row is authoritative here even if
        // its xattr was subsequently changed to `dehydrated` by a release:
        // a fresh hydration always moves the row to `hydrating` first, so a
        // stale completion must never re-materialise the released file.
        if record.state == ItemState::Synced {
            return Ok(());
        }

        Placeholder::mark_hydrated_file(&file, &item_id, record.ctag.as_deref())?;
        let pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::set_pin_file(&file, pin.as_str());
        if let Some(mtime) = record.remote_mtime {
            let times =
                std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
            file.set_times(times)?;
        }
        let local = LocalStat::from_metadata(&file.metadata()?);
        let content_hash = Placeholder::content_hash_file(&file);
        let committed = self.store.commit_hydration_once(
            &account.id,
            &item_id,
            OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
            Some(local.size),
            Some(local.ino),
            content_hash.as_deref(),
        )?;
        if !committed {
            // Another completion won the state transition while this one was
            // collecting metadata.  Keep the operation idempotent: the bytes
            // and xattrs are already published, but do not emit a second UI or
            // activity notification.
            return Ok(());
        }
        self.refresh_folder_ancestors(&account, &item_id)?;
        self.emit(SyncEvent::HydrationFinished {
            path: path.to_path_buf(),
            ok: true,
        });
        Ok(())
    }

    /// A staged fill failed after the record entered Hydrating. Put it back in
    /// a retryable placeholder state and release UI state.
    pub fn fail_hydration(&self, path: &Path) -> Result<()> {
        let (_account, record, _file) = self.open_owned_placeholder(path, false)?;
        let item_id = record.id;
        self.store.set_item_state(&item_id, ItemState::Dehydrated)?;
        self.emit(SyncEvent::HydrationFinished {
            path: path.to_path_buf(),
            ok: false,
        });
        Ok(())
    }

    /// Remove a unique hydration hand-off without reopening mutable ancestor
    /// path components outside the account root.
    pub fn remove_staging_file(&self, path: &Path) -> Result<()> {
        let account = self.account()?;
        self.rooted(&account)?.remove_file(path)
    }

    /// Turn a downloaded file back into a placeholder, releasing its blocks.
    ///
    /// Refuses while the file has unsaved local changes: freeing space must
    /// never be a way to lose an edit.
    pub fn dehydrate(&self, path: &Path) -> Result<u64> {
        self.dehydrate_with_retention(path, None)
    }

    /// User-requested space release. Unlike automatic cache pressure, this is
    /// also an explicit choice not to retain the file on this device.
    pub(super) fn release_space(&self, path: &Path) -> Result<u64> {
        self.dehydrate_with_retention(path, Some(PinState::Unpinned))
    }

    fn dehydrate_with_retention(&self, path: &Path, explicit_pin: Option<PinState>) -> Result<u64> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let file = rooted.open_write(path)?;
        if !file.metadata()?.is_file() {
            return Err(Error::other(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let item_id = Placeholder::item_id_file(&file)
            .or_else(|| {
                // Not marked yet — the usual case the first time, since the file
                // arrived through an ordinary download.
                self.store
                    .items_by_inode(&self.account_id)
                    .ok()?
                    .into_iter()
                    .find(|(_, item)| {
                        self.store
                            .local_path(&account, &item.id)
                            .ok()
                            .flatten()
                            .is_some_and(|p| p == path)
                    })
                    .map(|(_, item)| item.id)
            })
            .ok_or_else(|| Error::other(format!("{} is not a synced file", path.display())))?;

        let record = self
            .store
            .item(&item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;

        if record.account_id != self.account_id
            || self.store.local_path(&account, &item_id)?.as_deref() != Some(path)
        {
            return Err(Error::other(format!(
                "{} is not owned by this account",
                path.display()
            )));
        }

        // Never punch holes in bytes that are not safely on the server. The
        // content hash is the authority here, not size and mtime: it catches an
        // in-place edit that preserved both, and it catches an xattr/DB desync
        // where the record wrongly says Dehydrated while the file is actually
        // full of un-uploaded content. A genuine placeholder — still sparse —
        // owns nothing and passes straight through.
        if self.disposable_local_version(&record, &file).is_none() {
            return Err(Error::other(format!(
                "{} has local changes that are not uploaded yet",
                path.display()
            )));
        }

        let mtime = record.remote_mtime;
        let freed = Placeholder::dehydrate_file(&file, &item_id, record.ctag.as_deref())?;
        let previous_pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let pin = explicit_pin.unwrap_or(previous_pin);
        if explicit_pin.is_some() {
            Placeholder::set_pin_file(&file, pin.as_str())?;
        } else {
            let _ = Placeholder::set_pin_file(&file, pin.as_str());
        }
        if let Some(mtime) = mtime {
            let times =
                std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
            file.set_times(times)?;
        }
        if explicit_pin.is_some() {
            if let Err(error) = self.store.commit_space_release(&account.id, &item_id) {
                let _ = Placeholder::set_pin_file(&file, previous_pin.as_str());
                return Err(error);
            }
        } else {
            self.store.set_item_state(&item_id, ItemState::Dehydrated)?;
        }
        let _ = self.refresh_folder_ancestors(&account, &item_id);
        Ok(freed)
    }

    /// Create a placeholder for an item that has never been downloaded.
    pub fn materialise_placeholder(&self, item_id: &str) -> Result<PathBuf> {
        materialise_placeholder_for_account(&self.store, &self.account()?, item_id)
    }
}
