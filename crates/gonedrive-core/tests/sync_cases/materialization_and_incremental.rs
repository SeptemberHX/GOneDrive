#[tokio::test]
async fn materialize_all_builds_a_browsable_placeholder_tree() {
    // The gap the user spotted: the full directory structure lived only in the
    // database, so the file manager showed only downloaded folders. Windows
    // presents the whole tree as placeholders. materialize_all does the same:
    // every folder a real directory, every file a sparse placeholder, browsable
    // with no network.
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "文档"),
            folder("D2", "D1", "深层"),
            // An empty folder — the case make_on_demand missed entirely.
            folder("D3", ROOT, "空文件夹"),
            file("F1", "D2", "报告.docx", "c1", 5000, "2026-08-20T10:00:00Z"),
            file("F2", "D1", "笔记.txt", "c1", 20, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await;

    // Nothing on disk yet under the online-only default.
    assert!(!h.path("文档").exists());

    let watched = h.engine.materialize_all(|_, _| true).unwrap();

    // The whole tree is now real, including the empty folder.
    assert!(h.path("文档/深层").is_dir());
    assert!(h.path("空文件夹").is_dir());

    // Files are sparse placeholders carrying their real size and marks.
    for (rel, size) in [("文档/深层/报告.docx", 5000u64), ("文档/笔记.txt", 20)] {
        let p = h.path(rel);
        assert!(p.exists());
        assert_eq!(Placeholder::state(&p), PlaceholderState::Dehydrated);
        assert_eq!(Occupancy::read(&p).unwrap().apparent_size, size);
    }
    assert_eq!(watched.len(), 2, "both files need intercepting");
    // (Folder timestamps are covered by downloaded_folders_keep_their_remote_
    // timestamps; the folder() helper here carries no fileSystemInfo.)

    // Idempotent: a second run adds nothing and disturbs nothing.
    let again = h.engine.materialize_all(|_, _| true).unwrap();
    assert_eq!(again.len(), 2);
    assert_eq!(std::fs::read(h.path("文档/笔记.txt")).unwrap().len(), 20);
}

#[tokio::test]
async fn on_demand_mode_places_new_files_from_delta() {
    // With the tree materialised, a file that appears later in a delta must
    // become a placeholder immediately, not wait for a full re-materialisation.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    let mut config = Config {
        client_id: "test".into(),
        on_demand: true,
        ..Config::default()
    };
    config.sync_root = std::path::PathBuf::new(); // set by harness
    let h = Harness::with_config(config).await;
    h.mock_delta(None, vec![root_item(), folder("D1", ROOT, "Docs")], "v1")
        .await;
    h.sync().await;

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F9",
            "D1",
            "new.pdf",
            "c1",
            4096,
            "2026-08-21T10:00:00Z",
        )],
        "v2",
    )
    .await;
    h.sync().await;

    let p = h.path("Docs/new.pdf");
    assert!(
        p.exists(),
        "a new file should appear as a placeholder at once"
    );
    assert_eq!(Placeholder::state(&p), PlaceholderState::Dehydrated);
}

