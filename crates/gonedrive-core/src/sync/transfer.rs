//! Download transport and transfer-wide file/version primitives.

mod upload;

pub(crate) use upload::upload_file_rooted;
pub use upload::{file_fingerprint, upload_file, UploadTarget};

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::graph::GraphClient;
use crate::sync::plan::to_filetime;
use crate::sync::rooted_fs::RootedFs;

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
        self.transferred = self.transferred.saturating_add(bytes);
        let whole_seconds = self.transferred / self.limit;
        let fractional = self.transferred % self.limit;
        let due = Duration::from_secs(whole_seconds).saturating_add(Duration::from_secs_f64(
            fractional as f64 / self.limit as f64,
        ));
        due.checked_sub(self.started.elapsed())
            .filter(|d| !d.is_zero())
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

/// Linux filesystems commonly cap one directory entry at 255 bytes. A valid
/// remote name can sit right at that boundary, so appending our scratch suffix
/// verbatim would make the *internal* name invalid even though the destination
/// itself is representable.
const LOCAL_NAME_MAX_BYTES: usize = 255;

/// Identity and high-resolution timestamps for a local file at one instant.
///
/// Whole-second mtimes are sufficient for ordinary reconciliation, but not for
/// protecting a file while an HTTP request is in flight: an editor can rewrite
/// the same number of bytes inside one second. ctime also catches metadata and
/// in-place writes whose mtime was deliberately restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileVersion {
    exists: bool,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl FileVersion {
    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;

        Self {
            exists: true,
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
        }
    }

    pub(crate) fn capture(path: &Path) -> Result<Self> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => Ok(Self::from_metadata(&meta)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                exists: false,
                dev: 0,
                ino: 0,
                size: 0,
                mtime: 0,
                mtime_nsec: 0,
                ctime: 0,
                ctime_nsec: 0,
            }),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn capture_rooted(rooted: &RootedFs, path: &Path) -> Result<Self> {
        Ok(match rooted.metadata(path)? {
            Some(meta) => Self::from_metadata(&meta),
            None => Self::missing(),
        })
    }

    pub(crate) fn capture_file(file: &std::fs::File) -> Result<Self> {
        Ok(Self::from_metadata(&file.metadata()?))
    }

    fn missing() -> Self {
        Self {
            exists: false,
            dev: 0,
            ino: 0,
            size: 0,
            mtime: 0,
            mtime_nsec: 0,
            ctime: 0,
            ctime_nsec: 0,
        }
    }

    fn stable_identity(self) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}",
            u8::from(self.exists),
            self.dev,
            self.ino,
            self.size,
            self.mtime,
            self.mtime_nsec
        )
    }

    /// Compare the bytes-bearing properties while ignoring ctime. Writing the
    /// placeholder xattrs changes ctime even though the file content does not.
    pub(crate) fn same_content_as(self, other: Self) -> bool {
        self.exists == other.exists
            && self.dev == other.dev
            && self.ino == other.ino
            && self.size == other.size
            && self.mtime == other.mtime
            && self.mtime_nsec == other.mtime_nsec
    }
}

pub(crate) fn file_changed_error(path: &Path, operation: &str) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "{} changed while {operation}; retrying without overwriting it",
            path.display()
        ),
    ))
}

/// A download that has been atomically published to its destination.
#[derive(Debug)]
pub struct DownloadedFile {
    pub bytes: u64,
    pub(crate) version: FileVersion,
}

/// Inputs for a descriptor-rooted download.
///
/// Keeping the expected destination version beside the remote metadata makes
/// it harder for a caller to omit the local-overwrite precondition or swap two
/// adjacent optional arguments.
pub(crate) struct DownloadSpec<'a> {
    pub(crate) item_id: &'a str,
    pub(crate) dest: &'a Path,
    pub(crate) expected_destination: Option<FileVersion>,
    pub(crate) etag: Option<&'a str>,
    pub(crate) mtime: Option<OffsetDateTime>,
    pub(crate) rate_limit: u64,
}

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
) -> Result<DownloadedFile> {
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
) -> Result<DownloadedFile> {
    download_to_file_limited_inner(
        graph,
        item_id,
        dest,
        etag,
        mtime,
        rate_limit,
        on_progress,
        None,
        None,
    )
    .await
}

