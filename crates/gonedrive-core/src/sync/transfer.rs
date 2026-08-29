//! Moving bytes: downloads land atomically, uploads resume where they stopped.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::StreamExt;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::config::Config;
use crate::db::{Store, UploadSessionRecord};
use crate::error::{Error, Result};
use crate::graph::{ChunkOutcome, GraphClient};
use crate::models::DriveItem;
use crate::sync::plan::to_filetime;

/// Enforces a bytes-per-second ceiling by sleeping off any surplus.
///
/// Deliberately simple: the elapsed wall clock says how many bytes should have
/// passed; anything beyond that waits. Precise shaping is not the point — not
/// saturating the user's uplink while they are on a call is.
pub struct RatePacer {
    limit: u64,
    started: std::time::Instant,
    transferred: u64,
}

impl RatePacer {
    /// `limit` of zero disables pacing entirely.
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            started: std::time::Instant::now(),
            transferred: 0,
        }
    }

    /// How long to sleep after moving `bytes` more.
    pub fn record(&mut self, bytes: u64) -> Option<Duration> {
        if self.limit == 0 {
            return None;
        }
        self.transferred += bytes;
        let due = std::time::Duration::from_secs_f64(self.transferred as f64 / self.limit as f64);
        due.checked_sub(self.started.elapsed()).filter(|d| !d.is_zero())
    }

    pub async fn pace(&mut self, bytes: u64) {
        if let Some(delay) = self.record(bytes) {
            tokio::time::sleep(delay).await;
        }
    }
}

use std::time::Duration;

/// Suffix for the temporary file a download is written to.
const PARTIAL_SUFFIX: &str = ".gonedrive-partial";

/// Download an item's content to `dest`.
///
/// The bytes go to a sibling temporary file and are renamed into place only
/// once the transfer completes, so a crash or a lost connection can never leave
/// a truncated file where the user expects their document. The rename is atomic
/// because the temporary file is deliberately in the same directory, and
/// therefore the same filesystem.
/// `on_progress` returning `false` aborts the transfer. Without a way out, a
/// user who opened a multi-gigabyte file by accident has nothing to do but
/// wait for it or kill the application that is blocked on it.
pub async fn download_to_file(
    graph: &GraphClient,
    item_id: &str,
    dest: &Path,
    etag: Option<&str>,
    mtime: Option<OffsetDateTime>,
    mut on_progress: impl FnMut(u64, Option<u64>) -> bool,
) -> Result<u64> {
    download_to_file_limited(graph, item_id, dest, etag, mtime, 0, &mut on_progress).await
}

/// As [`download_to_file`], with a bytes-per-second ceiling (zero = none).
///
/// The transfer resumes: an attempt cut short by a dropped link leaves its
/// bytes in the temporary file, and the next call continues from that offset
/// with an HTTP range request. `etag` guards the resume — the service replies
/// `206 Partial Content` and streams only the remainder while the content still
/// matches, or `200 OK` with the whole file if it changed under us, in which
/// case the stale prefix is discarded. Without an `etag` we cannot prove a
/// leftover prefix still belongs to the current version, so we start over.
pub async fn download_to_file_limited(
    graph: &GraphClient,
    item_id: &str,
    dest: &Path,
    etag: Option<&str>,
    mtime: Option<OffsetDateTime>,
    rate_limit: u64,
    on_progress: &mut (impl FnMut(u64, Option<u64>) -> bool + ?Sized),
) -> Result<u64> {
    let parent = dest
        .parent()
        .ok_or_else(|| Error::other(format!("{} has no parent directory", dest.display())))?;
    tokio::fs::create_dir_all(parent).await?;

    let partial = partial_path(dest);

    // Pick up where an interrupted attempt left off, but only when we can
    // validate the leftover bytes against a version (`etag`); otherwise a stale
    // prefix could be spliced onto new content.
    let resume_from = match (etag, tokio::fs::metadata(&partial).await) {
        (Some(_), Ok(meta)) if meta.len() > 0 => meta.len(),
        _ => {
            let _ = tokio::fs::remove_file(&partial).await;
            0
        }
    };

    let response = graph.download_from(item_id, resume_from, etag).await?;

    // The service honours the resume only with 206; a 200 means it sent the
    // whole file (the eTag moved, or the range was ignored), so the leftover
    // prefix is discarded and we write from the top.
    let resuming =
        resume_from > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let total = if resuming {
        content_range_total(&response)
    } else {
        response.content_length()
    };

    let mut file = if resuming {
        tokio::fs::OpenOptions::new().append(true).open(&partial).await?
    } else {
        tokio::fs::File::create(&partial).await?
    };
    let mut written = if resuming { resume_from } else { 0 };
    let mut stream = response.bytes_stream();
    let mut pacer = RatePacer::new(rate_limit);

    let result = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            pacer.pace(chunk.len() as u64).await;
            if !on_progress(written, total) {
                return Err(Error::Cancelled);
            }
        }
        file.flush().await?;
        // Durability before the rename: otherwise a power loss can publish the
        // new name while the data behind it is still in page cache.
        file.sync_all().await?;
        Ok::<_, Error>(())
    }
    .await;

    if let Err(e) = result {
        // Keep the partial so the next attempt resumes from here — but only for
        // a failure a retry can recover from. A permanent error discards it, so
        // a genuinely broken transfer leaves no mis-resumable litter.
        if !e.is_transient() {
            let _ = tokio::fs::remove_file(&partial).await;
        }
        return Err(e);
    }
    drop(file);

    // A short read means the connection ended early. The prefix on disk is
    // valid (we wrote exactly the bytes streamed, in order), so keep it and
    // fail transiently: the next attempt resumes from here rather than
    // publishing a truncated file as a successful sync.
    if let Some(total) = total {
        if written != total {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("{} ended after {written} of {total} bytes", dest.display()),
            )));
        }
    }

    tokio::fs::rename(&partial, dest).await?;
    if let Some(mtime) = mtime {
        set_mtime(dest, mtime)?;
    }
    Ok(written)
}

