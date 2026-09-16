//! Local discovery and reconciliation, including cumulative delete safety.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use time::OffsetDateTime;

use crate::db::{Account, ItemRecord, ItemState, OpKind};
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::models::DriveItem;
use crate::sync::plan::{decide_local, LocalAction, LocalStat};

use super::engine::{Engine, FullScanReport, SafetyStopKind, SyncEvent, SyncPhase, SyncSummary};
use super::rooted_fs::RootedFs;

/// Watcher batches from one deletion wave are not guaranteed to arrive
/// together. Keep scoped deletes non-executable until the tree has been quiet
/// long enough for later batches to join the same safety decision.
const SCOPED_DELETE_SETTLE: Duration = Duration::from_secs(30);

/// Files the client itself creates, which must never be synced.
const INTERNAL_SUFFIXES: &[&str] = &[".gonedrive-partial"];

/// Markers that belong to this machine's desktop indexers, not to the drive.
const INTERNAL_NAMES: &[&str] = &[".trackerignore", ".nomedia"];

#[derive(Debug)]
struct ExpectedLocal {
    item_id: String,
    is_folder: bool,
    local_mtime: Option<i64>,
    local_size: Option<u64>,
    local_ino: Option<u64>,
    remote_mtime: Option<i64>,
    state: ItemState,
}

impl Engine {
    // ---- local -> remote --------------------------------------------------

    /// Walk the whole sync root and queue whatever differs from our record.
    ///
    /// This is the correctness backstop for events missed while the daemon was
    /// stopped or while inotify overflowed. The daemon runs it at startup and
    /// then at a low cadence; ordinary edits use [`Self::scan_local_paths`].
    pub fn scan_local(&self, summary: &mut SyncSummary) -> Result<()> {
        self.scan_local_scopes(summary, None, None)
    }

    /// Reconcile only directories touched by the filesystem watcher.
    ///
    /// Each path is revalidated under the sync root. Unknown directories are
    /// widened to their nearest known ancestor so a newly-created subtree is
    /// still discovered, while existing sibling subtrees are never walked.
    pub fn scan_local_paths(&self, paths: &[PathBuf], summary: &mut SyncSummary) -> Result<()> {
        self.scan_local_scopes(summary, Some(paths), None)
    }

    pub fn sync_local_paths(&self, paths: &[PathBuf]) -> Result<SyncSummary> {
        let phase = SyncPhase::start(self);
        let mut summary = SyncSummary::default();
        self.scan_local_paths(paths, &mut summary)?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        phase.finish(&summary);
        Ok(summary)
    }

    /// Reconcile a watcher batch, retaining which exact files reached
    /// close-write. A copied placeholder may inherit the source's xattrs; only
    /// a completed write (or a full matching content hash) proves its bytes are
    /// a real new file rather than sparse placeholder holes.
    pub fn sync_local_change_batch(
        &self,
        paths: &[PathBuf],
        completed_writes: &HashSet<PathBuf>,
    ) -> Result<SyncSummary> {
        let phase = SyncPhase::start(self);
        let mut summary = SyncSummary::default();
        self.scan_local_scopes(&mut summary, Some(paths), Some(completed_writes))?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        phase.finish(&summary);
        Ok(summary)
    }

