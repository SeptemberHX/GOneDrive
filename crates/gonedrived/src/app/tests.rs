use std::sync::Arc;
use std::time::Duration;

use gonedrive_core::sync::engine::QueueScope;
use gonedrive_ipc::reader_policy::FILE_MANAGER_COPY_READER;

use super::inflight::PathLocks;
use super::{
    core_error_status, decide_stage, is_account_level_state, is_browsing_reader,
    keyring_locked_message, missing_account_retry_due, outbound_scope, pending_snapshot,
    phase_completion_state, phase_state, remote_poll_ready, summary_has_reportable_activity,
    sync_barrier_ready, update_pending_status, StagePlan, PARTIAL_HYDRATION_CEILING,
};

#[test]
fn routine_delta_enumeration_does_not_generate_an_info_log() {
    use gonedrive_core::sync::engine::SyncSummary;

    let routine = SyncSummary {
        enumerated: 1,
        not_downloaded: 1,
        still_queued: 3,
        ..Default::default()
    };
    assert!(!summary_has_reportable_activity(&routine));

    for reportable in [
        SyncSummary {
            downloaded: 1,
            ..Default::default()
        },
        SyncSummary {
            failed: 1,
            ..Default::default()
        },
        SyncSummary {
            refused_deletions: 1,
            ..Default::default()
        },
    ] {
        assert!(summary_has_reportable_activity(&reportable));
    }
}

#[test]
fn pending_status_comes_from_the_whole_durable_queue_with_paths() {
    use gonedrive_core::db::{Account, ItemRecord, ItemState, OpKind, Store};

    let store = Store::open_in_memory().unwrap();
    let account = Account {
        id: "acct".into(),
        drive_id: "drive".into(),
        root_item_id: Some("ROOT".into()),
        display_name: None,
        address: None,
        root_local_path: "/home/u/OneDrive".into(),
        delta_link: None,
        enum_link: None,
        quota_total: None,
        quota_used: None,
        root_dev: None,
    };
    store.upsert_account(&account).unwrap();
    let root = ItemRecord::root("acct", "ROOT");
    store.upsert_item(&root).unwrap();
    let mut remote = ItemRecord::new("acct", "REMOTE", "remote.docx");
    remote.parent_id = Some("ROOT".into());
    remote.state = ItemState::DirtyRemote;
    store.upsert_item(&remote).unwrap();
    store
        .enqueue(
            "acct",
            OpKind::Download,
            Some("REMOTE"),
            Some(std::path::Path::new("/home/u/OneDrive/remote.docx")),
            None,
        )
        .unwrap();
    store
        .enqueue(
            "acct",
            OpKind::Upload,
            None,
            Some(std::path::Path::new("/home/u/OneDrive/local.txt")),
            None,
        )
        .unwrap();

    let (count, items) = pending_snapshot(&store).unwrap();
    assert_eq!(count, 2);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].kind, "download");
    assert_eq!(items[0].path, "OneDrive/remote.docx");
    assert_eq!(items[1].kind, "upload");
    assert_eq!(items[1].path, "OneDrive/local.txt");

    let mut paused = gonedrive_ipc::SyncStatus {
        state: "paused".into(),
        ..Default::default()
    };
    update_pending_status(&store, &mut paused).unwrap();
    assert_eq!(paused.state, "paused");
    assert_eq!(paused.pending, 2);
    assert_eq!(paused.pending_items.len(), 2);
    assert_eq!(paused.pending_items[0].path, "OneDrive/remote.docx");
    assert_eq!(paused.pending_items[1].path, "OneDrive/local.txt");
}

#[test]
fn a_locked_keyring_is_not_reported_as_an_expired_login() {
    let (state, message) = core_error_status(&gonedrive_core::Error::KeyringLocked(
        "prompt dismissed".into(),
    ));
    assert_eq!(state, "keyring_locked");
    assert_eq!(message, keyring_locked_message());

    let (state, _) = core_error_status(&gonedrive_core::Error::AuthRequired("revoked".into()));
    assert_eq!(state, "signed_out");
}

#[test]
fn only_transient_startup_auth_failures_retry_without_user_input() {
    assert!(missing_account_retry_due(false, "offline"));
    assert!(!missing_account_retry_due(false, "keyring_locked"));
    assert!(!missing_account_retry_due(false, "signed_out"));
    assert!(!missing_account_retry_due(false, "error"));

    assert!(missing_account_retry_due(true, "keyring_locked"));
    assert!(missing_account_retry_due(true, "signed_out"));
}

