//! End-to-end sync scenarios against a fake Microsoft Graph.
//!
//! These drive the real [`Engine`] — real database, real filesystem, real HTTP
//! client — with only the service replaced. They exist because the parts of a
//! sync client that destroy data are the *interactions* between components, and
//! a unit test of the conflict matrix cannot catch a download that lands in the
//! wrong directory or a delete that outruns its guard.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gonedrive_core::config::{Config, DownloadPolicy};
use gonedrive_core::db::{Account, ItemRecord, ItemState, PinState, Store};
use gonedrive_core::graph::GraphClient;
use gonedrive_core::sync::engine::{Engine, SyncSummary};
use gonedrive_core::TokenSource;
use serde_json::{json, Value};
use wiremock::matchers::{
    body_bytes, body_partial_json, header, header_exists, method, path, path_regex, query_param,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCOUNT: &str = "acct";
const ROOT: &str = "ROOT";

struct Harness {
    server: MockServer,
    store: Store,
    engine: Engine,
    config: Arc<Config>,
    root: PathBuf,
    _tmp: tempfile::TempDir,
}

impl Harness {
    /// A harness that mirrors the whole drive.
    ///
    /// Most tests here are about transfer and reconciliation mechanics, so they
    /// opt into `Always` to keep the thing under test in view. The shipped
    /// default is `Never`, and it has its own tests below.
    async fn new() -> Self {
        Self::with_policy(DownloadPolicy::Always).await
    }

    /// A harness with the shipped default: enumerate, download nothing.
    async fn metadata_only() -> Self {
        Self::with_policy(DownloadPolicy::Never).await
    }

    async fn with_config(mut config: Config) -> Self {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("OneDrive");
        std::fs::create_dir_all(&root).unwrap();
        config.sync_root = root.clone();
        config.client_id = "test".into();

        let store = Store::open_in_memory().unwrap();
        store
            .upsert_account(&Account {
                id: ACCOUNT.into(),
                drive_id: "drive1".into(),
                root_item_id: Some(ROOT.into()),
                display_name: Some("Test".into()),
                address: Some("t@example.com".into()),
                root_local_path: root.clone(),
                delta_link: None,
                enum_link: None,
                quota_total: None,
                quota_used: None,
                root_dev: None,
            })
            .unwrap();
        store.upsert_item(&ItemRecord::root(ACCOUNT, ROOT)).unwrap();

        let graph = Arc::new(
            GraphClient::new(reqwest::Client::new(), TokenSource::Static("token".into()))
                .with_base_url(server.uri()),
        );
        let config = Arc::new(config);
        let engine = Engine::new(store.clone(), graph, config.clone(), ACCOUNT.into());
        Self {
            server,
            store,
            engine,
            config,
            root,
            _tmp: tmp,
        }
    }

    async fn with_policy(download_new: DownloadPolicy) -> Self {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("OneDrive");
        std::fs::create_dir_all(&root).unwrap();

        let store = Store::open_in_memory().unwrap();
        store
            .upsert_account(&Account {
                id: ACCOUNT.into(),
                drive_id: "drive1".into(),
                root_item_id: Some(ROOT.into()),
                display_name: Some("Test".into()),
                address: Some("t@example.com".into()),
                root_local_path: root.clone(),
                delta_link: None,
                enum_link: None,
                quota_total: None,
                quota_used: None,
                root_dev: None,
            })
            .unwrap();

        // Seeded through the same constructor `login` uses. Hand-rolling this
        // is how the "root is born pinned" bug hid: the harness produced an
        // `Inherit` root while production produced a `Pinned` one, so the tests
        // could not see that every file was inheriting "download me".
        store.upsert_item(&ItemRecord::root(ACCOUNT, ROOT)).unwrap();

        let graph = Arc::new(
            GraphClient::new(reqwest::Client::new(), TokenSource::Static("token".into()))
                .with_base_url(server.uri()),
        );
        let config = Arc::new(Config {
            client_id: "test".into(),
            sync_root: root.clone(),
            download_new,
            ..Config::default()
        });
        let engine = Engine::new(store.clone(), graph, config.clone(), ACCOUNT.into());

        Self {
            server,
            store,
            engine,
            config,
            root,
            _tmp: tmp,
        }
    }

    /// Serve one delta page, ending the enumeration with `delta_token`.
    async fn mock_delta(&self, query: Option<(&str, &str)>, items: Vec<Value>, delta_token: &str) {
        let body = json!({
            "value": items,
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token={delta_token}", self.server.uri()),
        });
        let mut mock = Mock::given(method("GET")).and(path("/me/drive/root/delta"));
        if let Some((k, v)) = query {
            mock = mock.and(query_param(k, v));
        }
        mock.respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    async fn mock_download(&self, item_id: &str, content: &'static [u8]) {
        Mock::given(method("GET"))
            .and(path(format!("/me/drive/items/{item_id}/content")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(content))
            .mount(&self.server)
            .await;
    }

    async fn sync(&self) -> SyncSummary {
        self.engine.sync_once().await.expect("sync failed")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap()
    }
}

fn folder(id: &str, parent: &str, name: &str) -> Value {
    json!({
        "id": id, "name": name, "folder": {"childCount": 0},
        "parentReference": {"driveId": "drive1", "id": parent}
    })
}

fn file(id: &str, parent: &str, name: &str, ctag: &str, size: u64, mtime: &str) -> Value {
    json!({
        "id": id, "name": name, "size": size,
        "cTag": ctag, "eTag": format!("e-{ctag}"),
        "file": {"mimeType": "text/plain", "hashes": {"quickXorHash": ctag}},
        "parentReference": {"driveId": "drive1", "id": parent},
        "fileSystemInfo": {"lastModifiedDateTime": mtime}
    })
}

/// The drive root as it arrives in a delta page.
fn root_item() -> Value {
    json!({
        "id": ROOT, "name": "root", "root": {}, "folder": {"childCount": 1},
        "parentReference": {"driveId": "drive1"}
    })
}

fn tombstone(id: &str, parent: &str) -> Value {
    json!({
        "id": id, "deleted": {"state": "deleted"},
        "parentReference": {"driveId": "drive1", "id": parent}
    })
}

fn mtime_secs(path: &Path) -> i64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().mtime()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_sync_builds_the_local_tree_and_stamps_timestamps() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Documents"),
            file("F1", "D1", "report.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;

    assert!(h.path("Documents").is_dir());
    assert_eq!(h.read("Documents/report.txt"), "hello");
    assert_eq!(summary.downloaded, 1);

    // The file must carry the service's timestamp, not the download time —
    // otherwise the very next local scan reads it as an edit and uploads it back.
    let expected = time::macros::datetime!(2026-08-20 10:00:00 UTC).unix_timestamp();
    assert_eq!(mtime_secs(&h.path("Documents/report.txt")), expected);

    // And the deltaLink is now persisted for the next round.
    let account = h.store.account(ACCOUNT).unwrap().unwrap();
    assert!(account.delta_link.unwrap().contains("token=v1"));
}

#[tokio::test]
async fn a_second_sync_with_no_changes_transfers_nothing() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let summary = h.sync().await;

    // The steady state must be genuinely quiet: no download, no upload, and in
    // particular no re-upload of the file we just fetched.
    assert_eq!(
        summary,
        SyncSummary {
            still_queued: 0,
            ..Default::default()
        }
    );
    assert_eq!(h.read("a.txt"), "hello");
}

#[tokio::test]
async fn a_remote_edit_is_downloaded_over_the_local_copy() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file("F1", ROOT, "a.txt", "c2", 7, "2026-08-21T10:00:00Z")],
        "v2",
    )
    .await;
    h.mock_download("F1", b"goodbye").await;

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 1);
    assert_eq!(h.read("a.txt"), "goodbye");
}

