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
async fn on_demand_population_prefetches_one_directory_level() {
    use gonedrive_core::hydrate::{DirectoryPopulation, Placeholder, PlaceholderState};

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("A", ROOT, "A"),
            file("A-FILE", "A", "a.txt", "c1", 10, "2026-08-20T10:00:00Z"),
            folder("B", "A", "B"),
            file("B-FILE", "B", "b.txt", "c1", 20, "2026-08-20T10:00:00Z"),
            folder("C", "B", "C"),
            file("C-FILE", "C", "c.txt", "c1", 30, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);

    assert!(h.path("A").is_dir(), "the root's direct folder is published");
    assert_eq!(
        Placeholder::directory_population(&h.path("A")),
        DirectoryPopulation::Populated
    );
    assert!(
        h.engine.folder_needs_population(Path::new("A")).unwrap(),
        "an already-populated parent still needs the bounded lookahead"
    );
    assert!(h.path("A/a.txt").exists());
    assert_eq!(
        Placeholder::directory_population(&h.path("A/B")),
        DirectoryPopulation::Partial
    );
    assert!(!h.path("A/B/b.txt").exists());

    let first = h.engine.populate_folder(Path::new("A")).await.unwrap();
    assert!(!h.engine.folder_needs_population(Path::new("A")).unwrap());
    assert_eq!(first.files, 1, "the one-level B/b.txt preview");
    assert_eq!(
        Placeholder::directory_population(&h.path("A")),
        DirectoryPopulation::Populated
    );
    assert_eq!(
        Placeholder::state(&h.path("A/a.txt")),
        PlaceholderState::Dehydrated
    );
    assert_eq!(
        Placeholder::directory_population(&h.path("A/B")),
        DirectoryPopulation::Populated
    );
    assert!(
        h.path("A/B/b.txt").exists() && h.path("A/B/C").is_dir(),
        "opening A should publish B's direct children"
    );
    assert_eq!(
        Placeholder::directory_population(&h.path("A/B/C")),
        DirectoryPopulation::Partial
    );
    assert!(
        !h.path("A/B/C/c.txt").exists(),
        "the lookahead must stop before C's children"
    );

    let second = h.engine.populate_folder(Path::new("A/B")).await.unwrap();
    assert_eq!(second.files, 1);
    assert!(h.path("A/B/b.txt").exists());
    assert!(h.path("A/B/C").is_dir());
    assert!(
        h.path("A/B/C/c.txt").exists(),
        "opening B should advance the boundary by one level"
    );
}

#[tokio::test]
async fn a_slow_folder_snapshot_cannot_roll_back_a_newer_delta() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    let protector = Arc::new(TestPlaceholderProtector::default());
    h.engine.set_placeholder_protector(protector);
    h.mock_delta(None, vec![folder("A", ROOT, "A")], "v1")
        .await;
    assert_eq!(h.sync().await.failed, 0);

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/A/children"))
        .and(query_param("$top", "1000"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(300))
                .set_body_json(json!({
                    "value": [file(
                        "F", "A", "report.bin", "c1", 10,
                        "2026-08-20T10:00:00Z"
                    )]
                })),
        )
        .mount(&h.server)
        .await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F",
            "A",
            "report.bin",
            "c2",
            20,
            "2026-08-21T10:00:00Z",
        )],
        "v2",
    )
    .await;

    let (population, delta) = tokio::join!(
        h.engine.populate_folder(Path::new("A")),
        h.engine.sync_remote()
    );
    population.unwrap();
    delta.unwrap();

    let path = h.path("A/report.bin");
    assert_eq!(h.store.item("F").unwrap().unwrap().ctag.as_deref(), Some("c2"));
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::ctag(&path).as_deref(), Some("c2"));
    assert_eq!(Occupancy::read(&path).unwrap().apparent_size, 20);
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
    h.engine
        .populate_folder(Path::new("Docs"))
        .await
        .unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            folder("D9", "D1", "云端空目录"),
            file(
                "F9",
                "D1",
                "new.pdf",
                "c1",
                4096,
                "2026-08-21T10:00:00Z",
            ),
        ],
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
    assert!(
        h.path("Docs/云端空目录").is_dir(),
        "an empty remote folder must also appear in a browsable on-demand tree"
    );
}

