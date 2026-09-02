//! The daemon's side of the files-on-demand conversation.
//!
//! The privileged helper knows only paths; everything that requires an account
//! — which item a path is, how to fetch it, where the bytes come from — happens
//! here, unprivileged. The link between the two is the item id stored in the
//! placeholder's own extended attributes, so the helper never has to be told
//! about the database.

mod registration;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_ipc::hsm::{Event, Request, WatchOutcome, MAX_FRAME_SIZE, SOCKET_PATH};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{watch, Mutex};

use crate::app::{App, HydrationCapabilities, HydrationRequest, StageOutcome};

use gonedrive_ipc::reader_policy::is_background_reader as background_reader;
use registration::{fail_pending_marks, HelperFeatures, MarkWaiters, WatchedFiles};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A live connection to the privileged helper.
pub struct HsmClient {
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    closed: watch::Receiver<bool>,
    next_request: AtomicU64,
    pending_marks: Arc<Mutex<MarkWaiters>>,
    watched: Arc<Mutex<WatchedFiles>>,
    watched_directories: Arc<Mutex<WatchedFiles>>,
    features: watch::Receiver<HelperFeatures>,
    registration: Mutex<()>,
}

#[derive(Clone)]
struct HandlerState {
    app: Arc<App>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    partial_hydration: Arc<std::sync::atomic::AtomicBool>,
    file_manager_copy_detection: Arc<std::sync::atomic::AtomicBool>,
    features_tx: watch::Sender<HelperFeatures>,
    ready_tx: watch::Sender<bool>,
    pending_marks: Arc<Mutex<MarkWaiters>>,
    watched_directories: Arc<Mutex<WatchedFiles>>,
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
        let file_manager_copy_detection = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (closed_tx, closed) = watch::channel(false);
        let (ready_tx, mut ready) = watch::channel(false);
        let (features_tx, features) = watch::channel(HelperFeatures::default());
        let pending_marks = Arc::new(Mutex::new(HashMap::new()));
        let watched = Arc::new(Mutex::new(HashMap::new()));
        let watched_directories = Arc::new(Mutex::new(HashMap::new()));
        let client = Self {
            writer: writer.clone(),
            closed,
            next_request: AtomicU64::new(1),
            pending_marks: pending_marks.clone(),
            watched: watched.clone(),
            watched_directories: watched_directories.clone(),
            features,
            registration: Mutex::new(()),
        };
        client.send(&Request::Ping).await?;
        client.send(&Request::Features).await?;

