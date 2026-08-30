#[tokio::test]
async fn placeholders_can_be_created_for_files_never_downloaded() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    // The other entry point: rather than downloading a folder and freeing it,
    // materialise placeholders directly so a drive larger than the disk can
    // still be browsed in a file manager.
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file(
                "F1",
                "D1",
                "huge.bin",
                "c1",
                4 * 1024 * 1024,
                "2022-03-12T01:55:00Z",
            ),
        ],
        "v1",
    )
    .await;
    h.sync().await;
    assert!(!h.path("Docs/huge.bin").exists());

    let outcome = h.engine.make_on_demand(Path::new("Docs")).unwrap();
    assert_eq!(outcome.files, 1);

    let path = h.path("Docs/huge.bin");
    assert_eq!(
        Occupancy::read(&path).unwrap().apparent_size,
        4 * 1024 * 1024
    );
    assert_is_placeholder_on_disk(&path);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(
        mtime_secs(&path),
        time::macros::datetime!(2022-03-12 01:55:00 UTC).unix_timestamp()
    );

    // The engine can list what the helper needs to intercept.
    let watched = h.engine.dehydrated_paths().unwrap();
    assert!(watched.contains(&path));

    // HSM startup must be able to build the same inventory before an
    // Authenticator/Graph engine exists. A temporary token-endpoint outage may
    // disable downloads, but it must never expose sparse zeroes.
    let account = h.store.accounts().unwrap().pop().unwrap();
    let offline = gonedrive_core::sync::dehydrated_paths_for_account(&h.store, &account).unwrap();
    assert_eq!(offline, watched);
}

#[tokio::test]
async fn a_missing_placeholder_is_rebuilt_from_local_state() {
    use gonedrive_core::sync::{
        materialise_placeholder_for_account, placeholder_inventory_for_account,
    };

    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file(
                "F1",
                "D1",
                "restored.bin",
                "c1",
                8192,
                "2026-08-21T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);

    let account = h.store.accounts().unwrap().pop().unwrap();
    let before = placeholder_inventory_for_account(&h.store, &account).unwrap();
    assert!(before.existing.is_empty());
    assert_eq!(before.missing, vec!["F1"]);

    let path = materialise_placeholder_for_account(&h.store, &account, "F1").unwrap();
    assert_eq!(path, h.path("Docs/restored.bin"));
    assert_eq!(
        materialise_placeholder_for_account(&h.store, &account, "F1").unwrap(),
        path,
        "recovery must be idempotent"
    );
    let ready = placeholder_inventory_for_account(&h.store, &account).unwrap();
    assert_eq!(ready.existing, vec![path.clone()]);
    assert!(ready.missing.is_empty());
}

#[tokio::test]
async fn a_mark_failure_keeps_a_locked_placeholder_in_the_reconnect_inventory() {
    use gonedrive_core::sync::{
        lock_unwatched_placeholder_for_account, materialise_placeholder_for_account,
        placeholder_inventory_for_account,
    };
    use std::os::unix::fs::MetadataExt;

    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Docs"),
            file(
                "F1",
                "D1",
                "visible.bin",
                "c1",
                8192,
                "2026-08-21T10:00:00Z",
            ),
        ],
        "v1",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);

    let account = h.store.accounts().unwrap().pop().unwrap();
    let path = materialise_placeholder_for_account(&h.store, &account, "F1").unwrap();
    lock_unwatched_placeholder_for_account(&h.store, &account, &path).unwrap();

    assert!(path.exists(), "the namespace must remain browsable");
    assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0);
    let inventory = placeholder_inventory_for_account(&h.store, &account).unwrap();
    assert_eq!(inventory.existing, vec![path.clone()]);
    assert!(inventory.missing.is_empty());

    let discovery = h.engine.discover_local_changes().unwrap();
    assert!(discovery.dirty_paths.is_empty());
    assert!(discovery
        .placeholders
        .iter()
        .any(|(candidate, _, _)| candidate == &path));

    let mut summary = gonedrive_core::sync::SyncSummary::default();
    h.engine.scan_local(&mut summary).unwrap();
    assert_eq!(
        h.store.pending_count(ACCOUNT).unwrap(),
        0,
        "a safety-locked placeholder must never be queued as an upload"
    );
}

#[tokio::test]
async fn hydration_failure_leaves_the_placeholder_intact() {
    // The helper answers a failed hydration with a denial, so the reader gets
    // an I/O error. The file must still be a valid placeholder afterwards, not
    // a half-filled corpse.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    let content: &[u8] = b"contents";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/me/drive/items/F1/content$"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;

    assert!(h.engine.hydrate(&path).await.is_err());

    assert!(path.exists());
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::item_id(&path).as_deref(), Some("F1"));
}