#[tokio::test]
async fn a_remote_file_rename_moves_the_local_inode_without_reuploading() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "before.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    let inode = std::fs::metadata(h.path("before.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            ROOT,
            "after.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v2",
    )
    .await;

    let summary = h.sync().await;
    assert!(!h.path("before.txt").exists());
    assert_eq!(h.read("after.txt"), "hello");
    assert_eq!(
        std::fs::metadata(h.path("after.txt")).unwrap().ino(),
        inode.ino()
    );
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.uploaded, 0);
}

#[tokio::test]
async fn a_remote_rename_does_not_mark_a_local_edit_as_synced() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "before.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    std::fs::write(h.path("before.txt"), b"local edit").unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file(
            "F1",
            ROOT,
            "after.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v2",
    )
    .await;
    let uploaded = json!({
        "id": "F1", "name": "after.txt", "size": 10,
        "cTag": "c2", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-29T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .and(body_bytes(b"local edit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded.clone()))
        .mount(&h.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .and(body_partial_json(json!({"fileSystemInfo": {}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.uploaded, 1);
    assert!(!h.path("before.txt").exists());
    assert_eq!(h.read("after.txt"), "local edit");
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().ctag.as_deref(),
        Some("c2")
    );
}

#[tokio::test]
async fn a_remote_folder_rename_moves_descendants_not_duplicates_them() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            folder("D1", ROOT, "Before"),
            file("F1", "D1", "inside.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    h.server.reset().await;
    // Graph reports only the renamed folder, not all descendants.
    h.mock_delta(
        Some(("token", "v1")),
        vec![folder("D1", ROOT, "After")],
        "v2",
    )
    .await;
    let summary = h.sync().await;

    assert!(!h.path("Before").exists());
    assert_eq!(h.read("After/inside.txt"), "hello");
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.uploaded, 0);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().parent_id.as_deref(),
        Some("D1")
    );
}

#[tokio::test]
async fn a_child_on_an_earlier_delta_page_waits_for_its_parent() {
    let h = Harness::new().await;
    let next = format!("{}/me/drive/root/delta?page=2", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file("F1", "D1", "inside.txt", "c1", 5, "2026-08-20T10:00:00Z")],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [folder("D1", ROOT, "Parent")],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=v1", h.server.uri())
        })))
        .mount(&h.server)
        .await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;
    assert_eq!(h.read("Parent/inside.txt"), "hello");
    assert_eq!(summary.enumerated, 2);
    assert_eq!(summary.skipped, 0);
}

