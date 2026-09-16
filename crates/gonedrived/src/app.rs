//! The daemon supervisor: owns the engines, the poll loop and the status.

mod coordinator;
mod hydration;
mod mode;
mod placeholder_protector;

pub(crate) use mode::{ModeSwitch, SyncMode};

pub(crate) use hydration::{HydrationCapabilities, HydrationRequest, StageOutcome};

#[cfg(test)]
use coordinator::{
    missing_account_retry_due, outbound_scope, remote_poll_ready, sync_barrier_ready,
};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_core::config::Config;
use gonedrive_core::db::Store;
use gonedrive_core::graph::GraphClient;
use gonedrive_core::sync::engine::{Direction, Engine, SafetyStopKind, SyncEvent, SyncSummary};
use gonedrive_core::{Authenticator, Error, TokenStore};
use gonedrive_ipc::reader_policy::{
    is_browsing_reader, is_bulk_copier, is_file_manager_copy_reader, is_file_manager_reader,
    is_progressive_reader,
};
use gonedrive_ipc::{AccountInfo, Hydration, PendingItem, SyncRequestResult, SyncStatus};

use crate::placeholder_reports::PendingPlaceholderReports;

/// A head-read at or below this is served as a range, not a download: it is the
/// signature of content sniffing, and even when it is not, filling just the
/// window is always correct — the watch stays armed for the rest.
const PARTIAL_HYDRATION_CEILING: u64 = 256 * 1024;

/// Upper bound on one account's credential read at startup. Long enough for a
/// slow network token refresh, short enough that a locked keyring waiting on an
/// unanswered unlock prompt cannot hang the daemon.
const CREDENTIAL_LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const KEYRING_LOCKED_MESSAGE: &str =
    "系统钥匙环已锁定。请先解锁登录钥匙环，然后点击“立即同步”重试；已保存的登录凭据仍然保留。";

/// What to do for a blocked read, decided purely from the request — no I/O, so
/// the browsing/copier/consumer policy can be tested on its own.
#[derive(Debug, PartialEq)]
enum StagePlan {
    /// Let the open proceed; the reads that follow drive hydration.
    AllowOpen,
    /// Give the reader the whole file before it looks.
    Full,
    /// Serve `[offset, offset+length)` only if it is already local, else allow
    /// an offline read of the sparse file.
    BrowseLocal { offset: u64, length: u64 },
    /// Fetch just `[offset, offset+length)`.
    Range { offset: u64, length: u64 },
    /// Fail a speculative file-manager read without exposing sparse zeroes.
    DenyBrowse,
}

impl StagePlan {
    fn shows_progress(&self) -> bool {
        matches!(self, Self::Full)
    }
}

/// The staging policy, as a pure function of the request.
///
/// - On open: a sparse-aware bulk copier is hydrated fully (it would otherwise
///   SEEK over the holes and copy zeros); everything else is allowed through.
/// - A dedicated thumbnailer's sniff never touches the network. A negotiated
///   helper distinguishes a real Nautilus copy from speculative content sniffing:
///   the copy hydrates fully and the sniff is denied without exposing sparse
///   zeroes. Older helpers retain the conservative range/full fallback.
/// - A consumer's first head-read (offset 0, small, smaller than the file) is a
///   range; coming back for offset > 0 is sequential reading and escalates to a
///   full hydration rather than dozens of round-trips.
fn decide_stage(
    is_open: bool,
    reader: &str,
    offset: u64,
    length: u64,
    size: u64,
    allow_partial: bool,
    file_manager_copy_detection: bool,
) -> StagePlan {
    if is_file_manager_copy_reader(reader) {
        return StagePlan::Full;
    }
    if is_open {
        return if is_bulk_copier(reader) {
            StagePlan::Full
        } else {
            StagePlan::AllowOpen
        };
    }
    if is_browsing_reader(reader) {
        if is_file_manager_reader(reader) {
            if file_manager_copy_detection {
                return StagePlan::DenyBrowse;
            }
            let partial = allow_partial
                && size > 0
                && length > 0
                && offset == 0
                && length <= PARTIAL_HYDRATION_CEILING;
            return if partial {
                StagePlan::Range {
                    offset,
                    length: length.min(size.saturating_sub(offset)).max(1),
                }
            } else {
                StagePlan::Full
            };
        }
        return StagePlan::BrowseLocal {
            offset,
            length: length.max(1),
        };
    }
    if is_progressive_reader(reader) && allow_partial && size > 0 && length > 0 && offset < size {
        let length = length.min(size - offset);
        return if offset == 0 && length == size {
            StagePlan::Full
        } else {
            StagePlan::Range { offset, length }
        };
    }
    let partial = allow_partial
        && length > 0
        && offset == 0
        && length <= PARTIAL_HYDRATION_CEILING
        && length < size;
    if partial {
        StagePlan::Range {
            offset,
            length: length.min(size.saturating_sub(offset)).max(1),
        }
    } else {
        StagePlan::Full
    }
}

/// States that describe the account rather than a transfer.
///
/// Only the coordinator sets them, and only it can tell when they are over: a
/// transfer completing says nothing about whether the account is reachable.
/// `error` is deliberately absent — the event forwarder raises that one itself
/// for a failed cycle, so it must also be able to clear it on a clean one.
pub(super) fn is_account_level_state(state: &str) -> bool {
    matches!(state, "offline" | "keyring_locked" | "signed_out")
}

