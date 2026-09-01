//! Bulk materialization, space reclamation, and state repair policy.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::db::{Account, ItemState, PinState, Store};
use crate::error::{Error, Result};
use crate::hydrate::{DirectoryPopulation, Placeholder, PlaceholderState};
use crate::sync::plan::LocalStat;

use super::super::engine::{Engine, FreeSpaceOutcome, SyncEvent};
use super::super::rooted_fs::RootedFs;
use super::super::transfer;

/// Local files-on-demand state derived without a live Graph client.
#[derive(Debug, Default)]
pub struct PlaceholderInventory {
    /// Sparse files that exist and need a kernel interception mark.
    pub existing: Vec<PathBuf>,
    /// Remote item ids whose placeholder is absent or whose creation stopped
    /// before it became visible.
    pub missing: Vec<String>,
}

/// Discover every on-disk placeholder that needs kernel interception without
/// requiring a live Graph client. HSM protection is a local safety property:
/// it must come up even when the token endpoint or the login keyring is
/// temporarily unavailable.
pub fn dehydrated_paths_for_account(store: &Store, account: &Account) -> Result<Vec<PathBuf>> {
    Ok(placeholder_inventory_for_account(store, account)?.existing)
}

/// Inventory existing and missing placeholders in one database/path pass.
/// Startup already has to inspect these records to rebuild ephemeral fanotify
/// marks; carrying missing ids out of that pass avoids a second whole-drive
/// traversal when an earlier mark failure removed an unsafe sparse file.
pub fn placeholder_inventory_for_account(
    store: &Store,
    account: &Account,
) -> Result<PlaceholderInventory> {
    let rooted = RootedFs::open(&account.root_local_path)?;
    let mut inventory = PlaceholderInventory::default();

    // The database narrows the search; the file's own xattr decides. A
    // hydration interrupted by a crash leaves the record saying `Hydrating`
    // while the file on disk is still empty, and trusting the record there
    // would leave that file unwatched and silently readable as zeroes.
    for record in store.placeholder_inventory_candidates(&account.id)? {
        let Some(path) = store.local_path(account, &record.id)? else {
            continue;
        };
        let file = match rooted.open_read(&path) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if missing_placeholder_was_published(store, account, &rooted, &record, &path)? {
                    inventory.missing.push(record.id);
                }
                continue;
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                use std::os::unix::fs::MetadataExt;
                let path_file = rooted.open_path(&path)?;
                if path_file.metadata()?.mode() & 0o7777 == 0 {
                    inventory.existing.push(path);
                    continue;
                }
                return Err(Error::Io(error));
            }
            Err(error) => return Err(error),
        };
        if Placeholder::item_id_file(&file).as_deref() == Some(&record.id)
            && Placeholder::state_file(&file) == PlaceholderState::Dehydrated
        {
            inventory.existing.push(path);
        }
    }
    inventory.existing.sort();
    inventory.existing.dedup();
    inventory.missing.sort();
    inventory.missing.dedup();
    Ok(inventory)
}

