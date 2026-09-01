// Progressive first-enumeration, restart, deletion, and package scenarios.

#[tokio::test]
async fn first_login_can_browse_a_folder_while_the_first_incremental_delta_is_still_running() {
    use gonedrive_core::hydrate::{DirectoryPopulation, Placeholder, PlaceholderState};

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    let protector = Arc::new(TestPlaceholderProtector::default());
    h.engine.set_placeholder_protector(protector.clone());

    Mock::given(method("GET"))
        .and(path("/me/drive/items/ROOT/children"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [folder("A", ROOT, "A")]
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/A/children"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                file("A-FILE", "A", "ready.txt", "c1", 5, "2026-08-20T10:00:00Z"),
                folder("B", "A", "B")
            ]
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [],
            "@odata.deltaLink": format!(
                "{}/me/drive/root/delta?token=baseline",
                h.server.uri()
            )
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "baseline"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(1))
                .set_body_json(json!({
                    "value": [],
                    "@odata.deltaLink": format!(
                        "{}/me/drive/root/delta?token=v1",
                        h.server.uri()
                    )
                })),
        )
        .mount(&h.server)
        .await;

    let sync = h.engine.sync_remote();
    tokio::pin!(sync);
    let root_preview = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while !h.path("A").is_dir() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });
    tokio::select! {
        result = &mut sync => panic!("delta completed before its delayed response: {result:?}"),
        result = root_preview => result.expect("the root children preview did not appear promptly"),
    }
    assert_eq!(
        h.store.account(ACCOUNT).unwrap().unwrap().delta_link,
        Some(format!(
            "{}/me/drive/root/delta?token=baseline",
            h.server.uri()
        ))
    );
    let account = h.store.account(ACCOUNT).unwrap().unwrap();
    assert_eq!(
        gonedrive_core::sync::population_watch_directories_for_account(&h.store, &account)
            .unwrap(),
        vec![h.root.clone()],
        "the root is one population boundary regardless of its child count"
    );

    let populated = h.engine.populate_folder(Path::new("A")).await.unwrap();
    assert_eq!(populated.files, 1);
    assert_eq!(
        Placeholder::state(&h.path("A/ready.txt")),
        PlaceholderState::Dehydrated
    );
    assert_eq!(
        Placeholder::directory_population(&h.path("A/B")),
        DirectoryPopulation::Partial
    );
    assert!(!h.path("A/B/deep.txt").exists());
    assert_eq!(
        gonedrive_core::sync::population_watch_directories_for_account(&h.store, &account)
            .unwrap(),
        vec![h.root.clone(), h.path("A")],
        "only folders actually browsed become new population boundaries"
    );

    assert_eq!(sync.await.unwrap().enumerated, 0);
    assert!(protector.calls.load(Ordering::Acquire) > 0);
}

#[tokio::test]
async fn a_failed_root_listing_does_not_stop_delta_and_retries_next_poll() {
    use gonedrive_core::hydrate::{DirectoryPopulation, Placeholder};

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [],
            "@odata.deltaLink": format!(
                "{}/me/drive/root/delta?token=baseline",
                h.server.uri()
            )
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/ROOT/children"))
        .respond_with(ResponseTemplate::new(400).set_body_string("temporary listing failure"))
        .mount(&h.server)
        .await;
    h.mock_delta(Some(("token", "baseline")), vec![], "v1")
        .await;

    let first = h.engine.sync_remote().await.unwrap();
    assert_eq!(first.failed, 1);
    assert_eq!(
        h.store.folder_population(ACCOUNT, ROOT).unwrap(),
        Some(false)
    );
    assert!(!h.path("A").exists());

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/ROOT/children"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [folder("A", ROOT, "A")]
        })))
        .mount(&h.server)
        .await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2")
        .await;

    let second = h.engine.sync_remote().await.unwrap();
    assert_eq!(second.failed, 0);
    assert!(h.path("A").is_dir());
    assert_eq!(
        Placeholder::directory_population(&h.root),
        DirectoryPopulation::Populated
    );
}

