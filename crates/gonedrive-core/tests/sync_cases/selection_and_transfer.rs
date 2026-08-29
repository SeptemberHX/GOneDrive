#[tokio::test]
async fn a_first_sync_downloads_nothing_by_default() {
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Documents"),
            file("F1", "D1", "report.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            file(
                "F2",
                ROOT,
                "big.bin",
                "c1",
                9_000_000_000,
                "2026-08-20T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    // Deliberately no download mock: any attempt to fetch would 404 and show up
    // as a failure in the summary.

    let summary = h.sync().await;

    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.still_queued, 0, "nothing should be queued either");

    // Nothing at all is materialised for a drive the user has not chosen from —
    // not the files, and not the folders that would hold them.
    assert!(!h.path("Documents").exists());
    assert!(!h.path("big.bin").exists());

    // The metadata is all there, which is what the folder picker browses.
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.state, ItemState::Dehydrated);
    assert_eq!(record.size, 5);
    assert_eq!(record.pin_state, PinState::Inherit);
    assert_eq!(h.store.item("D1").unwrap().unwrap().name, "Documents");

    // And the metadata is all there.
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.state, ItemState::Dehydrated);
    assert_eq!(record.size, 5);
    assert_eq!(record.pin_state, PinState::Inherit);
}

#[tokio::test]
async fn pinning_a_folder_downloads_only_that_folder() {
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Wanted"),
            file("F1", "D1", "yes.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            folder("D2", ROOT, "Untouched"),
            file("F2", "D2", "no.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await;
    assert!(!h.path("Wanted/yes.txt").exists());

    let outcome = h
        .engine
        .set_pin(Path::new("Wanted"), PinState::Pinned)
        .unwrap();
    assert_eq!(outcome.files, 1);
    assert_eq!(outcome.bytes, 5);

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h.mock_download("F1", b"hello").await;
    // No mock for F2: fetching it would fail the test.

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(h.read("Wanted/yes.txt"), "hello");
    assert!(!h.path("Untouched/no.txt").exists());
}

#[tokio::test]
async fn new_files_inside_a_pinned_folder_are_downloaded_automatically() {
    // Pinning is a standing choice, not a one-off: something added to a pinned
    // folder later has to arrive without the user pinning it again.
    let h = Harness::metadata_only().await;
    h.mock_delta(None, vec![folder("D1", ROOT, "Wanted")], "v1")
        .await;
    h.sync().await;
    h.engine
        .set_pin(Path::new("Wanted"), PinState::Pinned)
        .unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F9",
            "D1",
            "later.txt",
            "c1",
            5,
            "2026-08-21T10:00:00Z",
        )],
        "v2",
    )
    .await;
    h.mock_download("F9", b"later").await;

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 1);
    assert_eq!(h.read("Wanted/later.txt"), "later");
}

#[tokio::test]
async fn an_unpinned_subfolder_is_excluded_from_its_pinned_parent() {
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Parent"),
            file("F1", "D1", "keep.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            folder("D2", "D1", "Huge"),
            file("F2", "D2", "skip.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await;

    h.engine
        .set_pin(Path::new("Parent"), PinState::Pinned)
        .unwrap();
    // The nearer, explicit choice must beat the inherited one.
    h.engine
        .set_pin(Path::new("Parent/Huge"), PinState::Unpinned)
        .unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;
    assert_eq!(summary.failed, 0);
    assert!(h.path("Parent/keep.txt").exists());
    assert!(!h.path("Parent/Huge/skip.txt").exists());
}

#[tokio::test]
async fn unpinning_stops_downloads_but_never_deletes_local_files() {
    // "Stop syncing this" and "delete my files" are different requests, and
    // only one of them was made.
    let h = Harness::new().await; // download everything first
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
    assert_eq!(h.read("Docs/a.txt"), "hello");

    let outcome = h
        .engine
        .set_pin(Path::new("Docs"), PinState::Unpinned)
        .unwrap();
    assert_eq!(outcome.files, 0, "unpinning queues no transfers");

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file("F2", "D1", "new.txt", "c1", 5, "2026-08-21T10:00:00Z")],
        "v2",
    )
    .await;
    // No download mock: the new file must not be fetched.

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.failed, 0);
    // The already-downloaded file is untouched.
    assert_eq!(h.read("Docs/a.txt"), "hello");
    assert!(!h.path("Docs/new.txt").exists());
}

