//! Non-blocking fanotify dispatch and bounded placeholder hydration workers.

use std::ffi::CString;
use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};

use anyhow::{bail, Context, Result};
use gonedrive_ipc::hsm::Event;
use gonedrive_ipc::reader_policy::{
    is_background_reader, is_bulk_copier, is_file_manager_reader, FILE_MANAGER_COPY_READER,
};

use crate::fanotify::PreContentEvent;
use crate::marks::remove_mark_by_event_fd;
use crate::server::send_to_session;
use crate::{ClientSession, FileIdentity, MarkKind, Pending, Reply, Shared, HYDRATE_TIMEOUT};

/// Poll for events and hand each one to a worker.
///
/// The loop itself must never block on anything but the poll: doing the fill
/// here would deadlock, because the write can block on the very event that has
/// not been answered yet.
pub(super) fn event_loop(
    shared: Arc<Shared>,
    workers: mpsc::SyncSender<PreContentEvent>,
) -> std::io::Result<()> {
    let mut poll = libc::pollfd {
        fd: shared.fan.as_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: a single valid pollfd, with a timeout.
        let rc = unsafe { libc::poll(&mut poll, 1, 1000) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc == 0 {
            continue;
        }
        match shared.fan.read_events() {
            Ok(events) => {
                for event in events {
                    match workers.try_send(event) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(event)) => {
                            tracing::warn!(pid = event.pid, "fanotify worker queue is full");
                            if event.requires_response {
                                let _ = shared.fan.respond(event.fd.as_raw_fd(), false);
                            }
                        }
                        Err(mpsc::TrySendError::Disconnected(event)) => {
                            if event.requires_response {
                                let _ = shared.fan.respond(event.fd.as_raw_fd(), false);
                            }
                            return Ok(());
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            // A permanent read error leaves POLLIN asserted and, more
            // importantly, means no blocked reader can be answered. Returning
            // terminates the helper so systemd can create a fresh fanotify
            // group; logging once is better than an immortal one-line-per-
            // second error loop and a registration deadlock.
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn handle_event(shared: Arc<Shared>, event: PreContentEvent) {
    let fd = event.fd.as_raw_fd();

    // A fanotify mark can outlive a path or a client connection. Trust an event
    // only when the descriptor the kernel supplied is the exact inode recorded
    // for a successful, uid-bound Watch request.
    let stat = match descriptor_stat(fd) {
        Ok(stat) => stat,
        Err(error) => {
            tracing::warn!(%error, "cannot identify fanotify event target");
            if event.requires_response {
                let _ = shared.fan.respond(fd, false);
            }
            return;
        }
    };
    let identity = FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    };

    // Registration itself must be able to inspect an inode after the kernel
    // mark exists but before the userspace registry entry is committed. This
    // window is small, but a cold helper restart crosses it once per existing
    // placeholder. Requiring a registry hit first makes the helper deny its
    // own/daemon reopen and prevents the sweep from ever completing.
    let daemon_is_reader = shared
        .sessions
        .lock()
        .unwrap()
        .get(&(stat.st_uid as u32))
        .is_some_and(|client| event.pid == client.pid);
    if event.requires_response && (event.pid == std::process::id() as i32 || daemon_is_reader) {
        if let Err(error) = shared.fan.respond(fd, true) {
            tracing::error!(%error, "could not allow the registration process");
        }
        return;
    }

    let (uid, kind, registered_path, child_notification) = {
        let marks = shared.marks.lock().unwrap();
        match marks.get(&identity) {
            Some(entry) if entry.uid == stat.st_uid => {
                (entry.uid, entry.kind, entry.path.clone(), false)
            }
            None if event.requires_response && event.is_open => {
                let Some(path) = event.path.as_deref() else {
                    let _ = shared.fan.respond(fd, false);
                    return;
                };
                let Some(parent) = marks.population_parent(stat.st_uid, path) else {
                    tracing::debug!(path = %path.display(), "denying an unregistered open target");
                    let _ = shared.fan.respond(fd, false);
                    return;
                };
                match stat.st_mode & libc::S_IFMT {
                    libc::S_IFDIR => (stat.st_uid, MarkKind::PopulationDirectory, parent, true),
                    libc::S_IFREG => {
                        // FAN_ONDIR adds directory events; it does not exclude
                        // ordinary files. A populated-parent mark therefore
                        // sees every direct child file too. Hydrated and local
                        // files need no callback, while a dehydrated file whose
                        // exact mark is not installed must remain fail-closed.
                        let allow = match unregistered_population_file_is_safe(fd) {
                            Ok(allow) => allow,
                            Err(error) => {
                                tracing::debug!(path = %path.display(), %error, "could not classify an unregistered population child");
                                false
                            }
                        };
                        let _ = shared.fan.respond(fd, allow);
                        return;
                    }
                    _ => {
                        tracing::debug!(path = %path.display(), "denying a non-file population child");
                        let _ = shared.fan.respond(fd, false);
                        return;
                    }
                }
            }
            _ => {
                tracing::debug!(
                    dev = identity.dev,
                    ino = identity.ino,
                    "denying an unregistered event target"
                );
                if event.requires_response {
                    let _ = shared.fan.respond(fd, false);
                }
                return;
            }
        }
    };
    let client = shared.sessions.lock().unwrap().get(&uid).cloned();
    if kind == MarkKind::PopulationDirectory && !child_notification {
        // FAN_EVENT_ON_CHILD marks can also report the marked directory
        // itself. It is already populated, so it never needs a callback and
        // must remain usable while the daemon is restarting.
        if event.requires_response {
            let _ = shared.fan.respond(fd, true);
        }
        return;
    }
    let Some(client) = client else {
        if event.requires_response {
            let _ = shared.fan.respond(fd, false);
        }
        return;
    };

    if kind == MarkKind::PopulationDirectory {
        handle_directory_open(&shared, &client, uid, &event, &registered_path);
        return;
    }

    // Placeholder marks consist exclusively of permission events. If the
    // kernel ever reports something else for one, dropping the notification
    // is safer than writing an invalid response into the fanotify group.
    if !event.requires_response {
        tracing::warn!(path = ?event.path, mask = "notification", "unexpected non-permission placeholder event");
        return;
    }

    // Allocate one id for the entire permission event, including the
    // already-materialised fast path below.  A path-only (id=0) completion can
    // otherwise be mistaken for a newer request when a release and a read
    // overlap on the same inode.
    let request_id = shared.next_id.fetch_add(1, Ordering::Relaxed);

    // Background sweeps are refused before any work is done, so they neither
    // download anything nor put a progress window on the user's screen for a
    // file they never asked for.
    if let Some(reader) = background_reader(event.pid) {
        tracing::debug!(
            pid = event.pid,
            reader,
            path = ?event.path,
            "refusing to hydrate for a background reader"
        );
        if let Err(e) = shared.fan.respond(fd, false) {
            tracing::error!(error = %e, "could not answer the blocked reader");
        }
        return;
    }

    // Partial hydration leaves the inode watched. If this exact range was
    // filled by an earlier read, the real bytes are already under the blocked
    // reader: allow it directly. Round-tripping through the daemon would copy
    // and fsync the same small range again for every Nautilus sniff.
    if !event.is_open {
        match requested_range_is_materialized(
            fd,
            event.offset,
            event.length,
            stat.st_size.max(0) as u64,
        ) {
            Ok(true) => {
                let file_size = stat.st_size.max(0) as u64;
                let fully_materialized = file_size > 0
                    && requested_range_is_materialized(fd, 0, file_size, file_size)
                        .unwrap_or(false);
                if fully_materialized {
                    if let Err(error) = mark_hydrated(fd) {
                        tracing::warn!(path = ?event.path, %error, "could not repair fully materialized placeholder state");
                    }
                }
                if let Err(error) = shared.fan.respond(fd, true) {
                    tracing::error!(%error, "could not allow a materialized range");
                }
                if fully_materialized {
                    // Keep the mark until the daemon acknowledges this
                    // completion.  A release can be punching holes at the
                    // same time; retiring the mark here would race that
                    // operation and leave the new placeholder unprotected.
                    // The daemon unwatches a hydrated path, while a
                    // dehydrated path deliberately keeps the mark.
                    if let Some(path) = &event.path {
                        let _ = send_to_session(
                            &client,
                            &Event::Filled {
                                id: request_id,
                                ok: true,
                                path: path.to_string_lossy().into_owned(),
                                full: true,
                            },
                        );
                    }
                }
                return;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(%error, "could not inspect placeholder extents");
            }
        }
    }

    // An open event: let the daemon decide whether this opener needs the whole
    // file up front (a sparse-aware copier does, because it will SEEK over the
    // holes without ever reading). Most openers are allowed straight through
    // and their reads are handled progressively by pre-content events.
    // An open by anything other than a sparse-aware copier is allowed here,
    // without a daemon round-trip: its reads are what drive hydration. Only a
    // copier — which will SEEK over the holes and never read — needs the whole
    // file fetched first, and only that case goes to the daemon. This keeps
    // opening a folder of placeholders fast: one cheap allow per file.
    if event.is_open {
        let comm = std::fs::read_to_string(format!("/proc/{}/comm", event.pid))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !is_bulk_copier(&comm) {
            if let Err(e) = shared.fan.respond(fd, true) {
                tracing::error!(error = %e, "could not answer an open");
            }
            return;
        }
        tracing::info!(reader = %comm, path = ?event.path, "copier open; hydrating whole file");
    }

    // Keep the request id on the fanotify worker's stack until the final
    // `Filled` event is sent.  The daemon uses it to reject a late completion
    // from an older range read that overlaps a newer whole-file request.
    let (allow, full) = match hydrate(&shared, &event, uid, &client, request_id) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, pid = event.pid, "hydration failed");
            (false, true)
        }
    };
    if allow && full {
        if let Err(error) = remove_mark_by_event_fd(&shared, identity, fd) {
            tracing::debug!(path = ?event.path, %error, "could not drop hydrated mark");
        }
    }

    // Answered no matter what: an unanswered pre-content event leaves the
    // reading process blocked forever.
    if let Err(e) = shared.fan.respond(fd, allow) {
        tracing::error!(error = %e, "could not answer the blocked reader");
    }
    if let Some(path) = &event.path {
        let _ = send_to_session(
            &client,
            &Event::Filled {
                id: request_id,
                ok: allow,
                path: path.to_string_lossy().into_owned(),
                full,
            },
        );
    }
}

fn handle_directory_open(
    shared: &Arc<Shared>,
    client: &ClientSession,
    uid: u32,
    event: &PreContentEvent,
    registered_path: &Path,
) {
    // A foreground open waits until the daemon has published every direct
    // child, matching CFAPI's FULL population contract.
    if !event.requires_response || !event.is_open {
        tracing::warn!(path = ?event.path, "unexpected directory fanotify event kind");
        if event.requires_response {
            let _ = shared.fan.respond(event.fd.as_raw_fd(), false);
        }
        return;
    }
    if event.pid == client.pid || event.pid == std::process::id() as i32 {
        let _ = shared.fan.respond(event.fd.as_raw_fd(), true);
        return;
    }
    if let Some(reader) = background_reader(event.pid) {
        tracing::debug!(
            pid = event.pid,
            reader,
            path = ?event.path,
            "allowing background open without populating a partial directory"
        );
        let _ = shared.fan.respond(event.fd.as_raw_fd(), true);
        return;
    }

    let reader = process_name(event.pid);
    let path = event.path.as_deref().unwrap_or(registered_path);
    let ok = match request_directory_population(shared, client, uid, path, reader, event.pid) {
        Ok(ok) => ok,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "directory population failed");
            false
        }
    };
    if let Err(error) = shared.fan.respond(event.fd.as_raw_fd(), ok) {
        tracing::error!(path = %path.display(), %error, "could not answer directory open");
    }
}

