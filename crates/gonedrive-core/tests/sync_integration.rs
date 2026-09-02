//! End-to-end sync scenarios against a fake Microsoft Graph.
//!
//! These drive the real [`Engine`] — real database, real filesystem, real HTTP
//! client — with only the service replaced. They exist because the parts of a
//! sync client that destroy data are the *interactions* between components, and
//! a unit test of the conflict matrix cannot catch a download that lands in the
//! wrong directory or a delete that outruns its guard.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use gonedrive_core::config::{Config, DownloadPolicy};
use gonedrive_core::db::{Account, ItemRecord, ItemState, OpKind, PinState, Store};
use gonedrive_core::graph::GraphClient;
use gonedrive_core::sync::engine::{Engine, PlaceholderProtector, QueueScope, SyncSummary};
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

#[derive(Default)]
struct TestPlaceholderProtector {
    calls: AtomicUsize,
}

impl PlaceholderProtector for TestPlaceholderProtector {
    fn protect<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = gonedrive_core::Result<()>> + Send + 'a>>
    {
        Box::pin(async move {
            use std::os::unix::fs::PermissionsExt;

            assert!(!paths.is_empty());
            for path in paths {
                let mut permissions = std::fs::metadata(path)?.permissions();
                assert_eq!(permissions.mode() & 0o7777, 0);
                // The real HSM runs as root and reads the saved xattr through
                // its authorised O_PATH descriptor. This unprivileged test
                // helper temporarily grants only its owner enough access to
                // inspect the same marker.
                permissions.set_mode(0o600);
                std::fs::set_permissions(path, permissions)?;
                let saved = xattr::get(path, "user.gonedrive.safety_mode")?
                    .expect("a progressive placeholder must be safety-locked");
                let saved: [u8; 4] = saved
                    .try_into()
                    .expect("the saved placeholder mode is a u32");
                let mut permissions = std::fs::metadata(path)?.permissions();
                permissions.set_mode(u32::from_le_bytes(saved));
                std::fs::set_permissions(path, permissions)?;
                xattr::remove(path, "user.gonedrive.safety_mode")?;
            }
            self.calls.fetch_add(1, Ordering::Release);
            Ok(())
        })
    }
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

    /// A legacy metadata-only harness: enumerate, download nothing, and keep
    /// the filesystem untouched. Explicit on-demand scenarios use
    /// `with_config(... on_demand: true)` so this helper can remain focused on
    /// transfer and reconciliation tests.
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
            on_demand: false,
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
        if query.is_none()
            && self.config.on_demand
            && self.config.download_new == DownloadPolicy::Never
        {
            let baseline = format!("bootstrap-{delta_token}");
            Mock::given(method("GET"))
                .and(path("/me/drive/root/delta"))
                .and(query_param("token", "latest"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [],
                    "@odata.deltaLink": format!(
                        "{}/me/drive/root/delta?token={baseline}",
                        self.server.uri()
                    )
                })))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/me/drive/root/delta"))
                .and(query_param("token", baseline.as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [],
                    "@odata.deltaLink": format!(
                        "{}/me/drive/root/delta?token={delta_token}",
                        self.server.uri()
                    )
                })))
                .mount(&self.server)
                .await;

            self.mock_children(&items, true).await;
            return;
        }

        self.mock_children(&items, false).await;
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

    async fn mock_children(&self, items: &[Value], include_empty_root: bool) {
        use std::collections::HashMap;

        let mut children: HashMap<String, Vec<Value>> = HashMap::new();
        for item in items {
            if item.get("root").is_some() || item.get("deleted").is_some() {
                continue;
            }
            if item.get("folder").is_some() {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    children.entry(id.to_string()).or_default();
                }
            }
            let Some(parent) = item.pointer("/parentReference/id").and_then(Value::as_str) else {
                continue;
            };
            children
                .entry(parent.to_string())
                .or_default()
                .push(item.clone());
        }
        if include_empty_root {
            children.entry(ROOT.to_string()).or_default();
        }
        for (parent, value) in children {
            Mock::given(method("GET"))
                .and(path(format!("/me/drive/items/{parent}/children")))
                .and(query_param("$top", "1000"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": value
                })))
                .mount(&self.server)
                .await;
        }
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
        // Most scenarios exercise cTag fallback. Tests that care about content
        // hashes provide a real quickXorHash for their exact byte fixture.
        "file": {"mimeType": "text/plain"},
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

fn ctime(path: &Path) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).unwrap();
    (metadata.ctime(), metadata.ctime_nsec())
}

fn mtime_rfc3339(path: &Path) -> String {
    time::OffsetDateTime::from_unix_timestamp(mtime_secs(path))
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

// ---------------------------------------------------------------------------

include!("sync_cases/remote_and_safety.rs");
include!("sync_cases/initial_delta.rs");

include!("sync_cases/selection_and_transfer.rs");

include!("sync_cases/conflicts_and_uploads.rs");
include!("sync_cases/on_demand.rs");
include!("sync_cases/moves.rs");
include!("sync_cases/materialization_and_incremental.rs");