fn core_error_status(error: &Error) -> (&'static str, String) {
    match error {
        Error::AuthRequired(message) => ("signed_out", message.clone()),
        Error::KeyringLocked(_) => ("keyring_locked", KEYRING_LOCKED_MESSAGE.into()),
        error if error.is_transient() => ("offline", detailed_error(error)),
        error => ("error", detailed_error(error)),
    }
}

/// Preserve the causal chain that `reqwest` attaches to transport failures.
/// Its top-level Display text only says that sending the request failed, which
/// hides the useful DNS, TLS, timeout, or connection-refused cause from both
/// the UI and logs.
fn detailed_error(error: &(dyn std::error::Error + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if messages.last() != Some(&message) {
            messages.push(message);
        }
        source = cause.source();
    }
    messages.join(": ")
}
use tokio::sync::{broadcast, Mutex, Notify, OwnedMutexGuard, RwLock};

/// Anything the front ends care about.
#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Status(SyncStatus),
    LoginCompleted(String),
    LoginFailed(String),
    SyncRequestCompleted {
        generation: u64,
        result: SyncRequestResult,
    },
    Conflict {
        original: String,
        copy: String,
    },
    ItemFailed {
        name: String,
        error: String,
    },
    /// A destructive step was refused as likely accidental data loss (an
    /// unmounted sync root, or an alarming mass deletion). Nothing was deleted.
    SafetyStop {
        reason: String,
    },
    HydrationStarted(Hydration),
    HydrationProgress(Hydration),
    HydrationFinished {
        path: String,
        ok: bool,
    },
}

/// A placeholder currently being filled.
struct InFlight {
    /// The HSM request id that created this entry.  Completion messages carry
    /// the same id so a late range callback cannot tear down a newer request
    /// for the same path.
    request_id: u64,
    hydration: Hydration,
    /// Only whole-file fills represent an operation the user is waiting for.
    /// Range fills are quiet file-manager reads and must never appear in the
    /// download monitor if it happens to connect while they are in flight.
    visible: bool,
    /// Set to abandon the transfer.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    staged: Option<std::path::PathBuf>,
    /// Held from staging until the helper reports Filled, so no second reader
    /// can truncate or consume the same sibling staging file.
    _path_guard: OwnedMutexGuard<()>,
    /// Only one helper completion may commit this request.  Fanotify can
    /// legitimately report the final materialised range more than once.
    finishing: bool,
}

#[derive(Default)]
struct PathLocks {
    locks: Mutex<HashMap<std::path::PathBuf, Weak<Mutex<()>>>>,
    /// Paths currently being turned into placeholders.  Fanotify can deliver
    /// a permission event after the release request has started but before a
    /// worker has handed it to the daemon; the reference count keeps that
    /// late event from starting a new hydration.  Counts (rather than a set)
    /// are required because overlapping subtree releases are allowed to wait
    /// on the same paths.
    releasing: StdMutex<HashMap<std::path::PathBuf, usize>>,
}

/// A cancellation-safe lease on the paths covered by a space release. It is
/// synchronous so a cancelled D-Bus future still clears its protection in
/// `Drop` rather than leaving a path blocked until the daemon restarts.
struct ReleaseBarrier<'a> {
    state: &'a StdMutex<HashMap<std::path::PathBuf, usize>>,
    paths: Vec<std::path::PathBuf>,
}

impl Drop for ReleaseBarrier<'_> {
    fn drop(&mut self) {
        let mut releasing = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for path in &self.paths {
            let remove = releasing.get_mut(path).is_some_and(|count| {
                *count = count.saturating_sub(1);
                *count == 0
            });
            if remove {
                releasing.remove(path);
            }
        }
    }
}

impl PathLocks {
    async fn acquire(&self, path: &std::path::Path) -> OwnedMutexGuard<()> {
        let path_lock = {
            let mut locks = self.locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(path).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        path_lock.lock_owned().await
    }

    /// Acquire a stable, deadlock-free set of path locks.  Space release can
    /// cover a whole subtree while hydration requests arrive concurrently; a
    /// sorted/deduplicated order keeps two overlapping subtree operations from
    /// waiting on each other forever.
    async fn acquire_many(&self, paths: &[std::path::PathBuf]) -> Vec<OwnedMutexGuard<()>> {
        let mut ordered = paths.to_vec();
        ordered.sort();
        ordered.dedup();
        let mut guards = Vec::with_capacity(ordered.len());
        for path in ordered {
            guards.push(self.acquire(&path).await);
        }
        guards
    }

    fn begin_release(&self, paths: &[std::path::PathBuf]) -> ReleaseBarrier<'_> {
        let paths = paths
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut releasing = self
            .releasing
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for path in &paths {
            *releasing.entry(path.clone()).or_default() += 1;
        }
        ReleaseBarrier {
            state: &self.releasing,
            paths,
        }
    }

    fn is_releasing(&self, path: &std::path::Path) -> bool {
        self.releasing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(path)
    }
}

