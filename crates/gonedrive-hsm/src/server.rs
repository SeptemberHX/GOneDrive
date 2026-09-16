//! Authenticated Unix-socket protocol for the privileged helper.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use gonedrive_ipc::hsm::{Event, Request, WatchOutcome, MAX_FRAME_SIZE, MAX_WATCH_BATCH};

use crate::marks::{
    begin_mark_sweep, cancel_mark_sweep, commit_mark_sweep, install_directory_mark, install_mark,
    open_authorised, register_roots, remove_marks_for_path,
};
use crate::shown;
use crate::worker::fail_pending_for_uid;
use crate::{ClientSession, FileIdentity, Reply, Shared};

pub(super) const MARK_ERROR_BYTES: usize = 128;
/// Cold registration is path-walk heavy. Four workers keep it responsive on a
/// large tree without turning a background service into an all-core workload.
const WATCH_BATCH_WORKERS: usize = 4;

pub(super) fn send_to_session(session: &ClientSession, event: &Event) -> Result<()> {
    send_to_stream(&session.stream, event)
}

/// How long a reply may take to reach the daemon.
///
/// Replies are one short line and the daemon reads continuously, so this only
/// ever expires on a daemon that has stopped reading — hung, stopped, or gone
/// without closing. Without it `write_all` blocks for ever, and because that
/// happens *before* the hydration timeout starts, a handful of such peers
/// permanently exhausted the worker pool.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn send_to_stream(stream: &Arc<Mutex<UnixStream>>, event: &Event) -> Result<()> {
    let mut line = serde_json::to_string(event)?;
    if line.len() > MAX_FRAME_SIZE {
        bail!("HSM event exceeds 64 KiB");
    }
    line.push('\n');
    let mut stream = stream.lock().unwrap();
    if let Err(error) = stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
    {
        // A timeout can leave half a line on the wire. The peer would read a
        // truncated frame and lose sync with every reply after it, so end the
        // connection and let it reconnect; pending events are denied by the
        // session guard, which fails closed.
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return Err(error.into());
    }
    Ok(())
}

/// Create the socket with a root-owned parent directory.
pub(super) fn bind_socket(path: &Path) -> Result<UnixListener> {
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
                shown(parent)
            );
        }
        // The directory is the access control: only root may replace the
        // socket; every connecting process is then checked with SO_PEERCRED.
        let mut perms = std::fs::metadata(parent)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(parent, perms)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                bail!("refusing to replace non-socket {}", shown(path));
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

/// Removes exactly the session installed by one connection on every exit path.
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
            cancel_mark_sweep(&self.shared, self.uid);
            // Keep inode marks armed while the daemon restarts. With no
            // matching session, permission events are denied rather than
            // exposing sparse placeholder zeroes.
        }
    }
}

pub(super) fn serve_daemon(shared: &Arc<Shared>, stream: UnixStream) -> Result<()> {
    let (uid, pid) = peer_creds(&stream)?;
    let writer = stream.try_clone()?;
    // The option is on the socket, so this covers every reply written through
    // this handle. See SEND_TIMEOUT.
    writer.set_write_timeout(Some(SEND_TIMEOUT))?;
    let writer = Arc::new(Mutex::new(writer));
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
            Err(error) => {
                let _ = shared.send(
                    uid,
                    &Event::Error {
                        message: format!("malformed request: {error}"),
                    },
                );
                continue;
            }
        };
        handle_request(shared, request, uid);
    }
    Ok(())
}

