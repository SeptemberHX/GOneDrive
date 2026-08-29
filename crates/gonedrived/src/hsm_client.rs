//! The daemon's side of the files-on-demand conversation.
//!
//! The privileged helper knows only paths; everything that requires an account
//! — which item a path is, how to fetch it, where the bytes come from — happens
//! here, unprivileged. The link between the two is the item id stored in the
//! placeholder's own extended attributes, so the helper never has to be told
//! about the database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use gonedrive_ipc::hsm::{Event, Request, MAX_FRAME_SIZE, SOCKET_PATH};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch, Mutex};

use crate::app::App;

use gonedrive_ipc::reader_policy::is_background_reader as background_reader;

type MarkResult = std::result::Result<Option<FileIdentity>, String>;
type MarkWaiters = HashMap<u64, oneshot::Sender<MarkResult>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

type WatchedFiles = HashMap<PathBuf, FileIdentity>;

/// A live connection to the privileged helper.
pub struct HsmClient {
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    closed: watch::Receiver<bool>,
    next_request: AtomicU64,
    pending_marks: Arc<Mutex<MarkWaiters>>,
    watched: Arc<Mutex<WatchedFiles>>,
}

impl HsmClient {
    /// Connect and start serving hydration requests.
    ///
    /// Returns an error rather than retrying when the helper is absent: files
    /// on demand simply is not available then, and pretending otherwise would
    /// leave the user with placeholders that read as zeroes.
    pub async fn connect(app: Arc<App>, socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket).await.with_context(|| {
            format!(
                "could not reach the files-on-demand helper at {}.\n\
                 Start it with: sudo gonedrive-hsm",
                socket.display()
            )
        })?;
        let (read_half, write_half) = stream.into_split();
        let writer = Arc::new(Mutex::new(write_half));

        // Set only when the helper explicitly claims it. An old helper would
        // treat a range reply as a whole-file one — writing 16 KiB, dropping
        // the watch, and declaring the file complete — so nothing optional is
        // used until the helper says it understands it.
        let partial_hydration = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (closed_tx, closed) = watch::channel(false);
        let pending_marks = Arc::new(Mutex::new(HashMap::new()));
        let watched = Arc::new(Mutex::new(HashMap::new()));
        let client = Self {
            writer: writer.clone(),
            closed,
            next_request: AtomicU64::new(1),
            pending_marks: pending_marks.clone(),
            watched: watched.clone(),
        };
        client.send(&Request::Ping).await?;
        client.send(&Request::Features).await?;

        tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(frame)) if !frame.iter().all(u8::is_ascii_whitespace) => {
                        let app = app.clone();
                        let writer = writer.clone();
                        let partial_hydration = partial_hydration.clone();
                        let pending_marks = pending_marks.clone();
                        let watched = watched.clone();
                        // Each hydration runs on its own task: a process is
                        // blocked on it, and a large file must not hold up the
                        // next reader.
                        tokio::spawn(async move {
                            handle(
                                app,
                                writer,
                                partial_hydration,
                                pending_marks,
                                watched,
                                frame,
                            )
                            .await
                        });
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        tracing::warn!("the files-on-demand helper closed the connection");
                        fail_pending_marks(&pending_marks, "helper closed the connection").await;
                        let _ = closed_tx.send(true);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "helper connection failed");
                        fail_pending_marks(&pending_marks, &e.to_string()).await;
                        let _ = closed_tx.send(true);
                        return;
                    }
                }
            }
        });

        Ok(client)
    }

    /// Resolves when the helper connection has died.
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        if *closed.borrow() {
            return;
        }
        while closed.changed().await.is_ok() {
            if *closed.borrow_and_update() {
                return;
            }
        }
    }

    pub fn default_socket() -> PathBuf {
        PathBuf::from(SOCKET_PATH)
    }

    async fn send(&self, request: &Request) -> Result<()> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Ask the kernel to intercept reads of this placeholder.
    pub async fn watch(&self, path: &Path) -> Result<()> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let identity = self
            .request_mark(
                id,
                Request::Watch {
                    id,
                    path: path.to_string_lossy().into_owned(),
                },
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("helper did not return the marked file identity"))?;
        self.watched
            .lock()
            .await
            .insert(path.to_path_buf(), identity);
        Ok(())
    }

    /// Stop intercepting: the file is hydrated, deleted, or no longer ours.
    pub async fn unwatch(&self, path: &Path) -> Result<()> {
        if !self.watched.lock().await.contains_key(path) {
            return Ok(());
        }
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .request_mark(
                id,
                Request::Unwatch {
                    id,
                    path: path.to_string_lossy().into_owned(),
                },
            )
            .await?;
        self.watched.lock().await.remove(path);
        Ok(())
    }

    async fn request_mark(&self, id: u64, request: Request) -> Result<Option<FileIdentity>> {
        const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

        let (tx, rx) = oneshot::channel();
        self.pending_marks.lock().await.insert(id, tx);
        if let Err(error) = self.send(&request).await {
            self.pending_marks.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(ACK_TIMEOUT, rx).await {
            Ok(Ok(Ok(identity))) => Ok(identity),
            Ok(Ok(Err(message))) => anyhow::bail!(message),
            Ok(Err(_)) => anyhow::bail!("helper dropped watch request {id}"),
            Err(_) => {
                self.pending_marks.lock().await.remove(&id);
                anyhow::bail!(
                    "helper did not acknowledge watch request {id} within {ACK_TIMEOUT:?}"
                )
            }
        }
    }

    /// Register every placeholder currently on disk.
    pub async fn watch_all(&self, app: &App) -> Result<usize> {
        let paths = app.dehydrated_paths().await?;
        self.prune_stale_watches().await?;
        let watched = self.watched.lock().await;
        let missing: Vec<_> = paths
            .into_iter()
            .filter(|path| !watched.contains_key(path))
            .collect();
        drop(watched);
        self.watch_paths(app, &missing).await
    }

    /// Register only placeholders created or replaced by the just-finished
    /// coordinator phase.
    pub async fn watch_paths(&self, app: &App, paths: &[PathBuf]) -> Result<usize> {
        let mut watched = 0usize;
        let mut failures = Vec::new();
        for path in paths {
            match self.watch(path).await {
                Ok(()) => watched += 1,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "could not watch");
                    if let Err(discard_error) = app.discard_unwatched_placeholder(path).await {
                        tracing::error!(
                            path = %path.display(),
                            error = %discard_error,
                            "could not hide unsafe placeholder"
                        );
                    }
                    failures.push(format!("{}: {error}", path.display()));
                }
            }
        }
        if failures.is_empty() {
            Ok(watched)
        } else {
            anyhow::bail!(
                "failed to register {} placeholder(s): {}",
                failures.len(),
                failures.join("; ")
            )
        }
    }

    /// Reconcile identities found by the low-frequency read-only verifier.
    /// Unchanged placeholders cost only a HashMap lookup; new inodes are sent
    /// through the helper's descriptor-bound Watch path.
    pub async fn reconcile_observed(
        &self,
        app: &App,
        placeholders: &[(PathBuf, u64, u64)],
    ) -> Result<usize> {
        // Discovery runs concurrently with remote polling, so its absence list
        // may already be stale. Only live xattrs may remove a mark: an older
        // report must never unwatch a placeholder a newer delta just created.
        self.prune_stale_watches().await?;

        let mut changed = Vec::new();
        {
            let watched = self.watched.lock().await;
            for (path, dev, ino) in placeholders {
                if gonedrive_core::hydrate::Placeholder::state(path)
                    != gonedrive_core::hydrate::PlaceholderState::Dehydrated
                {
                    continue;
                }
                let identity = FileIdentity {
                    dev: *dev,
                    ino: *ino,
                };
                if watched.get(path) != Some(&identity) {
                    changed.push(path.clone());
                }
            }
        }
        self.watch_paths(app, &changed).await
    }

    async fn prune_stale_watches(&self) -> Result<()> {
        let stale: Vec<_> = self
            .watched
            .lock()
            .await
            .keys()
            .filter(|path| {
                gonedrive_core::hydrate::Placeholder::state(path)
                    != gonedrive_core::hydrate::PlaceholderState::Dehydrated
            })
            .cloned()
            .collect();
        let mut failures = Vec::new();
        for path in stale {
            if let Err(error) = self.unwatch(&path).await {
                failures.push(format!("{}: {error}", path.display()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "failed to remove stale placeholder marks: {}",
                failures.join("; ")
            )
        }
    }
}

async fn fail_pending_marks(pending: &Mutex<MarkWaiters>, message: &str) {
    let requests: Vec<_> = pending
        .lock()
        .await
        .drain()
        .map(|(_, sender)| sender)
        .collect();
    for sender in requests {
        let _ = sender.send(Err(message.to_string()));
    }
}

async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> std::io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len().saturating_add(newline) > MAX_FRAME_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HSM event exceeds 64 KiB",
                ));
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return Ok(Some(frame));
        }
        if frame.len().saturating_add(available.len()) > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HSM event exceeds 64 KiB",
            ));
        }
        let len = available.len();
        frame.extend_from_slice(available);
        reader.consume(len);
    }
}