pub struct App {
    /// Read through [`App::config`]. A mode switch replaces this in place, so
    /// nothing may cache a snapshot across an await that could switch modes.
    config: std::sync::RwLock<Arc<Config>>,
    pub store: Store,
    pub tokens: Arc<TokenStore>,
    pub http: reqwest::Client,
    engines: RwLock<HashMap<String, Arc<Engine>>>,
    status: Arc<RwLock<SyncStatus>>,
    paused: Arc<AtomicBool>,
    approve_mass_delete_on_resume: Arc<AtomicBool>,
    force_full_scan: AtomicBool,
    /// Wakes the poll loop early: a local change, or an explicit SyncNow.
    pub wake: Arc<Notify>,
    next_sync_generation: AtomicU64,
    requested_sync_generation: AtomicU64,
    /// Placeholders being filled right now, keyed by absolute path.
    hydrations: Arc<RwLock<HashMap<std::path::PathBuf, InFlight>>>,
    hydration_locks: PathLocks,
    /// Serialises content downloads with space release.  A queued transfer can
    /// already have popped its durable row when the user asks to release a
    /// file; letting both write concurrently would materialise it again after
    /// the release returns.
    pub(crate) download_gate: Arc<Mutex<()>>,
    /// Serialises sync-mode switches. Two concurrent switches would each
    /// prune the tree and publish a configuration derived from a snapshot the
    /// other has already replaced.
    pub(crate) mode_switch: Mutex<()>,

    /// The privileged helper, when files-on-demand is available. Without it a
    /// placeholder is just a file full of zeroes that nothing will ever fill.
    hsm: RwLock<Option<Arc<crate::hsm_client::HsmClient>>>,
    /// A full local scan may beat HSM startup. Retain its actual inode paths
    /// until the helper can reconcile them instead of waiting for the next
    /// low-frequency verification pass.
    pending_placeholder_reports: Mutex<PendingPlaceholderReports>,
    /// Exact desktop Open requests, used to separate a gallery's selected file
    /// from speculative reads of neighbouring files in the same process.
    open_intents: Arc<crate::open_intent::OpenIntents>,
    /// The single progress window, if one is up. Guarded so a burst of
    /// hydrations does not stack three windows (as it did once). `Arc` so a
    /// spawned task can hold it without borrowing the whole `App`.
    monitor: Arc<std::sync::Mutex<Option<std::process::Child>>>,
    events: broadcast::Sender<DaemonEvent>,
}

impl App {
    /// The live configuration.
    ///
    /// Callers get a snapshot; a sync-mode switch publishes a new one through
    /// [`App::replace_config`]. Re-read it after any await that could switch
    /// modes rather than holding an early snapshot.
    pub fn config(&self) -> Arc<Config> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Publish a configuration that has already been persisted.
    pub(crate) fn replace_config(&self, config: Config) {
        *self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(config);
    }

    pub async fn new(config: Config) -> Result<Arc<Self>> {
        gonedrive_core::config::ensure_dirs()?;
        let store = Store::open(&gonedrive_core::config::state_db_path())
            .context("could not open the state database")?;
        let tokens = Arc::new(
            TokenStore::open()
                .await
                .context("could not reach the Secret Service (is a keyring running?)")?,
        );
        let (events, _) = broadcast::channel(256);

        let (initial_pending, initial_pending_items) = pending_snapshot(&store).unwrap_or_default();
        let mut initial_status = SyncStatus::idle(initial_pending);
        initial_status.pending_items = initial_pending_items;
        let open_intents = crate::open_intent::OpenIntents::new(config.sync_root.clone());

        let app = Arc::new(Self {
            config: std::sync::RwLock::new(Arc::new(config)),
            store,
            tokens,
            http: build_http_client()?,
            engines: RwLock::new(HashMap::new()),
            status: Arc::new(RwLock::new(initial_status)),
            paused: Arc::new(AtomicBool::new(false)),
            approve_mass_delete_on_resume: Arc::new(AtomicBool::new(false)),
            force_full_scan: AtomicBool::new(false),
            wake: Arc::new(Notify::new()),
            next_sync_generation: AtomicU64::new(1),
            requested_sync_generation: AtomicU64::new(0),
            hydrations: Arc::new(RwLock::new(HashMap::new())),
            hydration_locks: PathLocks::default(),
            download_gate: Arc::new(Mutex::new(())),
            mode_switch: Mutex::new(()),
            hsm: RwLock::new(None),
            pending_placeholder_reports: Mutex::new(PendingPlaceholderReports::default()),
            open_intents,
            monitor: Arc::new(std::sync::Mutex::new(None)),
            events,
        });
        Ok(app)
    }

