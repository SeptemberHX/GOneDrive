//! Simple and resumable uploads, including durable session recovery.

use std::io::{Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use bytes::Bytes;
use time::OffsetDateTime;
use tokio::io::AsyncReadExt;

use crate::config::Config;
use crate::db::{Store, UploadSessionRecord};
use crate::error::{Error, Result};
use crate::graph::{ChunkOutcome, GraphClient};
use crate::models::DriveItem;
use crate::sync::rooted_fs::RootedFs;

use super::{capture, file_changed_error, FileVersion, RatePacer};

/// Stable identity used to decide whether a persisted upload session still
/// belongs to the file currently at `path`.
pub fn file_fingerprint(path: &Path) -> Result<String> {
    let before = FileVersion::capture(path)?;
    let hash = crate::hash::quick_xor_file(path)?;
    let after = FileVersion::capture(path)?;
    if !after.same_content_as(before) {
        return Err(file_changed_error(path, "hashing it for upload resume"));
    }
    Ok(format!("{}:{hash}", before.stable_identity()))
}

/// Where an upload should go and what it should be called.
pub struct UploadTarget<'a> {
    pub account_id: &'a str,
    pub parent_id: &'a str,
    pub name: &'a str,
    /// The remote item being replaced, when this is an edit rather than a new
    /// file. Uploading to a known id avoids the whole conflict-behaviour
    /// question -- see `GraphClient::upload_simple`.
    pub item_id: Option<&'a str>,
    /// The etag of the remote item being replaced. Sent as `If-Match` so the
    /// service refuses (412) an overwrite of a version we have not pulled,
    /// rather than silently losing whoever edited it in the meantime.
    pub etag: Option<&'a str>,
    /// Stable key for resuming: the item id when we have one, else the path.
    pub session_key: &'a str,
}

pub(crate) struct UploadedFile {
    pub(crate) item: DriveItem,
    pub(crate) quickxor: String,
    /// The file changed after the uploaded bytes were captured. The service
    /// holds a consistent earlier version; the caller commits that and queues
    /// the newer content rather than treating the upload as failed.
    pub(crate) source_changed: bool,
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
    mut on_progress: impl FnMut(u64, u64) -> bool,
) -> Result<DriveItem> {
    Ok(
        upload_file_inner(graph, store, config, source, target, &mut on_progress, None)
            .await?
            .item,
    )
}

pub(crate) async fn upload_file_rooted(
    rooted: &RootedFs,
    graph: &GraphClient,
    store: &Store,
    config: &Config,
    source: &Path,
    target: UploadTarget<'_>,
    mut on_progress: impl FnMut(u64, u64) -> bool,
) -> Result<UploadedFile> {
    upload_file_inner(
        graph,
        store,
        config,
        source,
        target,
        &mut on_progress,
        Some(rooted),
    )
    .await
}

async fn upload_file_inner(
    graph: &GraphClient,
    store: &Store,
    config: &Config,
    source: &Path,
    target: UploadTarget<'_>,
    on_progress: &mut impl FnMut(u64, u64) -> bool,
    rooted: Option<&RootedFs>,
) -> Result<UploadedFile> {
    let path_version = capture(rooted, source)?;
    let mut source_file = match rooted {
        Some(rooted) => rooted.open_read(source)?,
        None => std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(source)?,
    };
    let meta = source_file.metadata()?;
    if !meta.is_file() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", source.display()),
        )));
    }
    let source_version = FileVersion::from_metadata(&meta);
    if source_version != path_version {
        return Err(file_changed_error(source, "opening it for upload"));
    }
    let size = meta.len();
    let mtime = OffsetDateTime::from(meta.modified()?);

    if size < config.upload_session_threshold {
        let capacity = usize::try_from(size).map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{} is too large to buffer in memory", source.display()),
            ))
        })?;
        let mut file = tokio::fs::File::from_std(source_file);
        let mut content = Vec::with_capacity(capacity);
        file.read_to_end(&mut content).await?;
        if capture(rooted, source)? != source_version {
            return Err(file_changed_error(source, "reading it for upload"));
        }
        if !on_progress(size, size) {
            return Err(Error::Cancelled);
        }
        let quickxor = crate::hash::quick_xor(&content);
        let item = graph
            .upload_simple(
                target.parent_id,
                target.name,
                target.item_id,
                Bytes::from(content),
                Some(mtime),
                target.etag,
            )
            .await?;
        let source_changed = capture(rooted, source)? != source_version;
        return Ok(UploadedFile {
            item,
            quickxor,
            source_changed,
        });
    }

    let hash = crate::hash::quick_xor_reader(&mut source_file)?;
    source_file.seek(SeekFrom::Start(0))?;
    let mut source_fingerprint = format!("{}:{hash}", source_version.stable_identity());
    if let Some(precondition) = target.etag {
        // A resumable session carries If-Match only when it is created. Bind
        // the persisted session to that remote version so a later pass cannot
        // resume an old session after delta observed a newer cloud edit.
        source_fingerprint.push_str(&format!(":remote:{}:{precondition}", precondition.len()));
    }
    if !capture(rooted, source)?.same_content_as(source_version) {
        return Err(file_changed_error(source, "preparing an upload session"));
    }
    let quickxor = hash.clone();
    let item = upload_resumable(
        graph,
        store,
        config,
        source,
        tokio::fs::File::from_std(source_file),
        size,
        mtime,
        source_version,
        source_fingerprint,
        hash,
        target,
        on_progress,
        rooted,
    )
    .await?;
    let source_changed = capture(rooted, source)? != source_version;
    Ok(UploadedFile {
        item,
        quickxor,
        source_changed,
    })
}

