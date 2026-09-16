#[tokio::test]
async fn converting_a_full_tree_to_lazy_keeps_content_and_prunes_the_rest() {
    // A device that synced in full mode (or materialised the whole drive) has
    // every item indexed and an inode for each. Switching it to files-on-demand
    // must not turn into a fresh login: downloaded bytes, pins and local edits
    // stay exactly where they are, while folders nobody opened become partial
    // directories that repopulate from Graph on first open.
    use gonedrive_core::db::PinState;
    use gonedrive_core::hydrate::{DirectoryPopulation, Placeholder, PlaceholderState};

    // Legacy full metadata sync: every item indexed up front.
    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: false,
        ..Config::default()
    })
    .await;
    let items = vec![
        root_item(),
        folder("A", ROOT, "A"),
        file("A-FILE", "A", "a.txt", "c1", 10, "2026-08-20T10:00:00Z"),
        folder("B", "A", "B"),
        file("B-FILE", "B", "b.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        folder("C", "B", "C"),
        file("C-FILE", "C", "c.txt", "c1", 30, "2026-08-20T10:00:00Z"),
        folder("D", ROOT, "D"),
        file("D-FILE", "D", "d.txt", "c1", 40, "2026-08-20T10:00:00Z"),
        folder("E", "D", "E"),
        file("E-FILE", "E", "e.txt", "c1", 50, "2026-08-20T10:00:00Z"),
        folder("F", ROOT, "F"),
        file("F-FILE", "F", "f.txt", "c1", 60, "2026-08-20T10:00:00Z"),
        folder("G", ROOT, "G"),
        file("G-FILE", "G", "g.txt", "c1", 70, "2026-08-20T10:00:00Z"),
        folder("H", "G", "H"),
        file("H-FILE", "H", "h.txt", "c1", 80, "2026-08-20T10:00:00Z"),
    ];
    h.mock_delta(None, items.clone(), "v1").await;
    assert_eq!(h.sync().await.failed, 0);

    // The whole drive as placeholders, then files-on-demand switched on: what
    // `gonedrived materialize` left the device in.
    let watched = h.engine.materialize_all(|_, _| true).unwrap();
    assert_eq!(watched.len(), 8, "one placeholder per file");
    h.engine.enable_on_demand();
    for file_path in [
        "A/a.txt", "A/B/b.txt", "A/B/C/c.txt", "D/d.txt", "D/E/e.txt", "F/f.txt", "G/g.txt", "G/H/h.txt",
    ] {
        assert_eq!(
            Placeholder::state(&h.path(file_path)),
            PlaceholderState::Dehydrated,
            "{file_path} is a placeholder before conversion"
        );
    }
    for folder_path in ["A", "A/B", "A/B/C", "D", "D/E", "F", "G", "G/H"] {
        assert_eq!(
            Placeholder::directory_population(&h.path(folder_path)),
            DirectoryPopulation::Populated,
            "{folder_path} is fully populated before conversion"
        );
    }

    // Three kinds of content the device must keep: a downloaded file, a pinned
    // folder, and a local file the index has never seen.
    h.mock_download("B-FILE", b"hello").await;
    h.engine
        .set_pin(Path::new("A/B/b.txt"), PinState::Pinned)
        .unwrap();
    assert_eq!(h.sync().await.downloaded, 1);
    assert_eq!(h.read("A/B/b.txt"), "hello");
    h.engine.set_pin(Path::new("F"), PinState::Pinned).unwrap();
    std::fs::write(h.path("D/E/local.txt"), "mine").unwrap();

    let outcome = h.engine.convert_to_lazy(|_, _| true).unwrap();

    // Ancestors of kept content stay fully indexed and published.
    assert_eq!(h.read("A/B/b.txt"), "hello");
    assert_eq!(Placeholder::state(&h.path("A/a.txt")), PlaceholderState::Dehydrated);
    assert_eq!(h.read("D/E/local.txt"), "mine");
    assert_eq!(Placeholder::state(&h.path("D/d.txt")), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::state(&h.path("D/E/e.txt")), PlaceholderState::Dehydrated);
    assert_eq!(Placeholder::state(&h.path("F/f.txt")), PlaceholderState::Dehydrated);
    for folder_path in ["A", "A/B", "D", "D/E", "F"] {
        assert_eq!(
            Placeholder::directory_population(&h.path(folder_path)),
            DirectoryPopulation::Populated,
            "{folder_path} must stay populated"
        );
    }

    // Folders with nothing but never-downloaded placeholders become partial:
    // the directory stays (its parent is populated), its contents go.
    for folder_path in ["A/B/C", "G"] {
        assert!(h.path(folder_path).is_dir(), "{folder_path} keeps its inode");
        assert_eq!(
            Placeholder::directory_population(&h.path(folder_path)),
            DirectoryPopulation::Partial,
            "{folder_path} is partial after conversion"
        );
        assert_eq!(
            std::fs::read_dir(h.path(folder_path)).unwrap().count(),
            0,
            "{folder_path} is empty after conversion"
        );
    }
    assert!(!h.path("G/H").exists(), "a pruned folder's subfolders are gone");
    for id in ["C-FILE", "G-FILE", "H", "H-FILE"] {
        assert!(h.store.item(id).unwrap().is_none(), "{id} row is dropped");
    }
    for id in ["A", "A-FILE", "B", "B-FILE", "C", "D", "D-FILE", "E", "E-FILE", "F", "F-FILE", "G"] {
        assert!(h.store.item(id).unwrap().is_some(), "{id} row survives");
    }
    assert_eq!(h.store.folder_population(ACCOUNT, "C").unwrap(), Some(false));
    assert_eq!(h.store.folder_population(ACCOUNT, "G").unwrap(), Some(false));
    assert_eq!(h.store.folder_population(ACCOUNT, "B").unwrap(), Some(true));

    assert_eq!(outcome.folders_pruned, 3, "C, G and H");
    assert_eq!(outcome.placeholders_removed, 3, "c.txt, g.txt and h.txt");
    assert_eq!(outcome.rows_removed, 4, "c.txt, h.txt, g.txt and H");
    assert_eq!(outcome.folders_kept, 5, "A, B, D, E and F");

    // The delta cursor is untouched: nothing is re-enumerated.
    assert!(h.store.account(ACCOUNT).unwrap().unwrap().delta_link.is_some());

    // Running it again on the converted tree has nothing left to remove.
    let again = h.engine.convert_to_lazy(|_, _| true).unwrap();
    assert_eq!(again.placeholders_removed, 0);
    assert_eq!(again.rows_removed, 0);

    // Opening a pruned folder repopulates it exactly like a lazy account.
    h.mock_children(&items, false).await;
    let opened = h.engine.populate_folder(Path::new("G")).await.unwrap();
    assert!(opened.files >= 1, "g.txt is published again");
    assert_eq!(Placeholder::state(&h.path("G/g.txt")), PlaceholderState::Dehydrated);
    assert!(h.path("G/H").is_dir(), "the lookahead republishes H");
    assert_eq!(
        Placeholder::directory_population(&h.path("G")),
        DirectoryPopulation::Populated
    );
    assert!(h.store.item("G-FILE").unwrap().is_some());
}

