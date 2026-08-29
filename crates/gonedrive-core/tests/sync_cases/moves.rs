#[tokio::test]
async fn a_cross_directory_rename_is_one_move_not_delete_plus_upload() {
    // Found live: moving a file to a folder created in the same pass turned
    // into a remote DELETE followed by a fresh upload — the file kept its
    // bytes but lost its identity and its server-side version history. Only
    // the move endpoint is mocked here; a delete or a content upload 404s
    // and fails the pass.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "来源"),
            file("F1", "D1", "文档.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    // The user makes a destination folder and moves the file into it.
    std::fs::create_dir(h.path("目的地")).unwrap();
    std::fs::rename(h.path("来源/文档.txt"), h.path("目的地/改名.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("POST"))
        .and(path(format!("/me/drive/items/{ROOT}/children")))
        .respond_with(ResponseTemplate::new(201).set_body_json(folder("D9", ROOT, "目的地")))
        .mount(&h.server)
        .await;
    let moved = json!({
        "id": "F1", "name": "改名.txt", "size": 5,
        "cTag": "c1", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": "D9"},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-20T10:00:00Z"}
    });
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(moved))
        .mount(&h.server)
        .await;

    let mut events = h.engine.subscribe();
    let summary = h.sync().await;
    let mut failures = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let gonedrive_core::sync::engine::SyncEvent::Failed { name, error } = event {
            failures.push(format!("{name}: {error}"));
        }
    }
    assert_eq!(
        summary.failed, 0,
        "unexpected failures: {failures:?}\n{summary:?}"
    );
    assert_eq!(summary.deleted_remote, 0);

    // Same item id, new home — identity and history preserved.
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.name, "改名.txt");
    assert_eq!(record.parent_id.as_deref(), Some("D9"));
    assert_eq!(record.state, ItemState::Synced);
}

#[tokio::test]
async fn moving_and_editing_a_file_applies_metadata_then_uploads_the_bytes() {
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

    std::fs::rename(h.path("before.txt"), h.path("after.txt")).unwrap();
    std::fs::write(h.path("after.txt"), b"edited after move").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let moved = json!({
        "id": "F1", "name": "after.txt", "size": 5,
        "cTag": "c1", "eTag": "e-moved",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-20T10:00:00Z"}
    });
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .and(header("If-Match", "e-c1"))
        .and(body_partial_json(json!({"name": "after.txt"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(moved))
        .mount(&h.server)
        .await;

    let uploaded = json!({
        "id": "F1", "name": "after.txt", "size": 17,
        "cTag": "c2", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-29T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .and(header("If-Match", "e-moved"))
        .and(body_bytes(b"edited after move"))
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
    assert_eq!(h.read("after.txt"), "edited after move");
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.name, "after.txt");
    assert_eq!(record.ctag.as_deref(), Some("c2"));
    assert_eq!(record.local_size, Some(17));

    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    assert_eq!(
        h.sync().await.uploaded,
        0,
        "the edit must not be suppressed or loop"
    );
}

#[tokio::test]
async fn a_dirty_record_over_a_placeholder_stops_looping_at_repair() {
    // Freeing a file that was dirty at the time leaves the record saying
    // "locally modified" over bytes that are now holes. Left alone, every
    // pass queued an upload and the guard refused it — forever.
    use gonedrive_core::db::ItemState;

    let h = harness_with_a_downloaded_file(b"content").await;
    h.engine.free_space(Path::new("Docs")).unwrap();
    h.store.set_item_state("F1", ItemState::DirtyLocal).unwrap();

    assert_eq!(h.engine.repair_states().unwrap(), 1);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().state,
        ItemState::Dehydrated
    );

    // And even未经修复, the scanner itself no longer trusts the record over
    // the file's own marks.
    h.store.set_item_state("F1", ItemState::DirtyLocal).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn deleting_a_local_file_propagates_even_under_the_online_only_default() {
    // Found live: with the default download policy, every locally-synced item
    // resolves to "unpinned", and the exclusion guard read every local
    // deletion as "supposed to be absent" — nothing the user deleted was ever
    // removed from the cloud.
    let h = Harness::metadata_only().await;
    h.mock_delta(None, vec![root_item()], "v1").await;
    h.sync().await;

    std::fs::write(h.path("mine.txt"), b"created here").unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "F7", "mine.txt").await;
    h.sync().await;
    assert!(h.store.item("F7").unwrap().is_some());

    std::fs::remove_file(h.path("mine.txt")).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 1,
        "the deletion must reach the cloud"
    );
    assert!(h.store.item("F7").unwrap().is_none());
}

#[tokio::test]
async fn an_uploaded_file_is_not_recorded_as_pinned() {
    // The resurrection chain's root cause: uploads stamped the record Pinned,
    // so a later local deletion was read as "a pinned file went missing —
    // re-create it", and the folder came back on every delta pass. An upload
    // records what the user's policy says, which under the default is not
    // "always keep".
    use gonedrive_core::db::PinState;

    let h = Harness::metadata_only().await; // default online-only policy
    h.mock_delta(None, vec![root_item()], "v1").await;
    h.sync().await;

    std::fs::write(h.path("fresh.txt"), b"x").unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "NEW", "fresh.txt").await;
    h.sync().await;

    let record = h.store.item("NEW").unwrap().unwrap();
    assert_ne!(
        record.pin_state,
        PinState::Pinned,
        "an upload must not pin; that resurrects deletions"
    );
    assert!(!h.store.explicitly_excluded("NEW").unwrap());
}