#[tokio::test]
async fn a_preview_reports_the_cost_without_changing_anything() {
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "a.bin", "c1", 1_000_000, "2026-08-20T10:00:00Z"),
            file("F2", "D1", "b.bin", "c1", 2_000_000, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await;

    let selection = gonedrive_core::sync::selection::Selection::new(
        h.store.clone(),
        h.config.clone(),
        ACCOUNT.into(),
    );
    let preview = selection.preview(Path::new("Docs")).unwrap();
    assert_eq!(preview.bytes, 3_000_000);
    assert_eq!(preview.files, 2);

    // Still nothing chosen, and still nothing queued.
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.pin_state, PinState::Inherit);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
}

#[tokio::test]
async fn the_drive_root_is_never_born_pinned() {
    // The root's pin decides every descendant's pin. A `Pinned` root silently
    // converts "download nothing by default" into "download the entire drive"
    // — which is exactly what happened once, against a 348 GB account.
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;

    let summary = h.sync().await;

    let root = h.store.item(ROOT).unwrap().unwrap();
    assert_eq!(
        root.pin_state,
        PinState::Inherit,
        "the root must stay Inherit so the account policy applies"
    );
    assert_eq!(summary.downloaded, 0);
    assert_eq!(
        h.store.pending_count(ACCOUNT).unwrap(),
        0,
        "no download should have been queued"
    );
    assert!(!h.path("Docs/a.txt").exists());
}

#[tokio::test]
async fn re_enumerating_the_root_does_not_clear_a_pin_set_on_it() {
    // The root is re-recorded on every delta page that mentions it. Doing that
    // must not undo a user's explicit "sync everything" choice.
    let h = Harness::metadata_only().await;
    h.mock_delta(None, vec![root_item(), folder("D1", ROOT, "Docs")], "v1")
        .await;
    h.sync().await;

    h.engine.set_pin(Path::new(""), PinState::Pinned).unwrap();
    assert_eq!(
        h.store.item(ROOT).unwrap().unwrap().pin_state,
        PinState::Pinned
    );

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![root_item()], "v2")
        .await;
    h.sync().await;

    assert_eq!(
        h.store.item(ROOT).unwrap().unwrap().pin_state,
        PinState::Pinned,
        "an explicit choice on the root must survive re-enumeration"
    );
}

#[tokio::test]
async fn excluded_folders_leave_nothing_on_disk() {
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Huge"),
            folder("D2", "D1", "Nested"),
            file("F1", "D2", "a.bin", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;

    h.sync().await;

    // Building the folder structure for a drive nobody selected leaves tens of
    // thousands of empty directories behind for no benefit.
    assert!(
        !h.path("Huge").exists(),
        "no directory should have been created"
    );
    // The metadata is still recorded, so the folder picker has something to show.
    assert!(h.store.item("D1").unwrap().is_some());
    assert!(h.store.item("F1").unwrap().is_some());
}

#[tokio::test]
async fn removing_a_local_file_in_an_excluded_folder_never_deletes_it_remotely() {
    // The path that would have destroyed 249 GB: once excluded content stops
    // being written to disk, a local scan sees "recorded but absent" and, with
    // no guard, reads that as the user deleting it.
    let h = Harness::new().await; // start by syncing everything
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Archive"),
            file("F1", "D1", "big.bin", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    assert!(h.path("Archive/big.bin").exists());

    // The user excludes the folder and reclaims the space by hand.
    h.engine
        .set_pin(Path::new("Archive"), PinState::Unpinned)
        .unwrap();
    std::fs::remove_file(h.path("Archive/big.bin")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // No DELETE mock is registered: attempting one would fail the sync, and the
    // assertions below would catch it either way.

    let summary = h.sync().await;

    assert_eq!(
        summary.deleted_remote, 0,
        "excluding a folder must never delete it on the service"
    );
    assert!(
        h.store.item("F1").unwrap().is_some(),
        "the remote record must survive"
    );
}

#[tokio::test]
async fn deleting_a_local_file_in_a_synced_folder_still_deletes_it_remotely() {
    // The guard above must not swallow real deletions inside chosen folders.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::remove_file(h.path("Docs/a.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.deleted_remote, 1);
}

#[tokio::test]
async fn editing_a_synced_file_uploads_the_new_content() {
    // Addressing an existing item by parent+name forces a conflict-behaviour
    // choice, and `fail` — correct for a genuinely new file — turns every edit
    // into `409 nameAlreadyExists`. A known file must be uploaded by id.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::write(h.path("Docs/a.txt"), b"hello, edited").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;

    let updated = json!({
        "id": "F1", "name": "a.txt", "size": 13,
        "cTag": "c2", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": "D1"},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-22T10:00:00Z"}
    });
    // Matching the id-addressed path exactly is the point of the test: the
    // parent+name form would not match and the upload would 404.
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_json(updated.clone()))
        .mount(&h.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(updated))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;

    assert_eq!(summary.uploaded, 1, "the edit should have been uploaded");
    assert_eq!(summary.failed, 0);

    // The record has to move on, or the next scan uploads the same edit again.
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.size, 13);
    assert_eq!(record.ctag.as_deref(), Some("c2"));
    assert_eq!(record.state, ItemState::Synced);

    // And the steady state is quiet.
    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 0, "the edit must not be re-uploaded");
}

