#[tokio::test]
async fn first_sync_builds_the_local_tree_and_stamps_timestamps() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Documents"),
            file("F1", "D1", "report.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;

    assert!(h.path("Documents").is_dir());
    assert_eq!(h.read("Documents/report.txt"), "hello");
    assert_eq!(summary.downloaded, 1);

    // The file must carry the service's timestamp, not the download time —
    // otherwise the very next local scan reads it as an edit and uploads it back.
    let expected = time::macros::datetime!(2026-08-20 10:00:00 UTC).unix_timestamp();
    assert_eq!(mtime_secs(&h.path("Documents/report.txt")), expected);

    // And the deltaLink is now persisted for the next round.
    let account = h.store.account(ACCOUNT).unwrap().unwrap();
    assert!(account.delta_link.unwrap().contains("token=v1"));
}

#[tokio::test]
async fn a_second_sync_with_no_changes_transfers_nothing() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let summary = h.sync().await;

    // The steady state must be genuinely quiet: no download, no upload, and in
    // particular no re-upload of the file we just fetched.
    assert_eq!(
        summary,
        SyncSummary {
            still_queued: 0,
            ..Default::default()
        }
    );
    assert_eq!(h.read("a.txt"), "hello");
}

#[tokio::test]
async fn a_remote_edit_is_downloaded_over_the_local_copy() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file("F1", ROOT, "a.txt", "c2", 7, "2026-08-21T10:00:00Z")],
        "v2",
    )
    .await;
    h.mock_download("F1", b"goodbye").await;

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 1);
    assert_eq!(h.read("a.txt"), "goodbye");
}

#[tokio::test]
async fn a_remote_file_rename_moves_the_local_inode_without_reuploading() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "before.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    let inode = std::fs::metadata(h.path("before.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            ROOT,
            "after.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v2",
    )
    .await;

    let summary = h.sync().await;
    assert!(!h.path("before.txt").exists());
    assert_eq!(h.read("after.txt"), "hello");
    assert_eq!(
        std::fs::metadata(h.path("after.txt")).unwrap().ino(),
        inode.ino()
    );
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.uploaded, 0);
}

#[tokio::test]
async fn a_remote_rename_does_not_mark_a_local_edit_as_synced() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "before.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    std::fs::write(h.path("before.txt"), b"local edit").unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            ROOT,
            "after.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v2",
    )
    .await;
    let uploaded = json!({
        "id": "F1", "name": "after.txt", "size": 10,
        "cTag": "c2", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-29T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .and(body_bytes(b"local edit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded.clone()))
        .mount(&h.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .and(body_partial_json(json!({"fileSystemInfo": {}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.uploaded, 1);
    assert!(!h.path("before.txt").exists());
    assert_eq!(h.read("after.txt"), "local edit");
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().ctag.as_deref(),
        Some("c2")
    );
}

#[tokio::test]
async fn a_remote_folder_rename_moves_descendants_not_duplicates_them() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Before"),
            file("F1", "D1", "inside.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    h.server.reset().await;
    // Graph reports only the renamed folder, not all descendants.
    h.mock_delta(
        Some(("token", "v1")),
        vec![folder("D1", ROOT, "After")],
        "v2",
    )
    .await;
    let summary = h.sync().await;

    assert!(!h.path("Before").exists());
    assert_eq!(h.read("After/inside.txt"), "hello");
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.uploaded, 0);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().parent_id.as_deref(),
        Some("D1")
    );
}

#[tokio::test]
async fn a_child_on_an_earlier_delta_page_waits_for_its_parent() {
    let h = Harness::new().await;
    let next = format!("{}/me/drive/root/delta?page=2", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file("F1", "D1", "inside.txt", "c1", 5, "2026-08-20T10:00:00Z")],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [folder("D1", ROOT, "Parent")],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=v1", h.server.uri())
        })))
        .mount(&h.server)
        .await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;
    assert_eq!(h.read("Parent/inside.txt"), "hello");
    assert_eq!(summary.enumerated, 2);
    assert_eq!(summary.skipped, 0);
}