#[tokio::test]
async fn a_placeholder_filled_during_the_conversion_survives_it() {
    // The conversion decides what to prune in one pass and unlinks in another.
    // On a large tree minutes separate them, and in between the helper can
    // fill a placeholder a user just opened, and the user can edit it. Those
    // bytes have never been uploaded, so unlinking them is data loss.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState, XATTR_STATE};

    let h = Harness::with_config(Config {
        client_id: "test".into(),
        download_new: DownloadPolicy::Never,
        on_demand: false,
        ..Config::default()
    })
    .await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("X", ROOT, "X"),
            file("X-FILE", "X", "x.txt", "c1", 10, "2026-08-20T10:00:00Z"),
            folder("Y", "X", "Y"),
            file("Y-FILE", "Y", "y.txt", "c1", 20, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    assert_eq!(h.sync().await.failed, 0);
    h.engine.materialize_all(|_, _| true).unwrap();
    h.engine.enable_on_demand();
    assert_eq!(
        Placeholder::state(&h.path("X/x.txt")),
        PlaceholderState::Dehydrated
    );

    // Pruning runs deepest first, so Y is unlinked before X is examined. Fill
    // X's placeholder in the gap between them, exactly as a foreground read
    // would.
    let filled = h.path("X/x.txt");
    let outcome = h
        .engine
        .convert_to_lazy(|done, _| {
            if done == 1 {
                std::fs::write(&filled, b"opened while converting").unwrap();
                xattr::set(&filled, XATTR_STATE, b"hydrated").unwrap();
            }
            true
        })
        .unwrap();

    assert_eq!(
        std::fs::read(h.path("X/x.txt")).unwrap(),
        b"opened while converting",
        "the bytes fetched during the conversion must survive it"
    );
    assert!(
        h.store.item("X-FILE").unwrap().is_some(),
        "dropping the row would orphan the file: no id, no state, nothing to sync it back"
    );
    assert!(
        outcome.placeholders_removed >= 1,
        "the untouched placeholder under Y is still pruned"
    );
}