    /// Read-only whole-tree verification suitable for `spawn_blocking` while
    /// remote polling continues. It records only dirty directory scopes and
    /// placeholder identities; the coordinator applies those findings later.
    pub fn discover_local_changes(&self) -> Result<FullScanReport> {
        let account = self.account()?;
        let Some(root_id) = account.root_item_id.clone() else {
            return Ok(FullScanReport::default());
        };
        let rooted = self.rooted(&account)?;
        let items = self.store.all_items(&self.account_id)?;
        let mut children = HashMap::<String, Vec<ItemRecord>>::new();
        for item in items {
            if let Some(parent) = item.parent_id.clone() {
                children.entry(parent).or_default().push(item);
            }
        }

        let mut expected = HashMap::<PathBuf, ExpectedLocal>::new();
        let mut stack = vec![(root_id, PathBuf::new())];
        while let Some((parent_id, parent_path)) = stack.pop() {
            for item in children.remove(&parent_id).unwrap_or_default() {
                let path = parent_path.join(&item.name);
                expected.insert(
                    path.clone(),
                    ExpectedLocal {
                        item_id: item.id.clone(),
                        is_folder: item.is_folder,
                        local_mtime: item.local_mtime.map(|time| time.unix_timestamp()),
                        local_size: item.local_size,
                        local_ino: item.local_ino,
                        remote_mtime: item.remote_mtime.map(|time| time.unix_timestamp()),
                        state: item.state,
                    },
                );
                if item.is_folder {
                    stack.push((item.id, path));
                }
            }
        }

        let mut report = FullScanReport::default();
        let mut dirty = std::collections::HashSet::new();
        let mut seen = std::collections::HashSet::new();
        let mut unreadable = Vec::new();
        let mut directories = vec![PathBuf::new()];

        while let Some(relative_dir) = directories.pop() {
            let absolute_dir = account.root_local_path.join(&relative_dir);
            let entries = match read_dir_readonly(&absolute_dir) {
                Ok(entries) => entries,
                Err(error) => {
                    tracing::warn!(path = %absolute_dir.display(), %error, "full-scan discovery cannot read directory");
                    let parent = relative_dir.parent().unwrap_or(Path::new(""));
                    dirty.insert(account.root_local_path.join(parent));
                    unreadable.push(relative_dir);
                    continue;
                }
            };
            report.scanned_entries = report.scanned_entries.saturating_add(entries.len() as u64);
            for (name, stat) in entries {
                let relative = relative_dir.join(&name);
                seen.insert(relative.clone());
                // Desktop-generated markers are outside the sync contract.
                // Keep a known one visible in `seen` so it is never mistaken
                // for a user deletion, but neither upload nor recurse into it.
                if INTERNAL_NAMES.contains(&name.as_str()) {
                    continue;
                }
                if stat.is_dir {
                    directories.push(relative.clone());
                }
                let Some(want) = expected.get(&relative) else {
                    dirty.insert(absolute_dir.clone());
                    continue;
                };
                if want.is_folder != stat.is_dir {
                    dirty.insert(absolute_dir.clone());
                    continue;
                }
                if stat.is_dir {
                    if want.remote_mtime.is_some_and(|mtime| mtime != stat.mtime) {
                        // Incremental reconciliation compares entries inside a
                        // supplied directory. Mark its parent so this directory
                        // entry itself is revalidated and restamped.
                        dirty.insert(absolute_dir.clone());
                    }
                    continue;
                }

                let path = account.root_local_path.join(&relative);
                let expects_placeholder = matches!(
                    want.state,
                    ItemState::Dehydrated | ItemState::Hydrating | ItemState::Error
                );
                if expects_placeholder
                    && owned_placeholder_state(&rooted, &path, &want.item_id)
                        == Some(PlaceholderState::Dehydrated)
                {
                    report.placeholders.push((path, stat.dev, stat.ino));
                    continue;
                }
                let changed = want.local_ino != Some(stat.ino)
                    || want.local_size != Some(stat.size)
                    || want.local_mtime != Some(stat.mtime);
                if changed || expects_placeholder {
                    dirty.insert(absolute_dir.clone());
                }
            }
        }

        for (relative, want) in expected {
            if want.local_ino.is_none()
                || matches!(want.state, ItemState::DirtyRemote | ItemState::Hydrating)
                || seen.contains(&relative)
                || unreadable.iter().any(|dir| relative.starts_with(dir))
            {
                continue;
            }
            let parent = relative.parent().unwrap_or(Path::new(""));
            dirty.insert(account.root_local_path.join(parent));
        }

        report.dirty_paths = dirty.into_iter().collect();
        report.dirty_paths.sort();
        Ok(report)
    }

    pub fn take_placeholder_marks(&self) -> Vec<PathBuf> {
        self.placeholder_marks
            .lock()
            .map(|mut paths| paths.drain().collect())
            .unwrap_or_default()
    }

    pub(super) fn note_placeholder(&self, path: &Path) {
        if let Ok(mut paths) = self.placeholder_marks.lock() {
            paths.insert(path.to_path_buf());
        }
    }

    /// Rebase the known online-only descendants of a moved directory for HSM.
    /// The inode marks already survive the rename; refreshing their paths here
    /// keeps reconnects and explicit unwatch operations correct without walking
    /// unrelated parts of a large sync tree.
    fn note_moved_placeholder_subtree(
        &self,
        account: &Account,
        rooted: &RootedFs,
        folder_id: &str,
        old_root: &Path,
        new_root: &Path,
    ) -> Result<()> {
        for record in self.store.files_in_subtree(folder_id)? {
            let Some(old_path) = self.store.local_path(account, &record.id)? else {
                continue;
            };
            let Ok(relative) = old_path.strip_prefix(old_root) else {
                continue;
            };
            let new_path = new_root.join(relative);
            if owned_placeholder_state(rooted, &new_path, &record.id)
                == Some(PlaceholderState::Dehydrated)
            {
                self.note_placeholder(&new_path);
            }
        }
        Ok(())
    }

