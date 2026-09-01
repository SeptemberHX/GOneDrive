use crate::config::Config;
use crate::db::Store;

use super::upload::read_chunk;
use super::*;

#[test]
fn a_zero_limit_never_paces() {
    let mut pacer = RatePacer::new(0);
    assert!(pacer.record(u64::MAX).is_none());
}

#[test]
fn the_pacer_sleeps_off_exactly_the_surplus() {
    // 1 MiB/s ceiling; pretend 1 MiB arrived instantly. The pacer should
    // ask for ~1s of sleep (minus the microseconds that really elapsed).
    let mut pacer = RatePacer::new(1024 * 1024);
    let delay = pacer.record(1024 * 1024).expect("should require a pause");
    assert!(delay <= Duration::from_secs(1));
    assert!(delay > Duration::from_millis(900), "asked only {delay:?}");

    // A transfer that is already slower than the ceiling never waits.
    let mut slow = RatePacer::new(u64::MAX / 2);
    assert!(slow.record(10).is_none());
}

#[test]
fn the_pacer_does_not_overflow_its_lifetime_counter() {
    let mut pacer = RatePacer::new(1);
    assert!(pacer.record(u64::MAX).is_some());
    assert!(pacer.record(1).is_some());
}

#[test]
fn partial_files_are_siblings_so_the_rename_is_atomic() {
    let dest = Path::new("/home/u/OneDrive/Docs/report.docx");
    let partial = partial_path(dest);
    assert_eq!(partial.parent(), dest.parent());
    // Dot-prefixed so the file manager never shows it.
    assert_eq!(
        partial.file_name().unwrap(),
        ".report.docx.gonedrive-partial"
    );
}

#[test]
fn partial_naming_handles_names_without_an_extension() {
    assert_eq!(
        partial_path(Path::new("/a/README")).file_name().unwrap(),
        ".README.gonedrive-partial"
    );
}

#[test]
fn hydration_staging_is_unique_and_still_hidden_from_scans() {
    let path = Path::new("/a/report.txt");
    let first = hydration_staging_sibling(path);
    let second = hydration_staging_sibling(path);
    assert_ne!(first, second);
    assert!(first
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(PARTIAL_SUFFIX));
    assert_eq!(first.parent(), path.parent());
}

#[test]
fn long_destination_names_get_bounded_deterministic_scratch_names() {
    use std::os::unix::ffi::OsStrExt;

    // The destination itself fits ext4, but adding our old suffix did not.
    let destination = PathBuf::from("/a").join("x".repeat(245));
    let first = staging_sibling(&destination);
    let second = staging_sibling(&destination);

    assert_eq!(first, second, "restart must find the same partial download");
    assert!(first.file_name().unwrap().as_bytes().len() <= 255);
    assert!(first
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(PARTIAL_SUFFIX));
}

#[test]
fn multibyte_names_and_unique_handoffs_never_exceed_name_max() {
    use std::os::unix::ffi::OsStrExt;

    // 80 CJK code points are only 80 visible characters but 240 UTF-8 bytes.
    let destination = PathBuf::from("/a").join("文".repeat(80));
    for scratch in [
        staging_sibling(&destination),
        hydration_staging_sibling(&destination),
        placeholder_staging_sibling(&destination),
    ] {
        assert!(
            scratch.file_name().unwrap().as_bytes().len() <= 255,
            "{} is too long",
            scratch.display()
        );
        assert!(scratch
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(PARTIAL_SUFFIX));
    }
}

#[tokio::test]
async fn an_interrupted_download_resumes_from_its_partial() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Regression (weak network): a transfer cut short by a dropped link
    // must continue from the bytes already on disk, not restart — a large
    // file on a flaky connection may never finish otherwise.
    let content: &[u8] = b"0123456789ABCDEF"; // 16 bytes
    let server = MockServer::start().await;
    let graph = GraphClient::new(
        reqwest::Client::new(),
        crate::TokenSource::Static("t".into()),
    )
    .with_base_url(server.uri());

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Docs/big.bin");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    // An earlier attempt landed the first 6 bytes before the signal dropped.
    std::fs::write(partial_path(&dest), &content[..6]).unwrap();

    // Only a ranged, version-checked request is served. A full download
    // would 404 here, so the test can only pass if the resume path is taken
    // — and the If-Range guard proves the leftover prefix is validated.
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .and(header("Range", "bytes=6-"))
        .and(header("If-Range", "etag-c1"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 6-15/16")
                .set_body_bytes(&content[6..]),
        )
        .mount(&server)
        .await;

    let written = download_to_file_limited(
        &graph,
        "F1",
        &dest,
        Some("etag-c1"),
        None,
        0,
        &mut |_, _| true,
    )
    .await
    .unwrap();

    assert_eq!(written.bytes, 16);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        content,
        "the whole file, resumed"
    );
    assert!(
        !partial_path(&dest).exists(),
        "the partial was renamed into place"
    );
}