#[tokio::test]
async fn enabling_on_demand_at_runtime_applies_to_later_delta_items() {
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    let h = Harness::metadata_only().await;
    h.mock_delta(None, vec![root_item(), folder("D1", ROOT, "Docs")], "v1")
        .await;
    assert_eq!(h.sync().await.failed, 0);

    // This is what the MaterializeTree command does in a running daemon. The
    // mode must affect later polling without requiring a process restart.
    h.engine.enable_on_demand();
    h.engine
        .populate_folder(Path::new(""))
        .await
        .unwrap();
    h.engine
        .populate_folder(Path::new("Docs"))
        .await
        .unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F9",
            "D1",
            "restored.pdf",
            "c1",
            4096,
            "2026-08-21T10:00:00Z",
        )],
        "v2",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);

    let path = h.path("Docs/restored.pdf");
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::item_id(&path).as_deref(), Some("F9"));
}

#[tokio::test]
async fn a_web_restore_rebuilds_the_local_on_demand_subtree() {
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

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
            folder("TODO", ROOT, "Todo"),
            folder("EXPENSE", "TODO", "2025 报销"),
            file(
                "F1",
                "EXPENSE",
                "receipt.pdf",
                "c1",
                4096,
                "2026-08-21T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);
    h.engine
        .populate_folder(Path::new("Todo"))
        .await
        .unwrap();
    h.engine
        .populate_folder(Path::new("Todo/2025 报销"))
        .await
        .unwrap();
    assert!(h.path("Todo/2025 报销/receipt.pdf").exists());

    // State immediately before this incident's web restore: the client's
    // successful remote delete already removed the database subtree, and the
    // old local path was absent because the user had moved its inode elsewhere.
    std::fs::remove_dir_all(h.path("Todo/2025 报销")).unwrap();
    h.store.delete_subtree("EXPENSE").unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![
            folder("EXPENSE", "TODO", "2025 报销"),
            file(
                "F1",
                "EXPENSE",
                "receipt.pdf",
                "c1",
                4096,
                "2026-08-21T10:00:00Z",
            ),
        ],
        "v2",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);

    let restored_folder = h.path("Todo/2025 报销");
    let restored = restored_folder.join("receipt.pdf");
    assert!(restored_folder.is_dir(), "the restored folder must be visible again");
    assert!(
        !restored.exists(),
        "a restored partial folder must not recursively publish descendants"
    );
    h.engine
        .populate_folder(Path::new("Todo/2025 报销"))
        .await
        .unwrap();
    assert!(restored.exists(), "opening the restored folder publishes its file");
    assert_eq!(Placeholder::state(&restored), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::item_id(&restored).as_deref(), Some("F1"));
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
    h.sync().await;
    h.engine
        .populate_folder(Path::new("Docs"))
        .await
        .unwrap();
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
async fn desktop_markers_are_neither_uploaded_nor_mistaken_for_deletions() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Media"),
            file(
                "F1",
                "D1",
                ".nomedia",
                "c1",
                5,
                "2026-08-20T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"known").await;
    h.sync().await;
    std::fs::write(h.path(".trackerignore"), b"local marker").unwrap();

    let report = h.engine.discover_local_changes().unwrap();
    assert!(report.dirty_paths.is_empty());
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);

    let summary = h
        .engine
        .sync_local_paths(&[h.root.clone(), h.path("Media")])
        .unwrap();

    assert_eq!(summary.still_queued, 0);
    assert!(h.store.item("F1").unwrap().is_some());
    assert_eq!(std::fs::read(h.path("Media/.nomedia")).unwrap(), b"known");
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
    let h = Harness::new().await;
    seed_two_synced_folders(&h);
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
async fn a_mass_delete_approval_survives_an_unrelated_scan() {
    // The user confirms the held deletions, but before the coordinator
    // re-scans them the watcher reports an unrelated write elsewhere. That
    // scan carries no deletions and must not spend the one-shot approval.
    let h = Harness::new().await;
    seed_two_synced_folders(&h);
    for folder in ["A", "B"] {
        for index in 0..6 {
            std::fs::remove_file(h.path(&format!("{folder}/{index}.txt"))).unwrap();
        }
    }
    let refused = h
        .engine
        .sync_local_paths(&[h.path("A"), h.path("B")])
        .unwrap();
    assert_eq!(refused.refused_deletions, 12);

    h.engine.approve_next_mass_delete();
    std::fs::create_dir_all(h.path("C")).unwrap();
    std::fs::write(h.path("C/new.txt"), b"unrelated").unwrap();
    let unrelated = h.engine.sync_local_paths(&[h.path("C")]).unwrap();
    assert_eq!(unrelated.refused_deletions, 0);

    let confirmed = h
        .engine
        .sync_local_paths(&[h.path("A"), h.path("B")])
        .unwrap();
    assert_eq!(
        confirmed.refused_deletions, 0,
        "the approval must still apply to the deletions it answered"
    );
    let deletes = h
        .store
        .pending_ops(ACCOUNT, 100)
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == gonedrive_core::db::OpKind::DeleteRemote)
        .count();
    assert_eq!(deletes, 12);
}

