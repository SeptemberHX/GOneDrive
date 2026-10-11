//! Remote delta collection and application to the local sync tree.

mod delta;

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::db::{Account, ItemRecord, ItemState, OpKind, PinState};
use crate::error::{Error, Result};
use crate::hydrate::{DirectoryPopulation, Placeholder, PlaceholderState};
use crate::models::DriveItem;
use crate::names::{conflict_copy_name, split_extension};
use crate::sync::plan::{decide_remote, record_from_remote, LocalStat, RemoteAction, SkipReason};

use super::engine::{stat_of, Engine, SyncEvent, SyncSummary};

/// Keep a reserved name to characters every filesystem accepts. Graph ids are
/// already conservative; this guards against a future one that is not.
fn sanitise_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}
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
                // A transient failure — typically the network not yet back
                // after resume from suspend — is already reported through the
                // returned error as the account's "offline" state, and the
                // next poll retries it. Logging it as a failed activity too
                // buries the real history under one entry per wake-up.
                if !e.is_transient() {
                    self.emit(SyncEvent::Failed {
                        name: "delta".into(),
                        path: None,
                        error: e.to_string(),
                    });
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Apply one item from a delta page.
    fn apply_remote(
        &self,
        account: &Account,
        remote: &DriveItem,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let _projection = self
            .remote_apply
            .lock()
            .map_err(|_| Error::other("remote namespace lock was poisoned"))?;
        self.apply_remote_locked(account, remote, summary)
    }

    fn apply_remote_locked(
        &self,
        account: &Account,
        remote: &DriveItem,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let known = self.store.item(&remote.id)?;

        if remote.is_deleted() {
            return self.apply_remote_tombstone(account, remote, known, summary);
        }

        // The drive root anchors every path; record it and stop. Its pin stays
        // `Inherit` — see `ItemRecord::root`.
        if remote.is_root() {
            let mut root = ItemRecord::root(&self.account_id, &remote.id);
            root.child_count = remote
                .folder
                .as_ref()
                .and_then(|folder| folder.child_count.map(u64::from));
            self.store.upsert_item(&root)?;
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
        if self.refresh_dehydrated_placeholder(account, remote, &known, &path, &name)? {
            return Ok(());
        }

        let action = decide_remote(remote, known.as_ref(), local.as_ref());
        let mut state = ItemState::Synced;
        let mut published_placeholder = None;
        let mut directory_population = None;
        let local_was_modified =
            known
                .as_ref()
                .zip(local.as_ref())
                .is_some_and(|(known, local)| {
                    !local.is_dir && crate::sync::plan::local_is_modified(known, local)
                });

        match action {
            RemoteAction::CreateDirectory => {
                // Selective sync creates only chosen directories. Once the user
                // materialises the drive, every later remote folder (including
                // an empty or restored one) belongs in the browsable tree.
                let on_demand = self.on_demand_enabled();
                if on_demand {
                    // Every newly discovered directory starts partial, including
                    // metadata-only descendants whose inode is not published yet.
                    // Its direct children are fetched on first enumeration or by
                    // the bounded one-level lookahead of an opened parent.
                    directory_population = Some(false);
                }
                let publish = if on_demand {
                    self.remote_parent_is_populated(account, parent_id)?
                } else {
                    self.wants_content(known.as_ref(), parent_id)?
                };
                if publish {
                    let rooted = self.rooted(account)?;
                    rooted.ensure_dir(&path)?;
                    if on_demand {
                        let dir = rooted.open_read(&path)?;
                        Placeholder::mark_dir_file(&dir, PlaceholderState::Dehydrated, &remote.id)?;
                        Placeholder::set_directory_population_file(
                            &dir,
                            DirectoryPopulation::Partial,
                        )?;
                    }
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
                // saw). Content extents, not the state xattr or `st_blocks`,
                // are the robust test: xattr metadata can make a tiny sparse
                // placeholder look fully allocated, while an
                // app that rewrites a file (LibreOffice's save-and-rename)
                // strips our xattrs, but full allocation still means real
                // content lives here.
                //
                // An item the user has *not* materialised and not asked for is
                // recorded but not fetched: a drive is routinely larger than
                // the disk under it, so enumerating everything and downloading
                // nothing is the only safe thing to do before the user chooses.
                let materialised = local.as_ref().is_some_and(|l| !l.is_dir)
                    && Placeholder::content_layout(&path)
                        .map(|layout| layout.is_fully_materialized())
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
                    if self.on_demand_enabled()
                        && self.remote_parent_is_populated(account, parent_id)?
                    {
                        self.create_placeholder_at(
                            account,
                            &path,
                            remote.size.unwrap_or(0),
                            &remote.id,
                            remote.ctag.as_deref(),
                            remote.effective_mtime(),
                        )?;
                        self.note_placeholder(&path);
                        published_placeholder = stat_of(&path);
                    }
                }
            }
            RemoteAction::RestampMtime => {
                if let Some(mtime) = remote.effective_mtime() {
                    let times = std::fs::FileTimes::new()
                        .set_modified(crate::sync::plan::to_filetime(mtime));
                    self.rooted(account)?.set_times(&path, times)?;
                }
                state = passive_state(known.as_ref());
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
                    path: path.clone(),
                });
                // The preserved copy goes up as a new file; the remote version
                // comes down under the original name. Neither is lost.
                self.store
                    .enqueue(&self.account_id, OpKind::Upload, None, Some(&copy), None)?;
                if remote.is_folder() {
                    // A file occupied the folder's name. After preserving that
                    // file, publish the remote directory itself; folders have no
                    // content endpoint and must never enter the download queue.
                    let rooted = self.rooted(account)?;
                    rooted.ensure_dir(&path)?;
                    if self.on_demand_enabled() {
                        let dir = rooted.open_read(&path)?;
                        Placeholder::mark_dir_file(&dir, PlaceholderState::Dehydrated, &remote.id)?;
                        Placeholder::set_directory_population_file(
                            &dir,
                            DirectoryPopulation::Partial,
                        )?;
                        directory_population = Some(false);
                    }
                    self.mark_dir(&remote.id)?;
                    state = ItemState::Synced;
                } else {
                    state = ItemState::DirtyRemote;
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Download,
                        Some(&remote.id),
                        Some(&path),
                        None,
                    )?;
                }
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
                } else {
                    state = passive_state(known.as_ref());
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
        // A placeholder published just now exists on disk from this moment.
        // Recording it here rather than at the next scan of its folder means a
        // rename or delete of it before that scan is recognised as one —
        // otherwise a later cTag-only delta re-creates it under the old name.
        if record.local_ino.is_none() {
            if let Some(placeholder) = published_placeholder {
                record.local_mtime = OffsetDateTime::from_unix_timestamp(placeholder.mtime).ok();
                record.local_size = Some(placeholder.size);
                record.local_ino = Some(placeholder.ino);
            }
        }
        if let Some(known) = &known {
            if state == ItemState::DirtyRemote {
                // Until the queued download atomically lands, these hashes
                // describe the local byte version that is still on disk. The
                // overwrite guard needs that baseline to distinguish an
                // untouched old copy from a concurrent local edit. The newly
                // downloaded bytes replace it in `commit_hydration`.
                record.quickxor = known.quickxor.clone();
                record.sha1 = known.sha1.clone();
            }
        }
        self.store.upsert_item(&record)?;
        if let Some(populated) = directory_population {
            self.store
                .set_folder_population(&self.account_id, &record.id, populated)?;
        }

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
    /// `inherited` is the pin this file's directory passes down; the scan
    /// resolves it once per directory, because it cannot differ between
    /// siblings. An explicit pin on the item itself still wins, which is the
    /// same rule [`Engine::wants_content`] applies.
    pub(super) fn refresh_marks_file(
        &self,
        record: &ItemRecord,
        seen: &crate::sync::local::Inspected,
        inherited: PinState,
    ) {
        if record.is_folder {
            return;
        }
        if !seen
            .file
            .metadata()
            .is_ok_and(|metadata| metadata.is_file())
        {
            return;
        }
        // Application atomic-save APIs replace the inode and intentionally do
        // not preserve user xattrs. Reclaiming that new inode would emit a
        // GFileMonitor attribute-change event after every successful upload,
        // which editors report as an external modification. Only maintain
        // marks on an inode that already proves it belongs to this item.
        if seen.item_id.as_deref() != Some(record.id.as_str()) {
            return;
        }
        let pin = match record.pin_state {
            PinState::Inherit => inherited,
            explicit => explicit,
        };
        let wanted = pin.as_str();
        if Placeholder::pin_file(&seen.file).as_deref() != Some(wanted) {
            let _ = Placeholder::set_pin_file(&seen.file, wanted);
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

    /// Whether children of this directory belong in the local namespace now.
    /// The root is the first populated directory. Child folders start partial
    /// and are promoted when a file manager asks to enumerate them, either
    /// directly or through the parent's bounded lookahead.
    fn remote_parent_is_populated(&self, account: &Account, parent_id: &str) -> Result<bool> {
        if account.root_item_id.as_deref() == Some(parent_id) {
            return Ok(true);
        }
        if let Some(populated) = self.store.folder_population(&self.account_id, parent_id)? {
            return Ok(populated);
        }
        let Some(path) = self.store.local_path(account, parent_id)? else {
            return Ok(false);
        };
        let rooted = self.rooted(account)?;
        let dir = match rooted.open_read(&path) {
            Ok(dir) if dir.metadata()?.is_dir() => dir,
            Ok(_) => return Ok(false),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false)
            }
            Err(error) => return Err(error),
        };
        let populated = match Placeholder::directory_population_file(&dir) {
            DirectoryPopulation::Populated => true,
            DirectoryPopulation::Partial => false,
            DirectoryPopulation::Unmanaged => {
                // Trees made by releases before lazy population already contain
                // every child. Adopt them as populated instead of hiding later
                // delta additions until each old directory is opened once.
                if Placeholder::item_id_file(&dir).as_deref() == Some(parent_id) {
                    Placeholder::set_directory_population_file(
                        &dir,
                        DirectoryPopulation::Populated,
                    )?;
                    true
                } else {
                    false
                }
            }
        };
        self.store
            .set_folder_population(&self.account_id, parent_id, populated)?;
        Ok(populated)
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

    /// Apply a tombstone: the service says this item is gone.
    fn apply_remote_tombstone(
        &self,
        account: &Account,
        remote: &DriveItem,
        known: Option<ItemRecord>,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let Some(known) = known else { return Ok(()) };
        let Some(path) = self.store.local_path(account, &known.id)? else {
            self.store.delete_subtree(&known.id)?;
            return Ok(());
        };
        let local = stat_of(&path);
        // An owned, still-dehydrated placeholder holds no local content
        // whatever its recorded stat says: a refresh rewrote its size on
        // disk without touching `local_size`, and one created from the
        // download branch never recorded a stat at all. Keeping it as a
        // "local edit" left an orphan the user could see but never open.
        let untouched_placeholder = !known.is_folder
            && super::local::confirmed_online_only(&self.rooted(account)?, &path, &known.id);
        match decide_remote(remote, Some(&known), local.as_ref()) {
            RemoteAction::KeepLocalAndReupload if !untouched_placeholder => {
                // Detach it from the remote item that no longer exists and
                // re-offer the local content as something new.
                self.store.delete_subtree(&known.id)?;
                self.store
                    .enqueue(&self.account_id, OpKind::Upload, None, Some(&path), None)?;
            }
            _ => self.remove_local(&known, &path, summary)?,
        }
        Ok(())
    }

    /// Refresh a dehydrated placeholder whose remote content changed, in place.
    ///
    /// It stays a placeholder: new size, new timestamp, new ctag. Its on-disk
    /// bytes are holes, not the user's edit, so the ordinary conflict and
    /// download logic — which would see "local differs" and copy it aside —
    /// must not run. The new content arrives when the file is next opened,
    /// which is what makes a cloud change to an un-downloaded file invisible
    /// to the user, as it should be.
    ///
    /// Returns whether this handled the item.
    fn refresh_dehydrated_placeholder(
        &self,
        account: &Account,
        remote: &DriveItem,
        known: &Option<ItemRecord>,
        path: &Path,
        name: &str,
    ) -> Result<bool> {
        if remote.is_folder() || remote.is_deleted() {
            return Ok(false);
        }
        let rooted = self.rooted(account)?;
        let placeholder = match rooted.open_read(path) {
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
                let file = rooted.open_write(path)?;
                if Placeholder::state_file(&file) != PlaceholderState::Dehydrated
                    || Placeholder::item_id_file(&file).as_deref() != Some(&existing.id)
                {
                    return Err(transfer::file_changed_error(
                        path,
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
            if changed {
                // The placeholder's on-disk size and mtime just changed;
                // keep the recorded local version in step so the scan does
                // not read the refresh as a local edit.
                if let Ok(Some(meta)) = self.rooted(account)?.metadata(path) {
                    let local = LocalStat::from_metadata(&meta);
                    self.store.set_local_stat(
                        &existing.id,
                        OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                        Some(local.size),
                        Some(local.ino),
                    )?;
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Move whatever holds `path` aside when it is an item this delta batch is
    /// about to move elsewhere. Returns where it was parked, or `None` when the
    /// occupant is not ours to move.
    fn park_rearranged_occupant(
        &self,
        rooted: &crate::sync::rooted_fs::RootedFs,
        path: &Path,
    ) -> Result<Option<PathBuf>> {
        let account = self.account()?;
        let Some(root_item_id) = account.root_item_id.as_deref() else {
            return Ok(None);
        };
        let Ok(relative) = path.strip_prefix(&account.root_local_path) else {
            return Ok(None);
        };
        let Some(occupant) = self
            .store
            .item_at_path(&self.account_id, root_item_id, relative)?
        else {
            // Nothing we track lives here, so the file is the user's.
            return Ok(None);
        };
        if !self
            .store
            .staged_delta_is_unapplied(&self.account_id, &occupant.id)?
        {
            // It is staying. Whatever is here is content this device owns.
            return Ok(None);
        }

        // The id makes the reserved name unique within the directory, so two
        // parked siblings cannot collide, and the dot keeps it out of the way
        // in a file manager for the moments it exists.
        let parked_name = format!(".gonedrive-moving-{}", sanitise_id(&occupant.id));
        let parked = path.with_file_name(&parked_name);
        rooted.rename_noreplace(path, &parked)?;

        // Both the record and any queued work have to follow the inode. A
        // download already queued for this item still names the old path; left
        // there it would write these bytes over whatever moves into that name
        // next, and the scan would then read the result as an edit of the
        // wrong file.
        let bookkeeping = self
            .store
            .rename_item_local(&occupant.id, &parked_name)
            .and_then(|()| {
                self.store
                    .rebase_pending_paths(&self.account_id, path, &parked)
            });
        if let Err(error) = bookkeeping {
            // The rename is the only half already on disk. Undo it so the two
            // cannot disagree about where this item lives: a record pointing
            // at a name that no longer exists reads to the next scan as a
            // deletion of a file the user never touched.
            let _ = self.store.rename_item_local(&occupant.id, &occupant.name);
            let _ = rooted.rename_noreplace(&parked, path);
            return Err(error);
        }
        Ok(Some(parked))
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
                    // The name may be held by an item this same batch is
                    // moving elsewhere — a rename chain (a→b, b→c) or a swap.
                    // That is a rearrangement, not a conflict: making the
                    // occupant a conflict copy would duplicate content the
                    // user never touched, upload it, and leave the wrong bytes
                    // under the right name. Park it instead; its own entry
                    // moves it on from there.
                    if let Some(parked) = self.park_rearranged_occupant(&rooted, new_path)? {
                        tracing::info!(
                            name = %new_path.display(),
                            parked = %parked.display(),
                            "parked an item this batch moves elsewhere"
                        );
                        continue;
                    }
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
                        path: new_path.to_path_buf(),
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

/// The state a branch that neither fetched nor published content may record.
///
/// `Synced` is a claim about local content, and these branches make no such
/// claim: restamping a timestamp and applying a metadata-only change both leave
/// the bytes exactly as they were. Writing `Synced` anyway discarded whatever
/// the record was still owed — a queued download, a hydration in flight — and
/// on a personal drive, where there is no remote hash to compare against,
/// nothing afterwards noticed the file had never arrived.
fn passive_state(known: Option<&ItemRecord>) -> ItemState {
    match known.map(|known| known.state) {
        Some(
            state @ (ItemState::DirtyRemote
            | ItemState::Hydrating
            | ItemState::DirtyLocal
            | ItemState::Conflict),
        ) => state,
        _ => ItemState::Synced,
    }
}