    fn scan_local_scopes(
        &self,
        summary: &mut SyncSummary,
        requested_paths: Option<&[PathBuf]>,
        completed_writes: Option<&HashSet<PathBuf>>,
    ) -> Result<()> {
        let account = self.account()?;
        let Some(root_id) = account.root_item_id.clone() else {
            // Nothing has been enumerated yet; there is no tree to compare to.
            return Ok(());
        };
        if !account.root_local_path.exists() {
            std::fs::create_dir_all(&account.root_local_path)?;
            return Ok(());
        }
        let rooted = self.rooted(&account)?;

        // The sync root must be the same filesystem it was last time. If it is
        // not, the most likely cause is that it is not mounted and we are
        // looking at an empty mountpoint — every record would read as "deleted
        // locally" and the account would be wiped server-side. Record the
        // device the first time; refuse to scan (and so refuse to delete)
        // whenever it has changed.
        {
            use std::os::unix::fs::MetadataExt;
            let current_dev = std::fs::metadata(&account.root_local_path)
                .map(|m| m.dev())
                .ok();
            match (account.root_dev, current_dev) {
                (Some(recorded), Some(now)) if recorded != now => {
                    let reason = format!(
                        "sync root {} is on a different filesystem than recorded \
                         (was dev {recorded}, now {now}) — refusing to scan so an \
                         unmounted root cannot delete the account",
                        account.root_local_path.display()
                    );
                    tracing::error!(%reason, "safety stop");
                    self.emit(SyncEvent::SafetyStop {
                        reason,
                        kind: SafetyStopKind::General,
                    });
                    return Ok(());
                }
                (None, Some(now)) => {
                    self.store.set_root_dev(&self.account_id, now)?;
                }
                _ => {}
            }
        }

        let full_scan = requested_paths.is_none();
        // `None` marks a directory that has no remote record yet — freshly
        // created locally. It is still walked: the far end of a rename often
        // lands inside exactly such a directory, and skipping it is how a
        // move used to decay into a remote delete plus a re-upload.
        let mut stack: Vec<(Option<String>, PathBuf)> = match requested_paths {
            None => vec![(Some(root_id.clone()), account.root_local_path.clone())],
            Some(paths) => self.incremental_scan_roots(&account, &root_id, paths)?,
        };
        if stack.is_empty() {
            return Ok(());
        }
        // Decisions that need the *whole* walk before they are safe:
        // a rename that crosses directories shows up as "new file here" in one
        // directory and "record missing" in another, and acting on either half
        // early turns a move into a remote delete plus a full re-upload.
        let mut appeared: Vec<(std::path::PathBuf, LocalStat)> = Vec::new();
        let mut vanished: Vec<ItemRecord> = Vec::new();
        let by_inode = self.store.items_by_inode(&self.account_id)?;
        // A remote move/update whose parent has not been indexed yet is kept
        // durably for replay. Until then, no local observation of that same id
        // may upload or delete against the older remote location/version.
        let (deferred_remote, deferred_paths) = self.deferred_remote_guards(&account)?;
        let mut moved_ids = std::collections::HashSet::new();

        while let Some((parent_id, dir)) = stack.pop() {
            let entries = match read_dir(&dir, &rooted, &|path| self.staging_is_claimed(path)) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!(path = %dir.display(), error = %e, "cannot read directory");
                    continue;
                }
            };
            let known: HashMap<String, ItemRecord> = match &parent_id {
                Some(parent_id) => self
                    .store
                    .children(&self.account_id, parent_id)?
                    .into_iter()
                    .map(|item| (crate::names::fold_case(&item.name), item))
                    .collect(),
                None => HashMap::new(),
            };

            // The pin every file here inherits unless it carries one of its
            // own. Resolving it per file walked the ancestor chain in SQL once
            // per file for an answer that cannot differ between siblings.
            let inherited_pin = match &parent_id {
                Some(parent_id) => self
                    .store
                    .effective_pin(parent_id, self.config.download_new.default_pin())?,
                None => self.config.download_new.default_pin(),
            };

            // One statement for the directory rather than one per file. A
            // queued deletion is withdrawn the moment its file turns up again,
            // and the usual case — nothing queued — now costs one statement
            // instead of one write transaction per entry.
            let present: Vec<&str> = entries
                .iter()
                .filter_map(|(name, _)| {
                    known
                        .get(&crate::names::fold_case(name))
                        .map(|record| record.id.as_str())
                })
                .collect();
            self.store
                .cancel_pending_deletes(&self.account_id, &present)?;