#[tokio::test]
async fn a_queued_upload_never_follows_a_replacement_symlink() {
    use std::os::unix::fs::symlink;

    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    let path = h.path("a.txt");
    std::fs::write(&path, b"queued local edit").unwrap();
    h.engine
        .sync_local_paths(std::slice::from_ref(&path))
        .unwrap();

    let outside = h.root.parent().unwrap().join("outside-secret.txt");
    std::fs::write(&outside, b"must never be uploaded").unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&outside, &path).unwrap();
    h.server.reset().await;

    let summary = h
        .engine
        .transfer_slice(
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            QueueScope::Outbound,
        )
        .await
        .unwrap();
    assert_eq!(summary.uploaded, 0);
    assert_eq!(summary.failed, 1);
    assert!(h.server.received_requests().await.unwrap().is_empty());
    assert_eq!(std::fs::read(&outside).unwrap(), b"must never be uploaded");
}

#[tokio::test]
async fn a_queued_download_never_follows_an_ancestor_symlink() {
    use std::os::unix::fs::symlink;

    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    h.engine.sync_remote().await.unwrap();
    std::fs::remove_dir(h.path("Docs")).unwrap();
    let outside = h.root.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, h.path("Docs")).unwrap();

    let summary = h
        .engine
        .transfer_slice(
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            QueueScope::Downloads,
        )
        .await
        .unwrap();
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.failed, 1);
    assert!(!outside.join("a.txt").exists());
}