#[test]
fn delta_waits_for_an_active_outbound_slice_to_commit() {
    assert!(remote_poll_ready(true, false, false));
    assert!(!remote_poll_ready(true, true, false));
    assert!(!remote_poll_ready(false, false, false));
    // A delta applied mid-scan moves files before it updates their records.
    assert!(!remote_poll_ready(true, false, true));
}

#[test]
fn transport_diagnostics_include_the_underlying_cause() {
    let transport = reqwest::Client::new().get("://").build().unwrap_err();
    let (_, message) = core_error_status(&gonedrive_core::Error::Http(transport));
    assert!(message.contains("builder error"), "{message}");
    assert!(message.contains("relative URL"), "{message}");
}

#[test]
fn remote_deletes_wait_for_a_successful_complete_local_verification() {
    assert_eq!(
        outbound_scope(false, false, false),
        QueueScope::OutboundNonDelete,
        "startup verification has not completed"
    );
    assert_eq!(
        outbound_scope(true, false, true),
        QueueScope::OutboundNonDelete,
        "a periodic verification may still discover more delete roots"
    );
    assert_eq!(
        outbound_scope(false, true, true),
        QueueScope::OutboundNonDelete,
        "a scoped scan may still add roots to the pending deletion wave"
    );
    assert_eq!(outbound_scope(false, false, true), QueueScope::Outbound);
}

#[test]
fn one_finished_phase_cannot_report_idle_while_another_is_active() {
    assert_eq!(phase_state(false, 2), "syncing");
    assert_eq!(phase_state(false, 1), "syncing");
    assert_eq!(phase_state(false, 0), "idle");
    assert_eq!(phase_state(true, 0), "paused");
    assert_eq!(phase_state(true, 3), "paused");
    assert_eq!(phase_completion_state(false, 0, true), "error");
    assert_eq!(phase_completion_state(false, 0, false), "idle");
    assert_eq!(phase_completion_state(false, 1, true), "syncing");
    assert_eq!(phase_completion_state(true, 0, true), "paused");
}

#[test]
fn explicit_sync_waits_for_observed_local_work_but_not_a_full_scan() {
    assert!(!sync_barrier_ready(
        true, false, false, false, false, false, false,
    ));
    assert!(!sync_barrier_ready(
        false, false, false, false, true, false, false,
    ));
    assert!(!sync_barrier_ready(
        false, true, false, false, false, false, false,
    ));
    assert!(!sync_barrier_ready(
        false, false, false, false, false, true, false,
    ));
    assert!(sync_barrier_ready(
        false, false, false, false, false, false, false,
    ));
}

#[tokio::test]
async fn hydration_locks_cover_the_whole_request_for_one_path() {
    let locks = Arc::new(PathLocks::default());
    let first = locks.acquire(std::path::Path::new("/tmp/a")).await;
    let waiting_locks = locks.clone();
    let mut second =
        tokio::spawn(async move { waiting_locks.acquire(std::path::Path::new("/tmp/a")).await });

    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err(),
        "a second fill of the same path must wait"
    );
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("second request should proceed after Filled releases the first")
        .unwrap();
}

#[tokio::test]
async fn overlapping_space_operations_acquire_path_locks_without_deadlock() {
    let locks = Arc::new(PathLocks::default());
    let left = locks.clone();
    let right = locks.clone();
    let first = tokio::spawn(async move {
        let paths = vec![
            std::path::PathBuf::from("/tmp/b"),
            std::path::PathBuf::from("/tmp/a"),
        ];
        let _guards = left.acquire_many(&paths).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    });
    let second = tokio::spawn(async move {
        let paths = vec![
            std::path::PathBuf::from("/tmp/a"),
            std::path::PathBuf::from("/tmp/b"),
        ];
        let _guards = right.acquire_many(&paths).await;
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        first.await.unwrap();
        second.await.unwrap();
    })
    .await
    .expect("overlapping subtree releases must not deadlock");
}

#[tokio::test]
async fn release_barrier_keeps_late_events_blocked_until_every_release_finishes() {
    let locks = PathLocks::default();
    let path = std::path::PathBuf::from("/tmp/releasing");

    // Two overlapping operations must not let the first completion clear the
    // protection that the second operation still needs.
    let first = locks.begin_release(std::slice::from_ref(&path));
    let second = locks.begin_release(std::slice::from_ref(&path));
    assert!(locks.is_releasing(&path));

    drop(first);
    assert!(locks.is_releasing(&path));
    drop(second);
    assert!(!locks.is_releasing(&path));
}

