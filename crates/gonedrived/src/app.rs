//! The daemon supervisor: owns the engines, the poll loop and the status.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_core::config::Config;
use gonedrive_core::db::Store;
use gonedrive_core::graph::GraphClient;
use gonedrive_core::sync::engine::{Direction, Engine, SyncEvent};
use gonedrive_core::{Authenticator, Error, TokenStore};
use gonedrive_ipc::{AccountInfo, Activity, Hydration, SyncStatus};
use tokio::sync::{broadcast, Notify, RwLock};

/// How much the poll interval grows after a pass that found nothing.
const IDLE_BACKOFF: f64 = 1.5;

/// Anything the front ends care about.
#[derive(Debug, Clone)]
pub enum DaemonEvent {
    Status(SyncStatus),
    LoginCompleted(String),
    LoginFailed(String),
    Conflict { original: String, copy: String },
    ItemFailed { name: String, error: String },
    HydrationStarted(Hydration),
    HydrationProgress(Hydration),
    HydrationFinished { path: String, ok: bool },
}

/// A placeholder currently being filled.
struct InFlight {
    hydration: Hydration,
    /// Set to abandon the transfer.
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

pub struct App {
    pub config: Arc<Config>,
    pub store: Store,
    pub tokens: Arc<TokenStore>,
    pub http: reqwest::Client,
    engines: RwLock<HashMap<String, Arc<Engine>>>,
    status: Arc<RwLock<SyncStatus>>,
    paused: AtomicBool,
    /// Wakes the poll loop early: a local change, or an explicit SyncNow.
    pub wake: Arc<Notify>,
    /// Placeholders being filled right now, keyed by absolute path.
    hydrations: Arc<RwLock<HashMap<std::path::PathBuf, InFlight>>>,
    /// Recently finished work, newest first. Bounded, because this is a
    /// convenience for the user rather than an audit log.
    activity: Arc<RwLock<std::collections::VecDeque<Activity>>>,

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
            paused: AtomicBool::new(false),
            wake: Arc::new(Notify::new()),
            hydrations: Arc::new(RwLock::new(HashMap::new())),
            activity: Arc::new(RwLock::new(std::collections::VecDeque::new())),
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

    /// Bring up an engine for every account already in the database.
    ///
    /// An account whose refresh token no longer works is reported and skipped
    /// rather than failing startup — with several accounts configured, one
    /// revoked grant must not take the others down.
    pub async fn load_accounts(&self) -> Result<usize> {
        let mut loaded = 0;
        for account in self.store.accounts()? {
            match self.attach_account(&account.id).await {
                Ok(()) => loaded += 1,
                Err(e) => {
                    tracing::error!(account = %account.id, error = %e, "could not sign in");
                    self.set_state("signed_out", Some(e.to_string())).await;
                }
            }
        }
        Ok(loaded)
    }