#[tokio::test]
async fn the_last_occurrence_of_an_item_in_delta_wins() {
    let h = Harness::new().await;
    let next = format!("{}/me/drive/root/delta?page=2", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file("F1", ROOT, "stale.txt", "c1", 5, "2026-08-20T10:00:00Z")],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file("F1", ROOT, "final.txt", "c2", 5, "2026-08-21T10:00:00Z")],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=v1", h.server.uri())
        })))
        .mount(&h.server)
        .await;
    h.mock_download("F1", b"final").await;

    h.sync().await;
    assert!(!h.path("stale.txt").exists());
    assert_eq!(h.read("final.txt"), "final");
}

#[tokio::test]
async fn a_remote_delete_removes_an_untouched_local_file() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    assert!(h.path("a.txt").exists());

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![tombstone("F1", ROOT)], "v2")
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.deleted_local, 1);
    assert!(!h.path("a.txt").exists());
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn a_remote_delete_never_destroys_an_unsaved_local_edit() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    // The user edits the file; only then does the remote delete arrive.
    std::fs::write(h.path("a.txt"), b"my important edit").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![tombstone("F1", ROOT)], "v2")
        .await;
    mock_upload(&h.server, "F2", "a.txt").await;

    h.sync().await;

    // The local content survives and is offered back to the service.
    assert_eq!(h.read("a.txt"), "my important edit");
}

#[tokio::test]
async fn a_new_local_file_is_uploaded() {
    let h = Harness::new().await;
    h.mock_delta(None, vec![], "v1").await;
    h.sync().await;

    std::fs::write(h.path("notes.txt"), b"local content").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "F9", "notes.txt").await;

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 1);

    // It is now tracked, so the next scan will not upload it again.
    let record = h.store.item("F9").unwrap().unwrap();
    assert_eq!(record.name, "notes.txt");
    assert_eq!(record.state, ItemState::Synced);
    assert!(record.local_ino.is_some());
}

#[tokio::test]
async fn a_local_directory_is_created_remotely() {
    let h = Harness::new().await;
    h.mock_delta(None, vec![], "v1").await;
    h.sync().await;

    std::fs::create_dir(h.path("New Folder")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("POST"))
        .and(path(format!("/me/drive/items/{ROOT}/children")))
        .respond_with(ResponseTemplate::new(201).set_body_json(folder("D9", ROOT, "New Folder")))
        .mount(&h.server)
        .await;

    h.sync().await;
    let record = h.store.item("D9").unwrap().unwrap();
    assert!(record.is_folder);
    assert_eq!(record.name, "New Folder");
}

#[tokio::test]
async fn a_file_changed_on_both_sides_keeps_both_versions() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::write(h.path("a.txt"), b"local version").unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file("F1", ROOT, "a.txt", "c2", 14, "2026-08-21T10:00:00Z")],
        "v2",
    )
    .await;
    h.mock_download("F1", b"remote version").await;
    mock_upload(&h.server, "F2", "conflict").await;

    let summary = h.sync().await;
    assert_eq!(summary.conflicts, 1);

    // The remote version takes the original name...
    assert_eq!(h.read("a.txt"), "remote version");

    // ...and the local one is still on disk under a conflict name.
    let copies: Vec<_> = std::fs::read_dir(&h.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("的冲突副本"))
        .collect();
    assert_eq!(
        copies.len(),
        1,
        "expected exactly one conflict copy, got {copies:?}"
    );
    assert_eq!(h.read(&copies[0]), "local version");
}

#[tokio::test]
async fn an_expired_delta_token_triggers_a_full_resync() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F2", ROOT, "b.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.mock_download("F2", b"world").await;
    h.sync().await;
    assert!(h.path("a.txt").exists() && h.path("b.txt").exists());

    h.server.reset().await;
    // The stored token is rejected...
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "v1"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": {"code": "resyncRequired", "message": "Resync required."}
        })))
        .mount(&h.server)
        .await;
    // ...and the fresh enumeration no longer mentions b.txt.
    h.mock_delta(
        Some(("$top", "1000")),
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v2",
    )
    .await;

    h.sync().await;

    // A plain re-enumeration reports what exists, never what was removed; the
    // generation sweep is what makes the deletion visible.
    assert!(h.path("a.txt").exists(), "a.txt should have survived");
    assert!(!h.path("b.txt").exists(), "b.txt was gone remotely");
    assert!(h.store.item("F2").unwrap().is_none());
}