#[tokio::test]
async fn a_queued_hydration_observes_the_release_barrier_after_waiting_for_the_path() {
    let locks = Arc::new(PathLocks::default());
    let path = std::path::PathBuf::from("/tmp/releasing");
    let first = locks.acquire(&path).await;
    let _release = locks.begin_release(std::slice::from_ref(&path));

    let waiting = locks.clone();
    let path_for_task = path.clone();
    let task = tokio::spawn(async move {
        let _guard = waiting.acquire(&path_for_task).await;
        waiting.is_releasing(&path_for_task)
    });
    let mut task = task;
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut task)
        .await
        .is_err());
    drop(first);

    // The worker can acquire the path only after the in-flight operation has
    // drained, but it still sees the barrier and must refuse a new fill.
    let observed = tokio::time::timeout(Duration::from_secs(1), &mut task)
        .await
        .unwrap()
        .unwrap();
    assert!(observed);
}

#[test]
fn a_bulk_copier_hydrates_fully_on_open_but_others_pass_through() {
    // cp/tar would SEEK over the holes and copy zeros, so they get the whole
    // file on open; a normal open is allowed and its reads drive the rest.
    assert_eq!(
        decide_stage(true, "cp", 0, 0, 1000, true, true),
        StagePlan::Full
    );
    assert_eq!(
        decide_stage(true, "tar", 0, 0, 1000, true, true),
        StagePlan::Full
    );
    assert_eq!(
        decide_stage(true, "vlc", 0, 0, 1000, true, true),
        StagePlan::AllowOpen
    );
    assert_eq!(
        decide_stage(true, "gedit", 0, 0, 1000, true, true),
        StagePlan::AllowOpen
    );
}

#[test]
fn thumbnail_reads_stay_offline_and_nautilus_copies_are_explicit() {
    // A thumbnailer sniffing a head must never reach the network.
    assert_eq!(
        decide_stage(false, "gdk-pixbuf-thum", 0, 4096, 1_000_000, true, true),
        StagePlan::BrowseLocal {
            offset: 0,
            length: 4096
        }
    );
    assert_eq!(
        decide_stage(
            false,
            "gdk-pixbuf-thum",
            0,
            1_000_000,
            1_000_000,
            true,
            true
        ),
        StagePlan::BrowseLocal {
            offset: 0,
            length: 1_000_000
        },
        "dedicated thumbnailers never trigger network hydration"
    );
    // A helper that can prove whether Nautilus has a writable copy destination
    // denies speculative reads. That keeps a cold directory fully offline and
    // never hands sparse placeholder zeroes to the file manager.
    assert_eq!(
        decide_stage(false, "nautilus", 0, 16_384, 1_000_000, true, true),
        StagePlan::DenyBrowse
    );
    assert_eq!(
        decide_stage(false, "nautilus", 0, 0, 1_000_000, true, true),
        StagePlan::DenyBrowse
    );

    // The helper labels only a Nautilus process with a same-name writable
    // destination as a real copy. It must receive the whole source before its
    // first byte, regardless of the requested range.
    assert_eq!(
        decide_stage(
            false,
            FILE_MANAGER_COPY_READER,
            0,
            16_384,
            1_000_000,
            true,
            true
        ),
        StagePlan::Full
    );

    // During a rolling upgrade, an old helper cannot prove copy intent. Keep
    // the previous conservative range policy so a copy cannot receive zeroes.
    assert_eq!(
        decide_stage(false, "nautilus", 0, 49_152, 45_568, true, false),
        StagePlan::Range {
            offset: 0,
            length: 45_568
        }
    );
    assert_eq!(
        decide_stage(false, "nautilus", 16_384, 16_384, 1_000_000, true, false),
        StagePlan::Full,
        "the legacy policy must still fail safe after a partial read"
    );
}

#[test]
fn only_whole_file_hydrations_are_visible_in_the_download_monitor() {
    assert!(StagePlan::Full.shows_progress());
    assert!(!StagePlan::Range {
        offset: 0,
        length: 16_384,
    }
    .shows_progress());
    assert!(!StagePlan::BrowseLocal {
        offset: 0,
        length: 16_384,
    }
    .shows_progress());
    assert!(!StagePlan::DenyBrowse.shows_progress());
    assert!(!StagePlan::AllowOpen.shows_progress());
}

