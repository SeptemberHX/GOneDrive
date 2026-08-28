//! The daemon's side of the files-on-demand conversation.
//!
//! The privileged helper knows only paths; everything that requires an account
//! — which item a path is, how to fetch it, where the bytes come from — happens
//! here, unprivileged. The link between the two is the item id stored in the
//! placeholder's own extended attributes, so the helper never has to be told
//! about the database.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use gonedrive_ipc::hsm::{Event, Request, SOCKET_PATH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::app::App;

/// Readers whose blocked reads must not pull content down.
///
/// The helper carries the same list, but this copy is the one that matters
/// operationally: it lives in the unprivileged daemon, so extending it when a
/// new background scanner shows up in the logs is a rebuild and a restart —
/// no root, no helper swap. Matched as a prefix of `comm` (kernel-truncated
/// to 15 characters).
const BACKGROUND_READERS: &[&str] = &[
    "localsearch",
    "tracker-miner",
    "tracker-extract",
    "tracker-store",
    "baloo_file",
    "thumbnailer",
    "tumblerd",
    "gsd-housekeep",
    "updatedb",
    "plocate",
    "mlocate",
    "clamd",
    "freshclam",
];

fn background_reader(comm: &str) -> Option<&'static str> {
    BACKGROUND_READERS
        .iter()
        .find(|prefix| comm.starts_with(*prefix))
        .copied()
}

/// A live connection to the privileged helper.
pub struct HsmClient {
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    closed: Arc<tokio::sync::Notify>,
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
        let closed = Arc::new(tokio::sync::Notify::new());
        let client = Self {
            writer: writer.clone(),
            closed: closed.clone(),
        };
        client.send(&Request::Ping).await?;
        client.send(&Request::Features).await?;

        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        let app = app.clone();
                        let writer = writer.clone();
                        let partial_hydration = partial_hydration.clone();
                        // Each hydration runs on its own task: a process is
                        // blocked on it, and a large file must not hold up the
                        // next reader.
                        tokio::spawn(async move {
                            handle(app, writer, partial_hydration, line).await
                        });
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        tracing::warn!("the files-on-demand helper closed the connection");
                        closed.notify_waiters();
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "helper connection failed");
                        closed.notify_waiters();
                        return;
                    }
                }
            }
        });

        Ok(client)
    }

    /// Resolves when the helper connection has died.
    pub async fn closed(&self) {
        self.closed.notified().await;
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
        self.send(&Request::Watch {
            path: path.to_string_lossy().into_owned(),
        })
        .await
    }

    /// Stop intercepting: the file is hydrated, deleted, or no longer ours.
    pub async fn unwatch(&self, path: &Path) -> Result<()> {
        self.send(&Request::Unwatch {
            path: path.to_string_lossy().into_owned(),
        })
        .await
    }

    /// Register every placeholder currently on disk.
    pub async fn watch_all(&self, app: &App) -> Result<usize> {
        let paths = app.dehydrated_paths().await?;
        for path in &paths {
            if let Err(e) = self.watch(path).await {
                tracing::warn!(path = %path.display(), error = %e, "could not watch");
            }
        }
        Ok(paths.len())
    }
}

async fn handle(
    app: Arc<App>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    partial_hydration: Arc<std::sync::atomic::AtomicBool>,
    line: String,
) {
    let event: Event = match serde_json::from_str(&line) {
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
                let mut reply =
                    match serde_json::to_string(&Request::Hydrated {
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
            let allow_partial =
                partial_hydration.load(std::sync::atomic::Ordering::Relaxed);
            let (ok, source, offset, full) =
                match app.stage_for(&path, offset, length, allow_partial).await {
                Ok((staged, offset, full)) => (
                    true,
                    Some(staged.to_string_lossy().into_owned()),
                    offset,
                    full,
                ),
                Err(e) => {
                    // Answering false makes the blocked reader see an ordinary
                    // I/O error. Not answering at all would block it forever.
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
            // A range fill completes nothing: the file stays a placeholder
            // and must keep reading as one.
            if ok && full {
                if let Err(e) = app.finish_hydration(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "could not finalise");
                }
            }
        }
        Event::Features { partial_hydration: partial } => {
            partial_hydration.store(partial, std::sync::atomic::Ordering::Relaxed);
            tracing::info!(partial_hydration = partial, "helper features");
        }
        Event::Pong { version } => tracing::info!(version, "files-on-demand helper connected"),
        Event::Error { message } => tracing::warn!(message, "helper reported an error"),
        Event::Ok => {}
    }
}
