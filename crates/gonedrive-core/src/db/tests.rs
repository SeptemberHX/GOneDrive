use super::*;

fn store_with_account() -> (Store, Account) {
    let store = Store::open_in_memory().unwrap();
    let account = Account {
        id: "acct".into(),
        drive_id: "drive1".into(),
        root_item_id: Some("ROOT".into()),
        display_name: Some("Test User".into()),
        address: Some("t@example.com".into()),
        root_local_path: PathBuf::from("/home/u/OneDrive"),
        delta_link: None,
        enum_link: None,
        quota_total: Some(1024),
        quota_used: Some(10),
        root_dev: None,
    };
    store.upsert_account(&account).unwrap();

    let mut root = ItemRecord::new("acct", "ROOT", "root");
    root.is_folder = true;
    root.state = ItemState::Synced;
    store.upsert_item(&root).unwrap();
    (store, account)
}

fn folder(id: &str, parent: &str, name: &str) -> ItemRecord {
    let mut item = ItemRecord::new("acct", id, name);
    item.parent_id = Some(parent.into());
    item.is_folder = true;
    item
}

fn file(id: &str, parent: &str, name: &str) -> ItemRecord {
    let mut item = ItemRecord::new("acct", id, name);
    item.parent_id = Some(parent.into());
    item
}

#[test]
fn migration_is_idempotent() {
    let store = Store::open_in_memory().unwrap();
    store.migrate().unwrap();
    store.migrate().unwrap();
    assert!(store.accounts().unwrap().is_empty());
}

