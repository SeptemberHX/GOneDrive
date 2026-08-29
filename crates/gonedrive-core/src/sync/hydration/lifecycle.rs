//! Placeholder state transitions for hydration and dehydration.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::db::ItemState;
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::sync::plan::LocalStat;

use super::super::engine::{Engine, SyncEvent};

impl Engine {
    /// Record that a placeholder the helper just filled is now complete.
    pub fn finish_hydration(&self, path: &Path) -> Result<()> {
        let (account, record, file) = self.open_owned_placeholder(path, true)?;
        let item_id = record.id.clone();

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
        self.store.set_local_stat(
            &item_id,
            OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
            Some(local.size),
            Some(local.ino),
        )?;
        self.store.set_item_state(&item_id, ItemState::Synced)?;
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

    /// Hide a placeholder that fanotify could not protect. Its remote metadata
    /// remains in the database, but no zero-readable sparse file remains on
    /// disk and local scanning will not mistake the absence for a user delete.
    pub fn discard_unwatched_placeholder(&self, path: &Path) -> Result<()> {
        let (account, record, file) = self.open_owned_placeholder(path, false)?;
        if Placeholder::state_file(&file) != PlaceholderState::Dehydrated {
            return Err(Error::other(format!(
                "{} is not dehydrated",
                path.display()
            )));
        }
        let item_id = record.id;
        drop(file);
        match self.rooted(&account)?.remove_file(path) {
            Ok(()) => {}
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.store.set_local_stat(&item_id, None, None, None)?;
        self.store.set_item_state(&item_id, ItemState::Dehydrated)?;
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
        let pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::set_pin_file(&file, pin.as_str());
        if let Some(mtime) = mtime {
            let times =
                std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
            file.set_times(times)?;
        }
        self.store.set_item_state(&item_id, ItemState::Dehydrated)?;
        let _ = self.refresh_folder_ancestors(&account, &item_id);
        Ok(freed)
    }

    /// Create a placeholder for an item that has never been downloaded.
    pub fn materialise_placeholder(&self, item_id: &str) -> Result<PathBuf> {
        let account = self.account()?;
        let record = self
            .store
            .item(item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;
        let path = self
            .store
            .local_path(&account, item_id)?
            .ok_or_else(|| Error::other(format!("no local path for {item_id}")))?;

        self.create_placeholder_at(
            &account,
            &path,
            record.size,
            item_id,
            record.ctag.as_deref(),
            record.remote_mtime,
        )?;
        self.store.set_item_state(item_id, ItemState::Dehydrated)?;
        Ok(path)
    }
}