#[tokio::test]
async fn the_last_occurrence_of_an_item_in_delta_wins() {
    let h = Harness::new().await;
    let next = format!("{}/me/drive/root/delta?page=2", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("$top", "1000"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file("F1", ROOT, "stale.txt", "c1", 5, "2026-08-20T10:00:00Z")],
            "@odata.nextLink": next
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [file("F1", ROOT, "final.txt", "c2", 5, "2026-08-21T10:00:00Z")],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=v1", h.server.uri())
        })))
        .mount(&h.server)
        .await;
    h.mock_download("F1", b"final").await;

    h.sync().await;
    assert!(!h.path("stale.txt").exists());
    assert_eq!(h.read("final.txt"), "final");
}

#[tokio::test]
async fn a_remote_delete_removes_an_untouched_local_file() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;
    assert!(h.path("a.txt").exists());

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![tombstone("F1", ROOT)], "v2")
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.deleted_local, 1);
    assert!(!h.path("a.txt").exists());
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn a_remote_delete_never_destroys_an_unsaved_local_edit() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    // The user edits the file; only then does the remote delete arrive.
    std::fs::write(h.path("a.txt"), b"my important edit").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![tombstone("F1", ROOT)], "v2")
        .await;
    mock_upload(&h.server, "F2", "a.txt").await;

    h.sync().await;

    // The local content survives and is offered back to the service.
    assert_eq!(h.read("a.txt"), "my important edit");
}

#[tokio::test]
async fn a_new_local_file_is_uploaded() {
    let h = Harness::new().await;
    h.mock_delta(None, vec![], "v1").await;
    h.sync().await;

    std::fs::write(h.path("notes.txt"), b"local content").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "F9", "notes.txt").await;

    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 1);

    // It is now tracked, so the next scan will not upload it again.
    let record = h.store.item("F9").unwrap().unwrap();
    assert_eq!(record.name, "notes.txt");
    assert_eq!(record.state, ItemState::Synced);
    assert!(record.local_ino.is_some());
}

#[tokio::test]
async fn a_local_directory_is_created_remotely() {
    let h = Harness::new().await;
    h.mock_delta(None, vec![], "v1").await;
    h.sync().await;

    std::fs::create_dir(h.path("New Folder")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("POST"))
        .and(path(format!("/me/drive/items/{ROOT}/children")))
        .respond_with(ResponseTemplate::new(201).set_body_json(folder("D9", ROOT, "New Folder")))
        .mount(&h.server)
        .await;

    h.sync().await;
    let record = h.store.item("D9").unwrap().unwrap();
    assert!(record.is_folder);
    assert_eq!(record.name, "New Folder");
}