    pub(crate) fn start_open_intent_monitor(self: &Arc<Self>) {
        let mut explicit = self.open_intents.subscribe();
        let app = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let path = match explicit.recv().await {
                    Ok(path) => path,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "desktop open intents were coalesced");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let Some(app) = app.upgrade() else {
                    return;
                };
                app.populate_desktop_directory(&path).await;
            }
        });
        tokio::spawn(self.open_intents.clone().supervise());
    }

    pub(crate) async fn desktop_open_decision(
        &self,
        path: &std::path::Path,
    ) -> crate::open_intent::OpenDecision {
        self.open_intents.decide(path).await
    }

    async fn populate_desktop_directory(&self, path: &std::path::Path) {
        if gonedrive_core::hydrate::Placeholder::directory_population(path)
            == gonedrive_core::hydrate::DirectoryPopulation::Unmanaged
        {
            return;
        }
        let Ok(relative) = path.strip_prefix(&self.config().sync_root) else {
            return;
        };
        let needs_population = {
            let engines = self.engines.read().await;
            match engines.values().next() {
                Some(engine) => match engine.folder_needs_population(relative) {
                    Ok(needs) => needs,
                    Err(error) => {
                        tracing::debug!(path = %path.display(), %error, "could not check directory population state");
                        false
                    }
                },
                None => false,
            }
        };
        if !needs_population {
            return;
        }
        tracing::debug!(path = %path.display(), "desktop navigated to a directory with pending lookahead");
        if let Err(error) = self.populate_folder(relative).await {
            tracing::warn!(path = %path.display(), %error, "could not populate the desktop's open directory");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: DaemonEvent) {
        let _ = self.events.send(event);
    }

    /// Bring up the engine for the account already in the database.
    ///
    /// Item ids and placeholder xattrs are not namespaced by account yet, so
    /// allowing two engines to scan the same root could copy content between
    /// accounts. Refuse that legacy state before either engine touches disk.
    pub async fn load_accounts(self: &Arc<Self>) -> Result<usize> {
        let accounts = self.store.accounts()?;
        if accounts.len() > 1 {
            let ids = accounts
                .iter()
                .map(|account| account.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "multiple accounts are configured ({ids}); GOnedrive currently supports one account per profile. Sign out of all but one account before starting the daemon"
            );
        }

        let mut loaded = 0;
        for account in accounts {
            match self.attach_account(&account.id).await {
                Ok(()) => loaded += 1,
                Err(e) => {
                    tracing::error!(account = %account.id, error = ?e, "could not sign in");
                    self.report_anyhow_error(&e).await;
                }
            }
        }
        Ok(loaded)
    }

    /// Retry accounts that exist in the database but could not be attached at
    /// startup, most commonly because the login keyring was locked. Called by
    /// an explicit SyncNow after the user has unlocked it.
    pub(super) async fn attach_missing_accounts(self: &Arc<Self>) -> Result<usize> {
        let active: std::collections::HashSet<_> =
            self.engines.read().await.keys().cloned().collect();
        let mut attached = 0;
        for account in self.store.accounts()? {
            if active.contains(&account.id) {
                continue;
            }
            self.attach_account(&account.id).await?;
            attached += 1;
        }
        Ok(attached)
    }

    /// Build and register the engine for one account.
    pub async fn attach_account(self: &Arc<Self>, account_id: &str) -> Result<()> {
        if let Some(other) = self
            .store
            .accounts()?
            .into_iter()
            .find(|account| account.id != account_id)
        {
            anyhow::bail!(
                "cannot attach account {account_id} while {} is configured; GOnedrive currently supports one account per profile",
                other.id
            );
        }
        if let Some(other) = self
            .engines
            .read()
            .await
            .keys()
            .find(|id| id.as_str() != account_id)
        {
            anyhow::bail!("account {other} is already active");
        }

        // Reading the refresh token can block on the Secret Service: a locked
        // keyring makes it wait on an unlock prompt that may never be answered.
        // Bound it so a stuck credential read can never hang daemon startup —
        // we are already on the bus (see runtime::run). The bound covers the
        // keyring read *and* the first token refresh, so a slow network trips
        // it as easily as a locked keyring; report it as the transient it is
        // (the account re-attaches on the ordinary remote timer) rather than
        // as a keyring lock, which only an explicit wake retries.
        let auth = match tokio::time::timeout(
            CREDENTIAL_LOAD_TIMEOUT,
            Authenticator::restore(
                self.http.clone(),
                self.config().client_id.clone(),
                account_id.to_string(),
                self.tokens.clone(),
            ),
        )
        .await
        {
            Ok(result) => result?,
            Err(_elapsed) => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out loading credentials (slow token endpoint or locked keyring)",
                ))
                .into())
            }
        };

        let graph = Arc::new(GraphClient::new(self.http.clone(), Arc::new(auth)));
        let engine = Arc::new(Engine::new(
            self.store.clone(),
            graph,
            self.config(),
            account_id.to_string(),
        ));
        self.install_placeholder_protector(&engine);

        // A crash mid-hydration leaves records claiming work that died with
        // the process; correct them before anything trusts the database.
        match engine.repair_states() {
            Ok(0) => {}
            Ok(n) => tracing::info!(repaired = n, "repaired stale hydration records"),
            Err(e) => tracing::warn!(error = %e, "could not repair states"),
        }

        self.spawn_event_forwarder(&engine);
        self.engines
            .write()
            .await
            .insert(account_id.to_string(), engine);
        Ok(())
    }

    pub async fn detach_account(&self, account_id: &str) {
        self.engines.write().await.remove(account_id);
    }

    /// Translate engine events into daemon status and front-end signals.
    ///
    /// The task owns a handle to the shared status so `Status()` over D-Bus
    /// reports the same thing the signals just announced; broadcasting alone
    /// would leave a UI that connects mid-transfer reading a stale value.
    fn spawn_event_forwarder(&self, engine: &Arc<Engine>) {
        let mut rx = engine.subscribe();
        let sender = self.events.clone();
        let shared = self.status.clone();
        let hydrations = self.hydrations.clone();
        let store = self.store.clone();
        let paused = self.paused.clone();
        let approve_mass_delete_on_resume = self.approve_mass_delete_on_resume.clone();

        tokio::spawn(async move {
            let mut current = SyncStatus::idle(0);
            let mut active_phases = 0u64;
            let mut cycle_failed = false;
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        match event {
                            SyncEvent::Started => {
                                if active_phases == 0 {
                                    cycle_failed = false;
                                    current.last_error.clear();
                                }
                                active_phases = active_phases.saturating_add(1);
                                current.state = "syncing".into();
                            }
                            SyncEvent::Enumerating { items, pages } => {
                                current.state = "enumerating".into();
                                current.current_item =
                                    format!("read {items} item(s) across {pages} page(s)");
                                current.transferred = items;
                                current.total = 0;
                            }
                            SyncEvent::Progress {
                                name,
                                transferred,
                                total,
                                direction,
                            } => {
                                current.state = match direction {
                                    Direction::Download => "downloading".into(),
                                    Direction::Upload => "uploading".into(),
                                };
                                current.current_item = name;
                                current.transferred = transferred;
                                current.total = total.unwrap_or(0);
                            }
                            SyncEvent::ItemDone {
                                name,
                                path,
                                direction,
                            } => {
                                current.current_item.clear();
                                current.transferred = 0;
                                current.total = 0;
                                record_activity(
                                    &store,
                                    match direction {
                                        Direction::Download => "downloaded",
                                        Direction::Upload => "uploaded",
                                    },
                                    name,
                                    onedrive_path(&store, &path),
                                    String::new(),
                                )
                                .await;
                            }
                            SyncEvent::Deleted { name, path } => {
                                record_activity(
                                    &store,
                                    "deleted",
                                    name,
                                    path.as_deref()
                                        .map(|path| onedrive_path(&store, path))
                                        .unwrap_or_default(),
                                    String::new(),
                                )
                                .await;
                            }
                            SyncEvent::Conflict {
                                original,
                                copy,
                                path,
                            } => {
                                record_activity(
                                    &store,
                                    "conflict",
                                    original.clone(),
                                    onedrive_path(&store, &path),
                                    copy.clone(),
                                )
                                .await;
                                let _ = sender.send(DaemonEvent::Conflict { original, copy });
                                continue;
                            }
                            SyncEvent::Failed { name, path, error } => {
                                cycle_failed = true;
                                current.last_error = error.clone();
                                record_activity(
                                    &store,
                                    "failed",
                                    name.clone(),
                                    path.as_deref()
                                        .map(|path| onedrive_path(&store, path))
                                        .unwrap_or_default(),
                                    error.clone(),
                                )
                                .await;
                                let _ = sender.send(DaemonEvent::ItemFailed { name, error });
                            }
                            SyncEvent::SafetyStop { reason, kind } => {
                                paused.store(true, Ordering::Release);
                                if kind == SafetyStopKind::MassDelete {
                                    approve_mass_delete_on_resume.store(true, Ordering::Release);
                                }
                                current.state = "paused".into();
                                current.last_error = reason.clone();
                                record_activity(
                                    &store,
                                    "safety-stop",
                                    reason.clone(),
                                    String::new(),
                                    String::new(),
                                )
                                .await;
                                let _ = sender.send(DaemonEvent::SafetyStop { reason });
                            }
                            SyncEvent::HydrationStarted { path, name, size } => {
                                let hydration = Hydration {
                                    path: path.to_string_lossy().into_owned(),
                                    name,
                                    transferred: 0,
                                    total: size,
                                };
                                if let Some(entry) = hydrations.write().await.get_mut(&path) {
                                    entry.hydration = hydration.clone();
                                }
                                let _ = sender.send(DaemonEvent::HydrationStarted(hydration));
                                continue;
                            }
                            SyncEvent::HydrationProgress {
                                path,
                                transferred,
                                total,
                            } => {
                                let mut guard = hydrations.write().await;
                                let Some(entry) = guard.get_mut(&path) else {
                                    continue;
                                };
                                entry.hydration.transferred = transferred;
                                entry.hydration.total = total;
                                let hydration = entry.hydration.clone();
                                drop(guard);
                                let _ = sender.send(DaemonEvent::HydrationProgress(hydration));
                                continue;
                            }
                            SyncEvent::HydrationFinished { path, ok } => {
                                // `finish_hydration`, `fail_hydration`, and
                                // `restore_mtime` own the terminal handshake
                                // and keep the path guard until their work is
                                // complete. Do not let this asynchronous event
                                // task drop a newer request's guard midway
                                // through that work. The fallback removal is
                                // only for an unexpected engine event with no
                                // owner in the daemon map.
                                let mut hydration_guard = hydrations.write().await;
                                let owned = hydration_guard
                                    .get(&path)
                                    .is_some_and(|entry| entry.finishing);
                                if !owned {
                                    hydration_guard.remove(&path);
                                }
                                drop(hydration_guard);
                                // On-demand fetches belong in the activity list
                                // just like scheduled downloads: to the user
                                // both are "this file arrived".
                                if ok {
                                    let name = path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned();
                                    let activity_path = onedrive_path(&store, &path);
                                    record_activity(
                                        &store,
                                        "downloaded",
                                        name,
                                        activity_path,
                                        "按需".into(),
                                    )
                                    .await;
                                }
                                let _ = sender.send(DaemonEvent::HydrationFinished {
                                    path: path.to_string_lossy().into_owned(),
                                    ok,
                                });
                                continue;
                            }
                            SyncEvent::Skipped { name, reason } => {
                                tracing::info!(%name, %reason, "skipped");
                                continue;
                            }
                            SyncEvent::Finished(summary) => {
                                cycle_failed |= summary.failed > 0;
                                active_phases = active_phases.saturating_sub(1);
                                // Delta enumeration by itself is routine and
                                // happens every poll. Keep info logs for real
                                // outcomes; an idle trace remains available
                                // when RUST_LOG explicitly enables debug.
                                if summary_has_reportable_activity(&summary) {
                                    tracing::info!(
                                        downloaded = summary.downloaded,
                                        uploaded = summary.uploaded,
                                        deleted_local = summary.deleted_local,
                                        deleted_remote = summary.deleted_remote,
                                        conflicts = summary.conflicts,
                                        skipped = summary.skipped,
                                        failed = summary.failed,
                                        refused_deletions = summary.refused_deletions,
                                        queued = summary.still_queued,
                                        "sync pass finished"
                                    );
                                } else {
                                    tracing::debug!(
                                        enumerated = summary.enumerated,
                                        not_downloaded = summary.not_downloaded,
                                        queued = summary.still_queued,
                                        "sync pass finished without changes"
                                    );
                                }
                                current.state = phase_completion_state(
                                    paused.load(Ordering::Acquire),
                                    active_phases,
                                    cycle_failed,
                                )
                                .into();
                                if let Err(error) = update_pending_status(&store, &mut current) {
                                    tracing::warn!(%error, "could not read the durable queue snapshot");
                                    current.pending = summary.still_queued;
                                    current.pending_items.clear();
                                }
                                if active_phases == 0 {
                                    current.current_item.clear();
                                    current.transferred = 0;
                                    current.total = 0;
                                }
                            }
                        }
                        {
                            let mut shared = shared.write().await;
                            // A finished transfer knows nothing about the
                            // account being unreachable. Declaring "idle" over
                            // that says "nothing to do, all is well", which is
                            // exactly what is false — the tray then showed
                            // idle while the daemon was offline. The
                            // coordinator sets these states and clears them
                            // when the account works again; leave them alone.
                            if current.state == "idle" && is_account_level_state(&shared.state) {
                                current.state.clone_from(&shared.state);
                                if current.last_error.is_empty() {
                                    current.last_error.clone_from(&shared.last_error);
                                }
                            }
                            *shared = current.clone();
                        }
                        let _ = sender.send(DaemonEvent::Status(current.clone()));
                    }
                    // Lagged: the UI missed some events, which is harmless —
                    // the next status message carries the current truth.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    pub async fn status(&self) -> SyncStatus {
        let mut status = self.status.read().await.clone();
        if let Err(error) = update_pending_status(&self.store, &mut status) {
            tracing::warn!(%error, "could not refresh the durable queue for Status");
        }
        status
    }

    /// Publish the authoritative durable queue after a direct UI mutation.
    /// These changes can happen while the coordinator is paused, when no
    /// engine phase will emit a `Finished` event to refresh the shared status.
    async fn refresh_pending_status(&self) {
        let pending = pending_snapshot(&self.store);
        let mut status = self.status.write().await;
        match pending {
            Ok(snapshot) => apply_pending_snapshot(&mut status, snapshot),
            Err(error) => {
                tracing::warn!(%error, "could not publish the durable queue snapshot");
                return;
            }
        }
        let snapshot = status.clone();
        drop(status);
        self.emit(DaemonEvent::Status(snapshot));
    }

    pub fn request_sync(&self) -> u64 {
        let generation = self.next_sync_generation.fetch_add(1, Ordering::Relaxed);
        self.requested_sync_generation
            .fetch_max(generation, Ordering::Release);
        self.wake.notify_one();
        generation
    }

    async fn set_state(&self, state: &str, error: Option<String>) {
        let pending = pending_snapshot(&self.store);
        let mut status = self.status.write().await;
        status.state = state.to_string();
        if let Some(error) = error {
            status.last_error = error;
        }
        match pending {
            Ok(snapshot) => apply_pending_snapshot(&mut status, snapshot),
            Err(error) => {
                tracing::warn!(%error, "could not refresh the durable queue while changing state");
            }
        }
        let snapshot = status.clone();
        drop(status);
        self.emit(DaemonEvent::Status(snapshot));
    }

    async fn report_core_error(&self, error: &Error) -> String {
        let (state, message) = core_error_status(error);
        self.set_state(state, Some(message.clone())).await;
        message
    }

    async fn report_anyhow_error(&self, error: &anyhow::Error) -> String {
        if let Some(error) = error.downcast_ref::<Error>() {
            self.report_core_error(error).await
        } else {
            let message = format!("{error:#}");
            self.set_state("error", Some(message.clone())).await;
            message
        }
    }

    /// Pause or resume syncing.
    ///
    /// The flag and the reported state are updated together: a UI that pauses
    /// and immediately reads `Status()` must not be told the daemon is idle,
    /// because "idle" and "paused" mean very different things to someone
    /// waiting for their file to upload.
    pub async fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        if paused {
            self.set_state("paused", None).await;
        } else {
            if self
                .approve_mass_delete_on_resume
                .swap(false, Ordering::AcqRel)
            {
                for engine in self.engines.read().await.values() {
                    engine.approve_next_mass_delete();
                }
                self.force_full_scan.store(true, Ordering::Release);
            }
            self.set_state("idle", None).await;
            self.wake.notify_one();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub async fn accounts(&self) -> Result<Vec<AccountInfo>> {
        Ok(self
            .store
            .accounts()?
            .into_iter()
            .map(|a| AccountInfo {
                id: a.id,
                display_name: a.display_name.unwrap_or_default(),
                address: a.address.unwrap_or_default(),
                root_path: a.root_local_path.to_string_lossy().into_owned(),
                quota_total: a.quota_total.unwrap_or(0),
                quota_used: a.quota_used.unwrap_or(0),
            })
            .collect())
    }

    pub fn notify_login_completed(&self, account_id: &str) {
        self.emit(DaemonEvent::LoginCompleted(account_id.to_string()));
    }

    pub fn notify_login_failed(&self, message: &str) {
        self.emit(DaemonEvent::LoginFailed(message.to_string()));
    }

    /// Work through queued transfers only, skipping enumeration.
    pub async fn transfer_all(&self) -> Result<()> {
        let _download_gate = self.download_gate.lock().await;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if let Err(e) = engine.transfer_only().await {
                tracing::error!(error = %e, "transfer pass failed");
                self.report_core_error(&e).await;
            }
        }
        Ok(())
    }

    /// One pass over every account. Returns whether anything actually moved.
    pub async fn sync_all(&self) -> bool {
        // The one-shot command can run beside a helper callback when a user
        // invokes it from a file manager.  Keep its queued content transfer
        // from racing a simultaneous release; remote enumeration itself is
        // still cheap and remains outside this gate in the long-running
        // coordinator.
        let _download_gate = self.download_gate.lock().await;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut did_work = false;

        for engine in &engines {
            match engine.sync_once().await {
                Ok(summary) => {
                    did_work |= summary.downloaded > 0
                        || summary.uploaded > 0
                        || summary.deleted_local > 0
                        || summary.deleted_remote > 0
                        || summary.still_queued > 0;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sync pass failed");
                    self.report_core_error(&e).await;
                }
            }
        }
        if let Err(error) = self.register_pending_placeholders(&engines).await {
            tracing::error!(%error, "could not register new placeholders");
            self.set_state("error", Some(error.to_string())).await;
        }
        did_work
    }

    /// Storage-sense pass: give space back when the disk runs low.
    ///
    /// Skipped entirely without the helper: an automatic action must never
    /// create placeholders that nothing will fill.
    async fn auto_free_if_low(&self) {
        let threshold = self.config().auto_free_below_bytes;
        if threshold == 0 {
            return;
        }
        let Some(hsm) = self.hsm.read().await.clone() else {
            return;
        };
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            let candidates = match engine.auto_free_candidates(threshold) {
                Ok(candidates) => candidates,
                Err(error) => {
                    tracing::warn!(%error, "could not plan auto-free");
                    continue;
                }
            };
            let _release_barrier = self.hydration_locks.begin_release(&candidates);
            {
                let _path_guards = self.hydration_locks.acquire_many(&candidates).await;
                let mut protected = Vec::new();
                let mut protection_failed = false;
                for path in &candidates {
                    match hsm.watch(path).await {
                        Ok(()) => protected.push(path.clone()),
                        Err(error) => {
                            tracing::error!(path = %path.display(), %error, "auto-free mark failed");
                            if let Err(lock_error) = self.lock_unwatched_placeholder(path).await {
                                tracing::error!(
                                    path = %path.display(),
                                    error = %lock_error,
                                    "could not safety-lock an unprotected auto-free target"
                                );
                            }
                            protection_failed = true;
                            break;
                        }
                    }
                }
                if protection_failed {
                    for path in protected {
                        let _ = hsm.unwatch(&path).await;
                    }
                } else {
                    let free_result = {
                        let _download_gate = self.download_gate.lock().await;
                        engine.auto_free(threshold)
                    };

                    // A queued fast-path event may have retired a mark while
                    // the file was still materialised. Re-register the final
                    // dehydrated set before allowing new events through.
                    let dehydrated: Vec<_> = candidates
                        .iter()
                        .filter(|path| {
                            gonedrive_core::hydrate::Placeholder::state(path)
                                == gonedrive_core::hydrate::PlaceholderState::Dehydrated
                        })
                        .cloned()
                        .collect();
                    let protection_error = if dehydrated.is_empty() {
                        None
                    } else {
                        self.protect_placeholders(&hsm, &dehydrated).await.err()
                    };

                    match free_result {
                        Ok(outcome) if outcome.files > 0 => {
                            tracing::info!(
                                files = outcome.files,
                                freed = outcome.freed,
                                "auto-released space"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "auto-free failed"),
                    }
                    if let Some(error) = protection_error {
                        tracing::error!(%error, "could not re-protect placeholders after auto-free");
                    }
                    for path in &candidates {
                        if gonedrive_core::hydrate::Placeholder::state(path)
                            != gonedrive_core::hydrate::PlaceholderState::Dehydrated
                        {
                            let _ = hsm.unwatch(path).await;
                        }
                    }
                }
            }
        }
    }
}