/// Total size from a `Content-Range: bytes START-END/TOTAL` header, if present.
fn content_range_total(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .rsplit('/')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Stamp a local file's modification time.
///
/// Without this every downloaded file carries the time it was fetched, and the
/// next local scan reads that as a local edit and uploads it straight back.
pub fn set_mtime(path: &Path, mtime: OffsetDateTime) -> Result<()> {
    // A directory cannot be opened for writing, so fall back to a read-only
    // handle — the kernel allows the owner to set times through one.
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .or_else(|_| std::fs::File::open(path))?;
    let times = std::fs::FileTimes::new().set_modified(to_filetime(mtime));
    file.set_times(times)?;
    Ok(())
}

fn partial_path(dest: &Path) -> PathBuf {
    staging_sibling(dest)
}

/// The staging file that belongs beside `path`.
///
/// Dot-prefixed on purpose: these exist for seconds, but a file manager showing
/// one for even that long reads as the client littering the user's folders —
/// and when a hand-off fails, the orphan would sit in plain view until the
/// janitor sweeps it. Hidden is where a scratch file belongs.
pub fn staging_sibling(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(PARTIAL_SUFFIX);
    path.with_file_name(name)
}

/// Where an upload should go and what it should be called.
pub struct UploadTarget<'a> {
    pub account_id: &'a str,
    pub parent_id: &'a str,
    pub name: &'a str,
    /// The remote item being replaced, when this is an edit rather than a new
    /// file. Uploading to a known id avoids the whole conflict-behaviour
    /// question — see `GraphClient::upload_simple`.
    pub item_id: Option<&'a str>,
    /// The etag of the remote item being replaced. Sent as `If-Match` so the
    /// service refuses (412) an overwrite of a version we have not pulled,
    /// rather than silently losing whoever edited it in the meantime.
    pub etag: Option<&'a str>,
    /// Stable key for resuming: the item id when we have one, else the path.
    pub session_key: &'a str,
}

/// Upload a local file, resuming a previous attempt when one is recorded.
///
/// Files under the 4 MiB simple-upload ceiling go in a single request; anything
/// larger uses a resumable session whose committed offset is persisted after
/// every chunk, so a daemon restart mid-transfer costs one chunk rather than
/// the whole file.
pub async fn upload_file(
    graph: &GraphClient,
    store: &Store,
    config: &Config,
    source: &Path,
    target: UploadTarget<'_>,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<DriveItem> {
    let meta = tokio::fs::metadata(source).await?;
    let size = meta.len();
    let mtime = OffsetDateTime::from(meta.modified()?);

    if size < config.upload_session_threshold {
        let content = Bytes::from(tokio::fs::read(source).await?);
        on_progress(size, size);
        return graph
            .upload_simple(
                target.parent_id,
                target.name,
                target.item_id,
                content,
                Some(mtime),
                target.etag,
            )
            .await;
    }

    upload_resumable(graph, store, config, source, size, mtime, target, &mut on_progress).await
}