#[tokio::test]
async fn a_remote_folder_collision_preserves_the_local_file_without_downloading_a_folder() {
    use gonedrive_core::hydrate::{DirectoryPopulation, Placeholder};

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    h.mock_delta(None, vec![], "v1").await;
    assert_eq!(h.engine.sync_remote().await.unwrap().failed, 0);
    std::fs::write(h.path("A"), b"local bytes must survive").unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![folder("REMOTE-A", ROOT, "A")],
        "v2",
    )
    .await;
    let conflicted = h.engine.sync_remote().await.unwrap();

    assert_eq!(conflicted.failed, 0);
    assert_eq!(conflicted.conflicts, 1);
    assert!(h.path("A").is_dir());
    assert_eq!(
        Placeholder::directory_population(&h.path("A")),
        DirectoryPopulation::Partial
    );
    let copies: Vec<_> = std::fs::read_dir(&h.root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains("的冲突副本"))
        .collect();
    assert_eq!(copies.len(), 1);
    assert_eq!(
        std::fs::read(copies[0].path()).unwrap(),
        b"local bytes must survive"
    );
    assert!(h.store.deferred_remote_ids(ACCOUNT).unwrap().is_empty());
    let pending = h.store.pending_ops(ACCOUNT, 10).unwrap();
    assert_eq!(pending.len(), 1, "only the preserved local file is uploaded");
    assert_eq!(pending[0].kind, OpKind::Upload);
    assert!(
        pending.iter().all(|op| op.kind != OpKind::Download),
        "a folder must never enter the content download queue"
    );
    assert!(h.store.delta_batch(ACCOUNT).unwrap().is_none());
    assert!(h
        .store
        .account(ACCOUNT)
        .unwrap()
        .unwrap()
        .delta_link
        .unwrap()
        .contains("token=v2"));
}

#[tokio::test]
async fn an_unrepresentable_remote_target_is_deferred_without_stopping_delta() {
    use std::os::unix::fs::symlink;

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    h.mock_delta(None, vec![], "v1").await;
    assert_eq!(h.engine.sync_remote().await.unwrap().failed, 0);
    let outside = h._tmp.path().join("outside.txt");
    std::fs::write(&outside, b"outside bytes").unwrap();
    symlink(&outside, h.path("A")).unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![folder("REMOTE-A", ROOT, "A")],
        "v2",
    )
    .await;
    let deferred = h.engine.sync_remote().await.unwrap_or_else(|error| {
        panic!(
            "sync failed after collision: {error}; batch={:?}; deferred={:?}; item={:?}",
            h.store.delta_batch(ACCOUNT).unwrap(),
            h.store.deferred_remote_ids(ACCOUNT).unwrap(),
            h.store.item("REMOTE-A").unwrap()
        )
    });

    assert_eq!(deferred.failed, 1);
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside bytes");
    assert!(std::fs::symlink_metadata(h.path("A")).unwrap().is_symlink());
    assert!(h
        .store
        .deferred_remote_ids(ACCOUNT)
        .unwrap()
        .contains("REMOTE-A"));
    assert!(h.store.delta_batch(ACCOUNT).unwrap().is_none());
    assert!(h
        .store
        .account(ACCOUNT)
        .unwrap()
        .unwrap()
        .delta_link
        .unwrap()
        .contains("token=v2"));

    std::fs::remove_file(h.path("A")).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3")
        .await;
    assert_eq!(h.engine.sync_remote().await.unwrap().failed, 0);

    assert!(h.store.deferred_remote_ids(ACCOUNT).unwrap().is_empty());
    assert!(h.path("A").is_dir());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside bytes");
}