#[allow(clippy::too_many_arguments)]
async fn upload_resumable(
    graph: &GraphClient,
    store: &Store,
    config: &Config,
    source: &Path,
    mut file: tokio::fs::File,
    size: u64,
    mtime: OffsetDateTime,
    source_version: FileVersion,
    source_fingerprint: String,
    expected_quickxor: String,
    target: UploadTarget<'_>,
    on_progress: &mut impl FnMut(u64, u64) -> bool,
    rooted: Option<&RootedFs>,
) -> Result<DriveItem> {
    let commit = UploadCommit {
        store,
        target: &target,
        source,
        source_version,
        rooted,
        size,
    };
    let (mut session, resumed) =
        resume_or_create(graph, store, &target, size, mtime, &source_fingerprint).await?;
    let mut offset = session.committed_offset;
    let mut pacer = RatePacer::new(config.upload_limit_bytes_per_sec);

    // `offset == size` is a durable marker for a final 200/201 whose driveItem
    // body was lost. Never send bytes or create a new same-name session from
    // that state; independently verify the cloud item until it becomes visible.
    if offset == size {
        return finish_reconciled_upload(
            graph,
            &commit,
            &expected_quickxor,
            "the final response was unreadable",
            on_progress,
        )
        .await;
    }

    if resumed {
        match graph.upload_session_status(&session.upload_url).await {
            Ok(status) => {
                let Some(server_offset) = status.next_offset() else {
                    store.set_upload_offset(target.account_id, target.session_key, size)?;
                    return finish_reconciled_upload(
                        graph,
                        &commit,
                        &expected_quickxor,
                        "the upload status contained no missing range",
                        on_progress,
                    )
                    .await;
                };
                if server_offset > size {
                    store.delete_upload_session(target.account_id, target.session_key)?;
                    return Err(Error::UploadSessionExpired {
                        item: target.name.to_string(),
                        reason: format!(
                            "the service requested byte {server_offset} beyond the {size}-byte file"
                        ),
                    });
                }
                offset = server_offset;
                session.committed_offset = offset;
                session.expires_at = status.expiration_date_time;
                store.save_upload_session(&session)?;
                if offset == size {
                    return finish_reconciled_upload(
                        graph,
                        &commit,
                        &expected_quickxor,
                        "the service reported that no bytes remain",
                        on_progress,
                    )
                    .await;
                }
                if !on_progress(offset, size) {
                    return Err(Error::Cancelled);
                }
            }
            Err(Error::UploadSessionExpired { .. }) => {
                if let Some(item) =
                    matching_remote_upload(graph, &target, size, &expected_quickxor).await?
                {
                    return finish_upload(&commit, item, on_progress);
                }
                store.delete_upload_session(target.account_id, target.session_key)?;
                (session, _) =
                    resume_or_create(graph, store, &target, size, mtime, &source_fingerprint)
                        .await?;
                offset = 0;
            }
            Err(error) => return Err(error),
        }
    }

    loop {
        if offset >= size {
            return finish_reconciled_upload(
                graph,
                &commit,
                &expected_quickxor,
                "all bytes were accepted without a readable final item",
                on_progress,
            )
            .await;
        }

        let len = config.upload_chunk_size.min(size - offset);
        let chunk_len = usize::try_from(len).map_err(|_| {
            Error::Config("upload_chunk_size does not fit this platform's address space".into())
        })?;
        let chunk = read_chunk(&mut file, offset, chunk_len).await?;

        match graph
            .upload_chunk(&session.upload_url, offset, size, chunk)
            .await
        {
            Ok(ChunkOutcome::Completed(item)) => {
                return finish_upload(&commit, *item, on_progress);
            }
            Ok(ChunkOutcome::CompletionUnknown { reason }) => {
                // Persist the ambiguity before doing another network request so
                // a crash cannot make the next process resend the final bytes.
                store.set_upload_offset(target.account_id, target.session_key, size)?;
                return finish_reconciled_upload(
                    graph,
                    &commit,
                    &expected_quickxor,
                    &reason,
                    on_progress,
                )
                .await;
            }
            Ok(ChunkOutcome::Accepted {
                next_offset,
                expires_at,
            }) => {
                // Trust the service's view of what it received over our own
                // arithmetic; a retried chunk can leave the two disagreeing.
                let next = next_offset.unwrap_or(offset + len);
                if next <= offset || next > size {
                    store.delete_upload_session(target.account_id, target.session_key)?;
                    return Err(Error::UploadSessionExpired {
                        item: target.name.to_string(),
                        reason: format!(
                            "the service returned invalid next offset {next} after {offset}"
                        ),
                    });
                }
                offset = next;
                session.committed_offset = offset;
                session.expires_at = expires_at.or(session.expires_at);
                store.save_upload_session(&session)?;
                pacer.pace(len).await;
                if !on_progress(offset, size) {
                    return Err(Error::Cancelled);
                }
            }
            Err(Error::UploadSessionExpired { .. }) => {
                if let Some(item) =
                    matching_remote_upload(graph, &target, size, &expected_quickxor).await?
                {
                    return finish_upload(&commit, item, on_progress);
                }
                // Start over rather than pushing bytes at a dead URL. The file
                // is re-read from the beginning, which is the only correct
                // recovery when the service has discarded the staging area.
                store.delete_upload_session(target.account_id, target.session_key)?;
                (session, _) =
                    resume_or_create(graph, store, &target, size, mtime, &source_fingerprint)
                        .await?;
                offset = 0;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn resume_or_create(
    graph: &GraphClient,
    store: &Store,
    target: &UploadTarget<'_>,
    size: u64,
    mtime: OffsetDateTime,
    source_fingerprint: &str,
) -> Result<(UploadSessionRecord, bool)> {
    if let Some(existing) = store.upload_session(target.account_id, target.session_key)? {
        let usable = existing.total_size == size
            && existing.committed_offset <= size
            && existing.source_fingerprint.as_deref() == Some(source_fingerprint);
        if usable {
            tracing::debug!(
                key = target.session_key,
                offset = existing.committed_offset,
                "resuming upload"
            );
            return Ok((existing, true));
        }
        // The file changed underneath us or the persisted state is invalid.
        // Do not reject a matching session from its cached expiry alone: every
        // accepted fragment extends that expiry and a crash can happen before
        // the refreshed timestamp is saved. The status GET above is authoritative.
        store.delete_upload_session(target.account_id, target.session_key)?;
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
        source_fingerprint: Some(source_fingerprint.to_string()),
    };
    store.save_upload_session(&record)?;
    Ok((record, false))
}

struct UploadCommit<'a, 'target> {
    store: &'a Store,
    target: &'a UploadTarget<'target>,
    source: &'a Path,
    source_version: FileVersion,
    rooted: Option<&'a RootedFs>,
    size: u64,
}

async fn finish_reconciled_upload(
    graph: &GraphClient,
    commit: &UploadCommit<'_, '_>,
    expected_quickxor: &str,
    reason: &str,
    on_progress: &mut impl FnMut(u64, u64) -> bool,
) -> Result<DriveItem> {
    if let Some(item) =
        matching_remote_upload(graph, commit.target, commit.size, expected_quickxor).await?
    {
        return finish_upload(commit, item, on_progress);
    }
    Err(Error::UploadCommitUncertain {
        item: commit.target.name.to_string(),
        reason: reason.to_string(),
    })
}

async fn matching_remote_upload(
    graph: &GraphClient,
    target: &UploadTarget<'_>,
    size: u64,
    expected_quickxor: &str,
) -> Result<Option<DriveItem>> {
    let result = match target.item_id {
        Some(item_id) => graph.item(item_id).await,
        None => graph.child_by_name(target.parent_id, target.name).await,
    };
    let item = match result {
        Ok(item) => item,
        Err(Error::Graph { status: 404, .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let remote_hash = item
        .hashes()
        .and_then(|hashes| hashes.quick_xor_hash.as_deref());
    if item.size == Some(size) && remote_hash == Some(expected_quickxor) {
        Ok(Some(item))
    } else {
        tracing::warn!(
            name = target.name,
            remote_size = ?item.size,
            expected_size = size,
            remote_hash = ?remote_hash,
            "cloud item did not match an ambiguously completed upload"
        );
        Ok(None)
    }
}

fn finish_upload(
    commit: &UploadCommit<'_, '_>,
    item: DriveItem,
    on_progress: &mut impl FnMut(u64, u64) -> bool,
) -> Result<DriveItem> {
    commit
        .store
        .delete_upload_session(commit.target.account_id, commit.target.session_key)?;
    // The service has committed the file. Finalise bookkeeping even if the
    // scheduling slice expired during the last request.
    let _ = on_progress(commit.size, commit.size);
    if capture(commit.rooted, commit.source)? != commit.source_version {
        return Err(file_changed_error(commit.source, "uploading"));
    }
    Ok(item)
}

pub(super) async fn read_chunk(
    file: &mut tokio::fs::File,
    offset: u64,
    len: usize,
) -> Result<Bytes> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buffer = vec![0u8; len];
    file.read_exact(&mut buffer).await?;
    Ok(Bytes::from(buffer))
}