#[tokio::test]
async fn an_oversized_partial_restarts_after_range_not_satisfiable() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let graph = GraphClient::new(
        reqwest::Client::new(),
        crate::TokenSource::Static("t".into()),
    )
    .with_base_url(server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .and(header("Range", "bytes=10-"))
        .respond_with(ResponseTemplate::new(416).set_body_json(serde_json::json!({
            "error": {"code": "invalidRange", "message": "range beyond EOF"}
        })))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"fresh"))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file.bin");
    std::fs::write(partial_path(&dest), b"too-long!!").unwrap();

    let downloaded = download_to_file_limited(
        &graph,
        "F1",
        &dest,
        Some("etag-c1"),
        None,
        0,
        &mut |_, _| true,
    )
    .await
    .unwrap();

    assert_eq!(downloaded.bytes, 5);
    assert_eq!(std::fs::read(dest).unwrap(), b"fresh");
}

#[tokio::test]
async fn an_overlong_range_response_is_never_published_or_resumed() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let graph = GraphClient::new(
        reqwest::Client::new(),
        crate::TokenSource::Static("t".into()),
    )
    .with_base_url(server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .and(header("Range", "bytes=1-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 1-2/3")
                .set_body_bytes(b"2345"),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("file.bin");
    let partial = partial_path(&dest);
    std::fs::write(&partial, b"1").unwrap();

    let result = download_to_file_limited(
        &graph,
        "F1",
        &dest,
        Some("etag-c1"),
        None,
        0,
        &mut |_, _| true,
    )
    .await;

    assert!(matches!(
        result,
        Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData
    ));
    assert!(!dest.exists());
    assert!(!partial.exists());
}

#[tokio::test]
async fn a_local_edit_during_download_is_not_overwritten() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let graph = GraphClient::new(
        reqwest::Client::new(),
        crate::TokenSource::Static("t".into()),
    )
    .with_base_url(server.uri());
    Mock::given(method("GET"))
        .and(path("/me/drive/items/F1/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_bytes(b"remote version"),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("report.txt");
    std::fs::write(&dest, b"old version").unwrap();

    let download = download_to_file(&graph, "F1", &dest, None, None, |_, _| true);
    let edit = async {
        tokio::time::sleep(Duration::from_millis(40)).await;
        std::fs::write(&dest, b"local edit").unwrap();
    };
    let (result, ()) = tokio::join!(download, edit);

    assert!(
        matches!(
            result,
            Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "unexpected download result: {result:?}"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"local edit");
    assert!(
        partial_path(&dest).exists(),
        "the remote bytes remain resumable"
    );
}

#[tokio::test]
async fn a_local_edit_during_upload_is_retried() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let graph = GraphClient::new(
        reqwest::Client::new(),
        crate::TokenSource::Static("t".into()),
    )
    .with_base_url(server.uri());
    let response = serde_json::json!({
        "id": "F1",
        "name": "report.txt",
        "size": 11,
        "file": {"mimeType": "text/plain"},
        "parentReference": {"driveId": "drive", "id": "ROOT"}
    });
    Mock::given(method("PUT"))
        .and(path("/me/drive/items/F1/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_json(response),
        )
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/me/drive/items/F1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "F1",
            "name": "report.txt",
            "size": 11,
            "file": {"mimeType": "text/plain"},
            "parentReference": {"driveId": "drive", "id": "ROOT"}
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("report.txt");
    std::fs::write(&source, b"old version").unwrap();
    let store = Store::open_in_memory().unwrap();
    let config = Config::default();

    let upload = upload_file(
        &graph,
        &store,
        &config,
        &source,
        UploadTarget {
            account_id: "acct",
            parent_id: "ROOT",
            name: "report.txt",
            item_id: Some("F1"),
            etag: None,
            session_key: "F1",
        },
        |_, _| true,
    );
    let edit = async {
        tokio::time::sleep(Duration::from_millis(40)).await;
        std::fs::write(&source, b"local edit!").unwrap();
    };
    let (result, ()) = tokio::join!(upload, edit);

    assert!(
        matches!(
            result,
            Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::WouldBlock
        ),
        "unexpected upload result: {result:?}"
    );
    assert_eq!(std::fs::read(&source).unwrap(), b"local edit!");
}

#[tokio::test]
async fn set_mtime_round_trips_through_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, b"hello").unwrap();

    let when = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
    set_mtime(&path, when).unwrap();

    let meta = std::fs::metadata(&path).unwrap();
    let got = OffsetDateTime::from(meta.modified().unwrap());
    assert_eq!(got.unix_timestamp(), when.unix_timestamp());
}

#[tokio::test]
async fn read_chunk_reads_the_requested_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("blob");
    let data: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    std::fs::write(&path, &data).unwrap();

    let mut file = tokio::fs::File::open(&path).await.unwrap();
    let chunk = read_chunk(&mut file, 1000, 200).await.unwrap();
    assert_eq!(&chunk[..], &data[1000..1200]);
}
