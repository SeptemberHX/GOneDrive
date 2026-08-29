//! Remote delta collection and application to the local sync tree.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::db::{Account, ItemRecord, ItemState, OpKind, PinState};
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::models::DriveItem;
use crate::names::{conflict_copy_name, split_extension};
use crate::sync::plan::{
    decide_remote, partition_delta, record_from_remote, RemoteAction, SkipReason,
};

use super::engine::{stat_of, Engine, SyncEvent, SyncSummary};
use super::transfer;
impl Engine {
    pub(super) async fn remote_phase(&self, summary: &mut SyncSummary) -> Result<()> {
        let remote_result = match self.store.delta_batch(&self.account_id)? {
            Some(batch) if batch.mode == "full" => self.full_resync(summary).await,
            _ => self.pull_remote(summary).await,
        };
        match remote_result {
            Ok(()) => {}
            Err(Error::ResyncRequired) => {
                tracing::warn!("delta token rejected; re-enumerating the drive");
                self.store.set_delta_link(&self.account_id, None)?;
                self.store.abort_delta_batch(&self.account_id)?;
                self.full_resync(summary).await?;
            }
            Err(e) => {
                self.emit(SyncEvent::Failed {
                    name: "delta".into(),
                    error: e.to_string(),
                });
                return Err(e);
            }
        }
        Ok(())
    }

    // ---- remote -> local --------------------------------------------------

    async fn pull_remote(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        let initial_link = account.enum_link.clone().or(account.delta_link.clone());
        if self.store.delta_batch(&self.account_id)?.is_none() {
            self.store
                .begin_delta_batch(&self.account_id, "incremental", None)?;
        }
        let batch = self
            .collect_delta_batch("incremental", initial_link.as_deref())
            .await?;
        self.apply_staged_delta(&account, summary)?;
        summary.enumerated = batch.item_count;
        self.store
            .finish_delta_batch(&self.account_id, batch.delta_link.as_deref())?;
        Ok(())
    }

    /// Re-enumerate the whole drive after the service rejected our deltaLink.
    ///
    /// Anything the enumeration does not mention is gone remotely, which a
    /// plain re-run of delta cannot tell us — a fresh enumeration reports what
    /// exists, never what was removed.
    async fn full_resync(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        let existing = self.store.delta_batch(&self.account_id)?;
        let (generation, initial_link) = match existing {
            Some(batch) if batch.mode == "full" => {
                let generation = batch
                    .generation
                    .ok_or_else(|| Error::other("full delta batch has no generation"))?;
                (generation, account.enum_link.clone())
            }
            Some(_) => {
                self.store.abort_delta_batch(&self.account_id)?;
                let generation = self.store.begin_resync(&self.account_id)?;
                self.store
                    .begin_delta_batch(&self.account_id, "full", Some(generation))?;
                (generation, None)
            }
            None => {
                let generation = self.store.begin_resync(&self.account_id)?;
                self.store
                    .begin_delta_batch(&self.account_id, "full", Some(generation))?;
                (generation, None)
            }
        };
        let batch = self
            .collect_delta_batch("full", initial_link.as_deref())
            .await?;
        let seen = self.apply_staged_delta(&account, summary)?;
        self.store.mark_seen(&seen, generation)?;
        summary.enumerated = batch.item_count;

        // Sweep what the enumeration never mentioned.
        for stale in self
            .store
            .items_missed_by_resync(&self.account_id, generation)?
        {
            let Some(path) = self.store.local_path(&account, &stale.id)? else {
                self.store.delete_subtree(&stale.id)?;
                continue;
            };
            // The item is gone from the server, so the local copy would
            // normally be removed. But a resync must honour the same guarantee
            // the delta tombstone path does (KeepLocalAndReupload): a local file
            // carrying edits that were never uploaded is preserved and
            // re-offered, never deleted. Without this the sweep silently loses
            // an unsaved edit the ordinary delete path would have kept.
            if !stale.is_folder {
                if let Some(local) = stat_of(&path) {
                    if crate::sync::plan::local_is_modified(&stale, &local) {
                        self.store.delete_subtree(&stale.id)?;
                        self.store.enqueue(
                            &self.account_id,
                            OpKind::Upload,
                            None,
                            Some(&path),
                            None,
                        )?;
                        continue;
                    }
                }
            }
            self.remove_local(&stale, &path, summary)?;
        }
        self.store
            .finish_delta_batch(&self.account_id, batch.delta_link.as_deref())?;
        Ok(())
    }