            for (name, stat) in &entries {
                let record = known.get(&crate::names::fold_case(name));
                if record.is_some_and(|record| deferred_remote.contains(&record.id)) {
                    continue;
                }
                let entry_path = dir.join(name);
                if deferred_paths.contains(&entry_path) {
                    // A remote item is waiting for this exact path to become
                    // representable. Treat the whole local subtree as frozen so
                    // it cannot be uploaded under the stale/competing identity.
                    continue;
                }
                // Unknown indexer markers are not uploaded; known ones are
                // still counted in `entries`/`on_disk`, so they are not
                // repeatedly queued as remote deletions either.
                if INTERNAL_NAMES.contains(&name.as_str()) {
                    continue;
                }

                // Delta or the hydration helper has already claimed this file.
                // A full helper fill writes the inode and marks its xattr before
                // the daemon can restore mtime/update SQLite; its close-write can
                // therefore reach this scan in that short Hydrating window. It
                // is client-owned I/O, not a user edit, and must never be queued
                // for upload. DirtyRemote is likewise owned by the download path.
                if !stat.is_dir
                    && record.is_some_and(|record| {
                        matches!(record.state, ItemState::DirtyRemote | ItemState::Hydrating)
                    })
                {
                    continue;
                }

                // A placeholder's bytes are not the user's content: dehydrated
                // means holes, hydrating means holes being filled. Judging
                // either as "locally modified" queues an upload of zeroes over
                // the real file in the cloud. Nothing about them is ours to
                // upload, so they only recurse (below) and refresh marks.
                let path = entry_path;
                let mut inspected = inspect_file(&rooted, &path);
                if !stat.is_dir {
                    let expected_id = record.map(|record| record.id.as_str());
                    let foreign_claim = inspected
                        .as_ref()
                        .and_then(|seen| seen.item_id.as_deref())
                        .is_some_and(|claimed| Some(claimed) != expected_id);
                    if foreign_claim && record.is_some() {
                        if !self.prepare_copied_placeholder(
                            &rooted,
                            &path,
                            stat,
                            expected_id,
                            completed_writes.is_some_and(|paths| paths.contains(&path)),
                        )? {
                            continue;
                        }
                        // It rewrote the inode's marks, so what we read above
                        // no longer describes the file.
                        inspected = inspect_file(&rooted, &path);
                    }
                }
                let placeholder_state = record.and_then(|record| match &inspected {
                    Some(seen) => {
                        (seen.item_id.as_deref() == Some(record.id.as_str())).then_some(seen.state)
                    }
                    // Unreadable. A placeholder left at mode 000 until the
                    // helper protects it is identified by shape instead.
                    None => owned_placeholder_state(&rooted, &path, &record.id),
                });
                let content_is_ours =
                    stat.is_dir || placeholder_state != Some(PlaceholderState::Dehydrated);
                if !content_is_ours {
                    if let Some(record) = record {
                        // Record the inode of a placeholder we can see on disk.
                        // Its content is not ours to upload, but its *presence*
                        // is what tells a later scan that deleting the online-
                        // only file was a real deletion to propagate — without
                        // this the file has no local_ino and the delete guard
                        // mistakes it for a download that never landed, so
                        // deleting an online-only file never reaches the cloud.
                        if !stat.is_dir
                            && placeholder_state == Some(PlaceholderState::Dehydrated)
                            && record.local_ino != Some(stat.ino)
                        {
                            self.store.set_local_stat(
                                &record.id,
                                OffsetDateTime::from_unix_timestamp(stat.mtime).ok(),
                                Some(stat.size),
                                Some(stat.ino),
                            )?;
                            self.note_placeholder(&path);
                        }
                        if full_scan && stat.is_dir {
                            stack.push((Some(record.id.clone()), dir.join(name)));
                        }
                    }
                    continue;
                }

                match decide_local(name, record, Some(stat)) {
                    LocalAction::CreateRemoteFolder => {
                        let new_path = dir.join(name);
                        let moved = self.moved_candidate(&account, &by_inode, &new_path, stat)?;
                        if let Some((known, old_path)) = moved {
                            if deferred_remote.contains(&known.id) {
                                continue;
                            }
                            self.store.enqueue_move(
                                &self.account_id,
                                &known.id,
                                &old_path,
                                &new_path,
                            )?;
                            moved_ids.insert(known.id.clone());
                            self.note_moved_placeholder_subtree(
                                &account, &rooted, &known.id, &old_path, &new_path,
                            )?;
                            // A full verification still inspects the moved
                            // subtree for edits. An incremental watcher pass
                            // skips it: the directory move itself covers every
                            // unchanged descendant in one remote operation.
                            if full_scan {
                                stack.push((Some(known.id.clone()), new_path));
                            }
                        } else {
                            // A parent that is itself new has no id to record
                            // yet; the runner resolves it from the path once the
                            // parent exists — the queue preserves creation order.
                            self.store.enqueue(
                                &self.account_id,
                                OpKind::CreateRemoteFolder,
                                parent_id.as_deref(),
                                Some(&new_path),
                                None,
                            )?;
                            stack.push((None, new_path));
                        }
                    }
                    LocalAction::UploadNew => {
                        // Deferred: whether this is truly new or the far end
                        // of a rename is only knowable once the whole tree has
                        // been walked and the vanished records collected.
                        appeared.push((dir.join(name), *stat));
                    }
                    LocalAction::UploadModified => {
                        let Some(record) = record else {
                            return Err(Error::other(format!(
                                "local planner marked {name} modified without a remote record"
                            )));
                        };
                        self.store.enqueue_conditional(
                            &self.account_id,
                            OpKind::Upload,
                            &record.id,
                            Some(&dir.join(name)),
                            parent_id.as_deref(),
                            record.etag.as_deref().or(record.ctag.as_deref()),
                        )?;
                        self.store
                            .set_item_state(&record.id, ItemState::DirtyLocal)?;
                    }
                    LocalAction::Skip(reason) => {
                        summary.skipped += 1;
                        let fresh = self
                            .reported_skips
                            .lock()
                            .map(|mut seen| seen.insert(name.clone()))
                            .unwrap_or(true);
                        if fresh {
                            self.emit(SyncEvent::Skipped {
                                name: name.clone(),
                                reason: format!("{reason:?}"),
                            });
                        }
                    }
                    LocalAction::Nothing | LocalAction::DeleteRemote => {}
                }

                if !stat.is_dir {
                    // The scan already stats every file, so keeping the file
                    // manager's marks in step costs one `getxattr` and a write
                    // only when they actually drifted.
                    // Only when the record describes a file; a folder record
                    // under a file's name is the type swap reported above.
                    if let (Some(record), Some(seen)) = (
                        record.filter(|record| !record.is_folder),
                        inspected.as_ref(),
                    ) {
                        self.refresh_marks_file(record, seen, inherited_pin);
                    }
                }

                if stat.is_dir {
                    // A file record under a directory's name would make every
                    // child inherit a file's id as its parent, and every
                    // resulting operation fail against the service for ever.
                    if let Some(record) = record.filter(|record| record.is_folder) {
                        // This walk has already paid for the `stat`, so noticing
                        // a directory whose timestamp drifted — from an earlier
                        // version, or an edit we did not make — is free here.
                        if record
                            .remote_mtime
                            .is_some_and(|t| t.unix_timestamp() != stat.mtime)
                        {
                            self.mark_dir(&record.id)?;
                        }
                        if full_scan {
                            stack.push((Some(record.id.clone()), dir.join(name)));
                        }
                    }
                }
            }