/// Populated directories that define lazy-enumeration boundaries for their
/// direct child directories. One mark on a populated parent covers any number
/// of partial children, keeping startup work proportional to browsed folders.
pub fn population_watch_directories_for_account(
    store: &Store,
    account: &Account,
) -> Result<Vec<PathBuf>> {
    let rooted = RootedFs::open(&account.root_local_path)?;
    let mut paths = Vec::new();
    for record in store.population_watch_candidates(&account.id)? {
        let Some(path) = store.local_path(account, &record.id)? else {
            continue;
        };
        let dir = match rooted.open_read(&path) {
            Ok(dir) if dir.metadata()?.is_dir() => dir,
            Ok(_) => continue,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if Placeholder::item_id_file(&dir).as_deref() == Some(record.id.as_str())
            && Placeholder::directory_population_file(&dir) == DirectoryPopulation::Populated
        {
            paths.push(path);
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn missing_placeholder_was_published(
    store: &Store,
    account: &Account,
    rooted: &RootedFs,
    record: &crate::db::ItemRecord,
    path: &Path,
) -> Result<bool> {
    // Once an inode was observed, its absence needs repair/reconciliation even
    // if its parent later became partial. The normal case for lazy metadata has
    // no local observation and is governed by its parent's population marker.
    if record.local_ino.is_some() {
        return Ok(true);
    }
    if record.parent_id.as_deref() == account.root_item_id.as_deref() {
        return Ok(true);
    }
    let Some(parent_id) = record.parent_id.as_deref() else {
        return Ok(false);
    };
    if store.folder_population(&account.id, parent_id)? == Some(false) {
        return Ok(false);
    }
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let dir = match rooted.open_read(parent) {
        Ok(dir) if dir.metadata()?.is_dir() => dir,
        Ok(_) => return Ok(false),
        // No population row means a pre-lazy tree. Preserve its historical
        // repair behaviour; only an explicit partial row proves the inode was
        // intentionally never published.
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    Ok(match Placeholder::directory_population_file(&dir) {
        DirectoryPopulation::Populated => true,
        DirectoryPopulation::Partial => false,
        DirectoryPopulation::Unmanaged => {
            Placeholder::item_id_file(&dir).as_deref() == Some(parent_id)
        }
    })
}

impl Engine {
    /// Files under a path that exist locally right now.
    ///
    /// The caller that is about to dehydrate them needs this list *first*, so
    /// it can lift their kernel watches before any file is written to: a
    /// dehydration writes into watched files, and a write into a watched file
    /// asks the watcher — us — whether to allow it.
    pub fn subtree_local_files(&self, relative: &Path) -> Result<Vec<PathBuf>> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        let files = if target.is_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target]
        };
        let mut out = Vec::new();
        for file in files {
            if let Some(path) = self.store.local_path(&account, &file.id)? {
                if rooted.open_read(&path).is_ok_and(|local| {
                    local.metadata().is_ok_and(|metadata| metadata.is_file())
                        && Placeholder::item_id_file(&local).as_deref() == Some(&file.id)
                }) {
                    out.push(path);
                }
            }
        }
        Ok(out)
    }

    /// Turn a downloaded subtree into placeholders, freeing its space.
    ///
    /// Files with unsaved local changes are left alone and reported: freeing
    /// space must never be a way to lose an edit.
    pub fn free_space(&self, relative: &Path) -> Result<FreeSpaceOutcome> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        let releasing_folder = target.is_folder;
        let files = if releasing_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target]
        };

        // Releasing a folder is also a standing choice for content discovered
        // under it later. Clear the folder pin once, canceling its queued
        // downloads, then let every successfully dehydrated child inherit that
        // choice. A single-file request records the exclusion on that file.
        if releasing_folder {
            self.set_pin(relative, PinState::Unpinned)?;
        }

        let mut outcome = FreeSpaceOutcome::default();
        for file in files {
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            if rooted.metadata(&path)?.is_none() {
                continue;
            }
            let released = if releasing_folder {
                self.dehydrate(&path)
            } else {
                self.release_space(&path)
            };
            match released {
                Ok(freed) => {
                    outcome.freed += freed;
                    outcome.files += 1;
                    outcome.paths.push(path);
                }
                Err(e) => outcome.skipped.push((path, e.to_string())),
            }
        }
        Ok(outcome)
    }

    /// Replace never-downloaded items in a subtree with placeholders.
    pub fn make_on_demand(&self, relative: &Path) -> Result<FreeSpaceOutcome> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        let files = if target.is_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target]
        };

        let mut outcome = FreeSpaceOutcome::default();
        for file in files {
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            if rooted.metadata(&path)?.is_some() {
                // Already on disk: freeing it is `free_space`'s job, not this one.
                continue;
            }
            match self.materialise_placeholder(&file.id) {
                Ok(path) => {
                    outcome.files += 1;
                    outcome.paths.push(path);
                }
                Err(e) => outcome.skipped.push((path, e.to_string())),
            }
        }
        Ok(outcome)
    }

    /// Publish one directory's direct children and mark that directory fully
    /// populated. Remote metadata for deeper descendants stays in SQLite until
    /// their own parent is browsed, matching Cloud Files' FETCH_PLACEHOLDERS
    /// contract without making first login create the whole drive's inodes.
    pub async fn populate_folder(&self, relative: &Path) -> Result<FreeSpaceOutcome> {
        let _population = self.folder_population.lock().await;
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let mut target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such folder: {}", relative.display())))?;
        if !target.is_folder {
            return Err(Error::other(format!(
                "{} is not a folder",
                relative.display()
            )));
        }
        let target_path = self
            .store
            .local_path(&account, &target.id)?
            .ok_or_else(|| Error::other(format!("no local path for {}", target.id)))?;
        rooted.ensure_dir(&target_path)?;
        let target_dir = rooted.open_read(&target_path)?;
        if !target_dir.metadata()?.is_dir() {
            return Err(Error::other(format!(
                "{} is occupied by a file",
                target_path.display()
            )));
        }

        // Repeated InfoProvider callbacks are common while Nautilus redraws.
        // Once populated, delta itself maintains direct additions and removals.
        let disk_population = Placeholder::directory_population_file(&target_dir);
        if disk_population == DirectoryPopulation::Populated {
            self.store
                .set_folder_population(&self.account_id, &target.id, true)?;
            self.record_directory_local_stat(&target.id, &target_dir)?;
            return Ok(FreeSpaceOutcome::default());
        }

        // A partial directory intentionally has no complete child set in SQLite.
        // Fetch only its direct children; the delta cursor was established first
        // and reconciles changes that race this non-snapshot pagination.
        if disk_population == DirectoryPopulation::Partial
            || self.store.folder_population(&self.account_id, &target.id)? == Some(false)
        {
            let mut summary = super::super::engine::SyncSummary::default();
            self.fetch_folder_children(&account, &target.id, &mut summary)
                .await?;
            target = self
                .store
                .item(&target.id)?
                .ok_or_else(|| Error::other("folder disappeared while it was populated"))?;
        }

        let mut outcome = FreeSpaceOutcome::default();
        for child in self.store.children(&self.account_id, &target.id)? {
            let Some(path) = self.store.local_path(&account, &child.id)? else {
                continue;
            };
            let result = if child.is_folder {
                self.publish_child_directory(&rooted, &child, &path)
                    .map(|()| None)
            } else {
                self.publish_child_placeholder(&account, &rooted, &child, &path)
                    .map(Some)
            };
            match result {
                Ok(Some(path)) => {
                    outcome.files = outcome.files.saturating_add(1);
                    outcome.paths.push(path);
                }
                Ok(None) => {}
                Err(error) => {
                    let detail = error.to_string();
                    let _ = self.store.set_item_state(&child.id, ItemState::Error);
                    self.emit(SyncEvent::Failed {
                        name: child.name,
                        path: Some(path.clone()),
                        error: detail.clone(),
                    });
                    outcome.skipped.push((path, detail));
                }
            }
        }

        // New file placeholders are published fail-closed (mode 000) until the
        // HSM has registered their exact inodes. Do not advertise this directory
        // as populated before that acknowledgement: otherwise a failed HSM call
        // leaves visible but unreadable entries and a later browse has no reason
        // to retry the population request.
        self.protect_pending_placeholders().await?;

        if Placeholder::state_file(&target_dir) == PlaceholderState::Unmanaged {
            Placeholder::mark_dir_file(&target_dir, PlaceholderState::Dehydrated, &target.id)?;
        }
        Placeholder::set_directory_population_file(&target_dir, DirectoryPopulation::Populated)?;
        self.store
            .set_folder_population(&self.account_id, &target.id, true)?;
        self.mark_dir(&target.id)?;
        let _ = self.recompute_folder_mark(&account, &target.id);
        self.stamp_directories(&account)?;
        self.record_directory_local_stat(&target.id, &target_dir)?;
        Ok(outcome)
    }

    fn publish_child_directory(
        &self,
        rooted: &RootedFs,
        child: &crate::db::ItemRecord,
        path: &Path,
    ) -> Result<()> {
        let expected_population = match self.store.folder_population(&self.account_id, &child.id)? {
            Some(false) => DirectoryPopulation::Partial,
            // `None` is a directory indexed by a pre-partial-population full
            // delta. Its child set is already complete and must stay usable
            // offline when an existing profile enables files-on-demand.
            Some(true) | None => DirectoryPopulation::Populated,
        };
        match rooted.open_read(path) {
            Ok(dir) if dir.metadata()?.is_dir() => {
                Placeholder::mark_dir_file(&dir, PlaceholderState::Dehydrated, &child.id)?;
                if Placeholder::directory_population_file(&dir) == DirectoryPopulation::Unmanaged {
                    Placeholder::set_directory_population_file(&dir, expected_population)?;
                }
            }
            Ok(_) => {
                return Err(Error::other(format!(
                    "{} must be a directory but is occupied by a file",
                    path.display()
                )))
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                rooted.ensure_dir(path)?;
                let dir = rooted.open_read(path)?;
                Placeholder::mark_dir_file(&dir, PlaceholderState::Dehydrated, &child.id)?;
                Placeholder::set_directory_population_file(&dir, expected_population)?;
            }
            Err(error) => return Err(error),
        }
        let population = Placeholder::directory_population_file(&rooted.open_read(path)?);
        self.store.set_folder_population(
            &self.account_id,
            &child.id,
            population == DirectoryPopulation::Populated,
        )?;
        let dir = rooted.open_read(path)?;
        self.record_directory_local_stat(&child.id, &dir)?;
        self.mark_dir(&child.id)
    }

    fn record_directory_local_stat(&self, item_id: &str, dir: &std::fs::File) -> Result<()> {
        let local = LocalStat::from_metadata(&dir.metadata()?);
        self.store.set_local_stat(
            item_id,
            OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
            Some(local.size),
            Some(local.ino),
        )?;
        Ok(())
    }

    fn publish_child_placeholder(
        &self,
        account: &Account,
        rooted: &RootedFs,
        child: &crate::db::ItemRecord,
        path: &Path,
    ) -> Result<PathBuf> {
        match rooted.open_read(path) {
            Ok(file)
                if file.metadata()?.is_file()
                    && Placeholder::state_file(&file) == PlaceholderState::Dehydrated
                    && Placeholder::item_id_file(&file).as_deref() == Some(&child.id) =>
            {
                return Ok(path.to_path_buf())
            }
            Ok(_) => {
                return Err(Error::other(format!(
                    "{} is occupied by local content",
                    path.display()
                )))
            }
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.create_placeholder_at(
            account,
            path,
            child.size,
            &child.id,
            child.ctag.as_deref(),
            child.remote_mtime,
        )?;
        self.store
            .set_item_state(&child.id, ItemState::Dehydrated)?;
        self.note_placeholder(path);
        Ok(path.to_path_buf())
    }

    /// Materialise every item currently indexed as a placeholder tree.
    ///
    /// This is the difference between selective sync and true files-on-demand:
    /// every folder becomes a real (near-free) directory and every file a
    /// sparse placeholder. In the normal partial-population mode, deeper
    /// folders are indexed only after their parent is opened; callers that
    /// explicitly want a complete tree must populate those folders first.
    /// Content still arrives only when a file is opened.
    ///
    /// Idempotent and resumable: anything already on disk is left as it is, so
    /// a run interrupted half-way simply continues next time. `progress` is
    /// called periodically with (done, total) and returns false to stop.
    /// Returns the placeholder paths that now need intercepting.
    pub fn materialize_all(
        &self,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<Vec<PathBuf>> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;

        // Directories first, shallowest to deepest, so a child never precedes
        // its parent. Empty folders matter here — `make_on_demand` only ever
        // created directories as a side effect of placing a file, so a folder
        // with no files was invisible.
        let folders = self.store.all_folders_shallow_first(&self.account_id)?;
        for folder in &folders {
            if let Some(path) = self.store.local_path(&account, &folder.id)? {
                let dir = match rooted.open_read(&path) {
                    Ok(dir) if dir.metadata()?.is_dir() => dir,
                    Ok(_) => {
                        return Err(Error::other(format!(
                            "{} must be a directory but is occupied by a file",
                            path.display()
                        )));
                    }
                    Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        rooted.ensure_dir(&path)?;
                        rooted.open_read(&path)?
                    }
                    Err(error) => return Err(error),
                };
                // Badge the folder so the file manager shows online/local on
                // directories, not only files. Unconditional so a tree
                // materialised by an earlier version — its folders unbadged —
                // gets marked on the next run. The aggregate is refined below.
                if Placeholder::state_file(&dir) == PlaceholderState::Unmanaged {
                    let _ =
                        Placeholder::mark_dir_file(&dir, PlaceholderState::Dehydrated, &folder.id);
                }
                Placeholder::set_directory_population_file(&dir, DirectoryPopulation::Populated)?;
                self.store
                    .set_folder_population(&self.account_id, &folder.id, true)?;
            }
        }

        let files = self.store.all_files(&self.account_id)?;
        let total = files.len() as u64;
        let mut done = 0u64;
        let mut watch = Vec::new();

        for file in files {
            done += 1;
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            let existing = match rooted.open_read(&path) {
                Ok(file) => Some(file),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            // Already present — downloaded, or a placeholder from a prior run.
            if let Some(existing) = existing {
                // A file whose xattr claims it is downloaded while the content
                // is still a hole is a mislabelled placeholder from an earlier
                // bug: rebuild it as a proper placeholder with the right time,
                // otherwise it never reads on demand and shows a wrong stamp.
                if Placeholder::state_file(&existing) == PlaceholderState::Hydrated
                    && Placeholder::item_id_file(&existing).as_deref() == Some(&file.id)
                {
                    use std::os::unix::fs::MetadataExt;
                    // Rebuilding truncates and zeroes the file. Only do it when
                    // the bytes really are the ones we recorded — otherwise this
                    // is a genuinely sparse file the user edited (a VM image, a
                    // preallocated database), and a rebuild would destroy the
                    // edit. If we never recorded a hash, leave it untouched
                    // rather than risk it.
                    if let Ok(writable) = rooted.open_write(&path) {
                        let before = transfer::FileVersion::capture_file(&writable)?;
                        let metadata = writable.metadata()?;
                        let hollow =
                            metadata.len() > 4096 && metadata.blocks() * 512 * 4 < metadata.len();
                        let safe_to_rebuild = Placeholder::state_file(&writable)
                            == PlaceholderState::Hydrated
                            && Placeholder::item_id_file(&writable).as_deref() == Some(&file.id)
                            && Placeholder::content_hash_file(&writable)
                                .zip(writable.try_clone().ok().and_then(|mut reader| {
                                    use std::io::{Seek, SeekFrom};
                                    reader.seek(SeekFrom::Start(0)).ok()?;
                                    crate::hash::quick_xor_reader(&mut reader).ok()
                                }))
                                .is_some_and(|(recorded, current)| recorded == current)
                            && transfer::FileVersion::capture_file(&writable)? == before;
                        if hollow && safe_to_rebuild {
                            Placeholder::create_file(
                                &writable,
                                file.size,
                                &file.id,
                                file.ctag.as_deref(),
                                file.remote_mtime,
                            )?;
                            let _ = self.store.set_item_state(&file.id, ItemState::Dehydrated);
                            watch.push(path);
                            continue;
                        }
                    }
                }
                if Placeholder::state_file(&existing) == PlaceholderState::Dehydrated
                    && Placeholder::item_id_file(&existing).as_deref() == Some(&file.id)
                {
                    // Self-heal a placeholder whose timestamp or size drifted
                    // from the record — a straggler created at a moment the
                    // remote time was not yet known keeps the materialisation
                    // time until corrected here, and re-materialise otherwise
                    // skips it forever.
                    if let Some(mtime) = file.remote_mtime {
                        use std::os::unix::fs::MetadataExt;
                        if let Ok(writable) = rooted.open_write(&path) {
                            let metadata = writable.metadata()?;
                            let still_owned = Placeholder::state_file(&writable)
                                == PlaceholderState::Dehydrated
                                && Placeholder::item_id_file(&writable).as_deref()
                                    == Some(&file.id);
                            let stale = (metadata.mtime() - mtime.unix_timestamp()).abs() > 2
                                || metadata.len() != file.size;
                            if still_owned && stale && metadata.len() != file.size {
                                // Size changed too: rebuild the placeholder.
                                Placeholder::create_file(
                                    &writable,
                                    file.size,
                                    &file.id,
                                    file.ctag.as_deref(),
                                    Some(mtime),
                                )?;
                            } else if still_owned && stale {
                                let times = std::fs::FileTimes::new()
                                    .set_modified(crate::sync::plan::to_filetime(mtime));
                                writable.set_times(times)?;
                            }
                        }
                    }
                    watch.push(path);
                }
                continue;
            }
            match self.create_placeholder_at(
                &account,
                &path,
                file.size,
                &file.id,
                file.ctag.as_deref(),
                file.remote_mtime,
            ) {
                Ok(()) => {
                    self.store.set_item_state(&file.id, ItemState::Dehydrated)?;
                    watch.push(path);
                }
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "placeholder failed"),
            }
            if done % 500 == 0 && !progress(done, total) {
                return Ok(watch);
            }
        }

        // Compute folder badges bottom-up: a folder is a check only if every
        // child is local. `folders` is shallowest-first, so reversing gives
        // deepest-first — each folder is decided after its subfolders are.
        for folder in folders.iter().rev() {
            let _ = self.recompute_folder_mark(&account, &folder.id);
        }

        // Directory timestamps last: creating children moved them off the
        // service's value.
        for folder in &folders {
            if let (Some(mtime), Some(path)) = (
                folder.remote_mtime,
                self.store.local_path(&account, &folder.id)?,
            ) {
                if path.is_dir() {
                    let times = std::fs::FileTimes::new()
                        .set_modified(crate::sync::plan::to_filetime(mtime));
                    let _ = rooted.set_times(&path, times);
                }
            }
        }

        progress(total, total);
        Ok(watch)
    }

    /// Reconcile database states with what the files themselves say.
    ///
    /// A crash mid-hydration leaves the record claiming `Hydrating` while the
    /// file's own xattr — written last — still says dehydrated. The record is
    /// the copy that lies, so it is the one corrected. Run at start-up.
    pub fn repair_states(&self) -> Result<usize> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let mut repaired = 0;
        for record in self
            .store
            .items_in_state(&self.account_id, ItemState::Hydrating)?
        {
            let Some(path) = self.store.local_path(&account, &record.id)? else {
                continue;
            };
            let state = match rooted.open_read(&path) {
                Ok(file)
                    if Placeholder::item_id_file(&file)
                        .is_some_and(|item_id| item_id != record.id) =>
                {
                    tracing::warn!(path = %path.display(), "not repairing state over another placeholder");
                    continue;
                }
                Ok(file) if Placeholder::state_file(&file) == PlaceholderState::Hydrated => {
                    let local = LocalStat::from_metadata(&file.metadata()?);
                    let content_hash = Placeholder::content_hash_file(&file);
                    self.store.commit_hydration(
                        &account.id,
                        &record.id,
                        OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                        Some(local.size),
                        Some(local.ino),
                        content_hash.as_deref(),
                    )?;
                    repaired += 1;
                    tracing::info!(path = %path.display(), state = ?ItemState::Synced, "repaired a stale hydrating record");
                    continue;
                }
                Ok(_) => ItemState::Dehydrated,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    ItemState::Dehydrated
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "cannot inspect stale hydrating record");
                    continue;
                }
            };
            self.store.set_item_state(&record.id, state)?;
            repaired += 1;
            tracing::info!(path = %path.display(), ?state, "repaired a stale hydrating record");
        }
        // A record claiming "locally modified" for a file whose own marks say
        // dehydrated is lying about ownership — freeing a file that was dirty
        // at the time leaves exactly this. Left alone, the scanner re-queues
        // an upload of it every single pass, forever, and the upload guard
        // refuses it every single pass, forever.
        for record in self
            .store
            .items_in_state(&self.account_id, ItemState::DirtyLocal)?
        {
            let Some(path) = self.store.local_path(&account, &record.id)? else {
                continue;
            };
            let Ok(file) = rooted.open_read(&path) else {
                continue;
            };
            if Placeholder::item_id_file(&file).as_deref() == Some(&record.id)
                && Placeholder::state_file(&file) == PlaceholderState::Dehydrated
            {
                self.store
                    .set_item_state(&record.id, ItemState::Dehydrated)?;
                repaired += 1;
                tracing::info!(path = %path.display(), "repaired a dirty record over a placeholder");
            }
        }
        Ok(repaired)
    }

    /// Give space back automatically, least-recently-used first.
    ///
    /// Only files whose effective pin is *not* "always keep" are candidates —
    /// the whole meaning of pinning is that pressure never takes those. Files
    /// with unsaved local changes are protected by [`Self::dehydrate`] as on
    /// every other path.
    pub fn auto_free(&self, keep_free: u64) -> Result<FreeSpaceOutcome> {
        let mut outcome = FreeSpaceOutcome::default();
        let (mut still_needed, candidates) = self.auto_free_plan(keep_free)?;

        for (allocated, path) in candidates {
            if still_needed == 0 {
                break;
            }
            match self.dehydrate(&path) {
                Ok(freed) => {
                    outcome.freed += freed;
                    outcome.files += 1;
                    outcome.paths.push(path);
                    still_needed = still_needed.saturating_sub(allocated);
                }
                Err(e) => outcome.skipped.push((path, e.to_string())),
            }
        }
        Ok(outcome)
    }

    /// Files that automatic pressure relief may dehydrate, in LRU order. The
    /// daemon uses this to install fanotify marks before calling [`Self::auto_free`].
    pub fn auto_free_candidates(&self, keep_free: u64) -> Result<Vec<PathBuf>> {
        Ok(self
            .auto_free_plan(keep_free)?
            .1
            .into_iter()
            .map(|(_, path)| path)
            .collect())
    }

    fn auto_free_plan(&self, keep_free: u64) -> Result<(u64, Vec<(u64, PathBuf)>)> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let Some(free) = crate::sync::selection::available_bytes(&account.root_local_path) else {
            return Ok((0, Vec::new()));
        };
        if free >= keep_free {
            return Ok((0, Vec::new()));
        }
        let still_needed = keep_free - free;
        tracing::info!(
            free_bytes = free,
            target = keep_free,
            "low on space; releasing unpinned files"
        );

        let default = self.config.download_new.default_pin();
        let mut candidates = Vec::new();
        for record in self
            .store
            .items_in_state(&self.account_id, ItemState::Synced)?
        {
            if record.is_folder
                || self.store.effective_pin(&record.id, default)? == crate::db::PinState::Pinned
            {
                continue;
            }
            let Some(path) = self.store.local_path(&account, &record.id)? else {
                continue;
            };
            let Ok(file) = rooted.open_read(&path) else {
                continue;
            };
            // Do not automatically reclaim an application-owned replacement
            // inode. It may still be open in an editor. An explicit user
            // action remains available and verifies the durable hash first.
            if Placeholder::item_id_file(&file).as_deref() != Some(&record.id) {
                continue;
            }
            let occupancy = crate::hydrate::Occupancy::read_file(&file)?;
            if Placeholder::content_layout_file(&file)?.has_no_data() {
                continue;
            }
            let atime = file
                .metadata()
                .ok()
                .and_then(|metadata| metadata.accessed().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            candidates.push((atime, occupancy.allocated, path));
        }
        candidates.sort_by_key(|(atime, _, _)| *atime);
        Ok((
            still_needed,
            candidates
                .into_iter()
                .map(|(_, allocated, path)| (allocated, path))
                .collect(),
        ))
    }

    /// Every placeholder that still needs intercepting, for registering with
    /// the privileged helper at start-up.
    pub fn dehydrated_paths(&self) -> Result<Vec<PathBuf>> {
        let account = self.account()?;
        dehydrated_paths_for_account(&self.store, &account)
    }
}