pub(crate) async fn download_to_file_limited_rooted(
    rooted: &RootedFs,
    graph: &GraphClient,
    spec: DownloadSpec<'_>,
    on_progress: &mut (impl FnMut(u64, Option<u64>) -> bool + ?Sized),
) -> Result<DownloadedFile> {
    download_to_file_limited_inner(
        graph,
        spec.item_id,
        spec.dest,
        spec.etag,
        spec.mtime,
        spec.rate_limit,
        on_progress,
        Some(rooted),
        spec.expected_destination,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn download_to_file_limited_inner(
    graph: &GraphClient,
    item_id: &str,
    dest: &Path,
    etag: Option<&str>,
    mtime: Option<OffsetDateTime>,
    rate_limit: u64,
    on_progress: &mut (impl FnMut(u64, Option<u64>) -> bool + ?Sized),
    rooted: Option<&RootedFs>,
    expected_destination: Option<FileVersion>,
) -> Result<DownloadedFile> {
    let parent = dest
        .parent()
        .ok_or_else(|| Error::other(format!("{} has no parent directory", dest.display())))?;
    if let Some(rooted) = rooted {
        rooted.ensure_dir(parent)?;
    } else {
        tokio::fs::create_dir_all(parent).await?;
    }
    let destination_version = capture(rooted, dest)?;
    if expected_destination.is_some_and(|expected| expected != destination_version) {
        return Err(file_changed_error(dest, "starting the download"));
    }

    let partial = partial_path(dest);

    // Pick up where an interrupted attempt left off, but only when we can
    // validate the leftover bytes against a version (`etag`); otherwise a stale
    // prefix could be spliced onto new content.
    let partial_meta = match rooted {
        Some(rooted) => rooted.metadata(&partial)?,
        None => tokio::fs::metadata(&partial).await.ok(),
    };
    let mut resume_from = match (etag, partial_meta) {
        (Some(_), Some(meta)) if meta.len() > 0 => meta.len(),
        _ => {
            remove_file(rooted, &partial).await;
            0
        }
    };

    let response = match graph.download_from(item_id, resume_from, etag).await {
        // A partial can be left longer than the current remote object after
        // corruption or a server-side replacement. Retrying that same range
        // would return 416 forever, so restart once from byte zero.
        Err(Error::Graph { status: 416, .. }) if resume_from > 0 => {
            remove_file(rooted, &partial).await;
            resume_from = 0;
            graph.download_from(item_id, 0, None).await?
        }
        result => result?,
    };

    // The service honours the resume only with 206; a 200 means it sent the
    // whole file (the eTag moved, or the range was ignored), so the leftover
    // prefix is discarded and we write from the top.
    let resuming = resume_from > 0 && response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let total = if resuming {
        content_range_total(&response)
    } else {
        response.content_length()
    };

    let mut file = if let Some(rooted) = rooted {
        let file = if resuming {
            rooted.open_append(&partial)?
        } else {
            rooted.create_file(&partial, true)?
        };
        tokio::fs::File::from_std(file)
    } else if resuming {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .await?
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
            written = written.checked_add(chunk.len() as u64).ok_or_else(|| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "download byte count overflowed u64",
                ))
            })?;
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
            remove_file(rooted, &partial).await;
        }
        return Err(e);
    }
    drop(file);

    // A short read means the connection ended early. The prefix on disk is
    // valid (we wrote exactly the bytes streamed, in order), so keep it and
    // fail transiently. An overlong response is invalid and must not become a
    // resumable prefix, because its next range would begin beyond EOF.
    if let Some(total) = total {
        if written < total {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("{} ended after {written} of {total} bytes", dest.display()),
            )));
        }
        if written > total {
            remove_file(rooted, &partial).await;
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} received {written} bytes, exceeding the declared {total}",
                    dest.display()
                ),
            )));
        }
    }

    if let Some(mtime) = mtime {
        // Prepare the timestamp before publishing the inode. Once the rename
        // happens, no follow-up write is allowed to race a user's edit.
        set_mtime_in(rooted, &partial, mtime)?;
    }
    if capture(rooted, dest)? != destination_version {
        return Err(file_changed_error(dest, "downloading"));
    }
    if let Some(rooted) = rooted {
        if destination_version.exists {
            rooted.rename(&partial, dest)?;
        } else {
            rooted.rename_noreplace(&partial, dest).map_err(|error| {
                if matches!(
                    error,
                    Error::Io(ref io_error)
                        if io_error.kind() == std::io::ErrorKind::AlreadyExists
                ) {
                    file_changed_error(dest, "publishing the download")
                } else {
                    error
                }
            })?;
        }
    } else {
        tokio::fs::rename(&partial, dest).await?;
    }
    Ok(DownloadedFile {
        bytes: written,
        version: capture(rooted, dest)?,
    })
}