    /// Build and register the engine for one account.
    pub async fn attach_account(&self, account_id: &str) -> Result<()> {
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
        let activity = self.activity.clone();

        tokio::spawn(async move {
            let mut current = SyncStatus::idle(0);
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        match event {
                            SyncEvent::Started => {
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
                                    &activity,
                                    match direction {
                                        Direction::Download => "downloaded",
                                        Direction::Upload => "uploaded",
                                    },
                                    name,
                                    String::new(),
                                )
                                .await;
                            }
                            SyncEvent::Conflict { original, copy } => {
                                record_activity(
                                    &activity,
                                    "conflict",
                                    original.clone(),
                                    copy.clone(),
                                )
                                .await;
                                let _ = sender.send(DaemonEvent::Conflict { original, copy });
                                continue;
                            }
                            SyncEvent::Failed { name, error } => {
                                current.last_error = error.clone();
                                record_activity(&activity, "failed", name.clone(), error.clone())
                                    .await;
                                let _ = sender.send(DaemonEvent::ItemFailed { name, error });
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
                                    record_activity(&activity, "downloaded", name, "按需".into())
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
                                current.state = "idle".into();
                                current.pending = summary.still_queued;
                                current.current_item.clear();
                                current.transferred = 0;
                                current.total = 0;
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

    /// A head-read this small is served as a range, not a download: it is the
    /// signature of content sniffing, and even when it is not, filling just
    /// the window is always correct — the watch stays armed for the rest.
    const PARTIAL_HYDRATION_CEILING: u64 = 256 * 1024;

    /// Stage what a blocked reader needs. Returns the staged path, the
    /// placement offset, and whether it completes the file.
    ///
    /// Two rules decide, in order:
    ///
    /// * **Who is reading.** A file manager reads files to *describe* them,
    ///   not to use them; whatever it asks for is served as a range and the
    ///   file's state never changes. Without this, looking at a folder of
    ///   small files flips every one of them to "downloaded" — silently
    ///   reversing the user's explicit online-only choice, which is precisely
    ///   the complaint that shaped this function.
    /// * **How much is asked.** For everyone else, a small head-read is served
    ///   as a range; anything more is a real consumer and hydrates the file,
    ///   the way Windows does it.
    pub async fn stage_for(
        &self,
        path: &std::path::Path,
        offset: u64,
        length: u64,
        reader: &str,
        is_open: bool,
        allow_partial: bool,
    ) -> Result<Option<(std::path::PathBuf, u64, bool)>> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let browsing = is_browsing_reader(reader);

        if is_open {
            // The read has not begun. A sparse-aware copier will now SEEK over
            // the holes and copy zeros unless the file is already whole, so
            // those tools are hydrated fully here. Everyone else is allowed
            // through untouched and their actual reads drive hydration — this
            // keeps a media player streaming a huge file instead of pulling it
            // all down just to read its header.
            if is_bulk_copier(reader) {
                let staged = self.stage_content(path).await?;
                return Ok(Some((staged, 0, true)));
            }
            return Ok(None); // allow the open; reads handle the rest
        }

        if browsing {
            // Browsing must never touch the network: opening a folder makes the
            // file manager sniff the head of every file for its type, and a
            // cloud fetch per file is exactly the "opening a folder loads for a
            // long time" the user reported. Serve the window only if it is
            // already on disk from a real earlier read; otherwise let the read
            // proceed against the sparse file (zeros) and answer offline. The
            // file's type comes from its name, as on Windows.
            let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
            if let Some(engine) = engines.first() {
                if let Ok(Some(staged)) =
                    engine.stage_range_if_local(path, offset, length.max(1)).await
                {
                    return Ok(Some((staged, offset, false)));
                }
            }
            return Ok(None); // allow the read with no fill, no network
        }

        // A consumer: only the very first head-read is a range. Coming back for
        // offset > 0 is sequential reading (`cat`, `cp`, a hash), and serving
        // 32 KiB at a time would be dozens of round-trips, so it escalates to
        // full hydration.
        let partial = allow_partial
            && length > 0
            && offset == 0
            && length <= Self::PARTIAL_HYDRATION_CEILING
            && length < size;

        if partial {
            let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
            let engine = engines
                .first()
                .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?;
            let length = length.min(size.saturating_sub(offset)).max(1);
            let staged = engine.stage_range(path, offset, length).await?;
            return Ok(Some((staged, offset, false)));
        }
        let staged = self.stage_content(path).await?;
        Ok(Some((staged, 0, true)))
    }

    /// Download a placeholder's content to a staging file for the helper.
    pub async fn stage_content(&self, path: &std::path::Path) -> Result<std::path::PathBuf> {
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.hydrations.write().await.insert(
            path.to_path_buf(),
            InFlight {
                hydration: Hydration {
                    path: path.to_string_lossy().into_owned(),
                    name: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    transferred: 0,
                    total: 0,
                },
                cancel: cancel.clone(),
            },
        );
        self.show_progress_soon(path.to_path_buf());

        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut last = None;
        for engine in engines {
            match engine.stage_content_cancellable(path, cancel.clone()).await {
                Ok(staged) => return Ok(staged),
                Err(Error::Other(message)) => last = Some(message),
                Err(e) => {
                    self.hydrations.write().await.remove(path);
                    return Err(e.into());
                }
            }
        }
        self.hydrations.write().await.remove(path);
        anyhow::bail!(
            "no account owns {}{}",
            path.display(),
            last.map(|m| format!(" ({m})")).unwrap_or_default()
        )
    }

    /// What was synced recently, newest first.
    pub async fn recent_activity(&self) -> Vec<Activity> {
        self.activity.read().await.iter().cloned().collect()
    }

    /// Placeholders being filled right now.
    pub async fn active_hydrations(&self) -> Vec<Hydration> {
        self.hydrations
            .read()
            .await
            .values()
            .map(|entry| entry.hydration.clone())
            .collect()
    }

    /// Abandon a hydration. The blocked reader gets an I/O error.
    pub async fn cancel_hydration(&self, path: &std::path::Path) -> bool {
        match self.hydrations.read().await.get(path) {
            Some(entry) => {
                entry.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Put a progress window on screen if this hydration is still running a
    /// moment from now.
    ///
    /// The delay is what keeps small files from flashing a window open and
    /// shut: most hydrations finish before anyone could read the title.
    fn show_progress_soon(&self, path: std::path::PathBuf) {
        const DELAY: Duration = Duration::from_millis(400);
        let hydrations = self.hydrations.clone();

        let monitor = self.monitor.clone();
        tokio::spawn(async move {
            tokio::time::sleep(DELAY).await;
            if !hydrations.read().await.contains_key(&path) {
                return; // finished before anyone would have noticed
            }
            ensure_progress_window(&monitor);
        });
    }

    /// Record that the helper finished filling a placeholder.
    pub async fn finish_hydration(&self, path: &std::path::Path) -> Result<()> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            engine.finish_hydration(path)?;
        }
        Ok(())
    }

    /// Fill a placeholder directly (FetchNow, the CLI, the context menu).
    ///
    /// The file may be under a kernel watch, and the daemon's writes are not
    /// exempt from it — writing into the holes would fire the very hook that
    /// asks us to hydrate, and a second download would race this one over the
    /// same bytes. So the watch comes off first; it goes back on only if the
    /// fill fails and the file is still empty.
    pub async fn hydrate(&self, path: &std::path::Path) -> Result<u64> {
        let hsm = self.hsm.read().await.clone();
        if let Some(hsm) = &hsm {
            let _ = hsm.unwatch(path).await;
        }

        let result = self.hydrate_inner(path).await;

        if result.is_err() {
            if let Some(hsm) = &hsm {
                if gonedrive_core::hydrate::Placeholder::state(path)
                    == gonedrive_core::hydrate::PlaceholderState::Dehydrated
                {
                    let _ = hsm.watch(path).await;
                }
            }
        }
        result
    }

    async fn hydrate_inner(&self, path: &std::path::Path) -> Result<u64> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        // The single-account case is the real one; report its real error
        // rather than collapsing everything into "no account owns this".
        if engines.len() == 1 {
            return Ok(engines[0].hydrate(path).await?);
        }
        for engine in engines {
            match engine.hydrate(path).await {
                Ok(bytes) => return Ok(bytes),
                Err(Error::Other(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("no account owns {}", path.display())
    }

    pub async fn set_hsm(&self, client: Option<Arc<crate::hsm_client::HsmClient>>) {
        *self.hsm.write().await = client;
    }

    /// Present the whole drive as a browsable placeholder tree.
    ///
    /// Runs on a blocking thread — it can touch a hundred thousand files — and
    /// registers each placeholder with the helper as it goes. Safe to call
    /// repeatedly: existing files are left untouched.
    pub async fn materialize_tree(self: &Arc<Self>) -> Result<u64> {
        let hsm = self.require_hsm().await?;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?
            .clone();

        tracing::info!("materialising the drive as a placeholder tree");
        let paths = tokio::task::spawn_blocking(move || {
            engine.materialize_all(|done, total| {
                if done % 5000 == 0 || done == total {
                    tracing::info!(done, total, "materialising");
                }
                true
            })
        })
        .await??;

        let count = paths.len() as u64;
        for path in paths {
            let _ = hsm.watch(&path).await;
        }
        tracing::info!(placeholders = count, "placeholder tree ready");
        Ok(count)
    }

    /// Turn a downloaded subtree back into placeholders.
    ///
    /// Refused outright without the helper. A placeholder nothing is watching
    /// reads back as zeroes — silently, with no error — and an application that
    /// opens one and saves it destroys the real content and uploads the
    /// wreckage. Refusing to free space is a far better outcome than that.
    pub async fn free_space(
        &self,
        relative: &std::path::Path,
    ) -> Result<gonedrive_core::sync::FreeSpaceOutcome> {
        let hsm = self.require_hsm().await?;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?;

        // Watches come off before anything is written. Dehydration writes into
        // the files it is freeing, and a write into a watched file blocks on a
        // question addressed to this very process — with a helper that does
        // not recognise our pid, that is a deadlock that takes the D-Bus
        // interface and every waiting file manager thread down with it.
        for path in engine.subtree_local_files(relative)? {
            let _ = hsm.unwatch(&path).await;
        }

        let outcome = engine.free_space(relative)?;
        // Re-arm from the ground truth, not from the outcome: the unwatch
        // above stripped the whole subtree, and a file the free *skipped* is
        // still a placeholder that needs intercepting. Watching only what was
        // freed once left skipped files readable with no one to fill them.
        for path in engine.subtree_local_files(relative)? {
            if gonedrive_core::hydrate::Placeholder::state(&path)
                == gonedrive_core::hydrate::PlaceholderState::Dehydrated
            {
                if let Err(e) = hsm.watch(&path).await {
                    tracing::error!(path = %path.display(), error = %e, "could not intercept");
                }
            }
        }
        Ok(outcome)
    }

    /// Whether the files-on-demand helper is connected.
    pub async fn on_demand_ready(&self) -> bool {
        self.hsm.read().await.is_some()
    }

    async fn require_hsm(&self) -> Result<Arc<crate::hsm_client::HsmClient>> {
        self.hsm.read().await.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "files on demand is not available: the helper is not running.\n\
                 Start it with `sudo gonedrive-hsm`. Without it a placeholder \
                 would read back as empty rather than fetching its content."
            )
        })
    }

    /// One level of the drive, with each entry's effective selection.
    pub async fn list_folder(
        &self,
        relative: &std::path::Path,
    ) -> Result<Vec<gonedrive_ipc::FolderEntry>> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?;
        Ok(engine
            .list(relative)?
            .into_iter()
            .map(|entry| gonedrive_ipc::FolderEntry {
                name: entry.item.name,
                is_folder: entry.item.is_folder,
                selected: entry.pin.wants_content(),
                explicit: entry.explicit,
                bytes: entry.bytes,
                files: entry.files,
            })
            .collect())
    }

    /// Choose whether a path's content is kept on this device.
    pub async fn set_pin(
        &self,
        relative: &std::path::Path,
        pin: gonedrive_core::db::PinState,
    ) -> Result<gonedrive_core::sync::PinOutcome> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?;
        Ok(engine.set_pin(relative, pin)?)
    }

    /// Create placeholders for never-downloaded files in a subtree.
    pub async fn make_on_demand(
        &self,
        relative: &std::path::Path,
    ) -> Result<gonedrive_core::sync::FreeSpaceOutcome> {
        let hsm = self.require_hsm().await?;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?;

        let outcome = engine.make_on_demand(relative)?;
        for path in &outcome.paths {
            if let Err(e) = hsm.watch(path).await {
                tracing::error!(path = %path.display(), error = %e, "could not intercept");
            }
        }
        Ok(outcome)
    }

    /// Placeholders on disk that still need intercepting.
    pub async fn dehydrated_paths(&self) -> Result<Vec<std::path::PathBuf>> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut out = Vec::new();
        for engine in engines {
            out.extend(engine.dehydrated_paths()?);
        }
        Ok(out)
    }

    /// Work through queued transfers only, skipping enumeration.
    pub async fn transfer_all(&self) -> Result<()> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if let Err(e) = engine.transfer_only().await {
                tracing::error!(error = %e, "transfer pass failed");
                self.set_state("error", Some(e.to_string())).await;
            }
        }
        Ok(())
    }