#[tokio::test]
async fn a_deferred_remote_target_freezes_the_same_local_path() {
    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    })
    .await;
    h.mock_delta(None, vec![], "v1").await;
    h.engine.sync_remote().await.unwrap();
    let path = h.path("blocked.txt");
    std::fs::write(&path, b"local data").unwrap();
    let remote = file(
        "REMOTE-BLOCKED",
        ROOT,
        "blocked.txt",
        "c1",
        11,
        "2026-08-20T10:00:00Z",
    );
    h.store
        .defer_remote_change(
            ACCOUNT,
            "REMOTE-BLOCKED",
            Some(ROOT),
            &serde_json::to_string(&remote).unwrap(),
        )
        .unwrap();

    let frozen = h
        .engine
        .sync_local_paths(std::slice::from_ref(&path))
        .unwrap();
    assert_eq!(frozen.still_queued, 0);
    assert_eq!(std::fs::read(&path).unwrap(), b"local data");

    h.store
        .remove_deferred_remote(ACCOUNT, "REMOTE-BLOCKED")
        .unwrap();
    let resumed = h
        .engine
        .sync_local_paths(std::slice::from_ref(&path))
        .unwrap();
    assert_eq!(resumed.still_queued, 1);
    assert_eq!(h.store.pending_ops(ACCOUNT, 10).unwrap()[0].kind, OpKind::Upload);
}

#[tokio::test]
async fn a_first_sync_makes_the_first_page_visible_before_the_last_page_arrives() {
    let mut config = Config {
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    };
    config.client_id = "test".into();
    let h = Harness::with_config(config).await;
    let protector = Arc::new(TestPlaceholderProtector::default());
    h.engine.set_placeholder_protector(protector.clone());
    let generation = h.store.begin_resync(ACCOUNT).unwrap();
    h.store
        .begin_delta_batch(ACCOUNT, "initial", Some(generation))
        .unwrap();

    let next = format!("{}/me/drive/root/delta?page=slow", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                folder("D1", ROOT, "Visible"),
                file("F1", ROOT, "now.txt", "c1", 4, "2026-08-20T10:00:00Z"),
                file("F2", "D1", "not-yet.txt", "c1", 4, "2026-08-20T10:00:00Z")
            ],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(1))
                .set_body_json(json!({
                    "value": [],
                    "@odata.deltaLink": format!(
                        "{}/me/drive/root/delta?token=v1",
                        h.server.uri()
                    )
                })),
        )
        .mount(&h.server)
        .await;

    let sync = h.engine.sync_remote();
    tokio::pin!(sync);
    let visible = async {
        loop {
            if h.path("now.txt").exists() && protector.calls.load(Ordering::Acquire) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        result = &mut sync => panic!("initial sync finished before its delayed last page: {result:?}"),
        result = tokio::time::timeout(std::time::Duration::from_secs(2), visible) => {
            result.expect("the first page never became visible");
        }
    }

    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_none());
    assert_ne!(
        std::fs::metadata(h.path("now.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0,
        "the protector must restore access only after registering the inode"
    );
    assert!(h.path("Visible").is_dir());
    assert!(
        !h.path("Visible/not-yet.txt").exists(),
        "a partial child directory must not publish its own children"
    );
    let summary = sync.await.unwrap();
    assert_eq!(summary.enumerated, 3);
    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_some());
}

#[tokio::test]
async fn an_interrupted_progressive_first_sync_resumes_without_reapplying_old_pages() {
    let mut config = Config {
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    };
    config.client_id = "test".into();
    let h = Harness::with_config(config).await;
    let first_protector = Arc::new(TestPlaceholderProtector::default());
    h.engine
        .set_placeholder_protector(first_protector.clone());
    let generation = h.store.begin_resync(ACCOUNT).unwrap();
    h.store
        .begin_delta_batch(ACCOUNT, "initial", Some(generation))
        .unwrap();

    let next = format!("{}/me/drive/root/delta?page=resume", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file(
                "F1", ROOT, "survives.txt", "c1", 4, "2026-08-20T10:00:00Z"
            )],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "resume"))
        .respond_with(ResponseTemplate::new(400).set_body_string("interrupted"))
        .mount(&h.server)
        .await;

    assert!(h.engine.sync_remote().await.is_err());
    assert!(h.path("survives.txt").exists());
    assert_eq!(first_protector.calls.load(Ordering::Acquire), 1);
    let batch = h.store.delta_batch(ACCOUNT).unwrap().unwrap();
    assert_eq!(batch.page_count, 1);
    assert!(!batch.complete);
    assert!(h.store.unapplied_delta_upserts(ACCOUNT).unwrap().is_empty());

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "resume"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [],
            "@odata.deltaLink": format!(
                "{}/me/drive/root/delta?token=v1",
                h.server.uri()
            )
        })))
        .mount(&h.server)
        .await;
    let graph = Arc::new(
        GraphClient::new(reqwest::Client::new(), TokenSource::Static("token".into()))
            .with_base_url(h.server.uri()),
    );
    let resumed = Engine::new(
        h.store.clone(),
        graph,
        h.config.clone(),
        ACCOUNT.into(),
    );
    let resumed_protector = Arc::new(TestPlaceholderProtector::default());
    resumed.set_placeholder_protector(resumed_protector.clone());

    let summary = resumed.sync_remote().await.unwrap();
    assert_eq!(summary.enumerated, 1);
    assert_eq!(resumed_protector.calls.load(Ordering::Acquire), 0);
    assert!(h.path("survives.txt").exists());
    assert!(h.store.delta_batch(ACCOUNT).unwrap().is_none());
    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_some());
}