async fn remove_file(rooted: Option<&RootedFs>, path: &Path) {
    match rooted {
        Some(rooted) => {
            let _ = rooted.remove_file(path);
        }
        None => {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

fn capture(rooted: Option<&RootedFs>, path: &Path) -> Result<FileVersion> {
    match rooted {
        Some(rooted) => FileVersion::capture_rooted(rooted, path),
        None => FileVersion::capture(path),
    }
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

fn set_mtime_in(rooted: Option<&RootedFs>, path: &Path, mtime: OffsetDateTime) -> Result<()> {
    let times = std::fs::FileTimes::new().set_modified(to_filetime(mtime));
    match rooted {
        Some(rooted) => rooted.set_times(path, times),
        None => set_mtime(path, mtime),
    }
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
    if os_name_len(&name) <= LOCAL_NAME_MAX_BYTES {
        return path.with_file_name(name);
    }

    // Downloads resume through this deterministic name after a restart. Hash
    // only the destination component: siblings share a directory, and distinct
    // long names still receive distinct scratch files without leaking length.
    let original = os_name_bytes(path.file_name().unwrap_or_default());
    let digest = Sha256::digest(original);
    path.with_file_name(format!(
        ".gonedrive-download-{}{}",
        hex::encode(&digest[..16]),
        PARTIAL_SUFFIX
    ))
}

/// A unique hand-off file for one fanotify hydration request.
///
/// It deliberately still ends in `PARTIAL_SUFFIX`, so the local scanner and
/// startup janitor treat it as scratch. It must not share the ordinary
/// download's staging name: the helper deletes its hand-off after filling, and
/// deleting an unrelated background download at that point corrupts both.
pub fn hydration_staging_sibling(path: &Path) -> PathBuf {
    unique_staging_sibling(path, "hydration")
}

/// A hidden, unique sibling used to publish a safety-locked placeholder with a
/// single no-replace rename. It retains the ordinary partial suffix so both the
/// scanner and startup janitor treat a crash residue as internal scratch.
pub fn placeholder_staging_sibling(path: &Path) -> PathBuf {
    unique_staging_sibling(path, "placeholder")
}

fn unique_staging_sibling(path: &Path, purpose: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // These hand-offs never resume and NEXT already provides uniqueness, so
    // carrying the user-visible name serves no purpose and can exceed NAME_MAX.
    path.with_file_name(format!(
        ".gonedrive-{purpose}-{}-{id}{}",
        std::process::id(),
        PARTIAL_SUFFIX
    ))
}

#[cfg(unix)]
fn os_name_bytes(name: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes()
}

#[cfg(unix)]
fn os_name_len(name: &std::ffi::OsStr) -> usize {
    os_name_bytes(name).len()
}

#[cfg(test)]
mod tests;