    /// One pass over every account. Returns whether anything actually moved.
    pub async fn sync_all(&self) -> bool {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut did_work = false;

        for engine in engines {
            match engine.sync_once().await {
                Ok(summary) => {
                    did_work |= summary.downloaded > 0
                        || summary.uploaded > 0
                        || summary.deleted_local > 0
                        || summary.deleted_remote > 0
                        || summary.still_queued > 0;
                }
                Err(Error::AuthRequired(message)) => {
                    tracing::error!(%message, "sign-in required");
                    self.set_state("signed_out", Some(message)).await;
                }
                Err(e) if e.is_transient() => {
                    tracing::warn!(error = %e, "sync pass failed; will retry");
                    self.set_state("offline", Some(e.to_string())).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "sync pass failed");
                    self.set_state("error", Some(e.to_string())).await;
                }
            }
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
            match engine.auto_free(threshold) {
                Ok(outcome) if outcome.files > 0 => {
                    tracing::info!(
                        files = outcome.files,
                        freed = outcome.freed,
                        "auto-released space"
                    );
                    for path in &outcome.paths {
                        let _ = hsm.watch(path).await;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "auto-free failed"),
            }
        }
    }

    /// The poll loop. Runs until `shutdown` resolves.
    ///
    /// The interval backs off while nothing is happening so an idle client is
    /// not hitting the service every 30 seconds all day, and snaps back to the
    /// configured interval the moment a pass does real work.
    pub async fn run(self: Arc<Self>, shutdown: impl std::future::Future<Output = ()>) {
        let base = Duration::from_secs(self.config.poll_interval_secs);
        let ceiling = Duration::from_secs(self.config.max_poll_interval_secs);
        let mut interval = base;
        tokio::pin!(shutdown);

        loop {
            if !self.is_paused() {
                interval = if self.sync_all().await {
                    base
                } else {
                    ceiling.min(interval.mul_f64(IDLE_BACKOFF))
                };
                self.auto_free_if_low().await;
            }

            tokio::select! {
                _ = &mut shutdown => {
                    tracing::info!("shutting down");
                    return;
                }
                _ = tokio::time::sleep(interval) => {}
                _ = self.wake.notified() => {
                    // A local change or an explicit request: resume the tight
                    // cadence rather than staying backed off. The extra beat
                    // matters: a pass's own writes (deletes landing, downloads
                    // renaming into place) re-arm the wake, and without it the
                    // loop tail-chases itself at full speed until the queue
                    // drains — a delta request every 700ms, observed live.
                    tokio::time::sleep(Duration::from_millis(
                        self.config.local_debounce_ms,
                    ))
                    .await;
                    interval = base;
                }
            }
        }
    }
}

fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("GOnedrive/", env!("CARGO_PKG_VERSION")))
        // Large uploads legitimately take a long time; only the connect phase
        // gets a short deadline.
        .connect_timeout(Duration::from_secs(20))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("could not build the HTTP client")
}


/// How many finished items the activity list keeps.
const ACTIVITY_LIMIT: usize = 100;

async fn record_activity(
    activity: &Arc<RwLock<std::collections::VecDeque<Activity>>>,
    kind: &str,
    name: String,
    detail: String,
) {
    let mut guard = activity.write().await;
    guard.push_front(Activity {
        kind: kind.to_string(),
        name,
        detail,
        when: time::OffsetDateTime::now_utc().unix_timestamp(),
    });
    guard.truncate(ACTIVITY_LIMIT);
}

/// Processes that read files in order to *describe* them — type sniffing,
/// previews — rather than to use them. Their reads are always served as
/// ranges and never change a file's state: browsing must have no side
/// effects, at any file size.
/// Tools that copy or archive a whole file and skip its holes rather than
/// reading them — so they must be given the entire file before they look.
fn is_bulk_copier(comm: &str) -> bool {
    const COPIERS: &[&str] = &[
        "cp", "mv", "install", "rsync", "tar", "dd", "cpio", "rclone",
        "7z", "7za", "zip", "gzip", "bsdtar", "pax", "cksum",
    ];
    // Exact match, not prefix: "cpuinfo"-style false hits would eagerly
    // download whole files for no reason.
    COPIERS.contains(&comm)
}

