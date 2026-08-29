//! GOnedrive's privileged files-on-demand helper.
//!
//! This is the only part of GOnedrive that runs as root, so it is kept as small
//! and as ignorant as possible. It holds no credentials, opens no sockets to
//! the internet, and parses nothing more complicated than a path and a byte
//! range. When a process reads a placeholder, the kernel blocks that read and
//! hands the event here; this program asks the unprivileged daemon to fill the
//! file and then unblocks the reader.
//!
//! Two rules shape the design:
//!
//! * **The event loop never does I/O.** Filling a file from inside the handler
//!   deadlocks: the write can itself block on the event we have not answered.
//!   Events are dispatched to workers and the loop goes straight back to
//!   reading.
//! * **Every request is answered.** An unanswered pre-content event leaves the
//!   reading process blocked forever, so failures respond with a denial, which
//!   the reader sees as an ordinary I/O error.

mod fanotify;
mod marks;
mod worker;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use gonedrive_ipc::hsm::{Event, Request, MAX_FRAME_SIZE, SOCKET_PATH};

use crate::fanotify::Fanotify;
use crate::marks::{install_mark, open_authorised, remove_marks_for_path};
#[cfg(test)]
use crate::worker::{background_reader, expected_fill_len, fill_from};
use crate::worker::{event_loop, fail_pending_for_uid, handle_event, remove_marks_for_uid};

/// How long a blocked reader waits before we give up and fail its read.
/// Long enough for a large file over a slow link, short enough that a wedged
/// daemon does not hang an application indefinitely.
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Pre-content events block real readers, but an attacker must not be able to
/// turn each one into an unbounded root-owned thread.
const WORKERS: usize = 64;
const WORK_QUEUE: usize = 256;

#[derive(Parser)]
#[command(
    name = "gonedrive-hsm",
    version,
    about = "GOnedrive files-on-demand helper"
)]
struct Cli {
    /// Unix socket to listen on.
    #[arg(long, default_value = SOCKET_PATH)]
    socket: PathBuf,

    /// Only serve this uid. Without it, each uid gets one isolated session.
    #[arg(long)]
    uid: Option<u32>,
}

/// A hydration the helper is waiting on.
enum Reply {
    /// Fill this window from the staged file, then place/finalise it.
    Fill(std::fs::File, u64, bool),
    /// Let the read proceed against the sparse file, unchanged. A browsing
    /// sniff of an untouched placeholder — no fetch happened.
    AllowEmpty,
    /// The content could not be provided; fail the read.
    Deny,
}

struct Pending {
    uid: u32,
    reply: mpsc::Sender<Reply>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

struct MarkEntry {
    uid: u32,
    path: PathBuf,
    /// Kept open so even a replaced or deleted pathname can be unmarked by
    /// descriptor, without reopening attacker-controlled path components.
    file: std::fs::File,
}

#[derive(Clone)]
struct ClientSession {
    pid: i32,
    stream: Arc<Mutex<UnixStream>>,
}

struct Shared {
    fan: Fanotify,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    sessions: Mutex<HashMap<u32, ClientSession>>,
    marks: Mutex<HashMap<FileIdentity, MarkEntry>>,
    allowed_uid: Option<u32>,
}

impl Shared {
    fn send(&self, uid: u32, event: &Event) -> Result<()> {
        let session = {
            let sessions = self.sessions.lock().unwrap();
            let Some(session) = sessions.get(&uid) else {
                bail!("no daemon is connected for uid {uid}");
            };
            session.clone()
        };
        send_to_session(&session, event)
    }

    fn begin_hydration(
        &self,
        uid: u32,
        session: &ClientSession,
        id: u64,
        pending: Pending,
        event: &Event,
    ) -> Result<()> {
        let stream = {
            // Hold the session table through pending insertion. Disconnect
            // cleanup takes the same lock before draining that uid, so a
            // request can belong wholly to the old or new connection, never
            // straddle both.
            let sessions = self.sessions.lock().unwrap();
            let Some(current) = sessions.get(&uid) else {
                bail!("no daemon is connected for uid {uid}");
            };
            if current.pid != session.pid || !Arc::ptr_eq(&current.stream, &session.stream) {
                bail!("daemon session for uid {uid} changed");
            }
            self.pending.lock().unwrap().insert(id, pending);
            current.stream.clone()
        };
        if let Err(error) = send_to_stream(&stream, event) {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }
        Ok(())
    }
}

fn send_to_session(session: &ClientSession, event: &Event) -> Result<()> {
    send_to_stream(&session.stream, event)
}

fn send_to_stream(stream: &Arc<Mutex<UnixStream>>, event: &Event) -> Result<()> {
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    let mut stream = stream.lock().unwrap();
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gonedrive_hsm=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let fan = Fanotify::open().context(
        "could not open a pre-content fanotify group.\n\
         This needs CAP_SYS_ADMIN (run as root) and Linux 6.14 or newer.",
    )?;

    let shared = Arc::new(Shared {
        fan,
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        marks: Mutex::new(HashMap::new()),
        allowed_uid: cli.uid,
    });

    let listener = bind_socket(&cli.socket)?;
    tracing::info!(socket = %cli.socket.display(), "listening");

    let (work_tx, work_rx) = mpsc::sync_channel(WORK_QUEUE);
    let work_rx = Arc::new(Mutex::new(work_rx));
    for index in 0..WORKERS {
        let shared = shared.clone();
        let work_rx = work_rx.clone();
        std::thread::Builder::new()
            .name(format!("hydrate-{index}"))
            .spawn(move || loop {
                let event = match work_rx.lock().unwrap().recv() {
                    Ok(event) => event,
                    Err(_) => return,
                };
                handle_event(shared.clone(), event);
            })?;
    }

    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("fanotify".into())
            .spawn(move || event_loop(shared, work_tx))?;
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_daemon(&shared, stream) {
                        tracing::warn!(error = %e, "daemon connection ended");
                    }
                });
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
    Ok(())
}

