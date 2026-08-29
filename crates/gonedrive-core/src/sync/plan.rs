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
    pub dev: u64,
    pub ino: u64,
}

impl LocalStat {
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            is_dir: meta.is_dir(),
            size: meta.len(),
            mtime: meta.mtime(),
            dev: meta.dev(),
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
    Download {
        overwrite: bool,
    },
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
pub fn decide_local(
    name: &str,
    known: Option<&ItemRecord>,
    local: Option<&LocalStat>,
) -> LocalAction {
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
    order.into_iter().filter_map(|i| slots[i].take()).collect()
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
mod tests;