#[tokio::test]
async fn one_unrepresentable_remote_name_does_not_stop_the_rest_of_first_sync() {
    let mut config = Config {
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    };
    config.client_id = "test".into();
    let h = Harness::with_config(config).await;
    let generation = h.store.begin_resync(ACCOUNT).unwrap();
    h.store
        .begin_delta_batch(ACCOUNT, "initial", Some(generation))
        .unwrap();

    // OneDrive limits characters, while ext4 limits UTF-8 bytes. This name is
    // legal in the former sense but cannot be one 300-byte Linux dirent.
    let unrepresentable = "文".repeat(100);
    let next = format!("{}/me/drive/root/delta?page=after-error", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file(
                "TOO-LONG", ROOT, &unrepresentable, "c1", 4,
                "2026-08-20T10:00:00Z"
            )],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "after-error"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file(
                "GOOD", ROOT, "visible-after-error.txt", "c2", 5,
                "2026-08-20T10:00:00Z"
            )],
            "@odata.deltaLink": format!(
                "{}/me/drive/root/delta?token=v1",
                h.server.uri()
            )
        })))
        .mount(&h.server)
        .await;

    let summary = h.engine.sync_remote().await.unwrap();

    assert_eq!(summary.enumerated, 2);
    assert_eq!(summary.failed, 1);
    assert!(h.path("visible-after-error.txt").exists());
    let failed = h.store.item("TOO-LONG").unwrap().unwrap();
    assert_eq!(failed.state, ItemState::Error);
    assert!(failed.local_ino.is_none());
    assert!(h.store.delta_batch(ACCOUNT).unwrap().is_none());
    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_some());
}