#[tokio::test]
async fn a_resync_sweep_keeps_a_locally_edited_file_it_would_have_deleted() {
    // The delta tombstone path preserves an unsaved local edit and re-uploads
    // it (KeepLocalAndReupload). The resync generation sweep took a different,
    // unguarded path and deleted the local file outright — losing the edit.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F2", ROOT, "b.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.mock_download("F2", b"world").await;
    h.sync().await;

    // The user edits b.txt after that sync — not yet uploaded.
    std::fs::write(h.path("b.txt"), b"EDITED LOCALLY, NOT YET UPLOADED").unwrap();

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "v1"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": {"code": "resyncRequired", "message": "Resync required."}
        })))
        .mount(&h.server)
        .await;
    // The fresh enumeration no longer mentions b.txt: gone remotely.
    h.mock_delta(
        Some(("$top", "1000")),
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v2",
    )
    .await;
    mock_upload(&h.server, "F2new", "b.txt").await;

    h.sync().await;

    // The edit must survive, exactly as it does down the ordinary delete path.
    assert!(
        h.path("b.txt").exists(),
        "a locally-edited file must survive the resync sweep"
    );
    assert_eq!(h.read("b.txt"), "EDITED LOCALLY, NOT YET UPLOADED");
}

#[tokio::test]
async fn throttling_is_retried_rather_than_failing_the_sync() {
    let h = Harness::new().await;
    // First attempt is throttled with a one-second Retry-After.
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 1);
    assert_eq!(h.read("a.txt"), "hello");
}

#[tokio::test]
async fn a_name_onedrive_rejects_is_reported_and_never_uploaded() {
    let h = Harness::new().await;
    h.mock_delta(None, vec![], "v1").await;
    h.sync().await;

    // Legal on ext4, rejected by the service.
    std::fs::write(h.path("a:b.txt"), b"x").unwrap();
    std::fs::write(h.path("CON"), b"x").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // No upload mock is registered: if the engine tried, the request would 404
    // and the test would fail on the summary below.

    let summary = h.sync().await;
    assert_eq!(summary.skipped, 2);
    assert_eq!(summary.uploaded, 0);
    assert_eq!(summary.failed, 0);
    // The files stay on disk untouched; we refuse to sync them, not to keep them.
    assert!(h.path("a:b.txt").exists());
}

