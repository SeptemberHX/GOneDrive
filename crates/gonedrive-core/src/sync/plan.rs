//! The sync decisions, as pure functions.
//!
//! Nothing here touches the filesystem, the network or the database. Every
//! input is a value and every output is an [`Action`], which is what makes the
//! conflict matrix — the part that is genuinely easy to get wrong and
//! catastrophic when wrong — exhaustively testable.

use time::OffsetDateTime;

use crate::db::ItemRecord;
use crate::error::NameError;
use crate::models::DriveItem;
use crate::names::validate_name;

/// What is on disk right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalStat {
    pub is_dir: bool,
    pub size: u64,
    /// Truncated to whole seconds, matching what we persist.
    pub mtime: i64,
    pub ino: u64,
}

impl LocalStat {
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            is_dir: meta.is_dir(),
            size: meta.len(),
            mtime: meta.mtime(),
            ino: meta.ino(),
        }
    }
}

/// Why an item was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Local and remote already agree.
    UpToDate,
    /// A OneNote notebook or similar: a folder-shaped thing that is not a tree.
    Package,
    /// The name cannot exist on OneDrive. Reported to the user.
    InvalidName(NameError),
    /// The parent has not arrived yet; retried on the next pass.
    MissingParent,
    /// A symlink, socket, fifo or device — nothing OneDrive can represent.
    NotARegularFile,
}

/// What to do about a change the service reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    /// Create the local directory (and record it).
    CreateDirectory,
    /// Fetch content. `overwrite` distinguishes a new file from a replacement,
    /// which matters only for logging and progress reporting.
    Download { overwrite: bool },
    /// Content is unchanged; only the timestamp needs re-stamping.
    RestampMtime,
    /// The item is gone remotely and untouched locally.
    DeleteLocal,
    /// Gone remotely but modified locally. The local edit wins: it is kept and
    /// re-uploaded as a new item rather than destroyed by a remote delete the
    /// user may not have intended to apply to their newer content.
    KeepLocalAndReupload,
    /// Both sides changed. The local copy is renamed aside and uploaded, then
    /// the remote version is downloaded under the original name — so neither
    /// version is ever lost.
    ConflictCopy,
    Skip(SkipReason),
}

/// What to do about a change we found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAction {
    CreateRemoteFolder,
    /// Upload a file the service has never seen.
    UploadNew,
    /// Upload over an item the service already has.
    UploadModified,
    /// The file is gone locally and was unmodified remotely.
    DeleteRemote,
    Nothing,
    Skip(SkipReason),
}

/// Whether the bytes on disk differ from what we recorded at the last sync.
///
/// Compared against our own record rather than against the remote metadata,
/// because size and mtime are the only things we can check without reading —
/// and hashing every file on every scan is not viable on a large drive.
pub fn local_is_modified(known: &ItemRecord, local: &LocalStat) -> bool {
    match (known.local_size, known.local_mtime) {
        (Some(size), Some(mtime)) => size != local.size || mtime.unix_timestamp() != local.mtime,
        // We have never recorded a local observation, so we cannot claim it is
        // unmodified. Treating it as modified is the safe direction: at worst
        // an unnecessary upload, versus silently discarding a local edit.
        _ => true,
    }
}

/// Whether the service's copy differs from what we last downloaded.
///
/// `cTag` changes only when *content* changes, unlike `eTag` which also moves
/// on metadata edits — so comparing cTags avoids re-downloading a file whose
/// only change was a rename.
fn remote_content_changed(known: &ItemRecord, remote: &DriveItem) -> bool {
    match (known.ctag.as_deref(), remote.ctag.as_deref()) {
        (Some(a), Some(b)) => a != b,
        // Personal OneDrive omits cTag on some item types; fall back to size
        // plus mtime, which is the same comparison we make locally.
        _ => {
            known.size != remote.size.unwrap_or(0)
                || known.remote_mtime.map(|t| t.unix_timestamp())
                    != remote.effective_mtime().map(|t| t.unix_timestamp())
        }
    }
}