#[tokio::test]
async fn a_representable_long_name_still_publishes_through_the_locked_staging_file() {
    let mut config = Config {
        download_new: DownloadPolicy::Never,
        on_demand: true,
        ..Config::default()
    };
    config.client_id = "test".into();
    let h = Harness::with_config(config).await;
    let protector = Arc::new(TestPlaceholderProtector::default());
    h.engine.set_placeholder_protector(protector.clone());

    // The destination fits NAME_MAX. The old implementation appended another
    // long internal suffix and failed before the no-replace publication.
    let name = format!("{}.pdf", "x".repeat(240));
    h.mock_delta(
        None,
        vec![file(
            "LONG-BUT-VALID",
            ROOT,
            &name,
            "c1",
            4,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;

    let summary = h.engine.sync_remote().await.unwrap();

    assert_eq!(summary.failed, 0);
    assert!(h.path(&name).exists());
    assert_eq!(protector.calls.load(Ordering::Acquire), 1);
    assert_eq!(
        h.store.item("LONG-BUT-VALID").unwrap().unwrap().state,
        ItemState::Dehydrated
    );
}

#[tokio::test]
async fn a_partial_initial_enumeration_never_applies_a_remote_delete() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "keep.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "old",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    h.store.set_delta_link(ACCOUNT, None).unwrap();
    h.server.reset().await;

    let next = format!("{}/me/drive/root/delta?page=slow-delete", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [tombstone("F1", ROOT)],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "slow-delete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(500))
                .set_body_json(json!({
                    "value": [],
                    "@odata.deltaLink": format!(
                        "{}/me/drive/root/delta?token=new",
                        h.server.uri()
                    )
                })),
        )
        .mount(&h.server)
        .await;

    let sync = h.engine.sync_remote();
    tokio::pin!(sync);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(h.path("keep.txt").exists());
    assert!(h.store.item("F1").unwrap().is_some());
    sync.await.unwrap();
    assert!(!h.path("keep.txt").exists());
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn onenote_package_descendants_are_persistently_excluded() {
    let h = Harness::metadata_only().await;
    let package = json!({
        "id": "P1", "name": "InfoQ.one", "package": {"type": "oneNote"},
        "parentReference": {"driveId": "drive1", "id": ROOT}
    });
    h.mock_delta(
        None,
        vec![
            file("PF1", "PD1", "section.one", "c1", 10, "2026-08-20T10:00:00Z"),
            folder("PD1", "P1", "internal"),
            // Graph does not promise parent-first page ordering. This is the
            // shape that used to fail with "unknown parent" after the package
            // itself was deliberately skipped.
            package,
        ],
        "v1",
    )
    .await;

    let summary = h.engine.sync_remote().await.unwrap();
    assert_eq!(summary.skipped, 1);
    assert!(h.store.remote_is_excluded(ACCOUNT, "P1").unwrap());
    assert!(h.store.remote_is_excluded(ACCOUNT, "PD1").unwrap());
    assert!(h.store.remote_is_excluded(ACCOUNT, "PF1").unwrap());
    assert!(!h.path("InfoQ.one").exists());

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "PF2",
            "PD1",
            "new-section.one",
            "c2",
            12,
            "2026-08-21T10:00:00Z",
        )],
        "v2",
    )
    .await;
    h.engine.sync_remote().await.unwrap();
    assert!(h.store.remote_is_excluded(ACCOUNT, "PF2").unwrap());
    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_some());
}

#[tokio::test]
async fn an_unknown_parent_is_deferred_without_stopping_the_delta_cursor() {
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![file(
            "ORPHAN",
            "MISSING",
            "important.txt",
            "c1",
            9,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;

    let summary = h.engine.sync_remote().await.unwrap();
    assert_eq!(summary.failed, 1);
    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_some());
    assert!(h.store.delta_batch(ACCOUNT).unwrap().is_none());
    assert!(h.store.item("ORPHAN").unwrap().is_none());
    assert!(h
        .store
        .deferred_remote_ids(ACCOUNT)
        .unwrap()
        .contains("ORPHAN"));

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![folder("MISSING", ROOT, "Recovered")],
        "v2",
    )
    .await;
    assert_eq!(h.engine.sync_remote().await.unwrap().failed, 0);
    let recovered = h.store.item("ORPHAN").unwrap().unwrap();
    assert_eq!(recovered.parent_id.as_deref(), Some("MISSING"));
    assert!(h.store.deferred_remote_ids(ACCOUNT).unwrap().is_empty());
}

#[tokio::test]
async fn a_known_local_item_is_frozen_until_its_deferred_remote_move_can_replay() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("DOCS", ROOT, "Docs"),
            file("F1", "DOCS", "note.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    assert_eq!(h.sync().await.failed, 0);

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            "MISSING",
            "note.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v2",
    )
    .await;
    let deferred = h.sync().await;
    assert_eq!(deferred.failed, 1);
    assert_eq!(h.read("Docs/note.txt"), "hello");
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v2")),
        vec![folder("MISSING", ROOT, "Archive")],
        "v3",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);
    assert!(!h.path("Docs/note.txt").exists());
    assert_eq!(h.read("Archive/note.txt"), "hello");
    assert!(h.store.deferred_remote_ids(ACCOUNT).unwrap().is_empty());
}