fn phase_state(paused: bool, active_phases: u64) -> &'static str {
    if paused {
        "paused"
    } else if active_phases == 0 {
        "idle"
    } else {
        "syncing"
    }
}

fn summary_has_reportable_activity(summary: &SyncSummary) -> bool {
    summary.downloaded > 0
        || summary.uploaded > 0
        || summary.deleted_local > 0
        || summary.deleted_remote > 0
        || summary.conflicts > 0
        || summary.skipped > 0
        || summary.failed > 0
        || summary.refused_deletions > 0
}

fn phase_completion_state(paused: bool, active_phases: u64, cycle_failed: bool) -> &'static str {
    if !paused && active_phases == 0 && cycle_failed {
        "error"
    } else {
        phase_state(paused, active_phases)
    }
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("GOnedrive/", env!("CARGO_PKG_VERSION")))
        // Large uploads legitimately take a long time, so there is no overall
        // request deadline; the connect phase gets a short one.
        .connect_timeout(Duration::from_secs(20))
        // But a *stalled* stream must not hang the client for minutes. This is
        // the per-read deadline: a body (or upload response) that goes silent
        // for this long — a signal drop on a train leaving the connection
        // half-open — fails fast and is retried, instead of blocking a sync
        // pass, or a file-open waiting on hydration, until the OS gives up on
        // the dead socket. It bounds each read, not the whole transfer, so a
        // slow-but-alive link still completes.
        .read_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("could not build the HTTP client")
}

