//! Local discovery and reconciliation, including cumulative delete safety.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use time::OffsetDateTime;

use crate::db::{Account, ItemRecord, ItemState, OpKind};
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::sync::plan::{decide_local, LocalAction, LocalStat};

use super::engine::{Engine, FullScanReport, SafetyStopKind, SyncEvent, SyncSummary};

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
        self.scan_local_scopes(summary, None)
    }

    /// Reconcile only directories touched by the filesystem watcher.
    ///
    /// Each path is revalidated under the sync root. Unknown directories are
    /// widened to their nearest known ancestor so a newly-created subtree is
    /// still discovered, while existing sibling subtrees are never walked.
    pub fn scan_local_paths(&self, paths: &[PathBuf], summary: &mut SyncSummary) -> Result<()> {
        self.scan_local_scopes(summary, Some(paths))
    }

    pub fn sync_local_paths(&self, paths: &[PathBuf]) -> Result<SyncSummary> {
        self.emit(SyncEvent::Started);
        let mut summary = SyncSummary::default();
        self.scan_local_paths(paths, &mut summary)?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        self.emit(SyncEvent::Finished(summary.clone()));
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

    fn scan_local_scopes(
        &self,
        summary: &mut SyncSummary,
        requested_paths: Option<&[PathBuf]>,
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

        while let Some((parent_id, dir)) = stack.pop() {
            let entries = match read_dir(&dir, &rooted) {
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

            for (name, stat) in &entries {
                let record = known.get(&crate::names::fold_case(name));
                if let Some(record) = record {
                    self.store
                        .cancel_pending_delete(&self.account_id, &record.id)?;
                }

                // Delta has already claimed this file and queued its newer
                // remote content. The download path performs the conflict-copy
                // check immediately before overwrite; queueing an upload here
                // would let both directions mutate the same item concurrently.
                if !stat.is_dir
                    && record.is_some_and(|record| record.state == ItemState::DirtyRemote)
                {
                    continue;
                }

                // A placeholder's bytes are not the user's content: dehydrated
                // means holes, hydrating means holes being filled. Judging
                // either as "locally modified" queues an upload of zeroes over
                // the real file in the cloud. Nothing about them is ours to
                // upload, so they only recurse (below) and refresh marks.
                let path = dir.join(name);
                let placeholder_state =
                    record.and_then(|record| owned_placeholder_state(&rooted, &path, &record.id));
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
                        // A parent that is itself new has no id to record yet;
                        // the runner resolves it from the path once the parent
                        // exists — the queue preserves creation order.
                        self.store.enqueue(
                            &self.account_id,
                            OpKind::CreateRemoteFolder,
                            parent_id.as_deref(),
                            Some(&dir.join(name)),
                            None,
                        )?;
                        stack.push((None, dir.join(name)));
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
                    if let Some(record) = record {
                        self.refresh_marks(record, &dir.join(name));
                    }
                }

                if stat.is_dir {
                    if let Some(record) = record {
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
        let by_inode = if appeared.is_empty() {
            HashMap::new()
        } else {
            self.store.items_by_inode(&self.account_id)?
        };
        let mut moved_ids = std::collections::HashSet::new();
        for (path, stat) in &appeared {
            // Parents are deliberately not recorded here: the runner resolves
            // them from the path, because a destination directory created in
            // this same pass has no remote id until its own queued creation
            // has run.
            let known = by_inode.get(&stat.ino).filter(|record| {
                vanished.iter().any(|v| v.id == record.id)
                    || self
                        .has_moved(&account, record, path.parent().unwrap_or(path), "")
                        .unwrap_or(false)
            });
            match known {
                Some(record) => {
                    moved_ids.insert(record.id.clone());
                    if !stat.is_dir && crate::sync::plan::local_is_modified(record, stat) {
                        // Moving preserves the remote identity, but it does not
                        // transfer bytes. If the file was also edited before
                        // this scan, commit the metadata and content work
                        // together so a crash cannot drop the upload half.
                        self.store
                            .enqueue_move_and_upload(&self.account_id, &record.id, path)?;
                    } else {
                        self.store.enqueue_conditional(
                            &self.account_id,
                            OpKind::MoveRemote,
                            &record.id,
                            Some(path),
                            None,
                            record.etag.as_deref().or(record.ctag.as_deref()),
                        )?;
                    }
                }
                None => {
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
        let approved = self
            .mass_delete_approval
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        if !deletes.is_empty() {
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

    /// Whether a known item really moved, rather than a new file coincidentally
    /// landing on a recycled inode.
    fn has_moved(
        &self,
        account: &Account,
        known: &ItemRecord,
        new_dir: &Path,
        new_name: &str,
    ) -> Result<bool> {
        let Some(old) = self.store.local_path(account, &known.id)? else {
            return Ok(false);
        };
        if old == new_dir.join(new_name) {
            return Ok(false);
        }
        // Inodes are recycled aggressively, so require that nothing remains at
        // the old path before believing this is the same file.
        Ok(!old.exists())
    }
}

fn owned_placeholder_state(
    rooted: &super::rooted_fs::RootedFs,
    path: &Path,
    expected_item_id: &str,
) -> Option<PlaceholderState> {
    let file = rooted.open_read(path).ok()?;
    (Placeholder::item_id_file(&file).as_deref() == Some(expected_item_id))
        .then(|| Placeholder::state_file(&file))
}

/// Directory entries worth considering, with their stats.
///
/// Symlinks are read with `symlink_metadata` and then dropped: following them
/// would let a link out of the sync root pull arbitrary files into the account.
fn read_dir(
    dir: &Path,
    rooted: &super::rooted_fs::RootedFs,
) -> std::io::Result<Vec<(String, LocalStat)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if INTERNAL_SUFFIXES.iter().any(|s| name.ends_with(s))
            || INTERNAL_NAMES.contains(&name.as_str())
        {
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
                if stale {
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

/// Read-only counterpart used by background discovery. It neither removes
/// stale staging files nor follows symlinks, so running it concurrently with
/// remote polling cannot mutate the sync tree.
fn read_dir_readonly(dir: &Path) -> std::io::Result<Vec<(String, LocalStat)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if INTERNAL_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
            || INTERNAL_NAMES.contains(&name.as_str())
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
