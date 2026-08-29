#[tokio::test]
async fn freeing_space_keeps_the_file_but_releases_its_blocks() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    let content: &[u8] = &[7u8; 512 * 1024];
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");
    assert!(Occupancy::read(&path).unwrap().allocated >= 512 * 1024);

    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();
    assert_eq!(outcome.files, 1);
    assert!(outcome.freed >= 512 * 1024);

    // The file is still there, still the right size, and costs nothing.
    assert!(path.exists());
    assert_eq!(Occupancy::read(&path).unwrap().apparent_size, 512 * 1024);
    assert_is_placeholder_on_disk(&path);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::item_id(&path).as_deref(), Some("F1"));

    // And its timestamp survived, so it does not look locally modified.
    assert_eq!(
        mtime_secs(&path),
        time::macros::datetime!(2022-03-12 01:55:00 UTC).unix_timestamp()
    );
}

#[tokio::test]
async fn hydrating_a_placeholder_restores_the_exact_bytes() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    // Large enough that the comparison means something: for a 40-byte file the
    // metadata block dwarfs the content either way.
    const CONTENT: &[u8] = &[0xABu8; 256 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");

    h.engine.free_space(Path::new("Docs")).unwrap();
    assert_is_placeholder_on_disk(&path);
    // A dehydrated placeholder reads as zeroes without the kernel hook, which
    // is exactly why the hook is not optional in real use.
    assert!(std::fs::read(&path).unwrap().iter().all(|&b| b == 0));
    let content = CONTENT;

    h.server.reset().await;
    h.mock_download("F1", content).await;

    // This is the call the privileged helper makes when a read is intercepted.
    let filled = h.engine.hydrate(&path).await.unwrap();

    assert_eq!(filled, content.len() as u64);
    assert_eq!(std::fs::read(&path).unwrap(), content);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);
    assert!(Occupancy::read(&path).unwrap().allocated > 0);
    // Hydration must not look like a local edit.
    assert_eq!(
        mtime_secs(&path),
        time::macros::datetime!(2022-03-12 01:55:00 UTC).unix_timestamp()
    );
}

#[tokio::test]
async fn hydration_rejects_a_forged_item_id_before_fetching_or_writing() {
    use gonedrive_core::hydrate::{Placeholder, XATTR_ITEM_ID};

    const CONTENT: &[u8] = &[0xABu8; 64 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();
    xattr::set(&path, XATTR_ITEM_ID, ROOT.as_bytes()).unwrap();
    h.server.reset().await;

    let error = h.engine.hydrate(&path).await.unwrap_err();
    assert!(error.to_string().contains("is not the path of item"));
    assert!(h.server.received_requests().await.unwrap().is_empty());
    assert!(std::fs::read(&path).unwrap().iter().all(|byte| *byte == 0));
    assert_eq!(Placeholder::item_id(&path).as_deref(), Some(ROOT));
}

#[tokio::test]
async fn a_cloud_edit_to_a_hydrated_on_demand_file_re_downloads_in_place() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    // Regression (sync-correctness / hydration): a file the user opened —
    // hydrated — in a folder they never pinned, then edited in the cloud. The
    // old code demoted it to Dehydrated but, because the materialised file
    // already existed, never punched it: the STALE bytes stayed readable and
    // nothing ever fetched the new ones — a silent cloud edit the user never
    // saw (observed live on 待购清单.xlsx). Chosen behaviour: once hydrated,
    // stay hydrated and follow the cloud in place.
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
            folder("D1", ROOT, "Docs"),
            file("F1", "D1", "note.txt", "c1", 3, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.sync().await;
    let path = h.path("Docs/note.txt");
    assert_eq!(
        Placeholder::state(&path),
        PlaceholderState::Dehydrated,
        "an unpinned file starts life as a placeholder on disk"
    );

    // The user opens it: it hydrates individually, the folder still unpinned.
    h.server.reset().await;
    h.mock_download("F1", b"old").await;
    assert_eq!(h.engine.hydrate(&path).await.unwrap(), 3);
    assert_eq!(h.read("Docs/note.txt"), "old");
    assert!(
        Occupancy::read(&path).unwrap().allocated >= 3,
        "now materialised"
    );

    // Now it changes in the cloud.
    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            "D1",
            "note.txt",
            "c2",
            5,
            "2026-08-21T10:00:00Z",
        )],
        "v2",
    )
    .await;
    h.mock_download("F1", b"newer").await;

    let summary = h.sync().await;

    // It followed the cloud in place — not dropped to a placeholder holding
    // the stale "old" bytes.
    assert_eq!(summary.downloaded, 1, "the hydrated file re-downloads");
    assert_eq!(h.read("Docs/note.txt"), "newer");
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().state,
        ItemState::Synced
    );
    assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);
}