/// How many finished items the activity list keeps.
const ACTIVITY_LIMIT: usize = 100;

async fn record_activity(store: &Store, kind: &str, name: String, path: String, detail: String) {
    let when = time::OffsetDateTime::now_utc().unix_timestamp();
    if let Err(e) = store.push_activity(kind, &name, &path, &detail, when, ACTIVITY_LIMIT as i64) {
        tracing::debug!(error = %e, "could not record activity");
    }
}

/// Read one coherent preview of the durable queue. A phase summary describes
/// only that phase and can be stale when pull/local workers overlap; SQLite is
/// the authority for both the count and the paths shown to the user.
fn pending_snapshot(store: &Store) -> gonedrive_core::Result<(u64, Vec<PendingItem>)> {
    const PREVIEW_LIMIT: usize = 5;

    let accounts = store.accounts()?;
    let mut total = 0u64;
    let mut preview = Vec::new();
    for account in &accounts {
        total = total.saturating_add(store.pending_count(&account.id)?);
        if preview.len() >= PREVIEW_LIMIT {
            continue;
        }
        for op in store.pending_ops(&account.id, PREVIEW_LIMIT - preview.len())? {
            let path = op
                .local_path
                .clone()
                .or_else(|| {
                    op.item_id
                        .as_deref()
                        .and_then(|id| store.local_path(account, id).ok().flatten())
                })
                .map(|path| onedrive_path(store, &path))
                .unwrap_or_else(|| {
                    op.item_id
                        .as_deref()
                        .and_then(|id| store.item(id).ok().flatten())
                        .map(|item| format!("OneDrive/{}", item.name))
                        .unwrap_or_else(|| "OneDrive/（路径不可用）".into())
                });
            preview.push(PendingItem {
                kind: op.kind.as_str().into(),
                path,
                attempts: op.attempts,
                retry_at: op.next_attempt_at,
                last_error: op.last_error.unwrap_or_default(),
            });
        }
    }
    Ok((total, preview))
}