/// Read one newline-delimited frame without allocating beyond the limit.
pub(super) fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
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
                    batch_marks: true,
                    mark_sweeps: true,
                    file_manager_copy_detection: true,
                    directory_open_notifications: true,
                    sync_roots: true,
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
                    message: result.err().map(|error| format!("{error:#}")),
                    dev: identity.map(|identity| identity.dev),
                    ino: identity.map(|identity| identity.ino),
                },
            );
        }
        Request::WatchBatch { id, paths } => {
            if paths.len() > MAX_WATCH_BATCH {
                let _ = shared.send(
                    uid,
                    &Event::WatchBatchResult {
                        id,
                        ok: false,
                        message: Some(format!(
                            "watch batch has {} paths; maximum is {MAX_WATCH_BATCH}",
                            paths.len()
                        )),
                        results: Vec::new(),
                    },
                );
                return;
            }
            let results = parallel_map_ordered(&paths, WATCH_BATCH_WORKERS, |path| {
                mark_outcome(install_mark(shared, Path::new(path), uid))
            });
            let _ = shared.send(
                uid,
                &Event::WatchBatchResult {
                    id,
                    ok: true,
                    message: None,
                    results,
                },
            );
        }
        Request::WatchDirectory { id, path } => {
            let path = PathBuf::from(path);
            let result = install_directory_mark(shared, &path, uid);
            let identity = result.as_ref().ok().copied();
            let _ = shared.send(
                uid,
                &Event::WatchResult {
                    id,
                    ok: result.is_ok(),
                    message: result.err().map(|error| format!("{error:#}")),
                    dev: identity.map(|identity| identity.dev),
                    ino: identity.map(|identity| identity.ino),
                },
            );
        }
        Request::WatchDirectoryBatch { id, paths } => {
            if paths.len() > MAX_WATCH_BATCH {
                let _ = shared.send(
                    uid,
                    &Event::WatchBatchResult {
                        id,
                        ok: false,
                        message: Some(format!(
                            "directory watch batch has {} paths; maximum is {MAX_WATCH_BATCH}",
                            paths.len()
                        )),
                        results: Vec::new(),
                    },
                );
                return;
            }
            let results = parallel_map_ordered(&paths, WATCH_BATCH_WORKERS, |path| {
                mark_outcome(install_directory_mark(shared, Path::new(path), uid))
            });
            let _ = shared.send(
                uid,
                &Event::WatchBatchResult {
                    id,
                    ok: true,
                    message: None,
                    results,
                },
            );
        }
        Request::BeginWatchSet { id } => {
            begin_mark_sweep(shared, uid);
            send_control_result(shared, uid, id, Ok(()));
        }
        Request::CommitWatchSet { id } => {
            let result = commit_mark_sweep(shared, uid).map(|removed| {
                tracing::info!(uid, removed, "retired stale placeholder marks");
            });
            send_control_result(shared, uid, id, result);
        }
        Request::AbortWatchSet { id } => {
            cancel_mark_sweep(shared, uid);
            send_control_result(shared, uid, id, Ok(()));
        }
        Request::RegisterRoots { id, paths } => {
            let result = register_roots(shared, &paths, uid).map(|count| {
                tracing::info!(uid, count, "registered sync roots");
            });
            send_control_result(shared, uid, id, result);
        }
        Request::Unwatch { id, path } => {
            let path = PathBuf::from(path);
            let result = remove_marks_for_path(shared, &path, uid);
            let _ = shared.send(
                uid,
                &Event::WatchResult {
                    id,
                    ok: result.is_ok(),
                    message: result.err().map(|error| format!("{error:#}")),
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
                    let roots = shared.roots_for(uid);
                    match open_authorised(&roots, &source, uid, libc::O_RDONLY) {
                        Ok(source) => Reply::Fill(source.file, offset, full),
                        Err(error) => {
                            tracing::warn!(%error, "refusing staged content");
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
        Request::DirectoryPopulated { id, ok } => {
            let belongs_to_client = shared
                .pending
                .lock()
                .unwrap()
                .get(&id)
                .is_some_and(|pending| pending.uid == uid);
            if !belongs_to_client {
                tracing::warn!(uid, id, "ignoring a directory reply for another session");
                return;
            }
            if let Some(pending) = shared.pending.lock().unwrap().remove(&id) {
                let _ = pending.reply.send(Reply::DirectoryPopulated(ok));
            }
        }
    }
}

/// Apply one batch concurrently while retaining wire-order results. Ordering
/// matters because the daemon zips each outcome back to its requested path.
pub(super) fn parallel_map_ordered<T, R>(
    items: &[T],
    max_workers: usize,
    operation: impl Fn(&T) -> R + Sync,
) -> Vec<R>
where
    T: Sync,
    R: Send,
{
    if items.is_empty() {
        return Vec::new();
    }
    let workers = items.len().min(max_workers.max(1));
    if workers == 1 {
        return items.iter().map(operation).collect();
    }
    let chunk_size = items.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let operation = &operation;
        let handles: Vec<_> = items
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || chunk.iter().map(operation).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("watch registration worker panicked"))
            .collect()
    })
}

fn mark_outcome(result: Result<FileIdentity>) -> WatchOutcome {
    match result {
        Ok(identity) => WatchOutcome {
            ok: true,
            message: None,
            dev: Some(identity.dev),
            ino: Some(identity.ino),
        },
        Err(error) => WatchOutcome {
            ok: false,
            message: Some(truncate_mark_error(format!("{error:#}"))),
            dev: None,
            ino: None,
        },
    }
}

fn send_control_result(shared: &Arc<Shared>, uid: u32, id: u64, result: Result<()>) {
    let _ = shared.send(
        uid,
        &Event::WatchResult {
            id,
            ok: result.is_ok(),
            message: result.err().map(|error| format!("{error:#}")),
            dev: None,
            ino: None,
        },
    );
}

pub(super) fn truncate_mark_error(mut message: String) -> String {
    if message.len() <= MARK_ERROR_BYTES {
        return message;
    }
    let mut boundary = MARK_ERROR_BYTES;
    while !message.is_char_boundary(boundary) {
        boundary -= 1;
    }
    message.truncate(boundary);
    message.push_str("...");
    message
}