fn is_browsing_reader(comm: &str) -> bool {
    // Anything that reads a file to *describe* it — the file manager itself,
    // its preview pane, and every thumbnailer. A thumbnail of an online-only
    // file simply does not render until the file is opened for real, exactly
    // as on Windows; it must never pull the whole image down. `comm` is
    // kernel-truncated to 15 characters, so these are prefixes.
    const BROWSERS: &[&str] = &[
        "nautilus",
        "sushi",              // GNOME's preview (spacebar)
        "glycin",             // GNOME's image loader/thumbnailer sandbox
        "gdk-pixbuf-thum",
        "gnome-thumbnail",
        "gst-video-thumb",
        "gst-audio-thumb",
        "heif-thumbnaile",
        "papers-thumbnai",
        "totem-video-thu",
        "ffmpegthumbnail",
        "evince-thumbnai",
    ];
    BROWSERS.iter().any(|prefix| comm.starts_with(prefix))
}

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
    let spawned = std::process::Command::new("gonedrive")
        .arg("--hydration-monitor")
        .spawn()
        .or_else(|_| {
            std::process::Command::new("./target/release/gonedrive")
                .arg("--hydration-monitor")
                .spawn()
        });
    match spawned {
        Ok(child) => *slot = Some(child),
        Err(e) => tracing::debug!(error = %e, "no progress window available"),
    }
}