/// Decide what to do about one item from a delta page.
///
/// * `known` — our record, if we have seen this item before.
/// * `local` — what is on disk at the item's path, if anything.
pub fn decide_remote(
    remote: &DriveItem,
    known: Option<&ItemRecord>,
    local: Option<&LocalStat>,
) -> RemoteAction {
    if remote.is_deleted() {
        return match (known, local) {
            (Some(known), Some(local)) if !local.is_dir && local_is_modified(known, local) => {
                RemoteAction::KeepLocalAndReupload
            }
            (_, Some(_)) => RemoteAction::DeleteLocal,
            // Never seen it, or already absent: nothing to do.
            (_, None) => RemoteAction::Skip(SkipReason::UpToDate),
        };
    }

    // Packages (OneNote notebooks) report a folder facet but are not trees we
    // can mirror; syncing one produces a directory of unusable fragments.
    if remote.is_package() {
        return RemoteAction::Skip(SkipReason::Package);
    }

    if let Some(name) = remote.name.as_deref() {
        // A name the service accepted may still be unusable for us; `.` and
        // `..` are the cases that would escape the sync root.
        if name == "." || name == ".." || name.contains('/') || name.contains('\0') {
            return RemoteAction::Skip(SkipReason::InvalidName(NameError::IllegalCharacter));
        }
    }

    if remote.is_folder() {
        return match local {
            Some(l) if l.is_dir => RemoteAction::Skip(SkipReason::UpToDate),
            // A file sits where the folder must go: the folder wins, and the
            // file is preserved as a conflict copy rather than deleted.
            Some(_) => RemoteAction::ConflictCopy,
            None => RemoteAction::CreateDirectory,
        };
    }

    match (known, local) {
        // Brand new to us.
        (None, None) => RemoteAction::Download { overwrite: false },
        // We have no record but a file is already there — e.g. the state
        // database was lost. Treat it as a conflict rather than overwriting
        // content we cannot prove is a copy of the remote version.
        (None, Some(_)) => RemoteAction::ConflictCopy,
        // Known to us but absent on disk: the user deleted it, or a previous
        // download failed. Re-fetch.
        (Some(_), None) => RemoteAction::Download { overwrite: false },
        (Some(known), Some(local)) => {
            let remote_changed = remote_content_changed(known, remote);
            let local_changed = local_is_modified(known, local);
            match (remote_changed, local_changed) {
                (true, true) => RemoteAction::ConflictCopy,
                (true, false) => RemoteAction::Download { overwrite: true },
                // Remote metadata moved without the content — or so the cTag
                // claims. But a cTag can run ahead of the bytes it names: a
                // pass advances the stored cTag when it records a remote
                // change, and if the download it queued never lands (the
                // daemon restarts, a transfer is interrupted) the next pass
                // sees "cTag matches" and would call a stale file synced,
                // leaving it permanently diverged with no event ever
                // re-triggering the fetch. Size is a cheap necessary condition
                // for content equality: when it disagrees the bytes are still
                // owed, so fetch regardless of the cTag. (`local` here is not a
                // local edit — that is the `(_, true)` arm — so overwriting is
                // safe and is what the user's other device already did.)
                (false, false) => {
                    if !local.is_dir && local.size != remote.size.unwrap_or(local.size) {
                        return RemoteAction::Download { overwrite: true };
                    }
                    let want = remote.effective_mtime().map(|t| t.unix_timestamp());
                    match want {
                        Some(want) if want != local.mtime => RemoteAction::RestampMtime,
                        _ => RemoteAction::Skip(SkipReason::UpToDate),
                    }
                }
                // Local-only change: the upload path owns this, not us.
                (false, true) => RemoteAction::Skip(SkipReason::UpToDate),
            }
        }
    }
}

/// Decide what to do about one entry found by a local scan.
///
/// `name` is the entry's own file name, validated here because this is the
/// last point before it would enter the upload queue.
pub fn decide_local(name: &str, known: Option<&ItemRecord>, local: Option<&LocalStat>) -> LocalAction {
    if let Err(reason) = validate_name(name) {
        return LocalAction::Skip(SkipReason::InvalidName(reason));
    }

    match (known, local) {
        (None, None) => LocalAction::Nothing,
        (None, Some(local)) if local.is_dir => LocalAction::CreateRemoteFolder,
        (None, Some(_)) => LocalAction::UploadNew,
        // Known to us and now gone from disk. Deleting remotely is right only
        // because the caller has already confirmed the parent directory still
        // exists — otherwise an unmounted drive would delete the whole account.
        (Some(_), None) => LocalAction::DeleteRemote,
        (Some(known), Some(local)) => {
            if local.is_dir {
                LocalAction::Nothing
            } else if local_is_modified(known, local) {
                LocalAction::UploadModified
            } else {
                LocalAction::Nothing
            }
        }
    }
}

