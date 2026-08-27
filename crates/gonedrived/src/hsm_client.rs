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

/// A live connection to the privileged helper.
pub struct HsmClient {
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
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

        let client = Self {
            writer: writer.clone(),
        };
        client.send(&Request::Ping).await?;

        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        let app = app.clone();
                        let writer = writer.clone();
                        // Each hydration runs on its own task: a process is
                        // blocked on it, and a large file must not hold up the
                        // next reader.
                        tokio::spawn(async move { handle(app, writer, line).await });
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        tracing::warn!("the files-on-demand helper closed the connection");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "helper connection failed");
                        return;
                    }
                }
            }
        });

        Ok(client)
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
            // or thumbnailer here is a bug to add to the helper's refusal list.
            tracing::info!(
                path = %path.display(),
                reader = %if reader.is_empty() { "unknown".into() } else { reader },
                offset,
                length,
                "on-demand read"
            );

            // The content is downloaded to a staging file rather than written
            // into the placeholder here: writing to a hole faults the page in,
            // which is a read, which re-enters the hook that asked for this
            // hydration. The helper writes it through the kernel's own
            // descriptor, which is exempt from that.
            let (ok, source) = match app.stage_content(&path).await {
                Ok(staged) => (true, Some(staged.to_string_lossy().into_owned())),
                Err(e) => {
                    // Answering false makes the blocked reader see an ordinary
                    // I/O error. Not answering at all would block it forever.
                    tracing::error!(path = %path.display(), error = %e, "could not stage content");
                    (false, None)
                }
            };

            let mut reply = match serde_json::to_string(&Request::Hydrated { id, ok, source }) {
                Ok(reply) => reply,
                Err(_) => return,
            };
            reply.push('\n');
            let mut writer = writer.lock().await;
            let _ = writer.write_all(reply.as_bytes()).await;
            let _ = writer.flush().await;
        }
        Event::Filled { ok, path, .. } => {
            let path = PathBuf::from(path);
            if ok {
                if let Err(e) = app.finish_hydration(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "could not finalise");
                }
            }
        }
        Event::Pong { version } => tracing::info!(version, "files-on-demand helper connected"),
        Event::Error { message } => tracing::warn!(message, "helper reported an error"),
        Event::Ok => {}
    }
}