fn request_directory_population(
    shared: &Arc<Shared>,
    client: &ClientSession,
    uid: u32,
    path: &Path,
    reader: String,
    pid: i32,
) -> Result<bool> {
    const POPULATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let request = Event::DirectoryOpened {
        id,
        path: path.to_string_lossy().into_owned(),
        reader,
        pid,
    };
    shared.begin_request(uid, client, id, Pending { uid, reply: tx }, &request)?;
    let reply = match rx.recv_timeout(POPULATE_TIMEOUT) {
        Ok(reply) => reply,
        Err(_) => {
            shared.pending.lock().unwrap().remove(&id);
            bail!("the daemon did not populate within {POPULATE_TIMEOUT:?}")
        }
    };
    Ok(matches!(reply, Reply::DirectoryPopulated(true)))
}

fn process_name(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim().to_string())
        .unwrap_or_default()
}

fn hydrate(
    shared: &Arc<Shared>,
    event: &PreContentEvent,
    uid: u32,
    session: &ClientSession,
    request_id: u64,
) -> Result<(bool, bool)> {
    let Some(path) = &event.path else {
        bail!("the event carried no resolvable path");
    };

    let (tx, rx) = mpsc::channel();

    let reader = hydration_reader(event.pid, path);
    let request = Event::Hydrate {
        id: request_id,
        path: path.to_string_lossy().into_owned(),
        offset: event.offset,
        // A zero-length range means the kernel did not say; ask for the whole
        // file rather than guessing at a window.
        length: event.length,
        reader: reader.clone(),
        is_open: event.is_open,
    };
    shared.begin_request(
        uid,
        session,
        request_id,
        Pending { uid, reply: tx },
        &request,
    )?;

    let reply = match rx.recv_timeout(HYDRATE_TIMEOUT) {
        Ok(reply) => reply,
        Err(_) => {
            shared.pending.lock().unwrap().remove(&request_id);
            bail!("the daemon did not answer within {HYDRATE_TIMEOUT:?}")
        }
    };
    let (mut staged, offset, requested_full) = match reply {
        Reply::Fill(file, offset, full) => (file, offset, full),
        // Browsing sniff of an untouched placeholder: allow, no fill, offline.
        Reply::AllowEmpty => return Ok((true, false)),
        // `full = false` distinguishes an intentional offline browse refusal
        // from a failed fill. The daemon has no hydration state to unwind for
        // this path and must not publish a spurious failure completion.
        Reply::Deny => return Ok((false, false)),
        Reply::DirectoryPopulated(_) => bail!("received a directory reply for a file hydration"),
    };

    let staged_len = staged.metadata()?.len();
    let target_len = descriptor_len(event.fd.as_raw_fd())?;
    let expected = expected_fill_len(requested_full, offset, event.length, target_len)?;
    if staged_len != expected {
        bail!(
            "staged content has {staged_len} bytes, expected {expected} for {}",
            path.display()
        );
    }

    // Written through the event's own descriptor. Any other descriptor would
    // fault the hole back through this same hook and loop forever.
    let written =
        fill_from(&mut staged, &event.fd, offset, expected).context("filling staged content")?;
    if written != expected {
        bail!(
            "filled {written} bytes, expected {expected} for {}",
            path.display()
        );
    }

    // Progressive reads can cover a whole small file without any one request
    // being labelled full. Once SEEK_HOLE confirms that no sparse range
    // remains, finalise exactly like a whole-file fill: otherwise real bytes
    // occupy disk forever while the xattr and UI still say online-only.
    let full = requested_full
        || (target_len > 0
            && requested_range_is_materialized(event.fd.as_raw_fd(), 0, target_len, target_len)
                .unwrap_or(false));

    if full {
        // The blocked reader resumes as soon as FAN_ALLOW is sent. Mark the
        // source first so a file manager copying xattrs along with the now-real
        // bytes cannot leave the destination claiming it is still a sparse
        // online-only placeholder. The daemon follows with the full set of
        // marks and the content hash after it receives Filled. SQLite remains
        // Hydrating until then, which keeps watcher reconciliation from treating
        // this helper-owned close-write as a local edit.
        if let Err(error) = mark_hydrated(event.fd.as_raw_fd()) {
            // The bytes are already complete, so an xattr failure must not turn
            // a successful download into an I/O error for the user's copy.
            tracing::warn!(path = %path.display(), %error, "could not mark hydrated before releasing reader");
        }
        tracing::info!(path = %path.display(), bytes = target_len, %reader, pid = event.pid, "hydrated");
    } else {
        // Only the requested window was filled; the watch stays, so a reader
        // that keeps going comes back for the rest.
        tracing::debug!(
            path = %path.display(),
            bytes = written,
            offset,
            %reader,
            pid = event.pid,
            "hydrated range"
        );
    }
    Ok((true, full))
}