#[tokio::test]
async fn a_file_changed_on_both_sides_keeps_both_versions() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::write(h.path("a.txt"), b"local version").unwrap();

    h.server.reset().await;
    h.mock_delta(
        Some(("token", "v1")),
        vec![file("F1", ROOT, "a.txt", "c2", 14, "2026-08-21T10:00:00Z")],
        "v2",
    )
    .await;
    h.mock_download("F1", b"remote version").await;
    mock_upload(&h.server, "F2", "conflict").await;

    let summary = h.sync().await;
    assert_eq!(summary.conflicts, 1);

    // The remote version takes the original name...
    assert_eq!(h.read("a.txt"), "remote version");

    // ...and the local one is still on disk under a conflict name.
    let copies: Vec<_> = std::fs::read_dir(&h.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("的冲突副本"))
        .collect();
    assert_eq!(
        copies.len(),
        1,
        "expected exactly one conflict copy, got {copies:?}"
    );
    assert_eq!(h.read(&copies[0]), "local version");
}

#[tokio::test]
async fn an_expired_delta_token_triggers_a_full_resync() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F2", ROOT, "b.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.mock_download("F2", b"world").await;
    h.sync().await;
    assert!(h.path("a.txt").exists() && h.path("b.txt").exists());

    h.server.reset().await;
    // The stored token is rejected...
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "v1"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": {"code": "resyncRequired", "message": "Resync required."}
        })))
        .mount(&h.server)
        .await;
    // ...and the fresh enumeration no longer mentions b.txt.
    h.mock_delta(
        Some(("$top", "1000")),
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v2",
    )
    .await;

    h.sync().await;

    // A plain re-enumeration reports what exists, never what was removed; the
    // generation sweep is what makes the deletion visible.
    assert!(h.path("a.txt").exists(), "a.txt should have survived");
    assert!(!h.path("b.txt").exists(), "b.txt was gone remotely");
    assert!(h.store.item("F2").unwrap().is_none());
}