#[test]
fn version_ten_delta_staging_migrates_without_discarding_rows() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            r#"CREATE TABLE accounts (id TEXT PRIMARY KEY);
             CREATE TABLE delta_batches (
                 account_id TEXT PRIMARY KEY,
                 mode TEXT NOT NULL,
                 generation INTEGER,
                 item_count INTEGER NOT NULL DEFAULT 0,
                 page_count INTEGER NOT NULL DEFAULT 0,
                 next_sequence INTEGER NOT NULL DEFAULT 0,
                 complete INTEGER NOT NULL DEFAULT 0,
                 delta_link TEXT
             );
             CREATE TABLE delta_staging (
                 account_id TEXT NOT NULL,
                 item_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL,
                 payload TEXT NOT NULL,
                 PRIMARY KEY (account_id, item_id)
             );
             INSERT INTO accounts (id) VALUES ('acct');
             INSERT INTO delta_staging (account_id, item_id, sequence, payload)
                 VALUES ('acct', 'F1', 7, '{"id":"F1","deleted":null}');
             INSERT INTO delta_staging (account_id, item_id, sequence, payload)
                 VALUES ('acct', 'D1', 8, '{"id":"D1","deleted":{}}');
             PRAGMA user_version = 10;"#,
        )
        .unwrap();

    let store = Store::from_connection(connection).unwrap();
    let staged: (String, i64, i64) = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT payload, applied, deleted FROM delta_staging WHERE item_id = 'F1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?)
        })
        .unwrap();
    assert_eq!(staged, (r#"{"id":"F1","deleted":null}"#.into(), 0, 0));
    let deleted: i64 = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT deleted FROM delta_staging WHERE item_id = 'D1'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(deleted, 1);
    let version: i64 = store
        .with_conn(|conn| Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn activity_persists_newest_first_and_is_trimmed() {
    let store = Store::open_in_memory().unwrap();
    for i in 0..5 {
        store
            .push_activity(
                "downloaded",
                &format!("f{i}.txt"),
                &format!("OneDrive/f{i}.txt"),
                "",
                1000 + i,
                3,
            )
            .unwrap();
    }
    let recent = store.recent_activity(10).unwrap();
    // Trimmed to the last 3, newest first.
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].name, "f4.txt");
    assert_eq!(recent[0].path, "OneDrive/f4.txt");
    assert_eq!(recent[2].name, "f2.txt");
    assert_eq!(recent[0].kind, "downloaded");
    assert_eq!(recent[0].when, 1004);
}

#[test]
fn upsert_updates_remote_fields_but_not_local_stats() {
    let (store, _) = store_with_account();
    let mut item = file("F1", "ROOT", "a.txt");
    item.size = 10;
    store.upsert_item(&item).unwrap();
    store
        .set_local_stat(
            "F1",
            Some(OffsetDateTime::from_unix_timestamp(1000).unwrap()),
            Some(10),
            Some(42),
        )
        .unwrap();

    // A later delta page carries no local knowledge; it must not erase ours.
    let mut updated = file("F1", "ROOT", "a.txt");
    updated.size = 20;
    updated.ctag = Some("c2".into());
    store.upsert_item(&updated).unwrap();

    let got = store.item("F1").unwrap().unwrap();
    assert_eq!(got.size, 20);
    assert_eq!(got.ctag.as_deref(), Some("c2"));
    assert_eq!(got.local_ino, Some(42));
    assert_eq!(got.local_size, Some(10));
}

#[test]
fn folder_child_count_is_preserved_when_a_later_remote_record_omits_it() {
    let (store, _) = store_with_account();
    let mut folder = folder("D1", "ROOT", "Documents");
    folder.child_count = Some(7);
    store.upsert_item(&folder).unwrap();

    // Metadata-only delta responses are allowed to omit the folder facet. An
    // omission is not an authoritative zero and must not erase the summary the
    // chooser already knows.
    let mut later = folder.clone();
    later.child_count = None;
    store.upsert_item(&later).unwrap();
    assert_eq!(store.item("D1").unwrap().unwrap().child_count, Some(7));

    // An explicit zero is meaningful (the remote folder became empty), and a
    // completed children pagination can refine the cached value as well.
    later.child_count = Some(0);
    store.upsert_item(&later).unwrap();
    assert_eq!(store.item("D1").unwrap().unwrap().child_count, Some(0));
    store.set_folder_child_count("acct", "D1", 3).unwrap();
    assert_eq!(store.item("D1").unwrap().unwrap().child_count, Some(3));
}

#[test]
fn population_watch_candidates_include_only_published_populated_directories() {
    let (store, _) = store_with_account();
    for item in [
        folder("READY", "ROOT", "Ready"),
        folder("PARTIAL", "ROOT", "Partial"),
        folder("REMOTE_ONLY", "READY", "Remote only"),
    ] {
        store.upsert_item(&item).unwrap();
    }
    store
        .set_local_stat("READY", None, Some(0), Some(101))
        .unwrap();
    store
        .set_local_stat("PARTIAL", None, Some(0), Some(102))
        .unwrap();
    store.set_folder_population("acct", "READY", true).unwrap();
    store
        .set_folder_population("acct", "PARTIAL", false)
        .unwrap();
    store
        .set_folder_population("acct", "REMOTE_ONLY", true)
        .unwrap();

    let candidates = store.population_watch_candidates("acct").unwrap();
    assert_eq!(
        candidates
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        vec!["READY"]
    );
}

#[test]
fn relative_path_walks_the_parent_chain() {
    let (store, account) = store_with_account();
    store
        .upsert_item(&folder("D1", "ROOT", "Documents"))
        .unwrap();
    store.upsert_item(&folder("D2", "D1", "Reports")).unwrap();
    store.upsert_item(&file("F1", "D2", "q3.docx")).unwrap();

    assert_eq!(
        store.relative_path("F1").unwrap().unwrap(),
        PathBuf::from("Documents/Reports/q3.docx")
    );
    // The root maps to the sync root itself, not to a "root" directory.
    assert_eq!(
        store.relative_path("ROOT").unwrap().unwrap(),
        PathBuf::new()
    );
    assert_eq!(
        store.local_path(&account, "F1").unwrap().unwrap(),
        PathBuf::from("/home/u/OneDrive/Documents/Reports/q3.docx")
    );
}

#[test]
fn relative_path_is_none_when_an_ancestor_is_missing() {
    let (store, _) = store_with_account();
    // A delta page can mention a child before its parent has arrived.
    store
        .upsert_item(&file("F1", "NOT_YET", "orphan.txt"))
        .unwrap();
    assert!(store.relative_path("F1").unwrap().is_none());
    assert!(store.relative_path("NOBODY").unwrap().is_none());
}

#[test]
fn renaming_a_folder_moves_its_whole_subtree() {
    let (store, _) = store_with_account();
    store.upsert_item(&folder("D1", "ROOT", "Old")).unwrap();
    store.upsert_item(&file("F1", "D1", "a.txt")).unwrap();

    let mut renamed = folder("D1", "ROOT", "New");
    store.upsert_item(&renamed).unwrap();
    assert_eq!(
        store.relative_path("F1").unwrap().unwrap(),
        PathBuf::from("New/a.txt")
    );

    // Moving the folder elsewhere moves the child too — still one UPDATE.
    store.upsert_item(&folder("D2", "ROOT", "Archive")).unwrap();
    renamed.parent_id = Some("D2".into());
    store.upsert_item(&renamed).unwrap();
    assert_eq!(
        store.relative_path("F1").unwrap().unwrap(),
        PathBuf::from("Archive/New/a.txt")
    );
}

#[test]
fn case_insensitive_names_collide_the_way_onedrive_does() {
    let (store, _) = store_with_account();
    store
        .upsert_item(&file("F1", "ROOT", "Report.docx"))
        .unwrap();

    // A *different* item with the same case-folded name is a real conflict
    // and must be refused rather than silently creating a second row.
    let err = store.upsert_item(&file("F2", "ROOT", "report.docx"));
    assert!(err.is_err(), "expected a uniqueness violation");

    // Lookup ignores case, matching how the service resolves names.
    let found = store.child_by_name("acct", "ROOT", "REPORT.DOCX").unwrap();
    assert_eq!(found.unwrap().id, "F1");
}

#[test]
fn same_name_in_different_folders_is_fine() {
    let (store, _) = store_with_account();
    store.upsert_item(&folder("D1", "ROOT", "a")).unwrap();
    store.upsert_item(&folder("D2", "ROOT", "b")).unwrap();
    store.upsert_item(&file("F1", "D1", "notes.txt")).unwrap();
    store.upsert_item(&file("F2", "D2", "notes.txt")).unwrap();
    assert_eq!(store.children("acct", "D1").unwrap().len(), 1);
}

#[test]
fn deleting_a_folder_removes_its_descendants() {
    let (store, _) = store_with_account();
    store.upsert_item(&folder("D1", "ROOT", "Docs")).unwrap();
    store.upsert_item(&folder("D2", "D1", "Sub")).unwrap();
    store.upsert_item(&file("F1", "D2", "deep.txt")).unwrap();
    store.upsert_item(&file("F2", "ROOT", "keep.txt")).unwrap();

    assert_eq!(store.delete_subtree("D1").unwrap(), 3);
    assert!(store.item("F1").unwrap().is_none());
    assert!(store.item("F2").unwrap().is_some());
}

#[test]
fn batch_upsert_is_atomic() {
    let (store, _) = store_with_account();
    let batch = vec![
        file("F1", "ROOT", "one.txt"),
        file("F2", "ROOT", "two.txt"),
        // Collides case-insensitively with F1, so the batch must roll back.
        file("F3", "ROOT", "ONE.TXT"),
    ];
    assert!(store.upsert_items(&batch).is_err());
    assert!(store.item("F1").unwrap().is_none());
    assert!(store.item("F2").unwrap().is_none());
}

#[test]
fn queued_work_is_deduplicated_per_item() {
    let (store, _) = store_with_account();
    store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();

    // An editor saving three times in a second must not produce three uploads.
    for _ in 0..3 {
        store
            .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
            .unwrap();
    }
    assert_eq!(store.pending_count("acct").unwrap(), 1);

    // A different kind of work on the same item is a separate operation.
    store
        .enqueue("acct", OpKind::Download, Some("F1"), None, None)
        .unwrap();
    assert_eq!(store.pending_count("acct").unwrap(), 2);
}

#[test]
fn queue_scopes_are_disjoint_and_downloads_sort_first() {
    let (store, _) = store_with_account();
    store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();
    store
        .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
        .unwrap();
    store
        .enqueue("acct", OpKind::Download, Some("F1"), None, None)
        .unwrap();

    let all = store.due_ops("acct", 10).unwrap();
    assert_eq!(all[0].kind, OpKind::Download);
    assert_eq!(store.due_download_ops("acct", 10).unwrap().len(), 1);
    assert_eq!(store.due_outbound_ops("acct", 10).unwrap().len(), 0);

    store.complete_op(all[0].id).unwrap();
    assert_eq!(store.due_outbound_ops("acct", 10).unwrap().len(), 1);

    store
        .enqueue("acct", OpKind::DeleteRemote, Some("F1"), None, None)
        .unwrap();
    assert_eq!(store.due_outbound_ops("acct", 10).unwrap().len(), 2);
    let non_delete = store.due_non_delete_outbound_ops("acct", 10).unwrap();
    assert_eq!(non_delete.len(), 1);
    assert_eq!(non_delete[0].kind, OpKind::Upload);
}

#[test]
fn conditional_queue_keeps_the_decision_time_remote_version() {
    let (store, _) = store_with_account();
    let mut item = file("F1", "ROOT", "a.txt");
    item.etag = Some("e1".into());
    store.upsert_item(&item).unwrap();
    store
        .enqueue_conditional(
            "acct",
            OpKind::Upload,
            "F1",
            Some(Path::new("/sync/a.txt")),
            Some("ROOT"),
            item.etag.as_deref(),
        )
        .unwrap();

    item.etag = Some("e2".into());
    store.upsert_item(&item).unwrap();
    let queued = store.due_ops("acct", 10).unwrap();
    assert_eq!(queued[0].precondition.as_deref(), Some("e1"));
}

#[test]
fn guarded_deletes_accumulate_across_batches_before_becoming_due() {
    let (store, _) = store_with_account();
    for index in 0..20 {
        store
            .upsert_item(&file(&format!("F{index}"), "ROOT", &format!("{index}.txt")))
            .unwrap();
    }

    let first_ids: Vec<_> = (0..6).map(|index| format!("F{index}")).collect();
    let first: Vec<_> = first_ids
        .iter()
        .map(|id| (id.as_str(), Some("e1")))
        .collect();
    let first_outcome = store
        .enqueue_guarded_deletes("acct", &first, 10, 0.5, std::time::Duration::from_secs(30))
        .unwrap();
    assert!(!first_outcome.refused);
    assert_eq!(first_outcome.impacted, 6);
    assert_eq!(store.pending_count("acct").unwrap(), 6);
    assert!(store.due_outbound_ops("acct", 100).unwrap().is_empty());

    let first_due: i64 = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT MAX(next_attempt_at) FROM pending_ops WHERE account_id = 'acct'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    store
        .enqueue_guarded_deletes("acct", &first, 10, 0.5, std::time::Duration::from_secs(300))
        .unwrap();
    let duplicate_due: i64 = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT MAX(next_attempt_at) FROM pending_ops WHERE account_id = 'acct'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        duplicate_due, first_due,
        "duplicate watcher reports must not extend the quiet window"
    );

    let second_ids: Vec<_> = (6..12).map(|index| format!("F{index}")).collect();
    let second: Vec<_> = second_ids
        .iter()
        .map(|id| (id.as_str(), Some("e1")))
        .collect();
    let second_outcome = store
        .enqueue_guarded_deletes("acct", &second, 10, 0.5, std::time::Duration::from_secs(30))
        .unwrap();
    assert!(second_outcome.refused);
    assert_eq!(second_outcome.impacted, 12);
    assert_eq!(second_outcome.total, 21);
    assert_eq!(store.pending_count("acct").unwrap(), 0);
}