            // Records with no matching entry on disk were deleted locally.
            let on_disk: std::collections::HashSet<String> = entries
                .iter()
                .map(|(name, _)| crate::names::fold_case(name))
                .collect();
            let Some(_parent_id) = parent_id else {
                continue; // a new directory has no records to have lost
            };
            for (folded, record) in &known {
                if on_disk.contains(folded) {
                    continue;
                }
                if deferred_remote.contains(&record.id) {
                    continue;
                }
                // A record that never had a local observation was never written
                // to disk in the first place — its download is still pending,
                // and deleting it remotely would be catastrophic.
                if record.local_ino.is_none() {
                    continue;
                }
                // A remote replacement has already queued a download. Its
                // temporary local absence is not a user deletion and must not
                // race that download into a DELETE against the fresh remote
                // version.
                if matches!(record.state, ItemState::DirtyRemote | ItemState::Hydrating) {
                    continue;
                }
                // Distinguish two kinds of "not wanted here". An *explicit*
                // exclusion ("don't keep this folder on this device") makes
                // local absence expected — deleting remotely would destroy
                // what the user chose not to sync. But an account-level
                // online-only *default* never expressed an opinion about this
                // item: it was on disk because the user put or fetched it
                // there, and its disappearance is the user deleting it. The
                // first guard here once ate every local deletion under the
                // default policy, silently.
                if self.store.explicitly_excluded(&record.id)? {
                    continue;
                }
                vanished.push(record.clone());
            }
        }

        // Pair the two halves of every rename before anything destructive:
        // a vanished record whose inode reappeared elsewhere is one PATCH,
        // not a remote delete plus a re-upload that forfeits the server-side
        // version history.
        for (path, stat) in &appeared {
            // Parents are deliberately not recorded here: the runner resolves
            // them from the path, because a destination directory created in
            // this same pass has no remote id until its own queued creation
            // has run.
            let moved = self.moved_candidate(&account, &by_inode, path, stat)?;
            match moved {
                Some((record, old_path)) => {
                    if deferred_remote.contains(&record.id) {
                        continue;
                    }
                    moved_ids.insert(record.id.clone());
                    let is_placeholder = !stat.is_dir
                        && owned_placeholder_state(&rooted, path, &record.id)
                            == Some(PlaceholderState::Dehydrated);
                    if is_placeholder {
                        self.note_placeholder(path);
                    }
                    if !is_placeholder && crate::sync::plan::local_is_modified(record, stat) {
                        // Moving preserves the remote identity, but it does not
                        // transfer bytes. If the file was also edited before
                        // this scan, commit the metadata and content work
                        // together so a crash cannot drop the upload half.
                        self.store.enqueue_move_and_upload(
                            &self.account_id,
                            &record.id,
                            &old_path,
                            path,
                        )?;
                    } else {
                        self.store
                            .enqueue_move(&self.account_id, &record.id, &old_path, path)?;
                    }
                }
                None => {
                    if !stat.is_dir
                        && !self.prepare_copied_placeholder(
                            &rooted,
                            path,
                            stat,
                            None,
                            completed_writes.is_some_and(|paths| paths.contains(path)),
                        )?
                    {
                        continue;
                    }
                    self.store
                        .enqueue(&self.account_id, OpKind::Upload, None, Some(path), None)?;
                }
            }
        }
        // Mass-deletion safety valve. Scoped watcher scans can split one
        // deletion wave across calls, so the store evaluates this batch with
        // every delete already waiting in the durable quiet window. Folder
        // descendants and overlapping roots are counted exactly once.
        let deletes: Vec<_> = vanished
            .iter()
            .filter(|record| !moved_ids.contains(&record.id))
            .map(|record| {
                (
                    record.id.as_str(),
                    record.etag.as_deref().or(record.ctag.as_deref()),
                )
            })
            .collect();
        if !deletes.is_empty() {
            // The approval is a one-shot answer to the refusal it followed.
            // Only a scan that actually carries deletions may spend it: a
            // watcher batch for some unrelated write must not silently eat the
            // user's confirmation and leave the held deletions refused again.
            let approved = self
                .mass_delete_approval
                .swap(false, std::sync::atomic::Ordering::AcqRel);
            let settle = if full_scan {
                Duration::ZERO
            } else {
                SCOPED_DELETE_SETTLE
            };
            let outcome = self.store.enqueue_guarded_deletes(
                &self.account_id,
                &deletes,
                if approved {
                    u64::MAX
                } else {
                    self.config.mass_delete_min
                },
                if approved {
                    1.0
                } else {
                    self.config.mass_delete_fraction
                },
                settle,
            )?;
            if outcome.refused {
                let fraction = outcome.impacted as f64 / outcome.total as f64;
                let reason = format!(
                    "the pending deletion window would delete {} of {} items \
                     ({:.0}%) — holding the deletions for confirmation instead \
                     of propagating what looks like an accidental mass loss",
                    outcome.impacted,
                    outcome.total,
                    fraction * 100.0
                );
                tracing::error!(%reason, "safety stop");
                self.emit(SyncEvent::SafetyStop {
                    reason,
                    kind: SafetyStopKind::MassDelete,
                });
                summary.refused_deletions += outcome.impacted;
            }
        }
        Ok(())
    }

    fn incremental_scan_roots(
        &self,
        account: &Account,
        root_id: &str,
        paths: &[PathBuf],
    ) -> Result<Vec<(Option<String>, PathBuf)>> {
        let root = &account.root_local_path;
        let mut roots = std::collections::HashMap::<PathBuf, String>::new();

        for supplied in paths {
            let absolute = if supplied.is_absolute() {
                supplied.clone()
            } else {
                root.join(supplied)
            };
            if absolute.strip_prefix(root).is_err() {
                tracing::warn!(path = %absolute.display(), "ignoring watcher path outside sync root");
                continue;
            }

            let mut directory = if absolute.is_dir() {
                absolute
            } else {
                absolute.parent().unwrap_or(root).to_path_buf()
            };
            loop {
                let relative = directory.strip_prefix(root).unwrap_or(Path::new(""));
                if let Some(record) = self
                    .store
                    .item_at_path(&self.account_id, root_id, relative)?
                    .filter(|record| record.is_folder)
                {
                    roots.insert(directory, record.id);
                    break;
                }
                if directory == *root || !directory.pop() {
                    roots.insert(root.clone(), root_id.to_string());
                    break;
                }
            }
        }

        Ok(roots
            .into_iter()
            .map(|(path, id)| (Some(id), path))
            .collect())
    }

    /// Remote changes that could not be projected locally must not race local
    /// reconciliation. Guard the old known path (moves/tombstones) and the new
    /// payload path (collisions) until deferred replay succeeds.
    fn deferred_remote_guards(
        &self,
        account: &Account,
    ) -> Result<(HashSet<String>, HashSet<PathBuf>)> {
        let mut ids = HashSet::new();
        let mut paths = HashSet::new();
        for payload in self.store.deferred_remote_payloads(&self.account_id)? {
            let item: DriveItem = serde_json::from_str(&payload)?;
            ids.insert(item.id.clone());
            if let Some(known) = self.store.item(&item.id)? {
                if let Some(path) = self.store.local_path(account, &known.id)? {
                    paths.insert(path);
                }
            }
            if let (Some(parent_id), Some(name)) = (item.parent_id(), item.name.as_deref()) {
                if let Some(parent) = self.store.relative_path(parent_id)? {
                    paths.insert(account.root_local_path.join(parent).join(name));
                }
            }
        }
        Ok((ids, paths))
    }

    /// Match a new path to a stable known inode without mistaking a conflict
    /// copy, a remote-owned replacement, or an inode from another filesystem
    /// for a user move.
    fn moved_candidate<'a>(
        &self,
        account: &Account,
        by_inode: &'a HashMap<u64, ItemRecord>,
        new_path: &Path,
        stat: &LocalStat,
    ) -> Result<Option<(&'a ItemRecord, PathBuf)>> {
        if account.root_dev != Some(stat.dev) {
            return Ok(None);
        }
        let Some(known) = by_inode.get(&stat.ino) else {
            return Ok(None);
        };
        if known.is_folder != stat.is_dir
            || matches!(
                known.state,
                ItemState::DirtyRemote | ItemState::Hydrating | ItemState::Conflict
            )
        {
            return Ok(None);
        }
        let Some(old) = self.store.local_path(account, &known.id)? else {
            return Ok(None);
        };
        if old == new_path {
            return Ok(None);
        }
        // Inodes are recycled aggressively, so require that nothing remains at
        // the old path before believing this is the same item. Preserve an I/O
        // failure instead of treating it as absence and issuing a remote move.
        match old.try_exists() {
            Ok(false) => {}
            Ok(true) => return Ok(None),
            Err(error) => return Err(error.into()),
        }

        // `rm -rf A && mkdir B` can hand B the inode A just released, and to
        // everything above this point that is indistinguishable from a move:
        // same inode, same device, nothing left at the old path. Believing it
        // hands A's remote identity — its sharing links and version history —
        // to B's contents, and for a directory renames the whole subtree.
        //
        // The one thing that does distinguish them is when the inode came into
        // existence. A move leaves birth time alone, so it predates the last
        // time we wrote the record; a recycled inode was born after it.
        let birth = self
            .rooted(account)
            .ok()
            .and_then(|rooted| rooted.open_read(new_path).ok())
            .and_then(|file| birth_time(&file));
        if inode_was_recycled(birth, self.store.item_recorded_at(&known.id)?) {
            tracing::debug!(
                path = %new_path.display(),
                ino = stat.ino,
                "the inode was recycled since the record was written; not a move"
            );
            return Ok(None);
        }
        Ok(Some((known, old)))
    }

    /// Turn a completed copy of one of our placeholders into an ordinary new
    /// file. File managers commonly preserve xattrs; without this check the
    /// destination impersonates the source item and either uploads sparse
    /// zeroes or is refused forever as a placeholder.
    fn prepare_copied_placeholder(
        &self,
        rooted: &RootedFs,
        path: &Path,
        stat: &LocalStat,
        expected_item_id: Option<&str>,
        completed_write: bool,
    ) -> Result<bool> {
        let file = match rooted.open_read(path) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Ok(false);
            }
            // Collected by the walk, gone by the time the appeared list is
            // processed (an editor's probe file, a build's temporary). It is
            // nothing to upload — and it must not abort the whole scan, which
            // would also discard every deletion the walk found.
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let Some(claimed_id) = Placeholder::item_id_file(&file) else {
            return Ok(true);
        };
        if expected_item_id == Some(claimed_id.as_str()) {
            return Ok(true);
        }

        let state = Placeholder::state_file(&file);
        if state == PlaceholderState::Unmanaged {
            Placeholder::clear_marks_file(&file)?;
            return Ok(true);
        }

        let source = self.store.item(&claimed_id)?;
        let size_matches = source
            .as_ref()
            .is_none_or(|record| record.size == stat.size);
        let expected_hash = Placeholder::content_hash_file(&file)
            .or_else(|| source.as_ref().and_then(|record| record.quickxor.clone()));
        let before = super::transfer::FileVersion::capture_file(&file)?;
        let exact_copy = if size_matches {
            if let Some(expected_hash) = expected_hash.as_deref() {
                let mut reader = file.try_clone()?;
                let actual = crate::hash::quick_xor_reader(&mut reader)?;
                super::transfer::FileVersion::capture_file(&file)?.same_content_as(before)
                    && actual == expected_hash
            } else {
                false
            }
        } else {
            false
        };
        let fully_materialized = Placeholder::content_layout_file(&file)?.is_fully_materialized();
        let safe_to_claim = exact_copy
            || (completed_write && (state == PlaceholderState::Hydrated || fully_materialized));
        if !safe_to_claim {
            tracing::debug!(
                path = %path.display(),
                claimed_item_id = %claimed_id,
                ?state,
                completed_write,
                fully_materialized,
                "deferring an incomplete copied placeholder"
            );
            return Ok(false);
        }

        Placeholder::clear_marks_file(&file)?;
        if !super::transfer::FileVersion::capture_file(&file)?.same_content_as(before) {
            // The copied identity is gone, so a later watcher/full-scan pass can
            // safely treat the still-changing bytes as an ordinary local file.
            return Ok(false);
        }
        tracing::info!(
            path = %path.display(),
            source_item_id = %claimed_id,
            "detached copied placeholder identity"
        );
        Ok(true)
    }
}