#[tokio::test]
async fn a_resync_sweep_keeps_a_locally_edited_file_it_would_have_deleted() {
    // The delta tombstone path preserves an unsaved local edit and re-uploads
    // it (KeepLocalAndReupload). The resync generation sweep took a different,
    // unguarded path and deleted the local file outright — losing the edit.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z"),
            file("F2", ROOT, "b.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.mock_download("F2", b"world").await;
    h.sync().await;

    // The user edits b.txt after that sync — not yet uploaded.
    std::fs::write(h.path("b.txt"), b"EDITED LOCALLY, NOT YET UPLOADED").unwrap();

    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .and(query_param("token", "v1"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({
            "error": {"code": "resyncRequired", "message": "Resync required."}
        })))
        .mount(&h.server)
        .await;
    // The fresh enumeration no longer mentions b.txt: gone remotely.
    h.mock_delta(
        Some(("$top", "1000")),
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v2",
    )
    .await;
    mock_upload(&h.server, "F2new", "b.txt").await;

    h.sync().await;

    // The edit must survive, exactly as it does down the ordinary delete path.
    assert!(
        h.path("b.txt").exists(),
        "a locally-edited file must survive the resync sweep"
    );
    assert_eq!(h.read("b.txt"), "EDITED LOCALLY, NOT YET UPLOADED");
}

#[tokio::test]
async fn throttling_is_retried_rather_than_failing_the_sync() {
    let h = Harness::new().await;
    // First attempt is throttled with a one-second Retry-After.
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;

    let summary = h.sync().await;
    assert_eq!(summary.downloaded, 1);
    assert_eq!(h.read("a.txt"), "hello");
}

#[tokio::test]
async fn a_name_onedrive_rejects_is_reported_and_never_uploaded() {
    let h = Harness::new().await;
    h.mock_delta(None, vec![], "v1").await;
    h.sync().await;

    // Legal on ext4, rejected by the service.
    std::fs::write(h.path("a:b.txt"), b"x").unwrap();
    std::fs::write(h.path("CON"), b"x").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // No upload mock is registered: if the engine tried, the request would 404
    // and the test would fail on the summary below.

    let summary = h.sync().await;
    assert_eq!(summary.skipped, 2);
    assert_eq!(summary.uploaded, 0);
    assert_eq!(summary.failed, 0);
    // The files stay on disk untouched; we refuse to sync them, not to keep them.
    assert!(h.path("a:b.txt").exists());
}

#[tokio::test]
async fn a_local_delete_is_propagated_remotely() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::remove_file(h.path("a.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.deleted_remote, 1);
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn deleting_an_online_only_file_propagates_the_delete() {
    // Deleting an online-only placeholder must delete it from the cloud, the
    // way it does on Windows. The bug: a placeholder never had its inode
    // recorded, so the delete guard (which spares a download that never
    // landed) mistook it for one and the deletion never reached the server.
    use gonedrive_core::hydrate::{Placeholder, PlaceholderState};
    // Never downloaded — a pure placeholder from materialisation, which is the
    // real case: run_download would have recorded the inode, materialize does
    // not.
    let h = Harness::metadata_only().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.sync().await;

    // Materialise it as an online-only placeholder on disk.
    h.engine.materialize_all(|_, _| true).unwrap();
    assert!(h.path("a.txt").exists());
    assert_eq!(
        Placeholder::state(&h.path("a.txt")),
        PlaceholderState::Dehydrated
    );
    assert!(
        h.store.item("F1").unwrap().unwrap().local_ino.is_none(),
        "materialise records no inode — that is the bug this guards"
    );

    // A scan records the placeholder's inode.
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    h.sync().await;
    assert!(
        h.store.item("F1").unwrap().unwrap().local_ino.is_some(),
        "the placeholder's inode should be recorded so a later delete is seen"
    );

    // Now delete the online-only file — it must reach the cloud.
    std::fs::remove_file(h.path("a.txt")).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 1,
        "deleting an online-only file must reach the cloud"
    );
    assert!(h.store.item("F1").unwrap().is_none());
}

#[tokio::test]
async fn a_pending_download_is_not_mistaken_for_a_local_delete() {
    // The dangerous case: an item we recorded but have not written to disk yet
    // looks exactly like a file the user deleted. Deleting it remotely would
    // destroy data the user never touched.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    // The download fails, so the record exists with no local file behind it.
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert!(!h.path("a.txt").exists());
    assert_eq!(
        summary.deleted_remote, 0,
        "must not have deleted anything remotely"
    );
    assert!(h.store.item("F1").unwrap().is_some());
}

#[tokio::test]
async fn a_scan_that_would_wipe_the_account_holds_the_deletions() {
    // The catastrophe this guards: the whole tree vanishing at once — an
    // unmounted-but-present root, a bad bind, an `rm -rf` — reads as "every
    // file was deleted" and would delete the entire account server-side. A
    // deletion that large is held for confirmation, never executed on sight.
    let h = Harness::new().await;
    let items: Vec<Value> = (0..12)
        .map(|i| {
            file(
                &format!("F{i}"),
                ROOT,
                &format!("f{i}.txt"),
                "c1",
                5,
                "2026-08-20T10:00:00Z",
            )
        })
        .collect();
    h.mock_delta(None, items, "v1").await;
    for i in 0..12 {
        h.mock_download(&format!("F{i}"), b"hello").await;
    }
    h.sync().await;

    // Everything disappears in one pass.
    for i in 0..12 {
        std::fs::remove_file(h.path(&format!("f{i}.txt"))).unwrap();
    }

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    // Deliberately no DELETE mock: a working guard enqueues no deletion at all.
    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 0,
        "a mass vanish must not delete remotely"
    );
    assert!(
        summary.refused_deletions >= 12,
        "the deletions should be held, not run"
    );
    for i in 0..12 {
        assert!(
            h.store.item(&format!("F{i}")).unwrap().is_some(),
            "every record must survive a held mass deletion"
        );
    }
}

#[tokio::test]
async fn a_sync_root_on_a_different_device_refuses_to_scan() {
    // An unmounted mountpoint still *exists* as an empty directory on a
    // different filesystem. The device number is the tell: if it changed since
    // the last scan, refuse — deleting to match an empty tree would wipe the
    // account.
    use std::os::unix::fs::MetadataExt;
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file("F1", ROOT, "a.txt", "c1", 5, "2026-08-20T10:00:00Z")],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await; // records the real device

    // Simulate the root becoming a different filesystem.
    let real_dev = std::fs::metadata(&h.root).unwrap().dev();
    h.store.set_root_dev(ACCOUNT, real_dev ^ 1).unwrap();
    std::fs::remove_file(h.path("a.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 0,
        "a device mismatch must never delete"
    );
    assert!(h.store.item("F1").unwrap().is_some());
}

/// A simple-upload mock: accepts the PUT and the follow-up mtime PATCH.
async fn mock_upload(server: &MockServer, new_id: &str, name: &str) {
    let created = json!({
        "id": new_id, "name": name, "size": 13,
        "cTag": "cu", "eTag": "eu",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-22T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path_regex(r"^/me/drive/items/.+/content$"))
        .respond_with(ResponseTemplate::new(201).set_body_json(created.clone()))
        .mount(server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/me/drive/items/.+$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(created))
        .mount(server)
        .await;
}

// ---------------------------------------------------------------------------
// The shipped default: enumerate everything, download nothing.
//
// A OneDrive can easily hold more than the disk it is synced to. Downloading on
// sight fills the disk and fails, after hours of transfer — so the default has
// to be that content arrives only when it is asked for.
// ---------------------------------------------------------------------------

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
    let staged = h.engine.stage_range(&path, 0, 16384).await.unwrap();

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
}

#[tokio::test]
async fn a_browsed_placeholder_can_still_be_freed() {
    // A range fill (a file manager sniffing the head) writes into the
    // placeholder and moves its mtime. That must never trip the
    // unsaved-changes guard: the bytes are cloud-owned cache, and refusing
    // made "free a file you merely looked at" impossible — found live as
    // `FreeUpSpace done files=0 kept=2`.
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

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
    assert_eq!(
        Occupancy::read(&path).unwrap().allocated,
        0,
        "cache punched"
    );

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
async fn a_cross_directory_rename_is_one_move_not_delete_plus_upload() {
    // Found live: moving a file to a folder created in the same pass turned
    // into a remote DELETE followed by a fresh upload — the file kept its
    // bytes but lost its identity and its server-side version history. Only
    // the move endpoint is mocked here; a delete or a content upload 404s
    // and fails the pass.
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![
            root_item(),
            folder("D1", ROOT, "来源"),
            file("F1", "D1", "文档.txt", "c1", 5, "2026-08-20T10:00:00Z"),
        ],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    // The user makes a destination folder and moves the file into it.
    std::fs::create_dir(h.path("目的地")).unwrap();
    std::fs::rename(h.path("来源/文档.txt"), h.path("目的地/改名.txt")).unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    Mock::given(method("POST"))
        .and(path(format!("/me/drive/items/{ROOT}/children")))
        .respond_with(ResponseTemplate::new(201).set_body_json(folder("D9", ROOT, "目的地")))
        .mount(&h.server)
        .await;
    let moved = json!({
        "id": "F1", "name": "改名.txt", "size": 5,
        "cTag": "c1", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": "D9"},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-20T10:00:00Z"}
    });
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(moved))
        .mount(&h.server)
        .await;

    let mut events = h.engine.subscribe();
    let summary = h.sync().await;
    let mut failures = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let gonedrive_core::sync::engine::SyncEvent::Failed { name, error } = event {
            failures.push(format!("{name}: {error}"));
        }
    }
    assert_eq!(
        summary.failed, 0,
        "unexpected failures: {failures:?}\n{summary:?}"
    );
    assert_eq!(summary.deleted_remote, 0);

    // Same item id, new home — identity and history preserved.
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.name, "改名.txt");
    assert_eq!(record.parent_id.as_deref(), Some("D9"));
    assert_eq!(record.state, ItemState::Synced);
}