#[test]
fn restore_then_redelete_starts_a_fresh_quiet_window() {
    let (store, _) = store_with_account();
    store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();
    let delete = [("F1", Some("e1"))];
    store
        .enqueue_guarded_deletes("acct", &delete, 10, 0.5, std::time::Duration::from_secs(30))
        .unwrap();
    let first_due: i64 = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT next_attempt_at FROM pending_ops WHERE item_id = 'F1'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();

    assert_eq!(store.cancel_pending_delete("acct", "F1").unwrap(), 1);
    store
        .enqueue_guarded_deletes(
            "acct",
            &delete,
            10,
            0.5,
            std::time::Duration::from_secs(300),
        )
        .unwrap();
    let second_due: i64 = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT next_attempt_at FROM pending_ops WHERE item_id = 'F1'",
                [],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert!(second_due > first_due);
}

#[test]
fn extending_the_delete_window_invalidates_an_already_selected_queue_row() {
    let (store, _) = store_with_account();
    for index in 0..20 {
        store
            .upsert_item(&file(&format!("F{index}"), "ROOT", &format!("{index}.txt")))
            .unwrap();
    }

    let selected_id = store
        .enqueue_conditional("acct", OpKind::DeleteRemote, "F0", None, None, Some("e1"))
        .unwrap();
    assert!(store.due_pending_op(selected_id).unwrap().is_some());

    store
        .enqueue_guarded_deletes(
            "acct",
            &[("F1", Some("e1"))],
            10,
            0.5,
            std::time::Duration::from_secs(30),
        )
        .unwrap();

    assert!(
        store.due_pending_op(selected_id).unwrap().is_none(),
        "execution-time reload must observe the newly extended quiet window"
    );
}