        let handler = HandlerState {
            app,
            writer,
            partial_hydration,
            file_manager_copy_detection,
            features_tx,
            ready_tx,
            pending_marks,
            watched_directories,
        };
        tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(frame)) if !frame.iter().all(u8::is_ascii_whitespace) => {
                        let handler = handler.clone();
                        // Each hydration runs on its own task: a process is
                        // blocked on it, and a large file must not hold up the
                        // next reader.
                        tokio::spawn(async move { handle(handler, frame).await });
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        tracing::warn!("the files-on-demand helper closed the connection");
                        fail_pending_marks(&handler.pending_marks, "helper closed the connection")
                            .await;
                        let _ = closed_tx.send(true);
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "helper connection failed");
                        fail_pending_marks(&handler.pending_marks, &e.to_string()).await;
                        let _ = closed_tx.send(true);
                        return;
                    }
                }
            }
        });

        // connect(2) succeeding only means that the root-owned socket accepted
        // a stream. The helper authenticates it and enforces one session per
        // uid afterwards. Do not publish the client until that server has
        // answered Ping, or a rejected second daemon can interpret BrokenPipe
        // mark requests as per-file failures and lock the whole tree.
        let mut handshake_closed = client.closed.clone();
        if let Err(error) =
            wait_for_handshake(&mut ready, &mut handshake_closed, HANDSHAKE_TIMEOUT).await
        {
            let _ = client.writer.lock().await.shutdown().await;
            return Err(error);
        }

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
        if line.len() > MAX_FRAME_SIZE {
            anyhow::bail!("HSM request exceeds 64 KiB");
        }
        line.push('\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

async fn wait_for_handshake(
    ready: &mut watch::Receiver<bool>,
    closed: &mut watch::Receiver<bool>,
    timeout: Duration,
) -> Result<()> {
    let handshake = async {
        loop {
            if *ready.borrow_and_update() {
                return Ok(());
            }
            if *closed.borrow_and_update() {
                anyhow::bail!("the files-on-demand helper rejected or closed the connection");
            }
            tokio::select! {
                changed = ready.changed() => {
                    if changed.is_err() {
                        anyhow::bail!("the files-on-demand helper dropped its handshake");
                    }
                }
                changed = closed.changed() => {
                    if changed.is_err() || *closed.borrow_and_update() {
                        anyhow::bail!("the files-on-demand helper rejected or closed the connection");
                    }
                }
            }
        }
    };
    tokio::time::timeout(timeout, handshake)
        .await
        .map_err(|_| {
            anyhow::anyhow!("the files-on-demand helper did not authenticate within {timeout:?}")
        })?
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

async fn handle(state: HandlerState, frame: Vec<u8>) {
    let HandlerState {
        app,
        writer,
        partial_hydration,
        file_manager_copy_detection,
        features_tx,
        ready_tx,
        pending_marks,
        watched_directories,
    } = state;
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
            // Range reads are extremely frequent (one large file can issue
            // tens of thousands), so retain this diagnostic only at debug.
            tracing::debug!(
                path = %path.display(),
                reader = %if reader.is_empty() { "unknown" } else { &reader },
                offset,
                length,
                "on-demand read"
            );

            // Policy lives here, unprivileged: a background sweep gets its
            // read refused before a byte is fetched or a window is shown.
            if let Some(matched) = background_reader(&reader) {
                tracing::debug!(reader = matched, "refusing a background reader");
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
            let copy_detection =
                file_manager_copy_detection.load(std::sync::atomic::Ordering::Relaxed);
            let (ok, source, offset, full) = match app
                .stage_for(HydrationRequest {
                    request_id: id,
                    path: &path,
                    offset,
                    length,
                    reader: &reader,
                    is_open,
                    capabilities: HydrationCapabilities {
                        partial: allow_partial,
                        file_manager_copy_detection: copy_detection,
                    },
                })
                .await
            {
                // Fill this window from a staged file.
                Ok(StageOutcome::Fill(staged, offset, full)) => (
                    true,
                    Some(staged.to_string_lossy().into_owned()),
                    offset,
                    full,
                ),
                // Allow the read with no fill: a browsing sniff of an
                // untouched placeholder reads zeros, offline.
                Ok(StageOutcome::Allow) => (true, None, 0, false),
                // A speculative Nautilus sniff. Denial is deliberate: allowing
                // the sparse read would make a copy race capable of receiving
                // zeroes. The helper labels proven copies separately.
                Ok(StageOutcome::Deny) => (false, None, 0, false),
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
        Event::Filled { id, ok, path, full } => {
            let path = PathBuf::from(path);
            // Remove this request's exact unique hand-off. Deriving the
            // ordinary download partial from `path` here once deleted an
            // unrelated scheduled download racing the same file.
            app.cleanup_hydration_staging(&path, id).await;
            if ok && full {
                if let Err(e) = app.finish_hydration(&path, id).await {
                    tracing::warn!(path = %path.display(), error = %e, "could not finalise");
                }
            } else if ok {
                // A range fill completes nothing — the file stays a placeholder
                // — but the pwrite moved its mtime to now. Put the record's time
                // back so browsing or a thumbnail never restamps a file.
                if let Err(e) = app.restore_mtime(&path, id).await {
                    tracing::debug!(path = %path.display(), error = %e, "could not restore mtime");
                }
            } else if !full {
                // A negotiated file-manager browse refusal did not start a
                // hydration. It is an expected offline result, not a failed
                // transfer and therefore has no state or UI event to unwind.
            } else if let Err(e) = app.fail_hydration(&path, id).await {
                tracing::debug!(path = %path.display(), error = %e, "could not reset hydration");
            }
        }
        Event::DirectoryOpened {
            id,
            path,
            reader,
            pid,
        } => {
            let path = PathBuf::from(path);
            let ok =
                populate_opened_directory(&app, &watched_directories, &path, &reader, pid).await;
            let mut reply = match serde_json::to_string(&Request::DirectoryPopulated { id, ok }) {
                Ok(reply) => reply,
                Err(_) => return,
            };
            reply.push('\n');
            let mut writer = writer.lock().await;
            let _ = writer.write_all(reply.as_bytes()).await;
            let _ = writer.flush().await;
        }
        Event::Features {
            partial_hydration: partial,
            batch_marks,
            mark_sweeps,
            file_manager_copy_detection: copy_detection,
            directory_open_notifications,
        } => {
            partial_hydration.store(partial, std::sync::atomic::Ordering::Relaxed);
            file_manager_copy_detection.store(copy_detection, std::sync::atomic::Ordering::Relaxed);
            let _ = features_tx.send(HelperFeatures {
                received: true,
                batch_marks,
                mark_sweeps,
                directory_open_notifications,
            });
            tracing::info!(
                partial_hydration = partial,
                batch_marks,
                mark_sweeps,
                file_manager_copy_detection = copy_detection,
                directory_open_notifications,
                "helper features"
            );
        }
        Event::Pong { version } => {
            let _ = ready_tx.send(true);
            tracing::info!(version, "files-on-demand helper connected");
        }
        Event::WatchResult {
            id,
            ok,
            message,
            dev,
            ino,
        } => {
            if let Some(reply) = pending_marks.lock().await.remove(&id) {
                let result = if ok {
                    Ok(vec![WatchOutcome {
                        ok: true,
                        message: None,
                        dev,
                        ino,
                    }])
                } else {
                    Err(message.unwrap_or_else(|| "fanotify mark failed".into()))
                };
                let _ = reply.send(result);
            }
        }
        Event::WatchBatchResult {
            id,
            ok,
            message,
            results,
        } => {
            if let Some(reply) = pending_marks.lock().await.remove(&id) {
                let result = if ok {
                    Ok(results)
                } else {
                    Err(message.unwrap_or_else(|| "fanotify batch mark failed".into()))
                };
                let _ = reply.send(result);
            }
        }
        Event::Error { message } => tracing::warn!(message, "helper reported an error"),
        Event::Ok => {}
    }
}

async fn populate_opened_directory(
    app: &Arc<App>,
    watched_directories: &Mutex<WatchedFiles>,
    path: &Path,
    reader: &str,
    pid: i32,
) -> bool {
    if background_reader(reader).is_some() {
        return true;
    }
    // The sync root has no parent population boundary. It is the local
    // namespace anchor and must remain browsable even while HSM marks are
    // being rebuilt after a daemon/helper restart.
    if path == app.config.sync_root {
        return true;
    }
    let watched_parent = match path.parent() {
        Some(parent) => watched_directories.lock().await.contains_key(parent),
        None => false,
    };
    if !watched_parent {
        tracing::warn!(path = %path.display(), pid, reader, "helper reported an untracked directory open");
        return false;
    }
    if gonedrive_core::hydrate::Placeholder::directory_population(path)
        != gonedrive_core::hydrate::DirectoryPopulation::Partial
    {
        return true;
    }
    if gonedrive_ipc::reader_policy::is_file_manager_reader(reader) {
        match app.desktop_open_decision(path).await {
            crate::open_intent::OpenDecision::Explicit => {}
            crate::open_intent::OpenDecision::Prefetch => {
                tracing::debug!(path = %path.display(), pid, reader, "file-manager directory prefetch stayed offline");
                return true;
            }
            crate::open_intent::OpenDecision::Unavailable => {
                tracing::debug!(path = %path.display(), "desktop intent monitor unavailable; using compatibility population");
            }
        }
    }
    let Ok(relative) = path.strip_prefix(&app.config.sync_root) else {
        tracing::warn!(path = %path.display(), "helper reported a directory outside the sync root");
        return false;
    };
    tracing::debug!(path = %path.display(), pid, reader, "foreground directory population");
    match app.populate_folder(relative).await {
        Ok(outcome) => {
            tracing::debug!(
                path = %path.display(),
                files = outcome.files,
                skipped = outcome.skipped.len(),
                "directory populated"
            );
            true
        }
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not populate opened directory");
            false
        }
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

    #[tokio::test]
    async fn helper_is_not_ready_until_an_authenticated_pong_arrives() {
        let (ready_tx, mut ready) = watch::channel(false);
        let (_closed_tx, mut closed) = watch::channel(false);
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = ready_tx.send(true);
        });
        wait_for_handshake(&mut ready, &mut closed, Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_rejected_helper_session_never_becomes_ready() {
        let (_ready_tx, mut ready) = watch::channel(false);
        let (closed_tx, mut closed) = watch::channel(false);
        closed_tx.send(true).unwrap();
        let error = wait_for_handshake(&mut ready, &mut closed, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("rejected or closed"));
    }
}
