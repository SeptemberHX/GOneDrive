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
        if let gonedrive_core::sync::engine::SyncEvent::Failed { name, error, .. } = event {
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
async fn a_folder_move_is_one_patch_even_when_watcher_halves_are_split() {
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};
    use std::time::{Duration, Instant};

    // This is the live failure shape: an online-only subtree was renamed from
    // Todo into Done. The source-side watcher batch first queued a delete, then
    // the destination-side batch mistook every directory and placeholder for
    // new content. The result was an empty directory tree followed by deletion
    // of the original remote subtree.
    let h = Harness::new().await;
    let mut expense = folder("EXPENSE", "TODO", "2025 报销");
    expense["eTag"] = json!("e-expense");
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("TODO", ROOT, "Todo"),
            folder("DONE", ROOT, "Done"),
            folder("YEAR", "DONE", "2025"),
            expense,
            folder("NESTED", "EXPENSE", "票据"),
            file(
                "LOCAL",
                "NESTED",
                "已下载.txt",
                "c-local",
                5,
                "2026-08-20T10:00:00Z",
            ),
            file(
                "ONLINE",
                "NESTED",
                "仅联机.pdf",
                "c-online",
                6,
                "2026-08-20T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    h.mock_download("LOCAL", b"local").await;
    h.mock_download("ONLINE", b"online").await;
    assert_eq!(h.sync().await.failed, 0);

    let old_root = h.path("Todo/2025 报销");
    let new_root = h.path("Done/2025/2025 报销");
    let old_online = old_root.join("票据/仅联机.pdf");
    let new_online = new_root.join("票据/仅联机.pdf");
    h.engine
        .free_space(Path::new("Todo/2025 报销/票据/仅联机.pdf"))
        .unwrap();
    assert_eq!(Placeholder::state(&old_online), PlaceholderState::Dehydrated);

    // Existing download work must follow the directory instead of retaining a
    // stale absolute path after the metadata move.
    h.store
        .enqueue(
            ACCOUNT,
            OpKind::Download,
            Some("ONLINE"),
            Some(&old_online),
            None,
        )
        .unwrap();
    std::fs::rename(&old_root, &new_root).unwrap();

    // inotify may deliver these in separate debounce windows. Source first is
    // the dangerous order because it has already recorded a delayed delete.
    h.engine
        .sync_local_paths(&[h.path("Todo")])
        .expect("source watcher batch");
    // A later source-side watcher batch may independently have classified a
    // descendant as gone. Recognising the moved directory must withdraw every
    // destructive operation in its subtree, not only the root delete.
    h.store
        .enqueue_conditional(
            ACCOUNT,
            OpKind::DeleteRemote,
            "ONLINE",
            None,
            None,
            Some("c-online"),
        )
        .unwrap();
    h.engine
        .sync_local_paths(&[h.path("Done/2025")])
        .expect("destination watcher batch");

    let refreshed_marks = h.engine.take_placeholder_marks();
    assert_eq!(
        refreshed_marks,
        vec![new_online.clone()],
        "the moved online-only subtree must refresh HSM without a whole-tree scan"
    );

    let queued = h.store.due_ops(ACCOUNT, 20).unwrap();
    assert_eq!(
        h.store.pending_count(ACCOUNT).unwrap(),
        queued.len() as u64,
        "no delayed source-side delete may survive the move transaction"
    );
    assert_eq!(
        queued
            .iter()
            .filter(|op| op.kind == OpKind::MoveRemote)
            .count(),
        1,
        "the folder identity must become one metadata move"
    );
    assert!(
        queued.iter().all(|op| op.kind != OpKind::DeleteRemote),
        "recognising the destination must withdraw the source-side delete"
    );
    assert!(
        queued
            .iter()
            .all(|op| op.kind != OpKind::CreateRemoteFolder && op.kind != OpKind::Upload),
        "a folder move must not recreate or re-upload its descendants"
    );
    assert_eq!(
        queued
            .iter()
            .find(|op| op.kind == OpKind::Download)
            .and_then(|op| op.local_path.as_deref()),
        Some(new_online.as_path()),
        "pre-existing descendant work must be rebased atomically"
    );

    h.server.reset().await;
    let moved = json!({
        "id": "EXPENSE", "name": "2025 报销", "eTag": "e-moved",
        "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": "YEAR"}
    });
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/EXPENSE"))
        .and(header("If-Match", "e-expense"))
        .and(body_partial_json(json!({
            "name": "2025 报销",
            "parentReference": {"id": "YEAR"}
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(moved))
        .expect(1)
        .mount(&h.server)
        .await;

    let summary = h
        .engine
        .transfer_slice(
            Instant::now() + Duration::from_secs(5),
            gonedrive_core::sync::engine::QueueScope::Outbound,
        )
        .await
        .unwrap();
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.uploaded, 1);
    assert_eq!(summary.deleted_remote, 0);
    assert_eq!(
        h.store.item("EXPENSE").unwrap().unwrap().parent_id.as_deref(),
        Some("YEAR")
    );
    assert_eq!(
        h.store.item("ONLINE").unwrap().unwrap().parent_id.as_deref(),
        Some("NESTED"),
        "descendant identities must survive the folder move"
    );
    assert_eq!(Placeholder::state(&new_online), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::item_id(&new_online).as_deref(), Some("ONLINE"));
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

#[tokio::test]
async fn a_rename_chain_in_one_batch_lands_every_file_under_its_own_name() {
    // a→b and b→c arrive together. Applying a→b finds b still occupied, and
    // the occupant is not a conflict: it is leaving. Preserving it as a
    // conflict copy duplicated content the user never touched and then put
    // a's bytes under b's name.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            file("A", ROOT, "a.txt", "c1", 3, "2026-08-20T10:00:00Z"),
            file("B", ROOT, "b.txt", "c2", 3, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("A", b"aaa").await;
    h.mock_download("B", b"bbb").await;
    h.sync().await;
    assert_eq!(h.read("a.txt"), "aaa");
    assert_eq!(h.read("b.txt"), "bbb");

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            file("A", ROOT, "b.txt", "c1", 3, "2026-08-20T10:00:00Z"),
            file("B", ROOT, "c.txt", "c2", 3, "2026-08-20T10:00:00Z"),
        ],
        "v2",
    )
    .await;
    let summary = h.sync().await;

    assert_eq!(summary.conflicts, 0, "a rearrangement is not a conflict");
    assert_eq!(h.read("b.txt"), "aaa");
    assert_eq!(h.read("c.txt"), "bbb");
    assert!(!h.path("a.txt").exists());
    assert_no_strays(&h, &["b.txt", "c.txt"]);
    assert!(h.store.due_outbound_ops(ACCOUNT, 10).unwrap().is_empty());
}

#[tokio::test]
async fn two_files_swapping_names_in_one_batch_keep_their_own_contents() {
    // The hardest shape: neither can move until the other has. Without a way
    // to step aside, one of them became a conflict copy and the other
    // inherited the wrong bytes.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            file("X", ROOT, "x.txt", "c1", 3, "2026-08-20T10:00:00Z"),
            file("Y", ROOT, "y.txt", "c2", 3, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("X", b"xxx").await;
    h.mock_download("Y", b"yyy").await;
    h.sync().await;

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            file("X", ROOT, "y.txt", "c1", 3, "2026-08-20T10:00:00Z"),
            file("Y", ROOT, "x.txt", "c2", 3, "2026-08-20T10:00:00Z"),
        ],
        "v2",
    )
    .await;
    let summary = h.sync().await;

    assert_eq!(summary.conflicts, 0);
    assert_eq!(h.read("y.txt"), "xxx", "X's bytes follow X to its new name");
    assert_eq!(h.read("x.txt"), "yyy", "Y's bytes follow Y to its new name");
    assert_no_strays(&h, &["x.txt", "y.txt"]);
    assert!(h.store.due_outbound_ops(ACCOUNT, 10).unwrap().is_empty());
}

/// No conflict copies, and no file left parked under a reserved name.
fn assert_no_strays(h: &Harness, expected: &[&str]) {
    let strays: Vec<_> = std::fs::read_dir(&h.root)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !expected.contains(&name.as_str()))
        .collect();
    assert!(strays.is_empty(), "unexpected leftovers: {strays:?}");
}