pub(super) fn mark_hydrated(fd: RawFd) -> std::io::Result<()> {
    let name = CString::new("user.gonedrive.state").expect("xattr name contains no NUL");
    let value = b"hydrated";
    // SAFETY: `name` is NUL-terminated, `value` is valid for its length, and
    // the fanotify event descriptor remains owned by `event` for this call.
    let rc = unsafe { libc::fsetxattr(fd, name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn descriptor_stat(fd: std::os::unix::io::RawFd) -> Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and `fd` belongs to the event.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fstat succeeded and initialized the structure.
    Ok(unsafe { stat.assume_init() })
}

pub(super) fn requested_range_is_materialized(
    fd: RawFd,
    offset: u64,
    length: u64,
    file_size: u64,
) -> std::io::Result<bool> {
    if length == 0 {
        return Ok(false);
    }
    let end = offset.saturating_add(length).min(file_size);
    if offset >= end {
        return Ok(true);
    }
    let offset = libc::off_t::try_from(offset).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "range offset exceeds the filesystem offset type",
        )
    })?;
    // SAFETY: `fd` is the valid descriptor supplied with this fanotify event.
    // SEEK_HOLE inspects extent metadata and does not read or fault file data.
    let next_hole = unsafe { libc::lseek(fd, offset, libc::SEEK_HOLE) };
    if next_hole < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            // Unsupported extent reporting and a wholly sparse tail both fall
            // back to the ordinary daemon hydration path.
            Some(libc::EINVAL) | Some(libc::ENXIO) => Ok(false),
            _ => Err(error),
        };
    }
    Ok(next_hole as u64 >= end)
}