#[test]
fn late_mutation_response_does_not_resurrect_a_deleted_item() {
    let (store, _) = store_with_account();
    let mut item = file("F1", "ROOT", "a.txt");
    item.etag = Some("e1".into());
    store.upsert_item(&item).unwrap();
    store.delete_subtree("F1").unwrap();

    item.etag = Some("e2".into());
    assert!(!store
        .upsert_item_after_mutation(&item, Some("e1"), true)
        .unwrap());
    assert!(store.item("F1").unwrap().is_none());
}

#[test]
fn subtree_count_includes_every_descendant() {
    let (store, _) = store_with_account();
    store.upsert_item(&folder("D1", "ROOT", "Docs")).unwrap();
    store.upsert_item(&folder("D2", "D1", "Nested")).unwrap();
    store.upsert_item(&file("F1", "D2", "a.txt")).unwrap();
    assert_eq!(store.subtree_item_count("D1").unwrap(), 3);
}

#[test]
fn edited_move_queues_metadata_and_content_together() {
    let (store, _) = store_with_account();
    store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();

    store
        .enqueue_move_and_upload(
            "acct",
            "F1",
            Path::new("/sync/a.txt"),
            Path::new("/sync/b.txt"),
        )
        .unwrap();

    let queued = store.due_ops("acct", 10).unwrap();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[0].kind, OpKind::MoveRemote);
    assert_eq!(queued[1].kind, OpKind::Upload);
    assert!(queued
        .iter()
        .all(|op| op.local_path.as_deref() == Some(Path::new("/sync/b.txt"))));
}