#[tokio::test]
async fn staging_never_writes_to_the_placeholder_itself() {
    // The bug this guards against cost a full end-to-end debugging round.
    //
    // On the files-on-demand path the daemon must not write into the
    // placeholder: filling a hole faults the page in, and a page fault *is* a
    // read, which re-enters the very kernel hook that asked for the hydration.
    // The result is an infinite loop — the file is hydrated once a second,
    // forever, while the process that tried to read it stays blocked.
    //
    // So the daemon downloads to a sibling and the privileged helper does the
    // write, through the descriptor the kernel handed it, which is exempt.
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    const CONTENT: &[u8] = &[0x5Au8; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    h.server.reset().await;
    h.mock_download("F1", CONTENT).await;

    let staged = h.engine.stage_content(&path).await.unwrap();

    // The content landed beside the placeholder, not in it.
    assert_ne!(staged, path);
    assert_eq!(
        staged.parent(),
        path.parent(),
        "must stay on one filesystem"
    );
    assert_eq!(std::fs::read(&staged).unwrap(), CONTENT);

    // The placeholder is untouched: still empty, still marked, still sparse.
    assert_is_placeholder_on_disk(&path);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(
        Occupancy::read(&path).unwrap().apparent_size,
        CONTENT.len() as u64
    );

    // And the staging name is one the local scanner already ignores, so it
    // cannot be mistaken for a new file and uploaded.
    assert!(staged
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".gonedrive-partial"));
}

#[tokio::test]
async fn an_interrupted_hydration_is_still_watched_afterwards() {
    // A hydration that dies mid-flight leaves the record saying `Hydrating`
    // while the file on disk is still empty. Trusting the record would leave
    // that file unwatched, and it would then read as zeroes — silently, with
    // no error, forever. The file's own xattr is what decides.
    use gonedrive_core::db::ItemState;

    const CONTENT: &[u8] = &[1u8; 64 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    // Simulate the crash: the record advanced, the file did not.
    h.store.set_item_state("F1", ItemState::Hydrating).unwrap();

    let watched = h.engine.dehydrated_paths().unwrap();
    assert!(
        watched.contains(&path),
        "a half-hydrated file must still be intercepted, got {watched:?}"
    );

    // The same holds for one that failed outright.
    h.store.set_item_state("F1", ItemState::Error).unwrap();
    assert!(h.engine.dehydrated_paths().unwrap().contains(&path));
}

#[tokio::test]
async fn a_hydrated_file_is_not_offered_for_interception() {
    // The mirror image: once the content is there, intercepting it again would
    // add latency to every read for nothing.
    const CONTENT: &[u8] = &[2u8; 32 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");

    assert!(!h.engine.dehydrated_paths().unwrap().contains(&path));

    h.engine.free_space(Path::new("Docs")).unwrap();
    assert!(h.engine.dehydrated_paths().unwrap().contains(&path));

    h.server.reset().await;
    h.mock_download("F1", CONTENT).await;
    h.engine.hydrate(&path).await.unwrap();
    assert!(!h.engine.dehydrated_paths().unwrap().contains(&path));
}

#[tokio::test]
async fn on_demand_hydration_retires_the_same_queued_download() {
    use gonedrive_core::db::OpKind;

    const CONTENT: &[u8] = &[0x31; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();
    h.store
        .enqueue(ACCOUNT, OpKind::Download, Some("F1"), Some(&path), None)
        .unwrap();
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 1);

    h.server.reset().await;
    h.mock_download("F1", CONTENT).await;
    h.engine.hydrate(&path).await.unwrap();

    assert_eq!(
        h.store.pending_count(ACCOUNT).unwrap(),
        0,
        "content already delivered on demand must not be downloaded again"
    );
}

#[tokio::test]
async fn helper_finished_hydration_retires_the_same_queued_download() {
    use gonedrive_core::db::OpKind;
    use gonedrive_core::hydrate::Placeholder;

    const CONTENT: &[u8] = &[0x32; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();
    h.store
        .enqueue(ACCOUNT, OpKind::Download, Some("F1"), Some(&path), None)
        .unwrap();

    Placeholder::fill(&path, 0, CONTENT).unwrap();
    h.engine.finish_hydration(&path).unwrap();

    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
    assert_eq!(std::fs::read(path).unwrap(), CONTENT);
}

#[tokio::test]
async fn stale_completed_download_is_retired_without_network_or_replacement() {
    use std::os::unix::fs::MetadataExt;

    use gonedrive_core::db::OpKind;

    const CONTENT: &[u8] = &[0x33; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    let before = std::fs::metadata(&path).unwrap();
    h.store
        .enqueue(ACCOUNT, OpKind::Download, Some("F1"), Some(&path), None)
        .unwrap();
    h.server.reset().await;

    let summary = h.engine.transfer_only().await.unwrap();

    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.failed, 0);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), CONTENT);
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), before.ino());
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_placeholder_is_never_uploaded_even_when_its_mtime_drifts() {
    // The scan judges "locally modified" by size+mtime. A hydration in
    // progress moves the mtime, and a crashed one leaves it moved — but the
    // bytes are holes. Uploading them would replace the real file in the
    // cloud with zeroes. This is the single worst thing this program could do.
    let h = harness_with_a_downloaded_file(&[9u8; 128 * 1024]).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    // Simulate the drift a dying hydration leaves behind.
    gonedrive_core::sync::transfer::set_mtime(
        &path,
        time::macros::datetime!(2026-01-01 00:00:00 UTC),
    )
    .unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // No upload mock: an attempted upload would 404 and fail the summary.

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 0, "holes must never be uploaded");
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn replacing_a_placeholder_with_real_content_is_uploaded() {
    // A record can still say Dehydrated after the user removes its placeholder
    // and creates an ordinary file at the same path. The file's own id/state,
    // not that stale database state, decides whether its bytes are uploadable.
    let h = harness_with_a_downloaded_file(&[9u8; 128 * 1024]).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"local content").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "F1", "big.bin").await;

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"local content");
}