/// Order a delta batch so a parent is always applied before its children.
///
/// Delta pages are mostly parent-first already, but a rename or a move can put
/// a child ahead of the folder it now lives in, and applying that order would
/// try to create a file inside a directory that does not exist yet.
///
/// Items whose parent is outside the batch keep their relative order, since
/// their parent either already exists locally or will be reported as missing.
pub fn order_parents_first(items: Vec<DriveItem>) -> Vec<DriveItem> {
    use std::collections::HashMap;

    let in_batch: HashMap<&str, usize> = items
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id.as_str(), i))
        .collect();

    // Depth within the batch: an item's depth is one more than its parent's,
    // or zero when the parent is not part of this batch. `visiting` is reused
    // across items because the recursion always unwinds it.
    let mut visiting = vec![false; items.len()];
    let depth: Vec<usize> = (0..items.len())
        .map(|i| batch_depth(i, &items, &in_batch, &mut visiting))
        .collect();

    let mut order: Vec<usize> = (0..items.len()).collect();
    // Stable sort keeps the service's own ordering within a depth level.
    order.sort_by_key(|&i| depth[i]);

    let mut slots: Vec<Option<DriveItem>> = items.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect()
}

fn batch_depth(
    index: usize,
    items: &[DriveItem],
    in_batch: &std::collections::HashMap<&str, usize>,
    visiting: &mut Vec<bool>,
) -> usize {
    // A cycle cannot occur in a well-formed tree, but a corrupt page must not
    // become an infinite recursion.
    if visiting[index] {
        return 0;
    }
    let Some(parent) = items[index].parent_id() else {
        return 0;
    };
    let Some(&parent_index) = in_batch.get(parent) else {
        return 0;
    };
    visiting[index] = true;
    let depth = batch_depth(parent_index, items, in_batch, visiting) + 1;
    visiting[index] = false;
    depth
}

/// Split a delta batch into the changes to apply and the tombstones to process.
///
/// Deletions are returned deepest-first so a folder's contents are removed
/// before the folder itself.
pub fn partition_delta(items: Vec<DriveItem>) -> (Vec<DriveItem>, Vec<DriveItem>) {
    let (deletes, upserts): (Vec<_>, Vec<_>) = items.into_iter().partition(DriveItem::is_deleted);
    let mut deletes = order_parents_first(deletes);
    deletes.reverse();
    (order_parents_first(upserts), deletes)
}

/// Convert a remote item into the record we persist for it.
pub fn record_from_remote(
    account_id: &str,
    remote: &DriveItem,
    state: crate::db::ItemState,
    pin_state: crate::db::PinState,
) -> ItemRecord {
    let hashes = remote.hashes();
    ItemRecord {
        id: remote.id.clone(),
        account_id: account_id.to_string(),
        // The root is the only item we deliberately record without a parent,
        // even though the service does give it one.
        parent_id: if remote.is_root() {
            None
        } else {
            remote.parent_id().map(str::to_string)
        },
        name: remote.name.clone().unwrap_or_else(|| "root".into()),
        is_folder: remote.is_folder(),
        size: remote.size.unwrap_or(0),
        etag: remote.etag.clone(),
        ctag: remote.ctag.clone(),
        quickxor: hashes.and_then(|h| h.quick_xor_hash.clone()),
        sha1: hashes.and_then(|h| h.sha1_hash.clone()),
        remote_mtime: remote.effective_mtime(),
        local_mtime: None,
        local_size: None,
        local_ino: None,
        state,
        pin_state,
    }
}