fn update_pending_status(store: &Store, status: &mut SyncStatus) -> gonedrive_core::Result<()> {
    let snapshot = pending_snapshot(store)?;
    apply_pending_snapshot(status, snapshot);
    Ok(())
}

fn apply_pending_snapshot(status: &mut SyncStatus, (pending, items): (u64, Vec<PendingItem>)) {
    status.pending = pending;
    status.pending_items = items;
}

fn onedrive_path(store: &Store, path: &std::path::Path) -> String {
    for account in store.accounts().unwrap_or_default() {
        if let Ok(relative) = path.strip_prefix(&account.root_local_path) {
            if relative.as_os_str().is_empty() {
                return "OneDrive".into();
            }
            return format!("OneDrive/{}", relative.display());
        }
    }
    path.display().to_string()
}

/// Processes that read files in order to *describe* them — type sniffing,
/// previews — rather than to use them. Their reads are always served as
/// ranges and never change a file's state: browsing must have no side
/// effects, at any file size.
/// Bring up the one progress window, if it is not already up.
///
/// The GApplication id is unique, so even a rare double-spawn folds into one
/// window — but tracking the child avoids the race entirely (a burst of
/// hydrations once stacked three windows) and lets it be reaped rather than
/// left defunct.
fn ensure_progress_window(monitor: &Arc<std::sync::Mutex<Option<std::process::Child>>>) {
    let Ok(mut slot) = monitor.lock() else {
        return;
    };
    if let Some(child) = slot.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            return; // still running
        }
    }
    let Some(executable) = std::env::current_exe()
        .ok()
        .map(|path| path.with_file_name("gonedrive"))
    else {
        return;
    };
    let spawned = std::process::Command::new(executable)
        .arg("--hydration-monitor")
        .spawn();
    match spawned {
        Ok(child) => *slot = Some(child),
        Err(e) => tracing::debug!(error = %e, "no progress window available"),
    }
}

#[cfg(test)]
mod tests;