#[tokio::test]
async fn a_stale_hydrating_record_is_repaired_from_the_file_itself() {
    use gonedrive_core::db::ItemState;
    use gonedrive_core::hydrate::Placeholder;

    // A crash mid-hydration leaves the record saying `Hydrating` forever. The
    // file's own marks are written last, so they are the truth to restore.
    let h = harness_with_a_downloaded_file(b"content").await;
    let path = h.path("Docs/big.bin");

    // Case 1: the fill never finished — file still dehydrated.
    h.engine.free_space(Path::new("Docs")).unwrap();
    h.store.set_item_state("F1", ItemState::Hydrating).unwrap();
    assert_eq!(h.engine.repair_states().unwrap(), 1);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().state,
        ItemState::Dehydrated
    );

    // Case 2: the fill finished but the record was never advanced.
    Placeholder::fill(&path, 0, b"content").unwrap();
    Placeholder::mark_hydrated(&path, "F1", Some("c1")).unwrap();
    h.store.set_item_state("F1", ItemState::Hydrating).unwrap();
    assert_eq!(h.engine.repair_states().unwrap(), 1);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().state,
        ItemState::Synced
    );
}

#[tokio::test]
async fn the_helper_close_write_cannot_upload_a_hydration_in_transition() {
    use gonedrive_core::db::ItemState;
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    // The helper has filled the inode and is about to notify the daemon. Its
    // pwrite moved mtime to now, while SQLite still describes the sparse
    // placeholder. A close-write scan can observe exactly this state.
    let content = b"remote content that must never be re-uploaded on open";
    let h = harness_with_a_downloaded_file(content).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();
    h.store.set_item_state("F1", ItemState::Hydrating).unwrap();
    Placeholder::fill(&path, 0, content).unwrap();
    // This is the helper's source-first marker. The database transition is the
    // authoritative sign that finalisation has not restored mtime yet.
    Placeholder::mark_hydrated(&path, "F1", Some("c1")).unwrap();
    assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);

    let completed = std::collections::HashSet::from([path.clone()]);
    let summary = h
        .engine
        .sync_local_change_batch(std::slice::from_ref(&path), &completed)
        .unwrap();

    assert_eq!(summary.still_queued, 0);
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);
}