#[tokio::test]
async fn a_file_parked_for_a_rearrangement_is_not_mistaken_for_a_deletion() {
    // Parking renames the inode and its record together. If a local scan runs
    // before the batch moves it on — the daemon restarted, the batch deferred
    // — the two must still agree: the record is not "missing from disk" (which
    // would delete the real file in the cloud) and the file is not "new"
    // (which would upload a duplicate).
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("X", ROOT, "x.txt", "c1", 3, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("X", b"xxx").await;
    h.sync().await;

    // Exactly what park_rearranged_occupant leaves behind.
    let parked_name = ".gonedrive-moving-X";
    std::fs::rename(h.path("x.txt"), h.path(parked_name)).unwrap();
    h.store.rename_item_local("X", parked_name).unwrap();

    let summary = h
        .engine
        .sync_local_paths(std::slice::from_ref(&h.root))
        .unwrap();

    assert_eq!(summary.failed, 0);
    let queued = h.store.pending_ops(ACCOUNT, 10).unwrap();
    assert!(
        queued.is_empty(),
        "a parked file is neither a deletion nor an addition: {queued:?}"
    );
    assert_eq!(h.read(parked_name), "xxx", "its bytes are untouched");
    assert_eq!(
        h.store.item("X").unwrap().unwrap().name,
        parked_name,
        "the record still follows the inode"
    );
}

#[tokio::test]
async fn a_folder_renamed_onto_a_deleted_folders_name_does_not_collide_with_it() {
    // The pass that promotes displaced tombstones only promotes files, so the
    // doomed folder is still in place when the renamed one arrives. Treating
    // it as a conflict filed a copy of a directory the user never touched and
    // then let the tombstone remove whichever directory ended up at the name.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("Y", ROOT, "docs"),
            file("YF", "Y", "old.txt", "c1", 3, "2026-08-20T10:00:00Z"),
            folder("X", ROOT, "archive"),
            file("XF", "X", "keep.txt", "c2", 4, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("YF", b"old").await;
    h.mock_download("XF", b"keep").await;
    h.sync().await;
    assert_eq!(h.read("docs/old.txt"), "old");
    assert_eq!(h.read("archive/keep.txt"), "keep");

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            folder("X", ROOT, "docs"),
            tombstone("YF", "Y"),
            tombstone("Y", ROOT),
        ],
        "v2",
    )
    .await;
    let summary = h.sync().await;

    assert_eq!(summary.conflicts, 0, "a rearrangement is not a conflict");
    assert_eq!(
        h.read("docs/keep.txt"),
        "keep",
        "the renamed folder keeps its own contents under the freed name"
    );
    assert!(!h.path("archive").exists(), "its old name is gone");
    assert!(h.store.item("Y").unwrap().is_none());
    assert!(h.store.item("YF").unwrap().is_none());
    assert!(h.store.due_outbound_ops(ACCOUNT, 10).unwrap().is_empty());
}