    /// Fetch pages into SQLite before touching the filesystem. This retains the
    /// last occurrence of each item and survives a daemon restart between
    /// pages, while still delaying deletes and child resolution until the feed
    /// is complete.
    async fn collect_delta_batch(
        &self,
        expected_mode: &str,
        initial_link: Option<&str>,
    ) -> Result<crate::db::DeltaBatch> {
        loop {
            let batch = self
                .store
                .delta_batch(&self.account_id)?
                .ok_or_else(|| Error::other("delta batch disappeared"))?;
            if batch.mode != expected_mode {
                return Err(Error::other(format!(
                    "cannot resume {} delta batch as {expected_mode}",
                    batch.mode
                )));
            }
            if batch.complete {
                return Ok(batch);
            }

            let account = self.account()?;
            let link = account.enum_link.as_deref().or(if batch.page_count == 0 {
                initial_link
            } else {
                None
            });
            if batch.page_count > 0 && link.is_none() {
                return Err(Error::other(
                    "incomplete delta batch has no continuation link",
                ));
            }
            let page = self.graph.delta(link).await?;
            let payloads = page
                .value
                .iter()
                .map(|item| Ok((item.id.clone(), serde_json::to_string(item)?)))
                .collect::<Result<Vec<_>>>()?;
            self.store.stage_delta_page(
                &self.account_id,
                &payloads,
                page.next_link.as_deref(),
                page.delta_link.as_deref(),
            )?;
            let current = self
                .store
                .delta_batch(&self.account_id)?
                .ok_or_else(|| Error::other("delta batch disappeared after staging"))?;
            self.emit(SyncEvent::Enumerating {
                items: current.item_count,
                pages: current.page_count,
            });
        }
    }