#[test]
fn a_consumers_first_head_read_is_a_range_then_it_escalates() {
    // Small head-read at offset 0: just the window.
    assert_eq!(
        decide_stage(false, "sha256sum", 0, 65536, 1_000_000, true, true),
        StagePlan::Range {
            offset: 0,
            length: 65536
        }
    );
    // Coming back for offset > 0 is sequential reading: hydrate fully.
    assert_eq!(
        decide_stage(false, "sha256sum", 65536, 65536, 1_000_000, true, true),
        StagePlan::Full
    );
    // A head-read above the ceiling is a real read, not a sniff: full.
    assert_eq!(
        decide_stage(
            false,
            "cat",
            0,
            PARTIAL_HYDRATION_CEILING + 1,
            1_000_000,
            true,
            true
        ),
        StagePlan::Full
    );
    // A read covering the whole (small) file is full, not a range.
    assert_eq!(
        decide_stage(false, "cat", 0, 500, 500, true, true),
        StagePlan::Full
    );
    // Partial hydration disabled by the helper: always full.
    assert_eq!(
        decide_stage(false, "cat", 0, 4096, 1_000_000, false, true),
        StagePlan::Full
    );
}

#[test]
fn a_gallery_viewer_stays_range_bound_across_prefetch_reads() {
    assert_eq!(
        decide_stage(false, "loupe", 0, 65_536, 2_000_000, true, true),
        StagePlan::Range {
            offset: 0,
            length: 65_536,
        }
    );
    assert_eq!(
        decide_stage(false, "loupe", 61_440, 69_632, 2_000_000, true, true,),
        StagePlan::Range {
            offset: 61_440,
            length: 69_632,
        },
        "a second Loupe read may be neighbour preloading, not user intent"
    );
    assert_eq!(
        decide_stage(false, "loupe", 0, 2_000_000, 2_000_000, true, true),
        StagePlan::Full,
        "an explicit request for every byte is a full hydration"
    );
    assert_eq!(
        decide_stage(false, "loupe", 1_950_000, 100_000, 2_000_000, true, true,),
        StagePlan::Range {
            offset: 1_950_000,
            length: 50_000,
        },
        "the final range is clamped to EOF"
    );
    assert_eq!(
        decide_stage(false, "loupe", 0, 65_536, 2_000_000, false, true),
        StagePlan::Full,
        "without helper range support a reader must never receive sparse zeroes"
    );
}

#[test]
fn thumbnailers_and_previews_browse_never_consume() {
    // The screenshot bug: clicking one image made Nautilus thumbnail the
    // others, downloading them. Every thumbnailer and preview helper must
    // read as "browsing" so it is served offline, never pulling content.
    for t in [
        "glycin-thumbnai",
        "gdk-pixbuf-thum",
        "gst-video-thumb",
        "gst-audio-thumb",
        "heif-thumbnaile",
        "papers-thumbnai",
        "nautilus",
        "sushi",
    ] {
        assert!(is_browsing_reader(t), "{t} must browse, not download");
    }
    // A real image viewer the user opened is a genuine consumer.
    for v in ["loupe", "eog", "gimp", "gwenview"] {
        assert!(!is_browsing_reader(v), "{v} is a real open");
    }
}

#[test]
fn file_managers_browse_and_applications_consume() {
    // The line this draws: looking at a file must never flip its state;
    // using it may.
    for browser in ["nautilus", "sushi"] {
        assert!(is_browsing_reader(browser), "{browser}");
    }
    for consumer in ["cat", "gedit", "cp", "libreoffice", "md5sum", ""] {
        assert!(!is_browsing_reader(consumer), "{consumer:?}");
    }
}

#[test]
fn a_finished_transfer_may_not_declare_idle_over_an_unreachable_account() {
    // The event forwarder keeps its own view of the sync phase and used to
    // write it over everything, including the coordinator's verdict on the
    // account. The tray then read "idle" while the daemon was offline.
    for state in ["offline", "keyring_locked", "signed_out"] {
        assert!(
            is_account_level_state(state),
            "{state} belongs to the account"
        );
    }
    // Raised by the forwarder itself for a failed cycle, so it must stay
    // clearable by the next clean one.
    assert!(!is_account_level_state("error"));
    for state in ["idle", "syncing", "enumerating", "downloading", "paused"] {
        assert!(!is_account_level_state(state), "{state} describes work");
    }
}