#[tokio::test]
async fn freeing_one_folder_leaves_its_siblings_alone() {
    // The report that prompted this: freeing `工具/软件` appeared to free
    // `工具/ss.txt` as well. The engine must only ever touch the subtree it
    // was pointed at.
    use gonedrive_core::hydrate::PlaceholderState;
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Tools"),
            folder("D2", "D1", "Soft"),
            file("F1", "D2", "inner.bin", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F2", "D1", "sibling.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"inner").await;
    h.mock_download("F2", b"sibli").await;
    h.sync().await;

    let outcome = h.engine.free_space(Path::new("Tools/Soft")).unwrap();
    assert_eq!(outcome.files, 1, "only the folder's own file");

    use gonedrive_core::hydrate::Placeholder;
    assert_eq!(
        Placeholder::state(&h.path("Tools/Soft/inner.bin")),
        PlaceholderState::Dehydrated
    );
    // The sibling keeps its content and its marks.
    assert_eq!(
        std::fs::read(h.path("Tools/sibling.txt")).unwrap(),
        b"sibli"
    );
    assert_eq!(
        Placeholder::state(&h.path("Tools/sibling.txt")),
        PlaceholderState::Hydrated
    );
}

#[tokio::test]
async fn auto_free_releases_lru_unpinned_files_and_spares_pinned_ones() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Keep"),
            file("F1", "D1", "pinned.bin", "c1", 5, "2026-08-20T10:00:00Z"),
            folder("D2", ROOT, "Cache"),
            file("F2", "D2", "old.bin", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F3", "D2", "new.bin", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    for (id, content) in [("F1", "pinnd"), ("F2", "old__"), ("F3", "new__")] {
        h.mock_download(id, content.as_bytes()).await;
    }
    h.sync().await;
    // Under the mirror-everything policy the default pin is "keep", so
    // nothing is evictable until the user says otherwise — that is the whole
    // point of the default. Mark the cache folder as not-kept explicitly.
    h.engine
        .set_pin(Path::new("Keep"), PinState::Pinned)
        .unwrap();
    h.engine
        .set_pin(Path::new("Cache"), PinState::Unpinned)
        .unwrap();

    // A threshold no disk satisfies forces the release path deterministically.
    let outcome = h.engine.auto_free(u64::MAX).unwrap();

    // Both unpinned files went; the pinned one is untouchable by pressure.
    assert_eq!(outcome.files, 2, "skipped: {:?}", outcome.skipped);
    assert_eq!(
        Placeholder::state(&h.path("Cache/old.bin")),
        PlaceholderState::Dehydrated
    );
    assert_eq!(
        Placeholder::state(&h.path("Cache/new.bin")),
        PlaceholderState::Dehydrated
    );
    assert!(
        Occupancy::read(&h.path("Keep/pinned.bin"))
            .unwrap()
            .allocated
            > 0
    );
    assert_eq!(std::fs::read(h.path("Keep/pinned.bin")).unwrap(), b"pinnd");

    // And a second pass finds nothing left to do.
    let again = h.engine.auto_free(u64::MAX).unwrap();
    assert_eq!(again.files, 0);
}

#[tokio::test]
async fn auto_free_never_touches_a_file_with_unsaved_changes() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "Cache"),
            file("F1", "D1", "edited.bin", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    h.engine
        .set_pin(Path::new("Cache"), PinState::Unpinned)
        .unwrap();

    std::fs::write(h.path("Cache/edited.bin"), b"an unsaved local edit").unwrap();

    let outcome = h.engine.auto_free(u64::MAX).unwrap();
    assert_eq!(outcome.files, 0);
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(
        std::fs::read(h.path("Cache/edited.bin")).unwrap(),
        b"an unsaved local edit"
    );
}