    /// Apply the final occurrence of every staged item in dependency order.
    /// Returns the ids present in a full enumeration for its generation sweep.
    fn apply_staged_delta(
        &self,
        account: &Account,
        summary: &mut SyncSummary,
    ) -> Result<Vec<String>> {
        let items = self
            .store
            .staged_delta_payloads(&self.account_id)?
            .into_iter()
            .map(|payload| serde_json::from_str::<DriveItem>(&payload).map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        let (upserts, deletes) = partition_delta(items);
        let seen = upserts.iter().map(|item| item.id.clone()).collect();
        for item in &upserts {
            self.apply_remote(account, item, summary)?;
        }
        for item in &deletes {
            self.apply_remote(account, item, summary)?;
        }
        Ok(seen)
    }

    /// Apply one item from a delta page.
    fn apply_remote(
        &self,
        account: &Account,
        remote: &DriveItem,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let known = self.store.item(&remote.id)?;

        if remote.is_deleted() {
            let Some(known) = known else { return Ok(()) };
            let Some(path) = self.store.local_path(account, &known.id)? else {
                self.store.delete_subtree(&known.id)?;
                return Ok(());
            };
            let local = stat_of(&path);
            match decide_remote(remote, Some(&known), local.as_ref()) {
                RemoteAction::KeepLocalAndReupload => {
                    // Detach it from the remote item that no longer exists and
                    // re-offer the local content as something new.
                    self.store.delete_subtree(&known.id)?;
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Upload,
                        None,
                        Some(&path),
                        None,
                    )?;
                }
                _ => self.remove_local(&known, &path, summary)?,
            }
            return Ok(());
        }

        // The drive root anchors every path; record it and stop. Its pin stays
        // `Inherit` — see `ItemRecord::root`.
        if remote.is_root() {
            self.store
                .upsert_item(&ItemRecord::root(&self.account_id, &remote.id))?;
            return Ok(());
        }

        let Some(name) = remote.name.clone() else {
            return Ok(());
        };

        // The path comes from the *parent chain plus this name*, not from any
        // record we already hold, so a rename is seen at its new location.
        let Some(parent_id) = remote.parent_id() else {
            return Err(Error::other(format!(
                "remote item {} ({name}) has no parent",
                remote.id
            )));
        };
        let Some(parent_rel) = self.store.relative_path(parent_id)? else {
            // The complete batch is topologically ordered. Advancing its
            // deltaLink with an unresolved parent would lose this child
            // permanently, so keep the batch for a later retry instead.
            return Err(Error::other(format!(
                "remote item {} ({name}) refers to unknown parent {parent_id}",
                remote.id
            )));
        };
        let path = account.root_local_path.join(&parent_rel).join(&name);
        if let Some(existing) = &known {
            if let Some(old_path) = self.store.local_path(account, &existing.id)? {
                if old_path != path && old_path.exists() {
                    self.move_local_for_remote(&old_path, &path, summary)?;
                }
            }
        }
        let local = stat_of(&path);
        if let Some(existing) = &known {
            let old_version = existing.etag.as_deref().or(existing.ctag.as_deref());
            let remote_version = remote.etag.as_deref().or(remote.ctag.as_deref());
            if old_version != remote_version {
                self.store.cancel_outbound_ops_for_version(
                    &self.account_id,
                    &existing.id,
                    old_version,
                )?;
            }
        }

        // A dehydrated placeholder whose remote content changed is refreshed in
        // place — new size, new timestamp, new ctag — and stays a placeholder.
        // Its on-disk bytes are holes, not the user's edit, so the ordinary
        // conflict/download logic (which would see "local differs" and copy it
        // aside) must not run. The new content arrives when the file is next
        // opened. This is what makes a cloud change to an un-downloaded file
        // invisible to the user, as it should be.
        if !remote.is_folder() && !remote.is_deleted() {
            let rooted = self.rooted(account)?;
            let placeholder = match rooted.open_read(&path) {
                Ok(file)
                    if Placeholder::state_file(&file) == PlaceholderState::Dehydrated
                        && known.as_ref().is_some_and(|existing| {
                            Placeholder::item_id_file(&file).as_deref() == Some(&existing.id)
                        }) =>
                {
                    Some(file)
                }
                Ok(_) => None,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            if let (Some(existing), Some(_probe)) = (&known, placeholder) {
                let changed = existing.ctag.as_deref() != remote.ctag.as_deref()
                    || existing.size != remote.size.unwrap_or(0);
                if changed {
                    // Fall back to the known record time if the delta page
                    // omitted fileSystemInfo, so a refresh never stamps the
                    // placeholder with "now".
                    let mtime = remote.effective_mtime().or(existing.remote_mtime);
                    let file = rooted.open_write(&path)?;
                    if Placeholder::state_file(&file) != PlaceholderState::Dehydrated
                        || Placeholder::item_id_file(&file).as_deref() != Some(&existing.id)
                    {
                        return Err(transfer::file_changed_error(
                            &path,
                            "refreshing the placeholder",
                        ));
                    }
                    Placeholder::create_file(
                        &file,
                        remote.size.unwrap_or(0),
                        &remote.id,
                        remote.ctag.as_deref(),
                        mtime,
                    )?;
                    tracing::debug!(name = %name, "refreshed placeholder from a remote change");
                }
                let mut record = record_from_remote(
                    &self.account_id,
                    remote,
                    ItemState::Dehydrated,
                    PinState::Inherit,
                );
                record.pin_state = existing.pin_state;
                self.store.upsert_item(&record)?;
                return Ok(());
            }
        }

        let action = decide_remote(remote, known.as_ref(), local.as_ref());
        let mut state = ItemState::Synced;
        let local_was_modified =
            known
                .as_ref()
                .zip(local.as_ref())
                .is_some_and(|(known, local)| {
                    !local.is_dir && crate::sync::plan::local_is_modified(known, local)
                });

        match action {
            RemoteAction::CreateDirectory => {
                // Only inside a selected subtree. Building the full folder
                // structure for a drive the user has not chosen leaves tens of
                // thousands of empty directories behind for no benefit.
                if self.wants_content(known.as_ref(), parent_id)? {
                    self.rooted(account)?.ensure_dir(&path)?;
                    self.mark_dir(&remote.id)?;
                } else {
                    summary.not_downloaded += 1;
                }
            }
            RemoteAction::Download { .. } => {
                // A file the user has already materialised locally stays
                // materialised: once its real bytes are on disk, a cloud change
                // re-downloads in place rather than dropping back to a
                // placeholder. The old behaviour demoted it to Dehydrated but —
                // because a materialised file already exists — never punched
                // it, leaving the *stale* bytes readable with no event ever
                // fetching the new ones (a cloud edit the user silently never
                // saw). Occupancy, not the state xattr, is the robust test: an
                // app that rewrites a file (LibreOffice's save-and-rename)
                // strips our xattrs, but full allocation still means real
                // content lives here.
                //
                // An item the user has *not* materialised and not asked for is
                // recorded but not fetched: a drive is routinely larger than
                // the disk under it, so enumerating everything and downloading
                // nothing is the only safe thing to do before the user chooses.
                let materialised = local.as_ref().is_some_and(|l| !l.is_dir)
                    && crate::hydrate::Occupancy::read(&path)
                        .map(|o| o.apparent_size > 0 && o.allocated >= o.apparent_size)
                        .unwrap_or(false);
                if materialised || self.wants_content(known.as_ref(), parent_id)? {
                    state = ItemState::DirtyRemote;
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Download,
                        Some(&remote.id),
                        Some(&path),
                        None,
                    )?;
                } else {
                    state = ItemState::Dehydrated;
                    summary.not_downloaded += 1;
                    // In on-demand mode a newly-seen file becomes a placeholder
                    // right away, so it shows up in the file manager without
                    // waiting for a full re-materialisation. Reached only when
                    // the file is not materialised, so `create` (which
                    // is exclusive) cannot overwrite a local file that appears
                    // after discovery.
                    if self.config.on_demand {
                        self.create_placeholder_at(
                            account,
                            &path,
                            remote.size.unwrap_or(0),
                            &remote.id,
                            remote.ctag.as_deref(),
                            remote.effective_mtime(),
                        )?;
                        self.note_placeholder(&path);
                    }
                }
            }
            RemoteAction::RestampMtime => {
                if let Some(mtime) = remote.effective_mtime() {
                    let times = std::fs::FileTimes::new()
                        .set_modified(crate::sync::plan::to_filetime(mtime));
                    self.rooted(account)?.set_times(&path, times)?;
                }
            }
            RemoteAction::ConflictCopy => {
                let copy = self.make_conflict_copy(&path)?;
                summary.conflicts += 1;
                self.emit(SyncEvent::Conflict {
                    original: name.clone(),
                    copy: copy
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                });
                // The preserved copy goes up as a new file; the remote version
                // comes down under the original name. Neither is lost.
                self.store
                    .enqueue(&self.account_id, OpKind::Upload, None, Some(&copy), None)?;
                state = ItemState::DirtyRemote;
                self.store.enqueue(
                    &self.account_id,
                    OpKind::Download,
                    Some(&remote.id),
                    Some(&path),
                    None,
                )?;
            }
            RemoteAction::Skip(reason) => {
                if !matches!(reason, SkipReason::UpToDate) {
                    summary.skipped += 1;
                    self.emit(SyncEvent::Skipped {
                        name: name.clone(),
                        reason: format!("{reason:?}"),
                    });
                    return Ok(());
                }
                // A metadata-only remote change (especially a rename) does not
                // own local content. Keep an already-edited file dirty and
                // upload it after applying the new name instead of advancing
                // its local baseline and silently suppressing the edit.
                if local_was_modified {
                    state = ItemState::DirtyLocal;
                    self.store.enqueue_conditional(
                        &self.account_id,
                        OpKind::Upload,
                        &remote.id,
                        Some(&path),
                        None,
                        remote.etag.as_deref().or(remote.ctag.as_deref()),
                    )?;
                }
            }
            RemoteAction::DeleteLocal | RemoteAction::KeepLocalAndReupload => {
                unreachable!("handled in the tombstone branch above")
            }
        }

        let mut record = record_from_remote(&self.account_id, remote, state, PinState::Inherit);
        if let Some(known) = &known {
            // Carry forward the user's retention choice and our last local
            // observation; the delta page knows nothing about either.
            record.pin_state = known.pin_state;
            record.local_mtime = known.local_mtime;
            record.local_size = known.local_size;
            record.local_ino = known.local_ino;
        }
        self.store.upsert_item(&record)?;

        if state == ItemState::Synced {
            if let Some(local) = stat_of(&path) {
                self.store.set_local_stat(
                    &record.id,
                    OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                    Some(local.size),
                    Some(local.ino),
                )?;
            }
        }
        Ok(())
    }