/// What one inode says about itself, read once.
///
/// The scan used to open every file three times — to see whose placeholder it
/// is, to read that placeholder's state, and again to refresh its marks —
/// reading the same two attributes each time. On a drive of a hundred thousand
/// files that is the bulk of a full scan's syscalls.
pub(super) struct Inspected {
    pub(super) file: std::fs::File,
    pub(super) item_id: Option<String>,
    pub(super) state: PlaceholderState,
}

fn inspect_file(rooted: &RootedFs, path: &Path) -> Option<Inspected> {
    let file = rooted.open_read(path).ok()?;
    let item_id = Placeholder::item_id_file(&file);
    let state = Placeholder::state_file(&file);
    Some(Inspected {
        file,
        item_id,
        state,
    })
}

pub(super) fn owned_placeholder_state(
    rooted: &super::rooted_fs::RootedFs,
    path: &Path,
    expected_item_id: &str,
) -> Option<PlaceholderState> {
    match rooted.open_read(path) {
        Ok(file) => (Placeholder::item_id_file(&file).as_deref() == Some(expected_item_id))
            .then(|| Placeholder::state_file(&file)),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            use std::os::unix::fs::MetadataExt;

            // Registration failures leave known placeholders mode 000 until
            // the HSM protects the inode. Their user xattrs are deliberately
            // unreadable to this unprivileged scanner, so identify only the
            // fail-closed shape at the database-owned path. Treating a user's
            // unrelated mode-000 file conservatively as unchanged is safer
            // than uploading sparse zeroes over the remote content.
            let file = rooted.open_path(path).ok()?;
            let metadata = file.metadata().ok()?;
            (metadata.is_file() && metadata.mode() & 0o7777 == 0)
                .then_some(PlaceholderState::Dehydrated)
        }
        Err(_) => None,
    }
}

