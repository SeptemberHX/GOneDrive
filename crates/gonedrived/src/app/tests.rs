
use std::sync::Arc;
use std::time::Duration;

use gonedrive_core::sync::engine::QueueScope;

use super::{
    decide_stage, is_browsing_reader, outbound_scope, phase_state, sync_barrier_ready, PathLocks,
    StagePlan, PARTIAL_HYDRATION_CEILING,
};

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
}

#[test]
fn explicit_sync_waits_for_full_scan_local_scan_and_transfers() {
    assert!(!sync_barrier_ready(
        false, false, false, false, false, false, false, false, false,
    ));
    assert!(!sync_barrier_ready(
        true, false, true, false, false, false, false, false, false,
    ));
    assert!(!sync_barrier_ready(
        true, false, false, false, false, false, true, false, false,
    ));
    assert!(!sync_barrier_ready(
        true, false, false, true, false, false, false, false, false,
    ));
    assert!(sync_barrier_ready(
        true, false, false, false, false, false, false, false, false,
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

#[test]
fn a_bulk_copier_hydrates_fully_on_open_but_others_pass_through() {
    // cp/tar would SEEK over the holes and copy zeros, so they get the whole
    // file on open; a normal open is allowed and its reads drive the rest.
    assert_eq!(decide_stage(true, "cp", 0, 0, 1000, true), StagePlan::Full);
    assert_eq!(decide_stage(true, "tar", 0, 0, 1000, true), StagePlan::Full);
    assert_eq!(
        decide_stage(true, "vlc", 0, 0, 1000, true),
        StagePlan::AllowOpen
    );
    assert_eq!(
        decide_stage(true, "gedit", 0, 0, 1000, true),
        StagePlan::AllowOpen
    );
}

#[test]
fn browsing_reads_are_served_local_or_offline_never_downloaded() {
    // A thumbnailer sniffing a head must never reach the network.
    assert_eq!(
        decide_stage(false, "gdk-pixbuf-thum", 0, 4096, 1_000_000, true),
        StagePlan::BrowseLocal {
            offset: 0,
            length: 4096
        }
    );
    // A zero-length browse sniff still asks for at least one byte.
    assert_eq!(
        decide_stage(false, "nautilus", 0, 0, 1_000_000, true),
        StagePlan::BrowseLocal {
            offset: 0,
            length: 1
        }
    );
}

#[test]
fn a_consumers_first_head_read_is_a_range_then_it_escalates() {
    // Small head-read at offset 0: just the window.
    assert_eq!(
        decide_stage(false, "sha256sum", 0, 65536, 1_000_000, true),
        StagePlan::Range {
            offset: 0,
            length: 65536
        }
    );
    // Coming back for offset > 0 is sequential reading: hydrate fully.
    assert_eq!(
        decide_stage(false, "sha256sum", 65536, 65536, 1_000_000, true),
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
            true
        ),
        StagePlan::Full
    );
    // A read covering the whole (small) file is full, not a range.
    assert_eq!(
        decide_stage(false, "cat", 0, 500, 500, true),
        StagePlan::Full
    );
    // Partial hydration disabled by the helper: always full.
    assert_eq!(
        decide_stage(false, "cat", 0, 4096, 1_000_000, false),
        StagePlan::Full
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