#[cfg(test)]
mod tests {
    use super::is_browsing_reader;

    #[test]
    fn thumbnailers_and_previews_browse_never_consume() {
        // The screenshot bug: clicking one image made Nautilus thumbnail the
        // others, downloading them. Every thumbnailer and preview helper must
        // read as "browsing" so it is served offline, never pulling content.
        for t in [
            "glycin-thumbnai", "gdk-pixbuf-thum", "gst-video-thumb",
            "gst-audio-thumb", "heif-thumbnaile", "papers-thumbnai",
            "nautilus", "sushi",
        ] {
            assert!(is_browsing_reader(t), "{t} must browse, not download");
        }
        // A real image viewer the user opened is a genuine consumer.
        for v in ["loupe", "eog", "gimp", "gwenview"] {
            assert!(!is_browsing_reader(v), "{v} is a real open");
        }
    }

    #[test]
    fn file_managers_browse_and_applications_consume() {
        // The line this draws: looking at a file must never flip its state;
        // using it may.
        for browser in ["nautilus", "sushi"] {
            assert!(is_browsing_reader(browser), "{browser}");
        }
        for consumer in ["cat", "gedit", "cp", "libreoffice", "md5sum", ""] {
            assert!(!is_browsing_reader(consumer), "{consumer:?}");
        }
    }
}