#[tokio::test]
async fn moving_and_editing_a_file_applies_metadata_then_uploads_the_bytes() {
    let h = Harness::new().await;
    h.mock_delta(
        None,
        vec![file(
            "F1",
            ROOT,
            "before.txt",
            "c1",
            5,
            "2026-08-20T10:00:00Z",
        )],
        "v1",
    )
    .await;
    h.mock_download("F1", b"hello").await;
    h.sync().await;

    std::fs::rename(h.path("before.txt"), h.path("after.txt")).unwrap();
    std::fs::write(h.path("after.txt"), b"edited after move").unwrap();

    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let moved = json!({
        "id": "F1", "name": "after.txt", "size": 5,
        "cTag": "c1", "eTag": "e-moved",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-20T10:00:00Z"}
    });
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .and(body_partial_json(json!({"name": "after.txt"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(moved))
        .mount(&h.server)
        .await;

    let uploaded = json!({
        "id": "F1", "name": "after.txt", "size": 17,
        "cTag": "c2", "eTag": "e2",
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive1", "id": ROOT},
        "fileSystemInfo": {"lastModifiedDateTime": "2026-08-29T10:00:00Z"}
    });
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .and(body_bytes(b"edited after move"))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded.clone()))
        .mount(&h.server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .and(body_partial_json(json!({"fileSystemInfo": {}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(summary.failed, 0);
    assert_eq!(h.read("after.txt"), "edited after move");
    let record = h.store.item("F1").unwrap().unwrap();
    assert_eq!(record.name, "after.txt");
    assert_eq!(record.ctag.as_deref(), Some("c2"));
    assert_eq!(record.local_size, Some(17));

    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    assert_eq!(
        h.sync().await.uploaded,
        0,
        "the edit must not be suppressed or loop"
    );
}

#[tokio::test]
async fn a_dirty_record_over_a_placeholder_stops_looping_at_repair() {
    // Freeing a file that was dirty at the time leaves the record saying
    // "locally modified" over bytes that are now holes. Left alone, every
    // pass queued an upload and the guard refused it — forever.
    use gonedrive_core::db::ItemState;

    let h = harness_with_a_downloaded_file(b"content").await;
    h.engine.free_space(Path::new("Docs")).unwrap();
    h.store.set_item_state("F1", ItemState::DirtyLocal).unwrap();

    assert_eq!(h.engine.repair_states().unwrap(), 1);
    assert_eq!(
        h.store.item("F1").unwrap().unwrap().state,
        ItemState::Dehydrated
    );

    // And even未经修复, the scanner itself no longer trusts the record over
    // the file's own marks.
    h.store.set_item_state("F1", ItemState::DirtyLocal).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    let summary = h.sync().await;
    assert_eq!(summary.uploaded, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn deleting_a_local_file_propagates_even_under_the_online_only_default() {
    // Found live: with the default download policy, every locally-synced item
    // resolves to "unpinned", and the exclusion guard read every local
    // deletion as "supposed to be absent" — nothing the user deleted was ever
    // removed from the cloud.
    let h = Harness::metadata_only().await;
    h.mock_delta(None, vec![root_item()], "v1").await;
    h.sync().await;

    std::fs::write(h.path("mine.txt"), b"created here").unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "F7", "mine.txt").await;
    h.sync().await;
    assert!(h.store.item("F7").unwrap().is_some());

    std::fs::remove_file(h.path("mine.txt")).unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v2")), vec![], "v3").await;
    Mock::given(method("DELETE"))
        .and(path("/me/drive/items/F7"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&h.server)
        .await;

    let summary = h.sync().await;
    assert_eq!(
        summary.deleted_remote, 1,
        "the deletion must reach the cloud"
    );
    assert!(h.store.item("F7").unwrap().is_none());
}

#[tokio::test]
async fn an_uploaded_file_is_not_recorded_as_pinned() {
    // The resurrection chain's root cause: uploads stamped the record Pinned,
    // so a later local deletion was read as "a pinned file went missing —
    // re-create it", and the folder came back on every delta pass. An upload
    // records what the user's policy says, which under the default is not
    // "always keep".
    use gonedrive_core::db::PinState;

    let h = Harness::metadata_only().await; // default online-only policy
    h.mock_delta(None, vec![root_item()], "v1").await;
    h.sync().await;

    std::fs::write(h.path("fresh.txt"), b"x").unwrap();
    h.server.reset().await;
    h.mock_delta(Some(("token", "v1")), vec![], "v2").await;
    mock_upload(&h.server, "NEW", "fresh.txt").await;
    h.sync().await;

    let record = h.store.item("NEW").unwrap().unwrap();
    assert_ne!(
        record.pin_state,
        PinState::Pinned,
        "an upload must not pin; that resurrects deletions"
    );
    assert!(!h.store.explicitly_excluded("NEW").unwrap());
}

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