#[allow(clippy::too_many_arguments)]
async fn upload_resumable(
    graph: &GraphClient,
    store: &Store,
    config: &Config,
    source: &Path,
    size: u64,
    mtime: OffsetDateTime,
    target: UploadTarget<'_>,
    on_progress: &mut impl FnMut(u64, u64),
) -> Result<DriveItem> {
    let mut session = resume_or_create(graph, store, &target, size, mtime).await?;
    let mut offset = session.committed_offset;

    let mut file = tokio::fs::File::open(source).await?;
    let mut pacer = RatePacer::new(config.upload_limit_bytes_per_sec);

    loop {
        if offset >= size {
            // Every byte is accounted for but the service never returned the
            // finished item, so the session is not in a state we can trust.
            store.delete_upload_session(target.session_key)?;
            return Err(Error::UploadSessionExpired {
                item: target.name.to_string(),
                reason: "all bytes were sent but the upload never completed".into(),
            });
        }

        let len = config.upload_chunk_size.min(size - offset);
        let chunk = read_chunk(&mut file, offset, len as usize).await?;

        match graph
            .upload_chunk(&session.upload_url, offset, size, chunk)
            .await
        {
            Ok(ChunkOutcome::Completed(item)) => {
                store.delete_upload_session(target.session_key)?;
                on_progress(size, size);
                return Ok(*item);
            }
            Ok(ChunkOutcome::Accepted { next_offset }) => {
                // Trust the service's view of what it received over our own
                // arithmetic; a retried chunk can leave the two disagreeing.
                offset = next_offset.unwrap_or(offset + len);
                store.set_upload_offset(target.session_key, offset)?;
                pacer.pace(len).await;
                on_progress(offset, size);
            }
            Err(Error::UploadSessionExpired { .. }) => {
                // Start over rather than pushing bytes at a dead URL. The file
                // is re-read from the beginning, which is the only correct
                // recovery when the service has discarded the staging area.
                store.delete_upload_session(target.session_key)?;
                session = resume_or_create(graph, store, &target, size, mtime).await?;
                offset = 0;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn resume_or_create(
    graph: &GraphClient,
    store: &Store,
    target: &UploadTarget<'_>,
    size: u64,
    mtime: OffsetDateTime,
) -> Result<UploadSessionRecord> {
    if let Some(existing) = store.upload_session(target.session_key)? {
        let usable = existing.total_size == size
            && existing
                .expires_at
                .is_none_or(|t| t > OffsetDateTime::now_utc());
        if usable {
            tracing::debug!(
                key = target.session_key,
                offset = existing.committed_offset,
                "resuming upload"
            );
            return Ok(existing);
        }
        // The file changed underneath us, or the session aged out.
        store.delete_upload_session(target.session_key)?;
    }

    let created = graph
        .create_upload_session(
            target.parent_id,
            target.name,
            target.item_id,
            Some(mtime),
            target.etag,
        )
        .await?;

    let record = UploadSessionRecord {
        key: target.session_key.to_string(),
        account_id: target.account_id.to_string(),
        upload_url: created.upload_url,
        expires_at: created.expiration_date_time,
        committed_offset: 0,
        total_size: size,
    };
    store.save_upload_session(&record)?;
    Ok(record)
}

async fn read_chunk(file: &mut tokio::fs::File, offset: u64, len: usize) -> Result<Bytes> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf).await?;
    Ok(Bytes::from(buf))
}

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn an_interrupted_download_resumes_from_its_partial() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Regression (weak network): a transfer cut short by a dropped link
        // must continue from the bytes already on disk, not restart — a large
        // file on a flaky connection may never finish otherwise.
        let content: &[u8] = b"0123456789ABCDEF"; // 16 bytes
        let server = MockServer::start().await;
        let graph = GraphClient::new(reqwest::Client::new(), crate::TokenSource::Static("t".into()))
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

        let written =
            download_to_file_limited(&graph, "F1", &dest, Some("etag-c1"), None, 0, &mut |_, _| true)
                .await
                .unwrap();

        assert_eq!(written, 16);
        assert_eq!(std::fs::read(&dest).unwrap(), content, "the whole file, resumed");
        assert!(!partial_path(&dest).exists(), "the partial was renamed into place");
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
}