/// Timestamp helper for stamping downloaded files.
pub fn to_filetime(t: OffsetDateTime) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(t.unix_timestamp().max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ItemState, PinState};

    fn remote_file(id: &str, parent: &str, name: &str, ctag: &str, size: u64) -> DriveItem {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": name,
            "size": size,
            "cTag": ctag,
            "eTag": format!("etag-{ctag}"),
            "file": {"mimeType": "text/plain"},
            "parentReference": {"id": parent},
            "fileSystemInfo": {"lastModifiedDateTime": "2026-08-20T10:00:00Z"}
        }))
        .unwrap()
    }

    fn remote_folder(id: &str, parent: &str, name: &str) -> DriveItem {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "folder": {"childCount": 0},
            "parentReference": {"id": parent}
        }))
        .unwrap()
    }

    fn tombstone(id: &str, parent: &str) -> DriveItem {
        serde_json::from_value(serde_json::json!({
            "id": id, "deleted": {"state": "deleted"},
            "parentReference": {"id": parent}
        }))
        .unwrap()
    }

    /// A record consistent with `remote`, as if we had just synced it.
    fn synced_record(remote: &DriveItem, local: &LocalStat) -> ItemRecord {
        let mut record = record_from_remote("acct", remote, ItemState::Synced, PinState::Pinned);
        record.local_size = Some(local.size);
        record.local_mtime = OffsetDateTime::from_unix_timestamp(local.mtime).ok();
        record.local_ino = Some(local.ino);
        record
    }

    fn stat(size: u64, mtime: i64) -> LocalStat {
        LocalStat {
            is_dir: false,
            size,
            mtime,
            ino: 1,
        }
    }

    fn dir_stat() -> LocalStat {
        LocalStat {
            is_dir: true,
            size: 4096,
            mtime: 0,
            ino: 2,
        }
    }

    // ---- the conflict matrix ---------------------------------------------

    #[test]
    fn unchanged_on_both_sides_does_nothing() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        // Align the local timestamp with the remote one so no restamp is
        // needed either — this is the fully-quiescent case.
        let local = stat(10, remote.effective_mtime().unwrap().unix_timestamp());
        let known = synced_record(&remote, &local);

        assert_eq!(
            decide_remote(&remote, Some(&known), Some(&local)),
            RemoteAction::Skip(SkipReason::UpToDate)
        );
    }

    #[test]
    fn remote_only_change_downloads_over_the_local_copy() {
        let old = remote_file("F1", "R", "a.txt", "c1", 10);
        let local = stat(10, 1_787_000_000);
        let known = synced_record(&old, &local);

        let new = remote_file("F1", "R", "a.txt", "c2", 20);
        assert_eq!(
            decide_remote(&new, Some(&known), Some(&local)),
            RemoteAction::Download { overwrite: true }
        );
    }

    #[test]
    fn local_only_change_is_left_to_the_upload_path() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let known = synced_record(&remote, &stat(10, 1_787_000_000));
        // Same cTag remotely, but the bytes on disk moved.
        let local = stat(99, 1_787_000_500);
        assert_eq!(
            decide_remote(&remote, Some(&known), Some(&local)),
            RemoteAction::Skip(SkipReason::UpToDate)
        );
        assert_eq!(
            decide_local("a.txt", Some(&known), Some(&local)),
            LocalAction::UploadModified
        );
    }

    #[test]
    fn both_sides_changed_makes_a_conflict_copy() {
        let old = remote_file("F1", "R", "a.txt", "c1", 10);
        let known = synced_record(&old, &stat(10, 1_787_000_000));

        let new = remote_file("F1", "R", "a.txt", "c2", 20);
        let local = stat(33, 1_787_000_900);
        assert_eq!(
            decide_remote(&new, Some(&known), Some(&local)),
            RemoteAction::ConflictCopy
        );
    }

    #[test]
    fn stale_content_under_a_matching_ctag_is_still_fetched() {
        // Regression (sync-correctness): a prior pass advanced the stored cTag
        // and size to the new remote version, but the download it queued was
        // interrupted — the daemon restarted before the bytes landed — so the
        // file on disk is the OLD content while the record's cTag already
        // equals the remote's. Change-detection by cTag alone reports "no
        // change", and without the size guard the file sits permanently
        // diverged: the cloud edit is never pulled and nothing re-triggers it.
        let remote = remote_file("F1", "R", "a.txt", "c2", 9603);
        let mtime = remote.effective_mtime().unwrap().unix_timestamp();
        // Local bytes are the stale, larger version. Crucially its size+mtime
        // match what we last recorded, so this is NOT a local edit (that path
        // is covered by `local_only_change_is_left_to_the_upload_path`).
        let local = stat(11191, mtime);
        let known = synced_record(&remote, &local); // cTag c2, size 9603, local_size 11191
        assert!(!local_is_modified(&known, &local), "guard: not a local edit");
        assert_eq!(
            decide_remote(&remote, Some(&known), Some(&local)),
            RemoteAction::Download { overwrite: true }
        );
    }

    #[test]
    fn metadata_only_remote_change_just_restamps() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let remote_mtime = remote.effective_mtime().unwrap().unix_timestamp();
        // Content matches; only the local timestamp is stale.
        let local = stat(10, remote_mtime - 3600);
        let known = synced_record(&remote, &local);
        assert_eq!(
            decide_remote(&remote, Some(&known), Some(&local)),
            RemoteAction::RestampMtime
        );
    }

    // ---- deletions --------------------------------------------------------

    #[test]
    fn remote_delete_removes_an_untouched_local_file() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let local = stat(10, 1_787_000_000);
        let known = synced_record(&remote, &local);
        assert_eq!(
            decide_remote(&tombstone("F1", "R"), Some(&known), Some(&local)),
            RemoteAction::DeleteLocal
        );
    }

    #[test]
    fn remote_delete_never_destroys_an_unsaved_local_edit() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let known = synced_record(&remote, &stat(10, 1_787_000_000));
        // The user edited it after the last sync; the remote delete must not win.
        let local = stat(4096, 1_787_009_999);
        assert_eq!(
            decide_remote(&tombstone("F1", "R"), Some(&known), Some(&local)),
            RemoteAction::KeepLocalAndReupload
        );
    }

    #[test]
    fn a_tombstone_for_something_we_do_not_have_is_a_no_op() {
        assert_eq!(
            decide_remote(&tombstone("F1", "R"), None, None),
            RemoteAction::Skip(SkipReason::UpToDate)
        );
    }

    // ---- first contact and lost state ------------------------------------

    #[test]
    fn a_new_remote_file_is_downloaded() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        assert_eq!(
            decide_remote(&remote, None, None),
            RemoteAction::Download { overwrite: false }
        );
    }

    #[test]
    fn an_unknown_file_already_on_disk_is_treated_as_a_conflict() {
        // This is the "the state database was lost" case. Overwriting would
        // silently destroy whatever the user had there.
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        assert_eq!(
            decide_remote(&remote, None, Some(&stat(999, 1))),
            RemoteAction::ConflictCopy
        );
    }

    #[test]
    fn a_known_file_missing_from_disk_is_refetched() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let known = synced_record(&remote, &stat(10, 1));
        assert_eq!(
            decide_remote(&remote, Some(&known), None),
            RemoteAction::Download { overwrite: false }
        );
    }

    #[test]
    fn a_record_with_no_local_observation_is_assumed_modified() {
        // Safe direction: an unnecessary upload beats a discarded edit.
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let known = record_from_remote("acct", &remote, ItemState::Synced, PinState::Pinned);
        assert!(local_is_modified(&known, &stat(10, 1)));
    }

    // ---- folders and packages --------------------------------------------

    #[test]
    fn folders_are_created_and_then_left_alone() {
        let folder = remote_folder("D1", "R", "Documents");
        assert_eq!(decide_remote(&folder, None, None), RemoteAction::CreateDirectory);
        assert_eq!(
            decide_remote(&folder, None, Some(&dir_stat())),
            RemoteAction::Skip(SkipReason::UpToDate)
        );
    }

    #[test]
    fn a_file_blocking_a_folder_becomes_a_conflict_copy() {
        let folder = remote_folder("D1", "R", "Documents");
        assert_eq!(
            decide_remote(&folder, None, Some(&stat(5, 1))),
            RemoteAction::ConflictCopy
        );
    }

    #[test]
    fn onenote_packages_are_skipped() {
        let package: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "NB", "name": "Notebook", "folder": {"childCount": 3},
            "package": {"type": "oneNote"}, "parentReference": {"id": "R"}
        }))
        .unwrap();
        assert_eq!(
            decide_remote(&package, None, None),
            RemoteAction::Skip(SkipReason::Package)
        );
    }

    #[test]
    fn remote_names_that_would_escape_the_sync_root_are_refused() {
        for name in ["..", ".", "a/b"] {
            let mut item = remote_file("F1", "R", "x", "c1", 1);
            item.name = Some(name.into());
            assert!(
                matches!(
                    decide_remote(&item, None, None),
                    RemoteAction::Skip(SkipReason::InvalidName(_))
                ),
                "accepted {name:?}"
            );
        }
    }

    // ---- local scan decisions --------------------------------------------

    #[test]
    fn local_scan_classifies_each_case() {
        let remote = remote_file("F1", "R", "a.txt", "c1", 10);
        let local = stat(10, 1_787_000_000);
        let known = synced_record(&remote, &local);

        assert_eq!(decide_local("a.txt", None, Some(&local)), LocalAction::UploadNew);
        assert_eq!(
            decide_local("Docs", None, Some(&dir_stat())),
            LocalAction::CreateRemoteFolder
        );
        assert_eq!(
            decide_local("a.txt", Some(&known), None),
            LocalAction::DeleteRemote
        );
        assert_eq!(
            decide_local("a.txt", Some(&known), Some(&local)),
            LocalAction::Nothing
        );
    }

    #[test]
    fn local_names_onedrive_rejects_never_enter_the_queue() {
        let local = stat(1, 1);
        for name in ["a:b", "CON", "trailing ", "~$doc.xlsx", "desktop.ini"] {
            assert!(
                matches!(
                    decide_local(name, None, Some(&local)),
                    LocalAction::Skip(SkipReason::InvalidName(_))
                ),
                "queued {name:?}"
            );
        }
    }

    // ---- ordering ---------------------------------------------------------

    #[test]
    fn children_are_ordered_after_their_parents() {
        // Deliberately reversed: deepest first.
        let items = vec![
            remote_file("F1", "D2", "deep.txt", "c", 1),
            remote_folder("D2", "D1", "Sub"),
            remote_folder("D1", "ROOT", "Docs"),
        ];
        let ordered = order_parents_first(items);
        let ids: Vec<_> = ordered.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["D1", "D2", "F1"]);
    }

    #[test]
    fn ordering_is_stable_within_a_level_and_keeps_every_item() {
        let items = vec![
            remote_file("A", "ROOT", "a", "c", 1),
            remote_file("B", "ROOT", "b", "c", 1),
            remote_file("C", "ROOT", "c", "c", 1),
        ];
        let ids: Vec<_> = order_parents_first(items)
            .iter()
            .map(|i| i.id.clone())
            .collect();
        assert_eq!(ids, ["A", "B", "C"]);
    }

    #[test]
    fn ordering_survives_a_parent_cycle_without_hanging() {
        // Not producible by a healthy service, but a corrupt page must not
        // become an infinite recursion.
        let mut a = remote_folder("A", "B", "a");
        let b = remote_folder("B", "A", "b");
        a.name = Some("a".into());
        let ordered = order_parents_first(vec![a, b]);
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn deletions_are_ordered_deepest_first() {
        let items = [
            remote_folder("D1", "ROOT", "Docs"),
            remote_file("F1", "D2", "deep.txt", "c", 1),
            remote_folder("D2", "D1", "Sub"),
        ];
        // Turn them all into tombstones, preserving the parent links.
        let deletes: Vec<DriveItem> = items
            .iter()
            .map(|i| tombstone(&i.id, i.parent_id().unwrap()))
            .collect();
        let (upserts, deletes) = partition_delta(deletes);
        assert!(upserts.is_empty());
        let ids: Vec<_> = deletes.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["F1", "D2", "D1"]);
    }

    #[test]
    fn partition_separates_tombstones_from_changes() {
        let items = vec![
            remote_file("F1", "ROOT", "a.txt", "c", 1),
            tombstone("F2", "ROOT"),
            remote_folder("D1", "ROOT", "Docs"),
        ];
        let (upserts, deletes) = partition_delta(items);
        assert_eq!(upserts.len(), 2);
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].id, "F2");
    }

    #[test]
    fn the_root_is_recorded_without_a_parent() {
        let root: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "ROOT", "name": "root", "root": {}, "folder": {"childCount": 2},
            "parentReference": {"driveId": "d1"}
        }))
        .unwrap();
        let record = record_from_remote("acct", &root, ItemState::Synced, PinState::Pinned);
        assert!(record.parent_id.is_none());
        assert!(record.is_folder);
    }
}