#[test]
fn failed_ops_back_off_and_come_back_when_due() {
    let (store, _) = store_with_account();
    store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();
    store
        .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
        .unwrap();

    let op = store.due_ops("acct", 10).unwrap().remove(0);
    let attempts = store
        .fail_op(op.id, "network down", std::time::Duration::from_secs(3600))
        .unwrap()
        .unwrap();
    assert_eq!(attempts, 1);
    // Still queued, but not yet due.
    assert_eq!(store.pending_count("acct").unwrap(), 1);
    assert!(store.due_ops("acct", 10).unwrap().is_empty());

    // Re-queueing the same work clears the backoff, which is what a fresh
    // local edit should do.
    store
        .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
        .unwrap();
    assert_eq!(store.due_ops("acct", 10).unwrap().len(), 1);

    store.complete_op(op.id).unwrap();
    assert_eq!(store.pending_count("acct").unwrap(), 0);
}

#[test]
fn upload_sessions_survive_for_resume() {
    let (store, _) = store_with_account();
    let session = UploadSessionRecord {
        key: "F1".into(),
        account_id: "acct".into(),
        upload_url: "https://upload".into(),
        expires_at: None,
        committed_offset: 0,
        total_size: 100 * 1024 * 1024,
        source_fingerprint: Some("dev:ino:size:time".into()),
    };
    store.save_upload_session(&session).unwrap();
    store
        .set_upload_offset("acct", "F1", 20 * 1024 * 1024)
        .unwrap();

    let got = store.upload_session("acct", "F1").unwrap().unwrap();
    assert_eq!(got.committed_offset, 20 * 1024 * 1024);
    assert_eq!(got.total_size, 100 * 1024 * 1024);

    store.delete_upload_session("acct", "F1").unwrap();
    assert!(store.upload_session("acct", "F1").unwrap().is_none());
}