#[tokio::test]
async fn a_brand_new_file_is_uploaded_by_name_and_refuses_to_clobber() {
    // The other half: a file the service has never seen has no id to address,
    // so it goes to parent+name — and must not overwrite anything already
    // sitting under that name.
    let h = Harness::new().await;
    h.mock_delta(None, vec![root_item()], "v1").await;
    h.sync().await;

    std::fs::write(h.path("fresh.txt"), b"new content").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let created = json!({
        "id": "NEW", "name": "fresh.txt", "size": 11,
        "cTag": "cn", "eTag": "en",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-22T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path_regex(r"^/me/drive/items/[^/]+:/fresh\.txt:/content$"))
        .and(query_param("@microsoft.graph.conflictBehavior", "fail"))
        .respond_with(ResponseTemplate::new(201).set_body_json(created.clone()))
        .mount(&h.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/NEW"))
        .respond_with(ResponseTemplate::new(200).set_body_json(created))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 1);
    assert_eq!(summary.failed, 0);
    assert_eq!(h.store.item("NEW").unwrap().unwrap().name, "fresh.txt");
}

#[tokio::test]
async fn downloaded_folders_keep_their_remote_timestamps() {
    // Writing a file into a directory updates that directory's mtime, so a
    // stamp applied at creation time is undone by the first download into it.
    // The stamp has to be re-applied after the contents land.
    let h = Harness::new().await;

    let docs = json!({
        "id": "D1", "name": "Docs", "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2019-04-06T05:46:00Z"}
    });
    h.mock_delta(
        None,
        vec![
            root_item(),
            docs,
            file("F1", "D1", "a.txt", "c1", 5, "2022-03-12T01:55:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    h.sync().await;

    let file_mtime = mtime_secs(&h.path("Docs/a.txt"));
    let dir_mtime = mtime_secs(&h.path("Docs"));

    assert_eq!(
        file_mtime,
        time::macros::datetime!(2022-03-12 01:55:00 UTC).unix_timestamp()
    );
    assert_eq!(
        dir_mtime,
        time::macros::datetime!(2019-04-06 05:46:00 UTC).unix_timestamp(),
        "the folder kept the download time instead of its own"
    );
}

#[tokio::test]
async fn nested_folders_are_stamped_deepest_first() {
    // Stamping a parent before its children are finished would be undone by the
    // child directory's own creation.
    let h = Harness::new().await;
    let outer = json!({
        "id": "D1", "name": "Outer", "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2017-05-26T03:40:00Z"}
    });
    let inner = json!({
        "id": "D2", "name": "Inner", "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": "D1"},
        "fileSystemInfo": {"lastModifiedDateTime": "2016-05-11T02:47:00Z"}
    });
    h.mock_delta(
        None,
        vec![
            root_item(),
            outer,
            inner,
            file("F1", "D2", "deep.txt", "c1", 5, "2014-09-05T04:47:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    h.sync().await;

    assert_eq!(
        mtime_secs(&h.path("Outer")),
        time::macros::datetime!(2017-05-26 03:40:00 UTC).unix_timestamp()
    );
    assert_eq!(
        mtime_secs(&h.path("Outer/Inner")),
        time::macros::datetime!(2016-05-11 02:47:00 UTC).unix_timestamp()
    );
}

#[tokio::test]
async fn a_pinned_folder_keeps_its_own_timestamp_too() {
    // The failure this guards against: the folder the user pinned is created by
    // `set_pin`, not by the delta applier, so marking only what the applier
    // created left the chosen folder itself showing the time of the sync.
    // Downloads into its subfolders bump it, and nothing put it back.
    let h = Harness::metadata_only().await;

    let chosen = json!({
        "id": "D1", "name": "Chosen", "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2023-04-13T19:04:00Z"}
    });
    let nested = json!({
        "id": "D2", "name": "Nested", "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": "D1"},
        "fileSystemInfo": {"lastModifiedDateTime": "2024-09-29T23:07:00Z"}
    });
    h.mock_delta(
        None,
        vec![
            root_item(),
            chosen,
            nested,
            file("F1", "D2", "a.pdf", "c1", 5, "2024-09-29T23:08:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await;

    h.engine
        .set_pin(Path::new("Chosen"), PinState::Pinned)
        .unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    assert_eq!(
        mtime_secs(&h.path("Chosen/Nested/a.pdf")),
        time::macros::datetime!(2024-09-29 23:08:00 UTC).unix_timestamp()
    );
    assert_eq!(
        mtime_secs(&h.path("Chosen/Nested")),
        time::macros::datetime!(2024-09-29 23:07:00 UTC).unix_timestamp()
    );
    assert_eq!(
        mtime_secs(&h.path("Chosen")),
        time::macros::datetime!(2023-04-13 19:04:00 UTC).unix_timestamp(),
        "the pinned folder itself kept the sync time"
    );
}

#[tokio::test]
async fn a_drifted_directory_timestamp_is_corrected_on_the_next_pass() {
    // The local scan already stats every directory it walks, so spotting one
    // whose timestamp drifted — from an earlier version of this client, or an
    // edit made outside it — costs nothing there.
    let h = Harness::new().await;
    let docs = json!({
        "id": "D1", "name": "Docs", "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2019-04-06T05:46:00Z"}
    });
    h.mock_delta(
        None,
        vec![
            root_item(),
            docs,
            file("F1", "D1", "a.txt", "c1", 5, "2022-03-12T01:55:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    // Something else moved it.
    let wrong = time::macros::datetime!(2026-01-01 00:00:00 UTC);
    gonedrive_core::sync::transfer::set_mtime(&h.path("Docs"), wrong).unwrap();
    assert_eq!(mtime_secs(&h.path("Docs")), wrong.unix_timestamp());

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h.sync().await;

    assert_eq!(
        mtime_secs(&h.path("Docs")),
        time::macros::datetime!(2019-04-06 05:46:00 UTC).unix_timestamp()
    );
}

// ---------------------------------------------------------------------------
// Files on demand.
// ---------------------------------------------------------------------------

/// A placeholder still costs whatever the filesystem needs for its metadata —
/// ext4 allocates a block for extended attributes that do not fit inline — so
/// "occupies nothing" means "occupies only overhead", not literally zero.
const PLACEHOLDER_OVERHEAD: u64 = 8 * 1024;

fn assert_is_placeholder_on_disk(path: &Path) {
    use gonedrive_core::hydrate::Occupancy;
    let occupancy = Occupancy::read(path).unwrap();
    assert!(
        occupancy.allocated <= PLACEHOLDER_OVERHEAD,
        "{} still occupies {} of {} apparent",
        path.display(),
        occupancy.allocated,
        occupancy.apparent_size
    );
}

/// Sync a folder with one file and return the harness.
async fn harness_with_a_downloaded_file(content: &'static [u8]) -> Harness {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file(
                "F1",
                "D1",
                "big.bin",
                "c1",
                content.len() as u64,
                "2022-03-12T01:55:00Z",
            ),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", content).await;
    h.sync().await;
    h
}
