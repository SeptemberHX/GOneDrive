//! Non-blocking fanotify dispatch and bounded placeholder hydration workers.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc};

use anyhow::{bail, Context, Result};
use gonedrive_ipc::hsm::Event;
use gonedrive_ipc::reader_policy::{is_background_reader, is_bulk_copier};

use crate::fanotify::PreContentEvent;
use crate::{
    send_to_session, ClientSession, FileIdentity, Pending, Reply, Shared, HYDRATE_TIMEOUT,
};

/// Poll for events and hand each one to a worker.
///
/// The loop itself must never block on anything but the poll: doing the fill
/// here would deadlock, because the write can block on the very event that has
/// not been answered yet.
pub(super) fn event_loop(shared: Arc<Shared>, workers: mpsc::SyncSender<PreContentEvent>) {
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
            tracing::error!(error = %err, "poll failed");
            return;
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
                            tracing::warn!(
                                pid = event.pid,
                                "hydration queue is full; denying read"
                            );
                            let _ = shared.fan.respond(event.fd.as_raw_fd(), false);
                        }
                        Err(mpsc::TrySendError::Disconnected(event)) => {
                            let _ = shared.fan.respond(event.fd.as_raw_fd(), false);
                            return;
                        }
                    }
                }
            }
            Err(e) => tracing::error!(error = %e, "reading events failed"),
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
            let _ = shared.fan.respond(fd, false);
            return;
        }
    };
    let identity = FileIdentity {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    };
    let uid = {
        let marks = shared.marks.lock().unwrap();
        match marks.get(&identity) {
            Some(entry) if entry.uid == stat.st_uid => entry.uid,
            _ => {
                tracing::warn!(
                    dev = identity.dev,
                    ino = identity.ino,
                    "denying an unregistered event target"
                );
                let _ = shared.fan.respond(fd, false);
                return;
            }
        }
    };
    let client = shared.sessions.lock().unwrap().get(&uid).cloned();
    let Some(client) = client else {
        let _ = shared.fan.respond(fd, false);
        return;
    };

    // The daemon's own access is always deliberate; blocking it on a question
    // only it can answer is a deadlock. Straight through.
    if event.pid == client.pid || event.pid == std::process::id() as i32 {
        if let Err(e) = shared.fan.respond(fd, true) {
            tracing::error!(error = %e, "could not answer our own client");
        }
        return;
    }

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

    let (allow, full) = match hydrate(&shared, &event, uid, &client) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, pid = event.pid, "hydration failed");
            (false, true)
        }
    };
    if allow && full {
        remove_mark_by_identity(&shared, identity);
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
                id: 0,
                ok: allow,
                path: path.to_string_lossy().into_owned(),
                full,
            },
        );
    }
}

fn hydrate(
    shared: &Arc<Shared>,
    event: &PreContentEvent,
    uid: u32,
    session: &ClientSession,
) -> Result<(bool, bool)> {
    let Some(path) = &event.path else {
        bail!("the event carried no resolvable path");
    };

    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();

    let reader = std::fs::read_to_string(format!("/proc/{}/comm", event.pid))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let request = Event::Hydrate {
        id,
        path: path.to_string_lossy().into_owned(),
        offset: event.offset,
        // A zero-length range means the kernel did not say; ask for the whole
        // file rather than guessing at a window.
        length: event.length,
        reader: reader.clone(),
        is_open: event.is_open,
    };
    shared.begin_hydration(uid, session, id, Pending { uid, reply: tx }, &request)?;

    let reply = match rx.recv_timeout(HYDRATE_TIMEOUT) {
        Ok(reply) => reply,
        Err(_) => {
            shared.pending.lock().unwrap().remove(&id);
            bail!("the daemon did not answer within {HYDRATE_TIMEOUT:?}")
        }
    };
    let (mut staged, offset, full) = match reply {
        Reply::Fill(file, offset, full) => (file, offset, full),
        // Browsing sniff of an untouched placeholder: allow, no fill, offline.
        Reply::AllowEmpty => return Ok((true, false)),
        Reply::Deny => return Ok((false, true)),
    };

    let staged_len = staged.metadata()?.len();
    let target_len = descriptor_len(event.fd.as_raw_fd())?;
    let expected = expected_fill_len(full, offset, event.length, target_len)?;
    if staged_len != expected {
        bail!(
            "staged content has {staged_len} bytes, expected {expected} for {}",
            path.display()
        );
    }

    // Written through the event's own descriptor. Any other descriptor would
    // fault the hole back through this same hook and loop forever.
    let written = fill_from(&mut staged, &event.fd, offset).context("filling staged content")?;
    if written != expected {
        bail!(
            "filled {written} bytes, expected {expected} for {}",
            path.display()
        );
    }

    if full {
        tracing::info!(path = %path.display(), bytes = written, %reader, pid = event.pid, "hydrated");
    } else {
        // Only the requested window was filled; the watch stays, so a reader
        // that keeps going comes back for the rest.
        tracing::info!(
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

fn descriptor_stat(fd: std::os::unix::io::RawFd) -> Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and `fd` belongs to the event.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fstat succeeded and initialized the structure.
    Ok(unsafe { stat.assume_init() })
}

fn descriptor_len(fd: std::os::unix::io::RawFd) -> Result<u64> {
    Ok(descriptor_stat(fd)?.st_size.max(0) as u64)
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
pub(super) fn fill_from(
    reader: &mut std::fs::File,
    target: &std::os::unix::io::OwnedFd,
    offset: u64,
) -> Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
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

fn remove_mark_by_identity(shared: &Arc<Shared>, identity: FileIdentity) {
    let entry = shared.marks.lock().unwrap().remove(&identity);
    if let Some(entry) = entry {
        if let Err(error) = shared.fan.unwatch_fd(entry.file.as_raw_fd()) {
            tracing::debug!(path = %entry.path.display(), %error, "could not drop hydrated mark");
        }
    }
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

pub(super) fn remove_marks_for_uid(shared: &Arc<Shared>, uid: u32) {
    let removed = {
        let mut marks = shared.marks.lock().unwrap();
        let identities: Vec<_> = marks
            .iter()
            .filter(|(_, entry)| entry.uid == uid)
            .map(|(identity, _)| *identity)
            .collect();
        identities
            .into_iter()
            .filter_map(|identity| marks.remove(&identity))
            .collect::<Vec<_>>()
    };
    for entry in removed {
        if let Err(error) = shared.fan.unwatch_fd(entry.file.as_raw_fd()) {
            tracing::debug!(path = %entry.path.display(), %error, "could not remove disconnected mark");
        }
    }
}