#[test]
fn upload_sessions_are_isolated_by_account() {
    let (store, first) = store_with_account();
    let mut second = first.clone();
    second.id = "acct-2".into();
    second.drive_id = "drive-2".into();
    second.root_item_id = None;
    second.root_local_path = PathBuf::from("/home/u/OtherDrive");
    store.upsert_account(&second).unwrap();

    for (account_id, upload_url) in [
        ("acct", "https://upload/first"),
        ("acct-2", "https://upload/second"),
    ] {
        store
            .save_upload_session(&UploadSessionRecord {
                key: "same-item-id".into(),
                account_id: account_id.into(),
                upload_url: upload_url.into(),
                expires_at: None,
                committed_offset: 0,
                total_size: 42,
                source_fingerprint: Some("fingerprint".into()),
            })
            .unwrap();
    }

    store.set_upload_offset("acct", "same-item-id", 20).unwrap();
    let first = store
        .upload_session("acct", "same-item-id")
        .unwrap()
        .unwrap();
    let second = store
        .upload_session("acct-2", "same-item-id")
        .unwrap()
        .unwrap();
    assert_eq!(first.upload_url, "https://upload/first");
    assert_eq!(first.committed_offset, 20);
    assert_eq!(second.upload_url, "https://upload/second");
    assert_eq!(second.committed_offset, 0);
}

#[test]
fn removing_an_account_cascades() {
    let (store, _) = store_with_account();
    store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();
    store
        .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
        .unwrap();

    store.remove_account("acct").unwrap();
    assert!(store.item("F1").unwrap().is_none());
    assert_eq!(store.pending_count("acct").unwrap(), 0);
}

#[test]
fn delta_link_round_trips_verbatim() {
    let (store, _) = store_with_account();
    // Opaque and full of characters we must not mangle.
    let link = "https://graph.microsoft.com/v1.0/me/drive/root/delta?token=aTEuMDs...%3d%3d";
    store.set_delta_link("acct", Some(link)).unwrap();
    assert_eq!(
        store
            .account("acct")
            .unwrap()
            .unwrap()
            .delta_link
            .as_deref(),
        Some(link)
    );

    store.set_delta_link("acct", None).unwrap();
    assert!(store.account("acct").unwrap().unwrap().delta_link.is_none());
}

#[test]
fn replacing_a_staged_item_makes_its_final_occurrence_pending_again() {
    let (store, _) = store_with_account();
    store.begin_delta_batch("acct", "initial", Some(1)).unwrap();
    store
        .stage_delta_page(
            "acct",
            &[("F1".into(), "first".into(), false)],
            Some("next"),
            None,
        )
        .unwrap();
    assert_eq!(store.unapplied_delta_upserts("acct").unwrap(), ["first"]);

    store.mark_staged_applied("acct", &["F1".into()]).unwrap();
    assert!(store.unapplied_delta_upserts("acct").unwrap().is_empty());

    store
        .stage_delta_page(
            "acct",
            &[("F1".into(), "last".into(), false)],
            Some("next-2"),
            None,
        )
        .unwrap();
    assert_eq!(store.unapplied_delta_upserts("acct").unwrap(), ["last"]);

    store
        .stage_delta_page(
            "acct",
            &[("F1".into(), "deleted".into(), true)],
            None,
            Some("done"),
        )
        .unwrap();
    assert!(store.unapplied_delta_upserts("acct").unwrap().is_empty());
    assert_eq!(store.unapplied_delta_deletes("acct").unwrap(), ["deleted"]);
    assert!(store.staged_upsert_ids("acct").unwrap().is_empty());
}

#[test]
fn remote_package_exclusions_persist_as_a_tree_and_prune_by_generation() {
    let (store, _) = store_with_account();
    store
        .upsert_remote_exclusion("acct", "PACKAGE", Some("ROOT"), "package", Some(1))
        .unwrap();
    store
        .upsert_remote_exclusion(
            "acct",
            "CHILD",
            Some("PACKAGE"),
            "package-descendant",
            Some(1),
        )
        .unwrap();
    assert!(store.remote_is_excluded("acct", "PACKAGE").unwrap());
    assert!(store.remote_is_excluded("acct", "CHILD").unwrap());

    store
        .upsert_remote_exclusion("acct", "PACKAGE", Some("ROOT"), "package", Some(2))
        .unwrap();
    store.prune_remote_exclusions("acct", 2).unwrap();
    assert!(store.remote_is_excluded("acct", "PACKAGE").unwrap());
    assert!(!store.remote_is_excluded("acct", "CHILD").unwrap());

    store
        .upsert_remote_exclusion(
            "acct",
            "CHILD",
            Some("PACKAGE"),
            "package-descendant",
            Some(2),
        )
        .unwrap();
    store
        .delete_remote_exclusion_subtree("acct", "PACKAGE")
        .unwrap();
    assert!(!store.remote_is_excluded("acct", "PACKAGE").unwrap());
    assert!(!store.remote_is_excluded("acct", "CHILD").unwrap());
}