#[tokio::test]
async fn a_local_delete_is_propagated_remotely() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::remove_file(h.path("a.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F1"))
        .and(header("If-Match", "e-c1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.deleted_remote, 1);
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn restoring_a_file_before_its_delete_runs_cancels_the_remote_delete() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::remove_file(h.path("a.txt")).unwrap();
    let mut scan = SyncSummary::default();
    h.engine.scan_local(&mut scan).unwrap();
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 1);

    // A local undo during the delete quiet window wins even if this operation
    // was already selected by a worker. The executor must re-check the path
    // immediately before issuing the irreversible request.
    std::fs::write(h.path("a.txt"), b"restored locally").unwrap();
    h.server.reset().await;
    let summary = h
        .engine
        .transfer_slice(
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            QueueScope::Outbound,
        )
        .await
        .unwrap();

    assert_eq!(summary.deleted_remote, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
    assert!(h.store.item("F1").unwrap().is_some());
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn remote_delete_fails_closed_when_local_absence_cannot_be_verified() {
    use std::os::unix::fs::symlink;

    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    let record = h.store.item("F1").unwrap().unwrap();
    std::fs::remove_file(h.path("Docs/a.txt")).unwrap();
    std::fs::remove_dir(h.path("Docs")).unwrap();
    symlink("Docs", h.path("Docs")).unwrap();
    h.store
        .enqueue_conditional(
            ACCOUNT,
            OpKind::DeleteRemote,
            "F1",
            None,
            None,
            record.etag.as_deref(),
        )
        .unwrap();

    h.server.reset().await;
    let summary = h
        .engine
        .transfer_slice(
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            QueueScope::Outbound,
        )
        .await
        .unwrap();

    assert_eq!(summary.deleted_remote, 0);
    assert_eq!(summary.failed, 1);
    assert!(h.store.item("F1").unwrap().is_some());
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_conditional_delete_412_keeps_the_newer_remote_record() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    std::fs::remove_file(h.path("a.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F1"))
        .and(header("If-Match", "e-c1"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.deleted_remote, 0);
    assert_eq!(summary.conflicts, 1);
    assert!(h.store.item("F1").unwrap().is_some());
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
}

#[tokio::test]
async fn deleting_an_online_only_file_propagates_the_delete() {
    // Deleting an online-only placeholder must delete it from the cloud, the
    // way it does on Windows. The bug: a placeholder never had its inode
    // recorded, so the delete guard (which spares a download that never
    // landed) mistook it for one and the deletion never reached the server.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};
    // Never downloaded — a pure placeholder from materialisation, which is the
    // real case: run_download would have recorded the inode, materialize does
    // not.
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.sync().await;

    // Materialise it as an online-only placeholder on disk.
    h.engine.materialize_all(|_, _| true).unwrap();
    assert!(h.path("a.txt").exists());
    assert_eq!(
        Placeholder::state(&h.path("a.txt")),
        PlaceholderState::Dehydrated
    );
    assert!(
        h.store.item("F1").unwrap().unwrap().local_ino.is_none(),
        "materialise records no inode — that is the bug this guards"
    );

    // A scan records the placeholder's inode.
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h.sync().await;
    assert!(
        h.store.item("F1").unwrap().unwrap().local_ino.is_some(),
        "the placeholder's inode should be recorded so a later delete is seen"
    );

    // Now delete the online-only file — it must reach the cloud.
    std::fs::remove_file(h.path("a.txt")).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 1,
        "deleting an online-only file must reach the cloud"
    );
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn a_pending_download_is_not_mistaken_for_a_local_delete() {
    // The dangerous case: an item we recorded but have not written to disk yet
    // looks exactly like a file the user deleted. Deleting it remotely would
    // destroy data the user never touched.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    // The download fails, so the record exists with no local file behind it.
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert!(!h.path("a.txt").exists());
    assert_eq!(
        summary.deleted_remote, 0,
        "must not have deleted anything remotely"
    );
    assert!(h.store.item("F1").unwrap().is_some());
}

#[tokio::test]
async fn a_scan_that_would_wipe_the_account_holds_the_deletions() {
    // The catastrophe this guards: the whole tree vanishing at once — an
    // unmounted-but-present root, a bad bind, an `rm -rf` — reads as "every
    // file was deleted" and would delete the entire account server-side. A
    // deletion that large is held for confirmation, never executed on sight.
    let h = Harness::new().await;
    let items: Vec<Value> = (0..12)
        .map(|i| {
            file(
                &format!("F{i}"),
                ROOT,
                &format!("f{i}.txt"),
                "c1",
                5,
                "2026-08-20T10:00:00Z",
            )
        })
        .collect();
    h.mock_delta(None, items, "v1").await;
    for i in 0..12 {
        h.mock_download(&format!("F{i}"), b"hello").await;
    }
    h.sync().await;

    // Everything disappears in one pass.
    for i in 0..12 {
        std::fs::remove_file(h.path(&format!("f{i}.txt"))).unwrap();
    }

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // Deliberately no DELETE mock: a working guard enqueues no deletion at all.
    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 0,
        "a mass vanish must not delete remotely"
    );
    assert!(
        summary.refused_deletions >= 12,
        "the deletions should be held, not run"
    );
    for i in 0..12 {
        assert!(
            h.store.item(&format!("F{i}")).unwrap().is_some(),
            "every record must survive a held mass deletion"
        );
    }
}

#[tokio::test]
async fn a_sync_root_on_a_different_device_refuses_to_scan() {
    // An unmounted mountpoint still *exists* as an empty directory on a
    // different filesystem. The device number is the tell: if it changed since
    // the last scan, refuse — deleting to match an empty tree would wipe the
    // account.
    use std::os::unix::fs::MetadataExt;
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await; // records the real device

    // Simulate the root becoming a different filesystem.
    let real_dev = std::fs::metadata(&h.root).unwrap().dev();
    h.store.set_root_dev(ACCOUNT, real_dev ^ 1).unwrap();
    std::fs::remove_file(h.path("a.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 0,
        "a device mismatch must never delete"
    );
    assert!(h.store.item("F1").unwrap().is_some());
}

/// A simple-upload mock: accepts the PUT and the follow-up mtime PATCH.
async fn mock_upload(server: &MockServer, new_id: &str, name: &str) {
    let created = json!({
        "id": new_id, "name": name, "size": 13,
        "cTag": "cu", "eTag": "eu",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-22T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path_regex(r"^/me/drive/items/.+/content$"))
        .respond_with(ResponseTemplate::new(201).set_body_json(created.clone()))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/me/drive/items/.+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(created))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// The shipped default: enumerate everything, download nothing.
//
// A OneDrive can easily hold more than the disk it is synced to. Downloading on
// sight fills the disk and fails, after hours of transfer — so the default has
// to be that content arrives only when it is asked for.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_same_batch_delete_and_recreate_under_one_name_is_not_a_conflict() {
    // A backup tool saves by deleting `a.txt` and creating a new one. Both
    // land in one delta batch. Applying the new item before the old one's
    // tombstone found the old file at the path and filed a conflict copy of
    // content the user never touched — then hit the unique name index.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    assert_eq!(h.read("a.txt"), "hello");

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            file("F2", ROOT, "a.txt", "c2", 5, "2026-08-21T10:00:00Z"),
            tombstone("F1", ROOT),
        ],
        "v2",
    )
    .await;
    h.mock_download("F2", b"world").await;
    let summary = h.sync().await;

    assert_eq!(summary.conflicts, 0);
    assert_eq!(h.read("a.txt"), "world");
    assert!(h.store.item("F1").unwrap().is_none());
    assert!(h.store.item("F2").unwrap().is_some());
    assert!(
        h.store.due_outbound_ops(ACCOUNT, 10).unwrap().is_empty(),
        "nothing local was changed, so nothing is uploaded"
    );
    let stray: Vec<_> = std::fs::read_dir(&h.root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "a.txt" && !name.starts_with('.'))
        .collect();
    assert!(stray.is_empty(), "no conflict copy may appear: {stray:?}");
}

#[tokio::test]
async fn a_same_batch_folder_delete_and_recreate_does_not_adopt_the_old_contents() {
    // Deleting a folder and recreating one with the same name lands both in
    // one batch. The old folder's tombstone must not jump ahead of its
    // children's: applied early it finds the directory non-empty, detaches
    // the subtree, and the new folder inherits the old files as unrecorded
    // content that the next scan would upload.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "docs"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    assert_eq!(h.read("docs/a.txt"), "hello");

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            folder("D2", ROOT, "docs"),
            file("F2", "D2", "b.txt", "c2", 5, "2026-08-21T10:00:00Z"),
            tombstone("F1", "D1"),
            tombstone("D1", ROOT),
        ],
        "v2",
    )
    .await;
    h.mock_download("F2", b"world").await;
    let summary = h.sync().await;

    assert_eq!(summary.conflicts, 0);
    assert!(!h.path("docs/a.txt").exists(), "the deleted file must not survive");
    assert_eq!(h.read("docs/b.txt"), "world");
    assert!(h.store.item("D1").unwrap().is_none());
    assert!(h.store.item("F1").unwrap().is_none());
    assert!(h.store.item("D2").unwrap().is_some());
    assert!(
        h.store.due_outbound_ops(ACCOUNT, 10).unwrap().is_empty(),
        "nothing local changed, so nothing may be uploaded"
    );
}

#[tokio::test]
async fn a_stopped_engine_neither_writes_nor_reaches_the_service() {
    // Signing out removes the engine from the daemon's map, but the
    // coordinator and the hydration path hold Arc clones across awaits. Work
    // already under way kept running: writing rows for an account about to be
    // deleted, and — through its authenticator — rotating a token whose
    // keyring entry had just been removed, which silently re-created it.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;

    h.engine.stop();

    // Cancelled, not a 404 from the mock server: the request never left.
    let error = h.engine.sync_remote().await.unwrap_err();
    assert!(
        matches!(error, gonedrive_core::Error::Cancelled),
        "unexpected error: {error:?}"
    );
    assert!(h.store.item("F1").unwrap().is_none(), "nothing was applied");

    std::fs::write(h.path("mine.txt"), b"local").unwrap();
    let error = h
        .engine
        .sync_local_paths(std::slice::from_ref(&h.root))
        .unwrap_err();
    assert!(
        matches!(error, gonedrive_core::Error::Cancelled),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        h.store.pending_count(ACCOUNT).unwrap(),
        0,
        "no work may be queued for a retired account"
    );
    assert!(h.engine.is_stopped());
}
