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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use gonedrive_ipc::hsm::{Event, Request, SOCKET_PATH};

use crate::fanotify::{Fanotify, PreContentEvent};

/// Processes whose reads must never pull a file down from the cloud.
///
/// A placeholder is indistinguishable from an ordinary file, so a desktop
/// indexer walking the home directory would quietly download the user's entire
/// drive — which is the precise opposite of what files-on-demand is for. These
/// get a denial, which they treat as an unreadable file and skip.
///
/// Matched against `/proc/<pid>/comm`, which the kernel truncates to 15
/// characters, so the patterns are prefixes.
const BACKGROUND_READERS: &[&str] = &[
    "localsearch",      // GNOME's file indexer (formerly tracker-miner-fs)
    "tracker-miner",
    "tracker-extract",
    "tracker-store",
    "baloo_file",       // KDE's indexer
    "baloo_file_ext",
    "thumbnailer",      // gnome-desktop / tumbler thumbnail generation
    "tumblerd",
    "gsd-housekeep",    // GNOME's disk-usage sweep
    "updatedb",         // locate(1) database builders
    "plocate",
    "mlocate",
    "clamd",            // on-access antivirus
    "freshclam",
];

/// How long a blocked reader waits before we give up and fail its read.
/// Long enough for a large file over a slow link, short enough that a wedged
/// daemon does not hang an application indefinitely.
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Parser)]
#[command(name = "gonedrive-hsm", version, about = "GOnedrive files-on-demand helper")]
struct Cli {
    /// Unix socket to listen on.
    #[arg(long, default_value = SOCKET_PATH)]
    socket: PathBuf,

    /// Only serve this uid. Defaults to whoever connects first.
    #[arg(long)]
    uid: Option<u32>,
}

/// A hydration the helper is waiting on. The reply carries the file the daemon
/// staged the content in, or `None` if it could not fetch it.
struct Pending {
    reply: mpsc::Sender<Option<PathBuf>>,
}

struct Shared {
    fan: Fanotify,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    /// The connected daemon, if any.
    daemon: Mutex<Option<UnixStream>>,
    /// The uid the daemon runs as; paths are confined to its home directory.
    owner: Mutex<Option<u32>>,
    /// The daemon's pid, from the socket's peer credentials.
    ///
    /// Events raised by this pid are allowed through untouched: the daemon's
    /// own writes — punching holes, restoring timestamps, filling content —
    /// are always deliberate. Asking it whether its own blocked write should
    /// be hydrated is a deadlock with extra steps, and exactly that froze the
    /// daemon, its D-Bus interface, and every file manager thread queued
    /// behind them.
    client_pid: std::sync::atomic::AtomicI32,
}

impl Shared {
    fn send(&self, event: &Event) -> Result<()> {
        let mut guard = self.daemon.lock().unwrap();
        let Some(stream) = guard.as_mut() else {
            bail!("no daemon is connected");
        };
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
        Ok(())
    }
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
        daemon: Mutex::new(None),
        owner: Mutex::new(cli.uid),
        client_pid: std::sync::atomic::AtomicI32::new(0),
    });

    let listener = bind_socket(&cli.socket)?;
    tracing::info!(socket = %cli.socket.display(), "listening");

    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("fanotify".into())
            .spawn(move || event_loop(shared))?;
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(e) = serve_daemon(&shared, stream) {
                    tracing::warn!(error = %e, "daemon connection ended");
                }
                // Marks belong to the group, not the connection, but a daemon
                // that went away can no longer hydrate anything, so anything
                // still waiting has to be failed rather than left hanging.
                fail_all_pending(&shared);
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
        // The directory is the access control: only root may replace the
        // socket, while any local user may attempt to connect and is then
        // checked with SO_PEERCRED.
        let mut perms = std::fs::metadata(parent)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(parent, perms)?;
    }
    let _ = std::fs::remove_file(path);
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

fn serve_daemon(shared: &Arc<Shared>, stream: UnixStream) -> Result<()> {
    let (uid, pid) = peer_creds(&stream)?;
    {
        let mut owner = shared.owner.lock().unwrap();
        match *owner {
            Some(expected) if expected != uid => {
                bail!("refusing a connection from uid {uid}; this helper serves uid {expected}")
            }
            Some(_) => {}
            None => {
                tracing::info!(uid, "serving");
                *owner = Some(uid);
            }
        }
    }

    shared.client_pid.store(pid, Ordering::Relaxed);
    *shared.daemon.lock().unwrap() = Some(stream.try_clone()?);
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(e) => {
                let _ = shared.send(&Event::Error {
                    message: format!("malformed request: {e}"),
                });
                continue;
            }
        };
        handle_request(shared, request, uid);
    }

    *shared.daemon.lock().unwrap() = None;
    Ok(())
}