    /// Keep a file's extended attributes in step with what we know.
    ///
    /// The file manager reads these directly rather than asking the daemon, so
    /// they are what the user actually sees; letting them drift means wrong
    /// emblems on correct files.
    pub(super) fn refresh_marks(&self, record: &ItemRecord, path: &Path) {
        if record.is_folder {
            return;
        }
        let Ok(account) = self.account() else {
            return;
        };
        let Ok(file) = self
            .rooted(&account)
            .and_then(|rooted| rooted.open_read(path))
        else {
            return;
        };
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return;
        }
        if Placeholder::item_id_file(&file).is_some_and(|item_id| item_id != record.id) {
            return;
        }
        let Ok(pin) = self
            .store
            .effective_pin(&record.id, self.config.download_new.default_pin())
        else {
            return;
        };
        let wanted = pin.as_str();
        if Placeholder::pin_file(&file).as_deref() != Some(wanted) {
            let _ = Placeholder::set_pin_file(&file, wanted);
        }
        // A file with content but no state mark predates this client knowing
        // about placeholders; claim it so it renders correctly.
        if Placeholder::state_file(&file) == PlaceholderState::Unmanaged
            && record.state == ItemState::Synced
        {
            let _ = Placeholder::mark_hydrated_file(&file, &record.id, record.ctag.as_deref());
        }
    }

    /// Whether this item's content should be on disk.
    ///
    /// An explicit choice on the item itself wins; otherwise the answer comes
    /// from the nearest pinned ancestor, and failing that from the account's
    /// download policy.
    fn wants_content(&self, known: Option<&ItemRecord>, parent_id: &str) -> Result<bool> {
        let default = self.config.download_new.default_pin();
        let pin = match known.map(|k| k.pin_state) {
            Some(PinState::Inherit) | None => self.store.effective_pin(parent_id, default)?,
            Some(explicit) => explicit,
        };
        Ok(pin.wants_content())
    }

    /// Record that a directory — and every directory above it — was written
    /// to, so their timestamps can be restored at the end of the pass.
    ///
    /// The ancestors matter: creating `a/b/c` moves the mtime of `a/b` and of
    /// `a` too, and marking only the leaf leaves both of those showing the time
    /// of the sync rather than the time the service reported.
    pub(super) fn mark_dir(&self, item_id: &str) -> Result<()> {
        let Ok(mut marked) = self.dir_stamps.lock() else {
            return Ok(());
        };
        let mut current = Some(item_id.to_string());
        while let Some(id) = current {
            if !marked.insert(id.clone()) {
                break; // this chain has already been walked
            }
            current = self.store.item(&id)?.and_then(|item| item.parent_id);
        }
        Ok(())
    }

    /// Apply the recorded directory timestamps, deepest first.
    ///
    /// Depth order is what makes one pass enough: stamping a parent before its
    /// children were finished would be undone by the child's own write.
    pub(super) fn stamp_directories(&self, account: &Account) -> Result<()> {
        let ids: Vec<String> = match self.dir_stamps.lock() {
            Ok(mut marked) => marked.drain().collect(),
            Err(_) => return Ok(()),
        };

        let mut targets = Vec::new();
        for id in ids {
            let Some(record) = self.store.item(&id)? else {
                continue;
            };
            let (Some(mtime), Some(path)) =
                (record.remote_mtime, self.store.local_path(account, &id)?)
            else {
                continue;
            };
            if path.is_dir() {
                targets.push((path.components().count(), path, mtime));
            }
        }

        targets.sort_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
        let rooted = self.rooted(account)?;
        for (_, path, mtime) in targets {
            let times =
                std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
            if let Err(e) = rooted.set_times(&path, times) {
                tracing::debug!(path = %path.display(), error = %e, "could not stamp directory");
            }
        }
        Ok(())
    }

    fn remove_local(
        &self,
        known: &ItemRecord,
        path: &Path,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let rooted = self.rooted(&self.account()?)?;
        let result = if known.is_folder {
            // Only an empty directory is removed. A non-empty one means the
            // user put something there that the remote delete says nothing
            // about, and recursive deletion would destroy it.
            rooted.remove_dir(path)
        } else {
            rooted.remove_file(path)
        };
        match result {
            Ok(()) => {
                summary.deleted_local += 1;
                self.store.delete_subtree(&known.id)?;
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.store.delete_subtree(&known.id)?;
            }
            Err(Error::Io(e))
                if known.is_folder && e.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
            {
                // Graph explicitly says a deleted folder should only be
                // removed locally once it is empty. What remains is local-only
                // content, so detach the remote identities and let the local
                // scan offer the tree back instead of recursively deleting it.
                tracing::info!(path = %path.display(), "kept non-empty remotely deleted folder");
                self.store.delete_subtree(&known.id)?;
            }
            Err(e) => {
                return Err(Error::other(format!(
                    "could not remove {} locally: {e}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Apply a remote rename/move to the existing local inode before updating
    /// its database record. Folder descendants follow automatically because
    /// their paths are derived from the renamed parent's id.
    fn move_local_for_remote(
        &self,
        old_path: &Path,
        new_path: &Path,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        if let Some(parent) = new_path.parent() {
            rooted.ensure_dir(parent)?;
        }

        // The destination can appear after any userspace existence check. Let
        // the kernel arbitrate the rename, preserve an occupant, then retry.
        // The bound prevents a process continuously recreating the target from
        // making one delta item spin forever.
        let mut moved = false;
        for _ in 0..128 {
            match rooted.rename_noreplace(old_path, new_path) {
                Ok(()) => {
                    moved = true;
                    break;
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let copy = match self.make_conflict_copy(new_path) {
                        Ok(copy) => copy,
                        // The occupant was removed after renameat2 reported it.
                        // There is nothing left to preserve, so retry the move.
                        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if rooted.metadata(&copy)?.is_some_and(|meta| meta.is_file()) {
                        self.store.enqueue(
                            &self.account_id,
                            OpKind::Upload,
                            None,
                            Some(&copy),
                            None,
                        )?;
                    }
                    summary.conflicts += 1;
                    self.emit(SyncEvent::Conflict {
                        original: new_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        copy: copy
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    });
                }
                Err(error) => return Err(error),
            }
        }
        if !moved {
            return Err(Error::other(format!(
                "destination {} kept reappearing during remote move",
                new_path.display()
            )));
        }
        self.store
            .rebase_pending_paths(&self.account_id, old_path, new_path)?;
        tracing::info!(from = %old_path.display(), to = %new_path.display(), "applied remote move");
        Ok(())
    }

    /// Move a local file aside under a conflict name, returning the new path.
    pub(super) fn make_conflict_copy(&self, path: &Path) -> Result<PathBuf> {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let copy_name = conflict_copy_name(&name, &self.hostname, OffsetDateTime::now_utc());
        let base = path.with_file_name(copy_name);
        let rooted = self.rooted(&self.account()?)?;

        // Two conflicts in the same second have the same base name. Pick the
        // numbered variant with RENAME_NOREPLACE so another process cannot
        // create it between selection and rename and be overwritten.
        for number in 1..=u32::MAX {
            let copy = numbered_copy_path(&base, number);
            match rooted.rename_noreplace(path, &copy) {
                Ok(()) => {
                    tracing::info!(from = %path.display(), to = %copy.display(), "kept local copy");
                    return Ok(copy);
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::other(format!(
            "could not allocate a conflict-copy name for {}",
            path.display()
        )))
    }
}

fn numbered_copy_path(base: &Path, number: u32) -> PathBuf {
    if number == 1 {
        return base.to_path_buf();
    }
    let name = base
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let (stem, extension) = split_extension(&name);
    let numbered = match extension {
        Some(extension) => format!("{stem} ({number}).{extension}"),
        None => format!("{stem} ({number})"),
    };
    base.with_file_name(numbered)
}