async fn handle(
    app: Arc<App>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    partial_hydration: Arc<std::sync::atomic::AtomicBool>,
    pending_marks: Arc<Mutex<MarkWaiters>>,
    watched: Arc<Mutex<WatchedFiles>>,
    frame: Vec<u8>,
) {
    let event: Event = match serde_json::from_slice(&frame) {
        Ok(event) => event,
        Err(e) => {
            tracing::warn!(error = %e, "malformed message from the helper");
            return;
        }
    };

    match event {
        Event::Hydrate {
            id,
            path,
            offset,
            length,
            reader,
            is_open,
        } => {
            let path = PathBuf::from(path);
            // The single most useful line in this log: it names the process
            // that keeps pulling files down. "gedit" is the user; an indexer
            // or thumbnailer here belongs on the refusal list above.
            tracing::info!(
                path = %path.display(),
                reader = %if reader.is_empty() { "unknown" } else { &reader },
                offset,
                length,
                "on-demand read"
            );

            // Policy lives here, unprivileged: a background sweep gets its
            // read refused before a byte is fetched or a window is shown.
            if let Some(matched) = background_reader(&reader) {
                tracing::info!(reader = matched, "refusing a background reader");
                let mut reply = match serde_json::to_string(&Request::Hydrated {
                    id,
                    ok: false,
                    source: None,
                    offset: 0,
                    full: true,
                }) {
                    Ok(reply) => reply,
                    Err(_) => return,
                };
                reply.push('\n');
                let mut writer = writer.lock().await;
                let _ = writer.write_all(reply.as_bytes()).await;
                let _ = writer.flush().await;
                return;
            }

            // The content is downloaded to a staging file rather than written
            // into the placeholder here: writing to a hole faults the page in,
            // which is a read, which re-enters the hook that asked for this
            // hydration. The helper writes it through the kernel's own
            // descriptor, which is exempt from that.
            let allow_partial = partial_hydration.load(std::sync::atomic::Ordering::Relaxed);
            let (ok, source, offset, full) = match app
                .stage_for(&path, offset, length, &reader, is_open, allow_partial)
                .await
            {
                // Fill this window from a staged file.
                Ok(Some((staged, offset, full))) => (
                    true,
                    Some(staged.to_string_lossy().into_owned()),
                    offset,
                    full,
                ),
                // Allow the read with no fill: a browsing sniff of an
                // untouched placeholder reads zeros, offline.
                Ok(None) => (true, None, 0, false),
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "could not stage content");
                    (false, None, 0, true)
                }
            };

            let mut reply = match serde_json::to_string(&Request::Hydrated {
                id,
                ok,
                source,
                offset,
                full,
            }) {
                Ok(reply) => reply,
                Err(_) => return,
            };
            reply.push('\n');
            let mut writer = writer.lock().await;
            let _ = writer.write_all(reply.as_bytes()).await;
            let _ = writer.flush().await;
        }
        Event::Filled { ok, path, full, .. } => {
            let path = PathBuf::from(path);
            // Remove this request's exact unique hand-off. Deriving the
            // ordinary download partial from `path` here once deleted an
            // unrelated scheduled download racing the same file.
            app.cleanup_hydration_staging(&path).await;
            if ok && full {
                watched.lock().await.remove(&path);
                if let Err(e) = app.finish_hydration(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "could not finalise");
                }
            } else if ok {
                // A range fill completes nothing — the file stays a placeholder
                // — but the pwrite moved its mtime to now. Put the record's time
                // back so browsing or a thumbnail never restamps a file.
                if let Err(e) = app.restore_mtime(&path).await {
                    tracing::debug!(path = %path.display(), error = %e, "could not restore mtime");
                }
            } else if let Err(e) = app.fail_hydration(&path).await {
                tracing::debug!(path = %path.display(), error = %e, "could not reset hydration");
            }
        }
        Event::Features {
            partial_hydration: partial,
        } => {
            partial_hydration.store(partial, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(partial_hydration = partial, "helper features");
        }
        Event::Pong { version } => tracing::info!(version, "files-on-demand helper connected"),
        Event::WatchResult {
            id,
            ok,
            message,
            dev,
            ino,
        } => {
            if let Some(reply) = pending_marks.lock().await.remove(&id) {
                let result = if ok {
                    Ok(dev.zip(ino).map(|(dev, ino)| FileIdentity { dev, ino }))
                } else {
                    Err(message.unwrap_or_else(|| "fanotify mark failed".into()))
                };
                let _ = reply.send(result);
            }
        }
        Event::Error { message } => tracing::warn!(message, "helper reported an error"),
        Event::Ok => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn helper_frames_are_bounded_before_json_parsing() {
        let oversized = vec![b'x'; MAX_FRAME_SIZE + 1];
        let mut reader = BufReader::new(oversized.as_slice());
        let error = read_frame(&mut reader).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
