//! The daemon supervisor: owns the engines, the poll loop and the status.

mod coordinator;
mod hydration;

#[cfg(test)]
use coordinator::{outbound_scope, sync_barrier_ready};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_core::config::Config;
use gonedrive_core::db::Store;
use gonedrive_core::graph::GraphClient;
use gonedrive_core::sync::engine::{Direction, Engine, SafetyStopKind, SyncEvent};
use gonedrive_core::{Authenticator, Error, TokenStore};
use gonedrive_ipc::reader_policy::{is_browsing_reader, is_bulk_copier};
use gonedrive_ipc::{AccountInfo, Hydration, SyncRequestResult, SyncStatus};

/// A head-read at or below this is served as a range, not a download: it is the
/// signature of content sniffing, and even when it is not, filling just the
/// window is always correct — the watch stays armed for the rest.
const PARTIAL_HYDRATION_CEILING: u64 = 256 * 1024;
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
}

/// The staging policy, as a pure function of the request.
///
/// - On open: a sparse-aware bulk copier is hydrated fully (it would otherwise
///   SEEK over the holes and copy zeros); everything else is allowed through.
/// - A browser/thumbnailer never touches the network: local-or-offline.
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
) -> StagePlan {
    if is_open {
        return if is_bulk_copier(reader) {
            StagePlan::Full
        } else {
            StagePlan::AllowOpen
        };
    }
    if is_browsing_reader(reader) {
        return StagePlan::BrowseLocal {
            offset,
            length: length.max(1),
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

fn core_error_status(error: &Error) -> (&'static str, String) {
    match error {
        Error::AuthRequired(message) => ("signed_out", message.clone()),
        Error::KeyringLocked(_) => ("keyring_locked", KEYRING_LOCKED_MESSAGE.into()),
        error if error.is_transient() => ("offline", error.to_string()),
        error => ("error", error.to_string()),
    }
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
    hydration: Hydration,
    /// Set to abandon the transfer.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    staged: Option<std::path::PathBuf>,
    /// Held from staging until the helper reports Filled, so no second reader
    /// can truncate or consume the same sibling staging file.
    _path_guard: OwnedMutexGuard<()>,
}

#[derive(Default)]
struct PathLocks {
    locks: Mutex<HashMap<std::path::PathBuf, Weak<Mutex<()>>>>,
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
}

pub struct App {
    pub config: Arc<Config>,
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

    /// The privileged helper, when files-on-demand is available. Without it a
    /// placeholder is just a file full of zeroes that nothing will ever fill.
    hsm: RwLock<Option<Arc<crate::hsm_client::HsmClient>>>,
    /// The single progress window, if one is up. Guarded so a burst of
    /// hydrations does not stack three windows (as it did once). `Arc` so a
    /// spawned task can hold it without borrowing the whole `App`.
    monitor: Arc<std::sync::Mutex<Option<std::process::Child>>>,
    events: broadcast::Sender<DaemonEvent>,
}

impl App {
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

        let app = Arc::new(Self {
            config: Arc::new(config),
            store,
            tokens,
            http: build_http_client()?,
            engines: RwLock::new(HashMap::new()),
            status: Arc::new(RwLock::new(SyncStatus::idle(0))),
            paused: Arc::new(AtomicBool::new(false)),
            approve_mass_delete_on_resume: Arc::new(AtomicBool::new(false)),
            force_full_scan: AtomicBool::new(false),
            wake: Arc::new(Notify::new()),
            next_sync_generation: AtomicU64::new(1),
            requested_sync_generation: AtomicU64::new(0),
            hydrations: Arc::new(RwLock::new(HashMap::new())),
            hydration_locks: PathLocks::default(),
            hsm: RwLock::new(None),
            monitor: Arc::new(std::sync::Mutex::new(None)),
            events,
        });
        Ok(app)
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
    pub async fn load_accounts(&self) -> Result<usize> {
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
                    tracing::error!(account = %account.id, error = %e, "could not sign in");
                    self.report_anyhow_error(&e).await;
                }
            }
        }
        Ok(loaded)
    }

    /// Retry accounts that exist in the database but could not be attached at
    /// startup, most commonly because the login keyring was locked. Called by
    /// an explicit SyncNow after the user has unlocked it.
    pub(super) async fn attach_missing_accounts(&self) -> Result<usize> {
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
    pub async fn attach_account(&self, account_id: &str) -> Result<()> {
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

        let auth = Authenticator::restore(
            self.http.clone(),
            self.config.client_id.clone(),
            account_id.to_string(),
            self.tokens.clone(),
        )
        .await?;

        let graph = Arc::new(GraphClient::new(self.http.clone(), Arc::new(auth)));
        let engine = Arc::new(Engine::new(
            self.store.clone(),
            graph,
            self.config.clone(),
            account_id.to_string(),
        ));

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
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        match event {
                            SyncEvent::Started => {
                                active_phases = active_phases.saturating_add(1);
                                current.state = "syncing".into();
                                current.last_error.clear();
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
                            SyncEvent::ItemDone { name, direction } => {
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
                                    String::new(),
                                )
                                .await;
                            }
                            SyncEvent::Deleted { name } => {
                                record_activity(&store, "deleted", name, String::new()).await;
                            }
                            SyncEvent::Conflict { original, copy } => {
                                record_activity(&store, "conflict", original.clone(), copy.clone())
                                    .await;
                                let _ = sender.send(DaemonEvent::Conflict { original, copy });
                                continue;
                            }
                            SyncEvent::Failed { name, error } => {
                                current.last_error = error.clone();
                                record_activity(&store, "failed", name.clone(), error.clone())
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
                                hydrations.write().await.remove(&path);
                                // On-demand fetches belong in the activity list
                                // just like scheduled downloads: to the user
                                // both are "this file arrived".
                                if ok {
                                    let name = path
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned();
                                    record_activity(&store, "downloaded", name, "按需".into())
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
                                active_phases = active_phases.saturating_sub(1);
                                // One line per pass with every counter: the
                                // log alone should answer "what did that sync
                                // just do", without anyone reproducing it.
                                if summary != Default::default() {
                                    tracing::info!(
                                        downloaded = summary.downloaded,
                                        uploaded = summary.uploaded,
                                        deleted_local = summary.deleted_local,
                                        deleted_remote = summary.deleted_remote,
                                        conflicts = summary.conflicts,
                                        skipped = summary.skipped,
                                        queued = summary.still_queued,
                                        "sync pass finished"
                                    );
                                }
                                current.state =
                                    phase_state(paused.load(Ordering::Acquire), active_phases)
                                        .into();
                                current.pending = summary.still_queued;
                                if active_phases == 0 {
                                    current.current_item.clear();
                                    current.transferred = 0;
                                    current.total = 0;
                                }
                            }
                        }
                        *shared.write().await = current.clone();
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
        self.status.read().await.clone()
    }

    pub fn request_sync(&self) -> u64 {
        let generation = self.next_sync_generation.fetch_add(1, Ordering::Relaxed);
        self.requested_sync_generation
            .fetch_max(generation, Ordering::Release);
        self.wake.notify_one();
        generation
    }

    async fn set_state(&self, state: &str, error: Option<String>) {
        let mut status = self.status.write().await;
        status.state = state.to_string();
        if let Some(error) = error {
            status.last_error = error;
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
            let message = error.to_string();
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
        let threshold = self.config.auto_free_below_bytes;
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
            let mut protected = Vec::new();
            let mut protection_failed = false;
            for path in &candidates {
                match hsm.watch(path).await {
                    Ok(()) => protected.push(path.clone()),
                    Err(error) => {
                        tracing::error!(path = %path.display(), %error, "auto-free mark failed");
                        protection_failed = true;
                        break;
                    }
                }
            }
            if protection_failed {
                for path in protected {
                    let _ = hsm.unwatch(&path).await;
                }
                continue;
            }
            match engine.auto_free(threshold) {
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
            for path in candidates {
                if gonedrive_core::hydrate::Placeholder::state(&path)
                    != gonedrive_core::hydrate::PlaceholderState::Dehydrated
                {
                    let _ = hsm.unwatch(&path).await;
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

async fn record_activity(store: &Store, kind: &str, name: String, detail: String) {
    let when = time::OffsetDateTime::now_utc().unix_timestamp();
    if let Err(e) = store.push_activity(kind, &name, &detail, when, ACTIVITY_LIMIT as i64) {
        tracing::debug!(error = %e, "could not record activity");
    }
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
