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
mod file_handle;
mod marks;
mod server;
mod worker;

use std::collections::HashMap;
use std::io::IsTerminal;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use gonedrive_ipc::hsm::{Event, SOCKET_PATH};

use crate::fanotify::Fanotify;
use crate::file_handle::{MountHandles, ReopenableFile};
use crate::marks::MarkRegistry;
#[cfg(test)]
use crate::marks::{open_authorised, open_root};
use crate::server::{bind_socket, send_to_session, send_to_stream, serve_daemon};
#[cfg(test)]
use crate::server::{parallel_map_ordered, read_frame, truncate_mark_error, MARK_ERROR_BYTES};
#[cfg(test)]
use crate::worker::{
    background_reader, expected_fill_len, fill_from, hydration_reader, mark_hydrated,
    requested_range_is_materialized, unregistered_population_file_is_safe,
};
use crate::worker::{event_loop, handle_event};
#[cfg(test)]
use gonedrive_ipc::hsm::MAX_FRAME_SIZE;

/// How long a blocked reader waits before we give up and fail its read.
/// Long enough for a large file over a slow link, short enough that a wedged
/// daemon does not hang an application indefinitely.
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Pre-content events block real readers, but an attacker must not be able to
/// turn each one into an unbounded root-owned thread.
const WORKERS: usize = 64;
const WORK_QUEUE: usize = 256;
/// How many workers one uid may occupy at once.
///
/// A blocked reader holds its worker for up to [`HYDRATE_TIMEOUT`], so without
/// a ceiling one user opening many placeholders at once takes the whole pool
/// and every other user's reads wait behind them. Three quarters leaves a
/// single user essentially unconstrained while keeping a floor for everyone
/// else. Events over the ceiling are denied, which is how a full queue already
/// behaves: the reader sees an I/O error rather than an unbounded wait.
const MAX_WORKERS_PER_UID: usize = WORKERS * 3 / 4;

#[cfg(debug_assertions)]
const DEFAULT_LOG_FILTER: &str = "gonedrive_hsm=info";
#[cfg(not(debug_assertions))]
const DEFAULT_LOG_FILTER: &str = "warn";

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
    /// Completion of a directory population callback.
    DirectoryPopulated(bool),
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
    path: Arc<Path>,
    file: ReopenableFile,
    kind: MarkKind,
    sweep: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkKind {
    Placeholder,
    PopulationDirectory,
}

#[derive(Clone)]
struct ClientSession {
    pid: i32,
    stream: Arc<Mutex<UnixStream>>,
}

/// A directory the daemon declared as one of its sync roots. Every path the
/// daemon names afterwards is resolved beneath this descriptor, so the helper's
/// root privilege never reaches a file outside the tree it was told about.
#[derive(Debug)]
struct SyncRoot {
    path: Arc<Path>,
    dir: std::fs::File,
}

struct Shared {
    fan: Fanotify,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    sessions: Mutex<HashMap<u32, ClientSession>>,
    marks: Mutex<MarkRegistry>,
    sweeps: Mutex<HashMap<u32, u64>>,
    /// Workers currently occupied per uid. See [`MAX_WORKERS_PER_UID`].
    in_flight: Arc<WorkerSlots>,
    /// Declared sync roots per uid. Like marks, they outlive the session that
    /// declared them: a reconnecting daemon re-declares before its first watch.
    roots: Mutex<HashMap<u32, Arc<Vec<SyncRoot>>>>,
    mounts: MountHandles,
    descriptor_fallbacks: AtomicU64,
    next_sweep: AtomicU64,
    allowed_uid: Option<u32>,
}

/// A path as it may safely appear in a message.
///
/// Names come from the peer's filesystem and can hold newlines or terminal
/// escape sequences. `Display` passes those straight through, which lets a
/// crafted name forge journal lines or steer the terminal of whoever reads
/// them; the debug form quotes and escapes instead.
pub(crate) fn shown(path: impl AsRef<Path>) -> String {
    format!("{:?}", path.as_ref())
}

/// How many workers each uid currently occupies.
#[derive(Default)]
struct WorkerSlots {
    held: Mutex<HashMap<u32, usize>>,
}

impl WorkerSlots {
    /// Take a slot for `uid`, or `None` when it already holds its share.
    fn claim(self: &Arc<Self>, uid: u32, ceiling: usize) -> Option<WorkerSlot> {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = held.entry(uid).or_insert(0);
        if *count >= ceiling {
            return None;
        }
        *count += 1;
        Some(WorkerSlot {
            slots: self.clone(),
            uid,
        })
    }
}

/// Occupies one of a uid's worker slots for as long as it is alive.
struct WorkerSlot {
    slots: Arc<WorkerSlots>,
    uid: u32,
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        let mut held = self
            .slots
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = held.get_mut(&self.uid) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.uid);
            }
        }
    }
}

impl Shared {
    /// Take a worker slot for `uid`, or `None` when it already holds its share.
    fn claim_worker(&self, uid: u32) -> Option<WorkerSlot> {
        self.in_flight.claim(uid, MAX_WORKERS_PER_UID)
    }

    /// The roots this uid's daemon declared; empty until it does, which
    /// refuses every path.
    fn roots_for(&self, uid: u32) -> Arc<Vec<SyncRoot>> {
        self.roots
            .lock()
            .unwrap()
            .get(&uid)
            .cloned()
            .unwrap_or_default()
    }

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

    fn begin_request(
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
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
        marks: Mutex::new(MarkRegistry::default()),
        sweeps: Mutex::new(HashMap::new()),
        in_flight: Arc::new(WorkerSlots::default()),
        roots: Mutex::new(HashMap::new()),
        mounts: MountHandles::default(),
        descriptor_fallbacks: AtomicU64::new(0),
        next_sweep: AtomicU64::new(1),
        allowed_uid: cli.uid,
    });

    let listener = bind_socket(&cli.socket)?;
    tracing::info!(socket = %shown(cli.socket), "listening");

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

    let (event_error_tx, event_error_rx) = mpsc::channel();
    {
        let shared = shared.clone();
        let event_error_tx = event_error_tx.clone();
        std::thread::Builder::new()
            .name("fanotify".into())
            .spawn(move || {
                let error = event_loop(shared, work_tx).err().unwrap_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "fanotify worker queue disconnected",
                    )
                });
                let _ = event_error_tx.send(error);
            })?;
    }

    listener.set_nonblocking(true)?;
    loop {
        if let Ok(error) = event_error_rx.try_recv() {
            return Err(error).context("fanotify event loop stopped; restarting helper");
        }
        match listener.accept() {
            Ok((stream, _address)) => {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_daemon(&shared, stream) {
                        tracing::warn!(error = %e, "daemon connection ended");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => tracing::warn!(%error, "accept failed"),
        }
    }
}

#[cfg(test)]
mod tests;
