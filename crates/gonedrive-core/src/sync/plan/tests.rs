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

fn set_quickxor(item: &mut DriveItem, value: &str) {
    item.file.as_mut().unwrap().hashes = Some(crate::models::Hashes {
        quick_xor_hash: Some(value.to_string()),
        sha1_hash: None,
        sha256_hash: None,
    });
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
        dev: 1,
        ino: 1,
    }
}

fn dir_stat() -> LocalStat {
    LocalStat {
        is_dir: true,
        size: 4096,
        mtime: 0,
        dev: 1,
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
fn a_matching_content_hash_suppresses_an_upload_echo_with_a_new_ctag() {
    let mut old = remote_file("F1", "R", "a.txt", "c1", 10);
    set_quickxor(&mut old, "same-content");
    let local = stat(10, old.effective_mtime().unwrap().unix_timestamp());
    let known = synced_record(&old, &local);

    let mut echoed = remote_file("F1", "R", "a.txt", "c2", 10);
    set_quickxor(&mut echoed, "same-content");
    assert_eq!(
        decide_remote(&echoed, Some(&known), Some(&local)),
        RemoteAction::Skip(SkipReason::UpToDate)
    );
}

#[test]
fn a_changed_content_hash_wins_even_when_the_ctag_did_not_move() {
    let mut old = remote_file("F1", "R", "a.txt", "c1", 10);
    set_quickxor(&mut old, "old-content");
    let local = stat(10, old.effective_mtime().unwrap().unix_timestamp());
    let known = synced_record(&old, &local);

    let mut changed = remote_file("F1", "R", "a.txt", "c1", 10);
    set_quickxor(&mut changed, "new-content");
    assert_eq!(
        decide_remote(&changed, Some(&known), Some(&local)),
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
    assert!(
        !local_is_modified(&known, &local),
        "guard: not a local edit"
    );
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
    assert_eq!(
        decide_remote(&folder, None, None),
        RemoteAction::CreateDirectory
    );
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

    assert_eq!(
        decide_local("a.txt", None, Some(&local)),
        LocalAction::UploadNew
    );
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