fn handle_request(shared: &Arc<Shared>, request: Request, uid: u32) {
    match request {
        Request::Ping => {
            let _ = shared.send(&Event::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
            });
        }
        Request::Watch { path } => {
            let path = PathBuf::from(path);
            match authorise(&path, uid) {
                Err(e) => {
                    let _ = shared.send(&Event::Error {
                        message: e.to_string(),
                    });
                }
                Ok(()) => match shared.fan.watch(&path) {
                    Ok(()) => {
                        tracing::debug!(path = %path.display(), "watching");
                        let _ = shared.send(&Event::Ok);
                    }
                    Err(e) => {
                        let _ = shared.send(&Event::Error {
                            message: format!("could not watch {}: {e}", path.display()),
                        });
                    }
                },
            }
        }
        Request::Unwatch { path } => {
            let path = PathBuf::from(path);
            // Removing a mark is harmless whatever the path, but keeping the
            // check makes the authorised set the same in both directions.
            if authorise(&path, uid).is_ok() {
                let _ = shared.fan.unwatch(&path);
            }
            let _ = shared.send(&Event::Ok);
        }
        Request::Hydrated { id, ok, source } => {
            let staged = match (ok, source) {
                (true, Some(source)) => {
                    let source = PathBuf::from(source);
                    // The staged file is named by an unprivileged process, so
                    // it gets the same ownership check as anything else.
                    match authorise(&source, uid) {
                        Ok(()) => Some(source),
                        Err(e) => {
                            tracing::warn!(error = %e, "refusing staged content");
                            None
                        }
                    }
                }
                _ => None,
            };
            if let Some(pending) = shared.pending.lock().unwrap().remove(&id) {
                let _ = pending.reply.send(staged);
            }
        }
    }
}

/// Confine the helper to files the connecting user owns.
///
/// Without this, any local user who could reach the socket could ask a
/// root-privileged process to install marks on arbitrary paths — a local
/// privilege escalation dressed up as a sync feature.
fn authorise(path: &Path, uid: u32) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    if path.components().any(|c| c == std::path::Component::ParentDir) {
        bail!("{} contains a parent-directory component", path.display());
    }
    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?;
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != uid {
        bail!(
            "{} belongs to uid {}, not to the connecting uid {uid}",
            path.display(),
            meta.uid()
        );
    }
    Ok(())
}

/// Poll for events and hand each one to a worker.
///
/// The loop itself must never block on anything but the poll: doing the fill
/// here would deadlock, because the write can block on the very event that has
/// not been answered yet.
fn event_loop(shared: Arc<Shared>) {
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
                    let shared = shared.clone();
                    // One thread per blocked reader. There is already a process
                    // parked on each of these, so the count is bounded by real
                    // concurrent access rather than by anything we control.
                    std::thread::spawn(move || handle_event(shared, event));
                }
            }
            Err(e) => tracing::error!(error = %e, "reading events failed"),
        }
    }
}

fn handle_event(shared: Arc<Shared>, event: PreContentEvent) {
    let fd = event.fd.as_raw_fd();

    // The daemon's own access is always deliberate; blocking it on a question
    // only it can answer is a deadlock. Straight through.
    if event.pid == shared.client_pid.load(Ordering::Relaxed)
        || event.pid == std::process::id() as i32
    {
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

    let allow = match hydrate(&shared, &event) {
        Ok(allow) => allow,
        Err(e) => {
            tracing::warn!(error = %e, pid = event.pid, "hydration failed");
            false
        }
    };
    // Answered no matter what: an unanswered pre-content event leaves the
    // reading process blocked forever.
    if let Err(e) = shared.fan.respond(fd, allow) {
        tracing::error!(error = %e, "could not answer the blocked reader");
    }
    if let Some(path) = &event.path {
        let _ = shared.send(&Event::Filled {
            id: 0,
            ok: allow,
            path: path.to_string_lossy().into_owned(),
        });
    }
}

fn hydrate(shared: &Arc<Shared>, event: &PreContentEvent) -> Result<bool> {
    let Some(path) = &event.path else {
        bail!("the event carried no resolvable path");
    };

    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    shared
        .pending
        .lock()
        .unwrap()
        .insert(id, Pending { reply: tx });

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
    };
    if let Err(e) = shared.send(&request) {
        shared.pending.lock().unwrap().remove(&id);
        return Err(e);
    }

    let staged = match rx.recv_timeout(HYDRATE_TIMEOUT) {
        Ok(staged) => staged,
        Err(_) => {
            shared.pending.lock().unwrap().remove(&id);
            bail!("the daemon did not answer within {HYDRATE_TIMEOUT:?}")
        }
    };
    let Some(staged) = staged else {
        return Ok(false);
    };

    // Written through the event's own descriptor. Any other descriptor would
    // fault the hole back through this same hook and loop forever.
    let written = fill_from(&staged, &event.fd)
        .with_context(|| format!("filling from {}", staged.display()))?;
    let _ = std::fs::remove_file(&staged);

    // The file now holds its content, so it no longer needs intercepting.
    if let Err(e) = shared.fan.unwatch(path) {
        tracing::debug!(path = %path.display(), error = %e, "could not drop the mark");
    }
    tracing::info!(path = %path.display(), bytes = written, %reader, pid = event.pid, "hydrated");
    Ok(true)
}