#[tokio::test]
async fn a_remote_change_to_a_placeholder_refreshes_it_silently() {
    // In on-demand mode most files are placeholders. When one changes in the
    // cloud, the tree must update — new size, new timestamp — without a
    // download and without a conflict copy (its on-disk bytes are holes, not a
    // local edit). The new content comes on the next open.
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    let mut config = Config {
        client_id: "test".into(),
        on_demand: true,
        ..Config::default()
    };
    config.sync_root = std::path::PathBuf::new();
    let h = Harness::with_config(config).await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "report.bin", "c1", 1000, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await; // on-demand: report.bin becomes a placeholder
    let path = h.path("Docs/report.bin");
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(Occupancy::read(&path).unwrap().apparent_size, 1000);

    // The file changes in the cloud: new ctag, new size, new time.
    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            "D1",
            "report.bin",
            "c2",
            5000,
            "2026-08-25T12:00:00Z",
        )],
        "v2",
    )
    .await;
    // No download mock and no conflict-copy upload mock: either would fail.

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.conflicts, 0);
    assert_eq!(summary.failed, 0);

    // The placeholder now reflects the new remote metadata, still a placeholder.
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(Occupancy::read(&path).unwrap().apparent_size, 5000);
    assert_eq!(
        mtime_secs(&path),
        time::macros::datetime!(2026-08-25 12:00:00 UTC).unix_timestamp()
    );
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.size, 5000);
    assert_eq!(record.ctag.as_deref(), Some("c2"));

    // No conflict-copy litter left behind.
    let copies: Vec<_> = std::fs::read_dir(h.path("Docs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("冲突"))
        .collect();
    assert!(
        copies.is_empty(),
        "a placeholder change must not conflict-copy"
    );
}

#[tokio::test]
async fn a_folder_is_a_check_only_when_everything_in_it_is_local() {
    // The user's correction: a folder shows the check only if it was kept, or
    // if every file inside is downloaded — not merely because one file is.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "混合"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F2", "D1", "b.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"aaaaa").await;
    h.mock_download("F2", b"bbbbb").await;
    h.sync().await;

    // Materialise so the folder gets a mark, then free everything: cloud.
    h.engine.materialize_all(|_, _| true).unwrap();
    h.engine.free_space(Path::new("混合")).unwrap();
    assert_eq!(
        Placeholder::state(&h.path("混合")),
        PlaceholderState::Dehydrated,
        "an all-online folder is cloud"
    );

    // Hydrate just one of the two files — folder must STAY cloud, because the
    // other is still online. This is the exact case the old logic got wrong.
    h.server.reset().await;
    h.mock_download("F1", b"aaaaa").await;
    h.engine.hydrate(&h.path("混合/a.txt")).await.unwrap();
    assert_eq!(
        Placeholder::state(&h.path("混合")),
        PlaceholderState::Dehydrated,
        "one local file out of two must not make the folder a check"
    );

    // Hydrate the second — now everything is local, folder becomes a check.
    h.server.reset().await;
    h.mock_download("F2", b"bbbbb").await;
    h.engine.hydrate(&h.path("混合/b.txt")).await.unwrap();
    assert_eq!(
        Placeholder::state(&h.path("混合")),
        PlaceholderState::Hydrated,
        "all files local => folder is a check"
    );

    // Free one again — back to cloud.
    h.engine.free_space(Path::new("混合/a.txt")).unwrap();
    assert_eq!(
        Placeholder::state(&h.path("混合")),
        PlaceholderState::Dehydrated
    );
}

#[tokio::test]
async fn an_explicitly_pinned_folder_is_a_check_even_while_online() {
    // "Always keep on this device" shows the check immediately, before its
    // contents have finished downloading.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "留存"),
            file(
                "F1",
                "D1",
                "big.bin",
                "c1",
                9_000_000,
                "2026-08-20T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    h.sync().await;
    h.engine.materialize_all(|_, _| true).unwrap();
    assert_eq!(
        Placeholder::state(&h.path("留存")),
        PlaceholderState::Dehydrated
    );

    // Pin it (no download mock needed to observe the badge).
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    static BIG: &[u8] = &[0u8; 9_000_000];
    h.mock_download("F1", BIG).await;
    h.engine
        .set_pin(Path::new("留存"), PinState::Pinned)
        .unwrap();

    assert_eq!(
        Placeholder::state(&h.path("留存")),
        PlaceholderState::Hydrated,
        "a pinned folder is a check right away"
    );
}

#[tokio::test]
async fn read_only_discovery_reports_work_without_mutating_the_queue() {
    let h = Harness::new().await;
    std::fs::write(h.path("new.txt"), b"local").unwrap();

    let report = h.engine.discover_local_changes().unwrap();

    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
    assert!(report.dirty_paths.contains(&h.root));
    assert!(h
        .store
        .account(ACCOUNT)
        .unwrap()
        .unwrap()
        .root_dev
        .is_none());

    h.engine.sync_local_paths(&report.dirty_paths).unwrap();
    let queued = h.store.due_outbound_ops(ACCOUNT, 10).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].local_path.as_deref(),
        Some(h.path("new.txt").as_path())
    );
}

#[tokio::test]
async fn incremental_scan_does_not_walk_an_unrelated_known_sibling() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("DA", ROOT, "A"),
            file("FA", "DA", "a.txt", "ca", 1, "2026-08-20T10:00:00Z"),
            folder("DB", ROOT, "B"),
            file("FB", "DB", "b.txt", "cb", 1, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("FA", b"a").await;
    h.mock_download("FB", b"b").await;
    h.sync().await;

    std::fs::write(h.path("A/a.txt"), b"changed-a").unwrap();
    std::fs::write(h.path("B/b.txt"), b"changed-b").unwrap();
    h.engine.sync_local_paths(&[h.path("A/a.txt")]).unwrap();

    let queued = h.store.due_outbound_ops(ACCOUNT, 10).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].item_id.as_deref(), Some("FA"));
    assert_eq!(queued[0].precondition.as_deref(), Some("e-ca"));
}