/// Directory entries worth considering, with their stats.
///
/// Symlinks are read with `symlink_metadata` and then dropped: following them
/// would let a link out of the sync root pull arbitrary files into the account.
fn read_dir(
    dir: &Path,
    rooted: &super::rooted_fs::RootedFs,
    staging_is_claimed: &dyn Fn(&Path) -> bool,
) -> std::io::Result<Vec<(String, LocalStat)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = utf8_name(&entry) else {
            continue;
        };
        if INTERNAL_SUFFIXES.iter().any(|s| name.ends_with(s)) {
            // Staging leftovers are pure cache: consumed within seconds when
            // things work, so one still here after fifteen minutes is an
            // orphan of a failed hand-off. The scan is already walking this
            // directory; the sweep costs one stat.
            if name.ends_with(".gonedrive-partial") {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > std::time::Duration::from_secs(900))
                    .unwrap_or(false);
                // Age alone does not make a partial an orphan. A download
                // that failed while connecting leaves its prefix untouched,
                // so its timestamp stops moving while it waits out a backoff
                // — and deleting it makes the retry start from zero.
                if stale && !staging_is_claimed(&entry.path()) {
                    let _ = rooted.remove_file(&entry.path());
                }
            }
            continue;
        }
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue, // vanished mid-scan
        };
        if !meta.is_file() && !meta.is_dir() {
            continue;
        }
        out.push((name, LocalStat::from_metadata(&meta)));
    }
    Ok(out)
}