#[tokio::test]
async fn a_range_stage_fetches_only_the_window_and_changes_nothing() {
    // A file manager sniffing content types reads the first 16 KiB of every
    // file it shows. Answering that with a full download turns "open a
    // folder" into "download the folder" — the bug as reported. A range stage
    // must fetch just the window and leave the placeholder exactly as it was.
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};
    use wiremock::matchers::header;

    const CONTENT: &[u8] = &[0x42u8; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    h.server.reset().await;
    // Only a ranged request is mocked: an un-ranged (full) download would 404
    // and fail the test.
    Mock::given(method("GET"))
        .and(path_regex(r"^/me/drive/items/F1/content$"))
        .and(header("Range", "bytes=0-16383"))
        .respond_with(ResponseTemplate::new(206).set_body_bytes(&CONTENT[..16384]))
        .mount(&h.server)
        .await;

    let mut events = h.engine.subscribe();
    let staged = h
        .engine
        .stage_range(&path, 0, 16384)
        .await
        .unwrap()
        .unwrap();

    // The staged sibling holds exactly the window.
    assert_eq!(std::fs::read(&staged).unwrap(), &CONTENT[..16384]);
    assert!(staged
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".gonedrive-partial"));

    // And the placeholder itself is untouched in every way that matters:
    // still sparse, still marked, still dehydrated in the database.
    assert_is_placeholder_on_disk(&path);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().state,
        gonedrive_core::db::ItemState::Dehydrated
    );
    assert_eq!(
        Occupancy::read(&path).unwrap().apparent_size,
        CONTENT.len() as u64
    );

    // Quiet by design: no progress events, no activity — nothing for a UI to
    // flash on screen over a 16 KiB sniff.
    assert!(
        events.try_recv().is_err(),
        "a range stage must not emit sync events"
    );
    let _ = std::fs::remove_file(&staged);

    // Simulate the helper placing the first response in the placeholder. A
    // repeated Nautilus sniff now reads that exact range directly: with no
    // remaining mock, any accidental second network request fails the test.
    Placeholder::fill(&path, 0, &CONTENT[..16384]).unwrap();
    h.server.reset().await;
    assert!(h
        .engine
        .stage_range(&path, 0, 16384)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_browsed_placeholder_can_still_be_freed() {
    // A range fill (a file manager sniffing the head) writes into the
    // placeholder and moves its mtime. That must never trip the
    // unsaved-changes guard: the bytes are cloud-owned cache, and refusing
    // made "free a file you merely looked at" impossible — found live as
    // `FreeUpSpace done files=0 kept=2`.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    const CONTENT: &[u8] = &[7u8; 64 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine.free_space(Path::new("Docs")).unwrap();

    // Simulate the helper filling the sniff window through the event fd.
    Placeholder::fill(&path, 0, &CONTENT[..16384]).unwrap();
    assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);

    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();
    assert_eq!(outcome.files, 1, "skipped: {:?}", outcome.skipped);
    assert!(outcome.skipped.is_empty());
    assert_is_placeholder_on_disk(&path);

    // The guard still protects what it is for: a *hydrated* file with a real
    // local edit.
    h.server.reset().await;
    h.mock_download("F1", CONTENT).await;
    h.engine.hydrate(&path).await.unwrap();
    std::fs::write(&path, b"a real unsaved edit").unwrap();
    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();
    assert_eq!(outcome.files, 0);
    assert_eq!(outcome.skipped.len(), 1);
}

#[tokio::test]
async fn freeing_a_pinned_file_clears_its_retention_choice() {
    use gonedrive_core::db::PinState;
    use gonedrive_core::hydrate::Placeholder;

    const CONTENT: &[u8] = &[0x31; 64 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine
        .set_pin(Path::new("Docs/big.bin"), PinState::Pinned)
        .unwrap();
    Placeholder::set_pin(&path, PinState::Pinned.as_str()).unwrap();
    h.store
        .enqueue(ACCOUNT, OpKind::Download, Some("F1"), Some(&path), None)
        .unwrap();
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 1);

    let outcome = h.engine.free_space(Path::new("Docs/big.bin")).unwrap();

    assert_eq!(outcome.files, 1, "skipped: {:?}", outcome.skipped);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().pin_state,
        PinState::Unpinned,
        "a released file must not be downloaded again by its stale pin"
    );
    assert_eq!(
        Placeholder::pin(&path).as_deref(),
        Some(PinState::Unpinned.as_str()),
        "the file-manager emblem must agree with the durable choice"
    );
    assert_eq!(
        h.store.pending_count(ACCOUNT).unwrap(),
        0,
        "release must cancel a stale download for the same file"
    );
}

#[tokio::test]
async fn a_file_skipped_by_free_space_keeps_its_pin() {
    use gonedrive_core::db::PinState;
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    const CONTENT: &[u8] = &[0x32; 64 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let path = h.path("Docs/big.bin");
    h.engine
        .set_pin(Path::new("Docs/big.bin"), PinState::Pinned)
        .unwrap();
    Placeholder::set_pin(&path, PinState::Pinned.as_str()).unwrap();
    std::fs::write(&path, b"unsaved local edit").unwrap();

    let outcome = h.engine.free_space(Path::new("Docs/big.bin")).unwrap();

    assert_eq!(outcome.files, 0);
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().pin_state,
        PinState::Pinned
    );
    assert_eq!(
        Placeholder::pin(&path).as_deref(),
        Some(PinState::Pinned.as_str())
    );
}