#[tokio::test]
async fn scoped_delete_batches_trip_the_cumulative_mass_delete_guard() {
    use std::os::unix::fs::MetadataExt;

    let h = Harness::new().await;
    for (folder_id, folder_name, file_prefix) in [("DA", "A", "A"), ("DB", "B", "B")] {
        let directory = h.path(folder_name);
        std::fs::create_dir_all(&directory).unwrap();
        for index in 0..10 {
            let name = format!("{index}.txt");
            let path = directory.join(&name);
            std::fs::write(&path, b"x").unwrap();
            let metadata = std::fs::metadata(&path).unwrap();
            let mut item = ItemRecord::new(ACCOUNT, format!("{file_prefix}{index}"), name);
            item.parent_id = Some(folder_id.into());
            item.size = metadata.len();
            item.etag = Some(format!("e-{file_prefix}{index}"));
            item.ctag = Some(format!("c-{file_prefix}{index}"));
            item.local_mtime = Some(time::OffsetDateTime::from(metadata.modified().unwrap()));
            item.local_size = Some(metadata.len());
            item.local_ino = Some(metadata.ino());
            item.state = ItemState::Synced;
            h.store.upsert_item(&item).unwrap();
        }
        let metadata = std::fs::metadata(&directory).unwrap();
        let mut folder = ItemRecord::new(ACCOUNT, folder_id, folder_name);
        folder.parent_id = Some(ROOT.into());
        folder.is_folder = true;
        folder.local_mtime = Some(time::OffsetDateTime::from(metadata.modified().unwrap()));
        folder.local_size = Some(metadata.len());
        folder.local_ino = Some(metadata.ino());
        folder.state = ItemState::Synced;
        h.store.upsert_item(&folder).unwrap();
    }
    assert_eq!(h.store.item_count(ACCOUNT).unwrap(), 23);

    for index in 0..6 {
        std::fs::remove_file(h.path(&format!("A/{index}.txt"))).unwrap();
    }
    let first = h.engine.sync_local_paths(&[h.path("A")]).unwrap();
    assert_eq!(first.refused_deletions, 0);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 6);
    assert!(h.store.due_outbound_ops(ACCOUNT, 100).unwrap().is_empty());

    for index in 0..6 {
        std::fs::remove_file(h.path(&format!("B/{index}.txt"))).unwrap();
    }
    let mut events = h.engine.subscribe();
    let second = h.engine.sync_local_paths(&[h.path("B")]).unwrap();
    assert_eq!(second.refused_deletions, 12);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);

    let mut safety_stop = false;
    while let Ok(event) = events.try_recv() {
        if matches!(
            event,
            gonedrive_core::sync::engine::SyncEvent::SafetyStop { .. }
        ) {
            safety_stop = true;
        }
    }
    assert!(safety_stop, "the cumulative refusal must be surfaced");

    h.engine.approve_next_mass_delete();
    let confirmed = h
        .engine
        .sync_local_paths(&[h.path("A"), h.path("B")])
        .unwrap();
    assert_eq!(confirmed.refused_deletions, 0);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 12);
}

#[tokio::test]
async fn remote_enumeration_and_download_drain_are_separate_phases() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "remote.txt",
            "c1",
            6,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;

    let remote = h.engine.sync_remote().await.unwrap();
    assert_eq!(remote.enumerated, 1);
    assert!(!h.path("remote.txt").exists());

    std::fs::write(h.path("local.txt"), b"local").unwrap();
    h.engine
        .sync_local_paths(std::slice::from_ref(&h.root))
        .unwrap();
    h.mock_download("F1", b"remote").await;

    let summary = h
        .engine
        .transfer_slice(
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            QueueScope::Downloads,
        )
        .await
        .unwrap();
    assert_eq!(summary.downloaded, 1);
    assert_eq!(h.read("remote.txt"), "remote");
    let outbound = h.store.due_outbound_ops(ACCOUNT, 10).unwrap();
    assert_eq!(outbound.len(), 1);
    assert_eq!(
        outbound[0].local_path.as_deref(),
        Some(h.path("local.txt").as_path())
    );
}