/// Create the socket with a root-owned parent directory.
fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid has no arguments and cannot fail.
        let effective_uid = unsafe { libc::geteuid() };
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != effective_uid
        {
            bail!(
                "socket directory {} must be a real directory owned by the helper uid",
                parent.display()
            );
        }
        // The directory is the access control: only root may replace the
        // socket, while any local user may attempt to connect and is then
        // checked with SO_PEERCRED.
        let mut perms = std::fs::metadata(parent)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(parent, perms)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                bail!("refusing to replace non-socket {}", path.display());
            }
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Read the connecting process's credentials.
fn peer_creds(stream: &UnixStream) -> Result<(u32, i32)> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((cred.uid, cred.pid))
}

/// Removes exactly the session installed by one connection, on every exit
/// path, including malformed/oversized frames and socket read errors.
struct SessionGuard {
    shared: Arc<Shared>,
    uid: u32,
    pid: i32,
    stream: Arc<Mutex<UnixStream>>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let disconnected = {
            let mut sessions = self
                .shared
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if sessions.get(&self.uid).is_some_and(|session| {
                session.pid == self.pid && Arc::ptr_eq(&session.stream, &self.stream)
            }) {
                sessions.remove(&self.uid);
                true
            } else {
                false
            }
        };
        if disconnected {
            fail_pending_for_uid(&self.shared, self.uid);
            remove_marks_for_uid(&self.shared, self.uid);
        }
    }
}

fn serve_daemon(shared: &Arc<Shared>, stream: UnixStream) -> Result<()> {
    let (uid, pid) = peer_creds(&stream)?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    if shared.allowed_uid.is_some_and(|expected| expected != uid) {
        bail!("refusing a connection from uid {uid}");
    }
    {
        let mut sessions = shared.sessions.lock().unwrap();
        if sessions.contains_key(&uid) {
            bail!("a daemon for uid {uid} is already connected");
        }
        sessions.insert(
            uid,
            ClientSession {
                pid,
                stream: writer.clone(),
            },
        );
    }
    tracing::info!(uid, pid, "serving daemon");
    let _session = SessionGuard {
        shared: shared.clone(),
        uid,
        pid,
        stream: writer,
    };

    let mut reader = BufReader::new(stream);
    while let Some(line) = read_frame(&mut reader)? {
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let request: Request = match serde_json::from_slice(&line) {
            Ok(request) => request,
            Err(e) => {
                let _ = shared.send(
                    uid,
                    &Event::Error {
                        message: format!("malformed request: {e}"),
                    },
                );
                continue;
            }
        };
        handle_request(shared, request, uid);
    }
    Ok(())
}

/// Read one newline-delimited JSON frame without ever allocating beyond the
/// protocol limit. An oversized frame closes only that uid's session.
fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if frame.len() + newline > MAX_FRAME_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "HSM request exceeds 64 KiB",
                ));
            }
            frame.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return Ok(Some(frame));
        }
        if frame.len() + available.len() > MAX_FRAME_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "HSM request exceeds 64 KiB",
            ));
        }
        let len = available.len();
        frame.extend_from_slice(available);
        reader.consume(len);
    }
}

fn handle_request(shared: &Arc<Shared>, request: Request, uid: u32) {
    match request {
        Request::Ping => {
            let _ = shared.send(
                uid,
                &Event::Pong {
                    version: env!("CARGO_PKG_VERSION").into(),
                },
            );
        }
        Request::Features => {
            let _ = shared.send(
                uid,
                &Event::Features {
                    partial_hydration: true,
                },
            );
        }
        Request::Watch { id, path } => {
            let path = PathBuf::from(path);
            let result = install_mark(shared, &path, uid);
            let identity = result.as_ref().ok().copied();
            let _ = shared.send(
                uid,
                &Event::WatchResult {
                    id,
                    ok: result.is_ok(),
                    message: result.err().map(|error| error.to_string()),
                    dev: identity.map(|identity| identity.dev),
                    ino: identity.map(|identity| identity.ino),
                },
            );
        }
        Request::Unwatch { id, path } => {
            let path = PathBuf::from(path);
            let result = remove_marks_for_path(shared, &path, uid);
            let _ = shared.send(
                uid,
                &Event::WatchResult {
                    id,
                    ok: result.is_ok(),
                    message: result.err().map(|error| error.to_string()),
                    dev: None,
                    ino: None,
                },
            );
        }
        Request::Hydrated {
            id,
            ok,
            source,
            offset,
            full,
        } => {
            let belongs_to_client = shared
                .pending
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|pending| pending.uid == uid);
            if !belongs_to_client {
                tracing::warn!(uid, id, "ignoring a hydration reply for another session");
                return;
            }
            let reply = match (ok, source) {
                (true, Some(source)) => {
                    let source = PathBuf::from(source);
                    // Open once, without following the final symlink. The same
                    // descriptor is validated, sized and copied by the worker.
                    match open_authorised(&source, uid, libc::O_RDONLY) {
                        Ok(source) => Reply::Fill(source.file, offset, full),
                        Err(e) => {
                            tracing::warn!(error = %e, "refusing staged content");
                            Reply::Deny
                        }
                    }
                }
                (true, None) => Reply::AllowEmpty,
                (false, _) => Reply::Deny,
            };
            if let Some(pending) = shared.pending.lock().unwrap().remove(&id) {
                let _ = pending.reply.send(reply);
            }
        }
    }
}

#[cfg(test)]
mod tests;