#[tokio::test]
async fn a_hydrated_file_is_not_re_uploaded_by_the_next_scan() {
    // The failure this guards against: hydration writes to the file, and if
    // that leaves it looking modified, the next scan uploads the copy we just
    // downloaded — over and over.
    let content: &[u8] = b"contents";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");

    h.engine.free_space(Path::new("Docs")).unwrap();
    h.server.reset().await;
    h.mock_download("F1", content).await;
    h.engine.hydrate(&path).await.unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // No upload mock: an attempted upload would fail and show in the summary.

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn freeing_space_refuses_a_file_with_unsaved_changes() {
    // Freeing space must never be a way to lose an edit.
    let content: &[u8] = b"original";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");

    std::fs::write(&path, b"an edit that has not been uploaded").unwrap();

    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();
    assert_eq!(outcome.files, 0, "nothing should have been freed");
    assert_eq!(outcome.skipped.len(), 1);
    assert!(outcome.skipped[0].1.contains("local changes"));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"an edit that has not been uploaded"
    );
}

#[tokio::test]
async fn freeing_space_refuses_an_in_place_edit_that_kept_size_and_mtime() {
    // The corruption the size+mtime heuristic cannot see: an in-place edit of
    // the same length with the mtime restored. Only the content hash catches
    // it — and freeing must never punch holes in bytes never uploaded.
    let content: &[u8] = b"12345678";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");

    // Edit in place to the same length, then put the mtime back: nothing the
    // heuristic keys on has moved.
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 8);
    std::fs::write(&path, b"EDITED!!").unwrap();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(before))
        .unwrap();
    drop(f);

    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();
    assert_eq!(
        outcome.files, 0,
        "a same-size, same-mtime edit must still be protected"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"EDITED!!",
        "the un-uploaded edit must not have been punched away"
    );
}

#[tokio::test]
async fn destructive_placeholder_actions_never_zero_a_sparse_file_the_user_edited() {
    // The materialize hollow-rebuild turns a "downloaded" file that is actually
    // a hole back into a placeholder. A genuinely sparse file the user edited
    // in place (a VM image, a preallocated database) looks hollow too — it must
    // not be truncated to holes, destroying the edit.
    use std::io::{Seek, SeekFrom, Write};
    let content: &[u8] = b"12345678";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");

    // Replace it with a large, genuinely-sparse file of the user's own: big
    // apparent size, little allocated, real bytes at each end — content that
    // differs from what we recorded on download.
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(1_000_000).unwrap();
        f.write_all(b"USER SPARSE DATA").unwrap();
        f.seek(SeekFrom::Start(999_990)).unwrap();
        f.write_all(b"TAIL").unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();
    assert_eq!(
        outcome.files, 0,
        "free-space must refuse user-owned sparse bytes"
    );
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    h.engine.materialize_all(|_, _| true).unwrap();

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a sparse edited file must survive materialize, never be rebuilt to holes"
    );
}