fn descriptor_len(fd: std::os::unix::io::RawFd) -> Result<u64> {
    Ok(descriptor_stat(fd)?.st_size.max(0) as u64)
}

/// Whether a regular file reported only through its populated-parent mark can
/// be opened without an exact placeholder mark. Missing state means it is a
/// normal local file; hydrated state means all bytes are already present.
/// Dehydrated or malformed state stays fail-closed until registration finishes.
pub(super) fn unregistered_population_file_is_safe(fd: RawFd) -> std::io::Result<bool> {
    const STATE_XATTR: &[u8] = b"user.gonedrive.state\0";
    let mut value = [0u8; 32];
    // SAFETY: `fd` belongs to the live fanotify event, the name is NUL
    // terminated, and `value` is a writable buffer of the supplied length.
    let len = unsafe {
        libc::fgetxattr(
            fd,
            STATE_XATTR.as_ptr().cast(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if len < 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENODATA) {
            Ok(true)
        } else {
            Err(error)
        };
    }
    Ok(matches!(&value[..len as usize], b"hydrated" | b"unmanaged"))
}

pub(super) fn expected_fill_len(
    full: bool,
    offset: u64,
    requested: u64,
    target_len: u64,
) -> Result<u64> {
    if full {
        if offset != 0 {
            bail!("a full hydration must start at offset zero");
        }
        return Ok(target_len);
    }
    if offset > target_len {
        bail!("hydration offset {offset} is past the {target_len}-byte target");
    }
    Ok(requested.min(target_len - offset))
}

/// Copy a staged file into the placeholder through the event descriptor,
/// starting at `offset` within the placeholder.
/// Copy at most `limit` bytes from the staged source into the placeholder at
/// `offset`. The limit is the length validated against the placeholder before
/// the copy; the source is a peer-owned file that could grow underneath us,
/// and its EOF must never decide how far a root-owned write reaches.
pub(super) fn fill_from(
    reader: &mut std::fs::File,
    target: &std::os::unix::io::OwnedFd,
    offset: u64,
    limit: u64,
) -> Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut reader = (&mut *reader).take(limit);
    let mut offset = offset;
    let mut total = 0u64;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut written = 0usize;
        while written < n {
            let write_offset = offset
                .checked_add(written as u64)
                .filter(|offset| *offset <= libc::off_t::MAX as u64)
                .ok_or_else(|| anyhow::anyhow!("hydration write offset overflow"))?;
            // SAFETY: writing `n - written` bytes from a buffer we own into a
            // descriptor the kernel gave us, at an explicit offset.
            let rc = unsafe {
                libc::pwrite(
                    target.as_raw_fd(),
                    buf[written..n].as_ptr() as *const libc::c_void,
                    n - written,
                    write_offset as libc::off_t,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if rc == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "pwrite made no progress while hydrating",
                )
                .into());
            }
            written += rc as usize;
        }
        offset = offset
            .checked_add(n as u64)
            .ok_or_else(|| anyhow::anyhow!("hydration offset overflow"))?;
        total = total
            .checked_add(n as u64)
            .ok_or_else(|| anyhow::anyhow!("hydration byte count overflow"))?;
    }
    Ok(total)
}