#[tokio::test]
async fn freeing_a_folder_clears_the_folder_policy_and_badge() {
    use gonedrive_core::db::PinState;
    use gonedrive_core::hydrate::Placeholder;

    const CONTENT: &[u8] = &[0x33; 64 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let folder = h.path("Docs");
    h.engine
        .set_pin(Path::new("Docs"), PinState::Pinned)
        .unwrap();
    assert_eq!(Placeholder::pin(&folder).as_deref(), Some("pinned"));

    let outcome = h.engine.free_space(Path::new("Docs")).unwrap();

    assert_eq!(outcome.files, 1, "skipped: {:?}", outcome.skipped);
    assert_eq!(
        h.store.item("D1").unwrap().unwrap().pin_state,
        PinState::Unpinned
    );
    assert_eq!(Placeholder::pin(&folder).as_deref(), Some("unpinned"));
    assert_eq!(
        h.store.effective_pin("F1", PinState::Pinned).unwrap(),
        PinState::Unpinned,
        "new or restored children must not be pulled back by the old folder pin"
    );
}

#[tokio::test]
async fn pinning_an_already_local_file_updates_its_badge_without_a_download() {
    use gonedrive_core::db::PinState;
    use gonedrive_core::hydrate::Placeholder;

    let h = harness_with_a_downloaded_file(b"local content").await;
    let path = h.path("Docs/big.bin");
    h.engine
        .set_pin(Path::new("Docs/big.bin"), PinState::Unpinned)
        .unwrap();
    assert_eq!(Placeholder::pin(&path).as_deref(), Some("unpinned"));

    let outcome = h
        .engine
        .set_pin(Path::new("Docs/big.bin"), PinState::Pinned)
        .unwrap();

    assert_eq!(outcome.files, 0, "the bytes are already local");
    assert_eq!(h.store.pending_count(ACCOUNT).unwrap(), 0);
    assert_eq!(Placeholder::pin(&path).as_deref(), Some("pinned"));
}

#[tokio::test]
async fn a_complete_copy_with_cloned_placeholder_marks_becomes_a_new_file() {
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState, XATTR_STATE};

    const CONTENT: &[u8] = &[23u8; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let source = h.path("Docs/big.bin");
    let copy = h.path("Docs/big copy.bin");
    std::fs::copy(&source, &copy).unwrap();
    // GIO copies these attributes after the bytes. Reproduce the live failure:
    // a fully populated destination still claims to be the source placeholder.
    Placeholder::mark_hydrated(&copy, "F1", Some("c1")).unwrap();
    xattr::set(&copy, XATTR_STATE, b"dehydrated").unwrap();

    let summary = h
        .engine
        .sync_local_paths(std::slice::from_ref(&copy))
        .unwrap();

    assert_eq!(summary.still_queued, 1);
    assert_eq!(Placeholder::state(&copy), PlaceholderState::Unmanaged);
    assert_eq!(Placeholder::item_id(&copy), None);
    assert_eq!(std::fs::read(&copy).unwrap(), CONTENT);
    assert_eq!(std::fs::read(&source).unwrap(), CONTENT);
}

#[tokio::test]
async fn a_sparse_copy_that_only_cloned_placeholder_marks_is_never_uploaded() {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    const CONTENT: &[u8] = &[41u8; 128 * 1024];
    let h = harness_with_a_downloaded_file(CONTENT).await;
    let copy = h.path("Docs/incomplete copy.bin");
    Placeholder::create(&copy, CONTENT.len() as u64, "F1", Some("c1"), None).unwrap();
    let completed = std::collections::HashSet::from([copy.clone()]);

    let summary = h
        .engine
        .sync_local_change_batch(std::slice::from_ref(&copy), &completed)
        .unwrap();

    assert_eq!(summary.still_queued, 0, "sparse holes must not enter upload");
    assert_eq!(Placeholder::state(&copy), PlaceholderState::Dehydrated);
    assert!(Occupancy::read(&copy).unwrap().is_sparse());
}

#[tokio::test]
async fn a_finished_edited_copy_is_detached_after_close_write() {
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};

    let h = harness_with_a_downloaded_file(b"source content").await;
    let copy = h.path("Docs/edited copy.bin");
    std::fs::write(&copy, b"a different finished document").unwrap();
    Placeholder::mark_hydrated(&copy, "F1", Some("c1")).unwrap();
    let completed = std::collections::HashSet::from([copy.clone()]);

    let summary = h
        .engine
        .sync_local_change_batch(std::slice::from_ref(&copy), &completed)
        .unwrap();

    assert_eq!(summary.still_queued, 1);
    assert_eq!(Placeholder::state(&copy), PlaceholderState::Unmanaged);
    assert_eq!(std::fs::read(&copy).unwrap(), b"a different finished document");
}