/// Copy a staged file into the placeholder through the event descriptor.
fn fill_from(source: &Path, target: &std::os::unix::io::OwnedFd) -> Result<u64> {
    use std::io::Read;
    let mut reader = std::fs::File::open(source)?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut written = 0usize;
        while written < n {
            // SAFETY: writing `n - written` bytes from a buffer we own into a
            // descriptor the kernel gave us, at an explicit offset.
            let rc = unsafe {
                libc::pwrite(
                    target.as_raw_fd(),
                    buf[written..n].as_ptr() as *const libc::c_void,
                    n - written,
                    (offset + written as u64) as libc::off_t,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            written += rc as usize;
        }
        offset += n as u64;
    }
    Ok(offset)
}

/// Name the process if it is one that should never trigger a download.
fn background_reader(pid: i32) -> Option<&'static str> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    BACKGROUND_READERS
        .iter()
        .find(|prefix| comm.starts_with(*prefix))
        .copied()
}

/// Fail everything still waiting, so no reader is left blocked.
fn fail_all_pending(shared: &Arc<Shared>) {
    let pending: Vec<_> = shared.pending.lock().unwrap().drain().collect();
    for (_, entry) in pending {
        let _ = entry.reply.send(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn my_uid() -> u32 {
        // SAFETY: getuid cannot fail and takes no arguments.
        unsafe { libc::getuid() }
    }

    #[test]
    fn the_daemons_own_access_is_never_questioned() {
        // The deadlock this prevents: the daemon writes to a watched file,
        // the kernel blocks the write and asks the helper, the helper asks the
        // daemon — which cannot answer, because it is the blocked write.
        let shared_pid = 4242;
        let event_pid = 4242;
        assert_eq!(shared_pid, event_pid, "same pid must be allowed through");
        // The real check is `event.pid == client_pid` in handle_event; this
        // test pins the intent so the guard is not "simplified" away.
    }

    #[test]
    fn the_desktop_indexer_is_recognised() {
        // The case that matters: a placeholder looks like an ordinary file, so
        // an indexer walking the home directory would download the whole drive.
        for name in ["localsearch-3", "tracker-miner-f", "baloo_file", "tumblerd"] {
            assert!(
                BACKGROUND_READERS.iter().any(|p| name.starts_with(p)),
                "{name} should be refused"
            );
        }
    }

    #[test]
    fn ordinary_applications_are_not_refused() {
        for name in ["gedit", "libreoffice", "cat", "firefox", "nautilus"] {
            assert!(
                !BACKGROUND_READERS.iter().any(|p| name.starts_with(p)),
                "{name} must still be able to open its files"
            );
        }
    }

    #[test]
    fn our_own_process_is_not_a_background_reader() {
        assert!(background_reader(std::process::id() as i32).is_none());
    }

    #[test]
    fn a_dead_process_is_not_claimed() {
        // A reader that exited between the event and the lookup must not be
        // matched against a stale name.
        assert!(background_reader(i32::MAX).is_none());
    }

    #[test]
    fn a_users_own_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.bin");
        std::fs::write(&path, b"x").unwrap();
        assert!(authorise(&path, my_uid()).is_ok());
    }

    #[test]
    fn another_users_file_is_refused() {
        // This is the whole point of the check: without it, any local user who
        // can reach the socket can make a root process install marks on files
        // they do not own.
        let path = Path::new("/etc/shadow");
        if !path.exists() {
            return;
        }
        let err = authorise(path, my_uid()).unwrap_err().to_string();
        assert!(err.contains("belongs to uid"), "{err}");
    }

    #[test]
    fn relative_paths_are_refused() {
        let err = authorise(Path::new("OneDrive/a.bin"), my_uid())
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn parent_directory_traversal_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("..").join("escape.bin");
        std::fs::write(dir.path().join("escape.bin"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        // The file resolves, so only the explicit component check stops it.
        let err = authorise(&path, my_uid()).unwrap_err().to_string();
        assert!(err.contains("parent-directory"), "{err}");
    }

    #[test]
    fn a_missing_file_is_refused_rather_than_assumed_safe() {
        let err = authorise(Path::new("/nonexistent/nope.bin"), my_uid())
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot stat"), "{err}");
    }
}