/// Classify a file-manager read without trusting its process name alone.
///
/// Nautilus reads file headers both for display metadata and for a real copy.
/// During a copy it has already opened the destination for writing before the
/// first source read; a browse has no such descriptor. Binding the decision to
/// the same PID and filename keeps ordinary folder rendering offline while a
/// copy receives real bytes before its first read is released.
pub(super) fn hydration_reader(pid: i32, source: &Path) -> String {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    if is_file_manager_reader(&comm) {
        if let Some(destination) = writable_copy_destination(pid, source) {
            tracing::info!(
                source = %source.display(),
                destination = %destination.display(),
                pid,
                "file-manager copy detected"
            );
            return FILE_MANAGER_COPY_READER.into();
        }
    }
    comm
}

pub(super) fn writable_copy_destination(pid: i32, source: &Path) -> Option<PathBuf> {
    let source_name = source.file_name()?;
    let descriptors = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    for descriptor in descriptors.flatten() {
        let fd = descriptor.file_name();
        let flags = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{}", fd.to_string_lossy()))
            .ok()
            .and_then(|info| parse_open_flags(&info));
        let Some(flags) = flags else {
            continue;
        };
        if flags & libc::O_ACCMODE == libc::O_RDONLY {
            continue;
        }
        let Ok(target) = std::fs::read_link(descriptor.path()) else {
            continue;
        };
        if target == source || target.file_name() != Some(source_name) {
            continue;
        }
        if std::fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_file()) {
            return Some(target);
        }
    }
    None
}

fn parse_open_flags(fdinfo: &str) -> Option<i32> {
    let flags = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))?;
    i32::from_str_radix(flags.trim(), 8).ok()
}

/// Name the process if it is one that should never trigger a download.
pub(super) fn background_reader(pid: i32) -> Option<&'static str> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    is_background_reader(comm.trim())
}

/// Fail everything still waiting, so no reader is left blocked.
pub(super) fn fail_pending_for_uid(shared: &Arc<Shared>, uid: u32) {
    let pending = {
        let mut all = shared.pending.lock().unwrap();
        let ids: Vec<_> = all
            .iter()
            .filter(|(_, entry)| entry.uid == uid)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| all.remove(&id))
            .collect::<Vec<_>>()
    };
    for entry in pending {
        let _ = entry.reply.send(Reply::Deny);
    }
}
