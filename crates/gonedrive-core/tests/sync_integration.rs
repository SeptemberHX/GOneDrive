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
use gonedrive_core::db::{Account, ItemRecord, ItemState, OpKind, PinState, Store};
use gonedrive_core::graph::GraphClient;
use gonedrive_core::sync::engine::{Engine, QueueScope, SyncSummary};
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

include!("sync_cases/remote_and_safety.rs");

include!("sync_cases/selection_and_transfer.rs");

include!("sync_cases/conflicts_and_uploads.rs");
include!("sync_cases/on_demand.rs");
include!("sync_cases/moves.rs");
include!("sync_cases/materialization_and_incremental.rs");