#[test]
fn a_resync_sweep_visits_children_before_their_parents() {
    // A parent folder handled before its child folder is found non-empty,
    // detaches the subtree, and strands the child directory on disk with no
    // record — the next scan then re-creates the remotely deleted folder.
    let (store, _account) = store_with_account();
    store.upsert_item(&folder("A", "ROOT", "A")).unwrap();
    store.upsert_item(&folder("B", "A", "B")).unwrap();
    store.upsert_item(&folder("C", "B", "C")).unwrap();
    store.upsert_item(&file("f", "C", "deep.txt")).unwrap();
    store.upsert_item(&file("g", "ROOT", "top.txt")).unwrap();
    store.upsert_item(&folder("D", "ROOT", "D")).unwrap();
    // Only D survived the re-enumeration.
    store.mark_seen(&["D".to_string()], 1).unwrap();

    let missed: Vec<String> = store
        .items_missed_by_resync("acct", 1)
        .unwrap()
        .into_iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(missed, ["f", "C", "B", "g", "A"]);
}

#[test]
fn an_item_written_during_a_resync_is_not_swept_by_it() {
    // The enumeration snapshot predates the upload; the record must still
    // survive the sweep or the user's freshly uploaded file is deleted.
    let (store, _account) = store_with_account();
    store.upsert_item(&file("old", "ROOT", "old.txt")).unwrap();
    let generation = store.begin_resync("acct").unwrap();
    store.upsert_item(&file("new", "ROOT", "new.txt")).unwrap();

    let missed: Vec<String> = store
        .items_missed_by_resync("acct", generation)
        .unwrap()
        .into_iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(missed, ["old"]);
}

#[test]
fn by_item_queue_deletions_use_an_index() {
    // Completing a hydration, releasing space and dropping a subtree all
    // delete by item. Scanning the queue for each one turns a whole-drive pin
    // into one scan per download.
    let store = Store::open_in_memory().unwrap();
    let plan: String = store
        .with_conn(|conn| {
            Ok(conn.query_row(
                "EXPLAIN QUERY PLAN
                 DELETE FROM pending_ops WHERE account_id = 'a' AND item_id = 'i'",
                [],
                |row| row.get::<_, String>(3),
            )?)
        })
        .unwrap();
    assert!(
        plan.contains("pending_ops_by_item"),
        "expected an index scan, got: {plan}"
    );
}

#[test]
fn an_existing_store_gains_the_by_item_queue_index() {
    // A fresh store gets the index from the base schema; every store already
    // on disk only gets it from the migration, and those are the ones with a
    // queue large enough for the missing index to matter.
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            r#"CREATE TABLE accounts (id TEXT PRIMARY KEY);
             CREATE TABLE pending_ops (
                 id              INTEGER PRIMARY KEY AUTOINCREMENT,
                 account_id      TEXT NOT NULL,
                 kind            TEXT NOT NULL,
                 item_id         TEXT,
                 local_path      TEXT,
                 payload         TEXT,
                 precondition    TEXT,
                 attempts        INTEGER NOT NULL DEFAULT 0,
                 next_attempt_at INTEGER NOT NULL DEFAULT 0,
                 last_error      TEXT
             );
             INSERT INTO accounts (id) VALUES ('acct');
             PRAGMA user_version = 14;"#,
        )
        .unwrap();

    let store = Store::from_connection(connection).unwrap();
    let indexes: Vec<String> = store
        .with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master
                  WHERE type = 'index' AND tbl_name = 'pending_ops'",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .unwrap();
    assert!(
        indexes.iter().any(|name| name == "pending_ops_by_item"),
        "migration must add the index: {indexes:?}"
    );
}