impl Engine {
    /// Whether a staging file belongs to a download that is still queued.
    ///
    /// Staging names are derived from the destination — and hashed when the
    /// destination's name is too long — so this asks the question forwards,
    /// from each queued download to the name it would use.
    fn staging_is_claimed(&self, staging: &Path) -> bool {
        let Ok(queued) = self.store.download_op_paths(&self.account_id) else {
            // Unable to tell, so keep the file: a resumable prefix is worth
            // more than the space an orphan occupies until the next pass.
            return true;
        };
        queued
            .iter()
            .any(|destination| super::transfer::staging_sibling(destination) == staging)
    }
}

/// Whether an inode matching a record's was created after that record was last
/// written — so it is a different file that inherited a recycled inode number,
/// not the recorded one moved.
///
/// A rename leaves birth time alone, so a genuine move (even one followed by an
/// edit) predates the record. Without a birth time, or without a recorded time,
/// this falls back to trusting the inode number, which is what every version
/// before this did.
fn inode_was_recycled(birth: Option<i64>, recorded_at: Option<i64>) -> bool {
    match (birth, recorded_at) {
        (Some(birth), Some(recorded_at)) => birth > recorded_at,
        _ => false,
    }
}

/// When this inode came into existence, in seconds, or `None` where the
/// filesystem does not record it.
///
/// ext4, btrfs and xfs all keep a birth time; tmpfs and some network
/// filesystems do not, and there the caller falls back to trusting the inode
/// number alone, which is what every version before this did.
fn birth_time(file: &std::fs::File) -> Option<i64> {
    use std::os::fd::AsRawFd;

    // SAFETY: zeroed is a valid initial `statx`, and the call only writes into
    // it. The empty path with AT_EMPTY_PATH addresses the descriptor itself.
    let mut out: libc::statx = unsafe { std::mem::zeroed() };
    let empty = c"";
    let rc = unsafe {
        libc::statx(
            file.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_BTIME,
            &mut out,
        )
    };
    if rc != 0 || out.stx_mask & libc::STATX_BTIME == 0 {
        return None;
    }
    Some(out.stx_btime.tv_sec)
}

/// A directory entry's name as a `String`, or `None` for a name that is not
/// valid UTF-8. OneDrive names are Unicode, so such an entry could never
/// round-trip; worse, a lossy conversion yields a path that does not exist on
/// disk, and opening it used to abort the whole scan.
fn utf8_name(entry: &std::fs::DirEntry) -> Option<String> {
    match entry.file_name().to_str() {
        Some(name) => Some(name.to_owned()),
        None => {
            tracing::warn!(path = ?entry.path(), "skipping a name that is not valid UTF-8");
            None
        }
    }
}

/// Read-only counterpart used by background discovery. It neither removes
/// stale staging files nor follows symlinks, so running it concurrently with
/// remote polling cannot mutate the sync tree.
fn read_dir_readonly(dir: &Path) -> std::io::Result<Vec<(String, LocalStat)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = utf8_name(&entry) else {
            continue;
        };
        if INTERNAL_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
        {
            continue;
        }
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.is_file() || meta.is_dir() {
            out.push((name, LocalStat::from_metadata(&meta)));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod recycled_inode_tests {
    use super::inode_was_recycled;

    #[test]
    fn an_inode_born_after_the_record_was_written_is_a_different_file() {
        // `rm -rf A && mkdir B` can hand B the inode A just released. Every
        // other signal — same inode, same device, nothing at the old path —
        // reads as a move, and believing it hands A's remote identity, its
        // sharing links and its version history to B's contents.
        assert!(inode_was_recycled(Some(200), Some(100)));
    }

    #[test]
    fn a_move_keeps_the_inodes_original_birth_time() {
        // Renaming does not change birth time, so it still predates the record
        // — including when the file was edited after being moved.
        assert!(!inode_was_recycled(Some(100), Some(200)));
        // Written and recorded within the same second is the ordinary case for
        // a file we just downloaded.
        assert!(!inode_was_recycled(Some(100), Some(100)));
    }

    #[test]
    fn without_a_birth_time_the_inode_number_is_still_trusted() {
        // tmpfs and some network filesystems do not record one. Refusing every
        // move there would turn each one into a delete and a fresh upload.
        assert!(!inode_was_recycled(None, Some(100)));
        assert!(!inode_was_recycled(Some(100), None));
        assert!(!inode_was_recycled(None, None));
    }
}