#[tokio::test]
async fn a_name_that_is_not_utf8_neither_uploads_nor_aborts_the_scan() {
    // OneDrive names are Unicode, so such a file can never sync. It used to
    // be worse than skipped: the lossy name pointed at a path that does not
    // exist, opening it failed, and the whole scan — every deletion it had
    // found included — was thrown away, on every scan, forever.
    use std::os::unix::ffi::OsStrExt;

    let h = Harness::new().await;
    seed_two_synced_folders(&h);
    std::fs::remove_file(h.path("A/0.txt")).unwrap();
    let bad = h
        .path("A")
        .join(std::ffi::OsStr::from_bytes(b"bad\xff.txt"));
    std::fs::write(&bad, b"x").unwrap();

    let summary = h.engine.sync_local_paths(&[h.path("A")]).unwrap();
    assert_eq!(summary.refused_deletions, 0);
    let ops = h.store.pending_ops(ACCOUNT, 100).unwrap();
    assert_eq!(ops.len(), 1, "only the deletion is queued");
    assert_eq!(ops[0].kind, gonedrive_core::db::OpKind::DeleteRemote);
}

/// Two synced folders of ten files each, plus the root: 23 records that all
/// exist on disk with matching inode and size metadata.
fn seed_two_synced_folders(h: &Harness) {
    use std::os::unix::fs::MetadataExt;

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

#[tokio::test]
async fn a_failed_sync_phase_still_emits_its_terminal_event() {
    let h = Harness::new().await;
    let mut events = h.engine.subscribe();
    h.store.remove_account(ACCOUNT).unwrap();

    assert!(h.engine.sync_local_paths(&[]).is_err());
    assert!(matches!(
        events.try_recv().unwrap(),
        gonedrive_core::sync::engine::SyncEvent::Started
    ));
    assert!(matches!(
        events.try_recv().unwrap(),
        gonedrive_core::sync::engine::SyncEvent::Finished(_)
    ));
}

#[tokio::test]
async fn a_file_replaced_by_a_directory_is_reported_not_retried_for_ever() {
    // The recorded identity is a file's. Uploading to it, or hanging the new
    // directory's children under it, is refused by the service on every
    // attempt — and the scan re-queued both on every pass.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "swap", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    assert_eq!(h.read("swap"), "hello");

    std::fs::remove_file(h.path("swap")).unwrap();
    std::fs::create_dir(h.path("swap")).unwrap();
    std::fs::write(h.path("swap/inside.txt"), b"new").unwrap();

    let summary = h
        .engine
        .sync_local_paths(std::slice::from_ref(&h.root))
        .unwrap();

    assert!(summary.skipped >= 1, "the swap must be reported");
    let ops = h.store.pending_ops(ACCOUNT, 100).unwrap();
    assert!(
        ops.is_empty(),
        "nothing may be queued against a mismatched identity: {ops:?}"
    );
    assert!(
        h.store.item("F1").unwrap().is_some(),
        "the remote file is untouched; only the local swap is refused"
    );
}