#[tokio::test]
async fn a_download_preserves_a_hidden_local_edit_as_a_conflict_copy() {
    // A remote change plus an in-place local edit that kept size and mtime: the
    // heuristic judges the remote the newer side and would overwrite, losing
    // the hidden edit. The content-hash guard must turn it into a conflict.
    let content: &[u8] = b"AAAAAAAA";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");

    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, b"BBBBBBBB").unwrap();
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(before))
        .unwrap();
    drop(f);

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file("F1", "D1", "big.bin", "c2", 8, "2022-05-01T00:00:00Z")],
        "v2",
    )
    .await;
    h.mock_download("F1", b"REMOTE!!").await;
    mock_upload(&h.server, "F1copy", "conflict").await;

    let summary = h.sync().await;
    assert!(
        summary.conflicts >= 1,
        "a hidden edit must become a conflict, not be overwritten"
    );
    assert_eq!(
        h.read("Docs/big.bin"),
        "REMOTE!!",
        "the remote takes the original name"
    );

    let copies: Vec<_> = std::fs::read_dir(h.path("Docs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("的冲突副本"))
        .collect();
    assert_eq!(
        copies.len(),
        1,
        "the hidden edit must survive as a conflict copy: {copies:?}"
    );
    assert_eq!(h.read(&format!("Docs/{}", copies[0])), "BBBBBBBB");
}

#[tokio::test]
async fn an_upload_that_would_overwrite_a_newer_remote_becomes_a_conflict() {
    // Someone edits the remote between our pull and our push. Without If-Match
    // our upload replaces their edit — a lost update. With it the service
    // returns 412 and we must keep our copy, not overwrite theirs.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::write(h.path("a.txt"), b"my local edit").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // Rejects only a *conditional* upload — matching on If-Match proves the
    // header was actually sent. An unconditional PUT would find no matching
    // mock and never get its 412.
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .and(header_exists("If-Match"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert!(
        summary.conflicts >= 1,
        "a 412 must become a conflict, not a lost update"
    );

    let copies: Vec<_> = std::fs::read_dir(&h.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("的冲突副本"))
        .collect();
    assert_eq!(
        copies.len(),
        1,
        "our local edit must be preserved: {copies:?}"
    );
    assert_eq!(h.read(&copies[0]), "my local edit");
}

#[tokio::test]
async fn a_local_file_differing_only_in_case_loses_no_data() {
    // OneDrive is case-insensitive, ext4 is not: the user can have both
    // Report.txt and report.txt on disk while the service can hold only one.
    // Whatever we do to reconcile that, neither file's bytes may be destroyed.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "Report.txt",
            "c1",
            8,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;
    h.mock_download("F1", b"REMOTE!!").await;
    h.sync().await;
    assert_eq!(h.read("Report.txt"), "REMOTE!!");

    // A genuinely different file that folds to the same name.
    std::fs::write(h.path("report.txt"), b"my local file").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "F2", "report.txt").await;

    let _ = h.sync().await;

    // Neither file's content may be silently lost.
    assert!(
        h.path("Report.txt").exists(),
        "the synced file must survive"
    );
    assert_eq!(
        h.read("Report.txt"),
        "REMOTE!!",
        "the synced file must not be clobbered"
    );
    assert!(
        h.path("report.txt").exists(),
        "the new local file must survive"
    );
    assert_eq!(h.read("report.txt"), "my local file");
}

/// The DriveItem a completed chunk upload returns.
fn uploaded_big_item() -> Value {
    json!({
        "id": "NEW", "name": "big.bin", "size": 400_000,
        "cTag": "cu", "eTag": "eu",
        "file": {"mimeType": "application/octet-stream"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-28T00:00:00Z"}
    })
}

async fn chunked_upload_harness() -> Harness {
    let config = Config {
        upload_session_threshold: 1000,
        upload_chunk_size: 327_680, // 320 KiB
        ..Config::default()
    };
    let h = Harness::with_config(config).await;
    h.mock_delta(None, vec![root_item()], "v1").await;
    h.sync().await;
    std::fs::write(h.path("big.bin"), vec![7u8; 400_000]).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h
}

#[tokio::test]
async fn a_large_upload_completes_in_multiple_chunks() {
    let h = chunked_upload_harness().await;
    let upload_url = format!("{}/session-a", h.server.uri());
    Mock::given(method("POST"))
        .and(path_regex(r"createUploadSession$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"uploadUrl": upload_url.clone(), "expirationDateTime": "2030-01-01T00:00:00Z"}),
        ))
        .mount(&h.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/session-a"))
        .and(header("Content-Range", "bytes 0-327679/400000"))
        .respond_with(ResponseTemplate::new(202).set_body_json(
            json!({"uploadUrl": upload_url, "nextExpectedRanges": ["327680-399999"]}),
        ))
        .mount(&h.server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/session-a"))
        .and(header("Content-Range", "bytes 327680-399999/400000"))
        .respond_with(ResponseTemplate::new(201).set_body_json(uploaded_big_item()))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 1, "the chunked upload should complete");
    assert!(h.store.item("NEW").unwrap().is_some());
}
#[tokio::test]
async fn an_interrupted_upload_resumes_from_its_committed_offset() {
    // A previous attempt got the first chunk in before dying, leaving a
    // persisted session. The next pass must resume from there — send only the
    // tail — not restart the whole upload.
    let h = chunked_upload_harness().await;
    let file = h.path("big.bin");
    let upload_url = format!("{}/resume-here", h.server.uri());
    h.store
        .save_upload_session(&gonedrive_core::db::UploadSessionRecord {
            key: file.to_string_lossy().into_owned(),
            account_id: ACCOUNT.into(),
            upload_url: upload_url.clone(),
            expires_at: None,
            committed_offset: 327_680,
            total_size: 400_000,
            source_fingerprint: Some(
                gonedrive_core::sync::transfer::file_fingerprint(&file).unwrap(),
            ),
        })
        .unwrap();

    // Restarting would create a new session; a resume must not. expect(0)
    // fails the test on drop if it is ever called.
    Mock::given(method("POST"))
        .and(path_regex(r"createUploadSession$"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&h.server)
        .await;
    // Only the tail remains.
    Mock::given(method("PUT"))
        .and(path("/resume-here"))
        .and(header("Content-Range", "bytes 327680-399999/400000"))
        .respond_with(ResponseTemplate::new(201).set_body_json(uploaded_big_item()))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(
        summary.uploaded, 1,
        "the upload should resume from its offset and complete"
    );
    assert!(h.store.item("NEW").unwrap().is_some());
}
