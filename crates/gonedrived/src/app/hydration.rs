//! On-demand hydration, HSM registration and local retention controls.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_core::sync::engine::Engine;
use gonedrive_core::sync::{
    lock_unwatched_placeholder_for_account, materialise_placeholder_for_account,
    placeholder_inventory_for_account, population_watch_directories_for_account,
};
use gonedrive_core::Error;
use gonedrive_ipc::{Activity, Hydration};

use super::{decide_stage, ensure_progress_window, App, InFlight, StagePlan, ACTIVITY_LIMIT};
use crate::open_intent::OpenDecision;

impl App {
    pub async fn stage_for(&self, request: HydrationRequest<'_>) -> Result<StageOutcome> {
        let HydrationRequest {
            request_id,
            path,
            offset,
            length,
            reader,
            is_open,
            capabilities,
        } = request;
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let mut plan = decide_stage(
            is_open,
            reader,
            offset,
            length,
            size,
            capabilities.partial,
            capabilities.file_manager_copy_detection,
        );
        if !is_open && gonedrive_ipc::reader_policy::is_progressive_reader(reader) {
            plan = match self.open_intents.decide(path).await {
                OpenDecision::Explicit => StagePlan::Full,
                OpenDecision::Prefetch => StagePlan::BrowseLocal {
                    offset,
                    length: length.max(1),
                },
                OpenDecision::Unavailable => plan,
            };
        }
        match plan {
            StagePlan::AllowOpen | StagePlan::BrowseLocal { .. } => return Ok(StageOutcome::Allow),
            StagePlan::DenyBrowse => return Ok(StageOutcome::Deny),
            StagePlan::Full | StagePlan::Range { .. } => {}
        }
        let cancel = match self
            .begin_hydration(path, plan.shows_progress(), request_id)
            .await
        {
            HydrationStart::Started(cancel) => cancel,
            HydrationStart::AlreadyMaterialized => {
                // Another request for this path completed while we waited for
                // the per-path lock. Its real bytes are already under the
                // blocked reader, so do not stage or record the same download
                // again.
                return Ok(StageOutcome::Allow);
            }
            HydrationStart::Releasing => {
                // A permission event may have been queued in fanotify before
                // the user asked to release this file. It must not turn the
                // freshly-created placeholder back into a download after the
                // release returns. Denying this one blocked read is safe and
                // lets a later, genuinely new open retry normally.
                tracing::debug!(path = %path.display(), "denying hydration while space is being released");
                return Ok(StageOutcome::Deny);
            }
        };

        // Queue transfers and explicit space release share this gate.  The
        // per-path guard is acquired first (and retained in `InFlight`), so a
        // release and a blocked read cannot deadlock while waiting in opposite
        // orders.
        let _download_gate = self.download_gate.lock().await;
        let result = match plan {
            // Let the open proceed untouched; the reads that follow drive
            // hydration. Keeps a media player streaming instead of pulling the
            // whole file just to read a header.
            StagePlan::AllowOpen | StagePlan::BrowseLocal { .. } | StagePlan::DenyBrowse => {
                unreachable!()
            }
            // A sparse-aware copier (on open) or a consumer past the head:
            // give it the whole file before it looks.
            StagePlan::Full => self
                .stage_content(path, cancel)
                .await
                .map(|staged| StageOutcome::Fill(staged, 0, true)),
            // The first head-read of a consumer: fetch just that window.
            StagePlan::Range { offset, length } => {
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                match engines.first() {
                    Some(engine) => engine
                        .stage_range(path, offset, length)
                        .await
                        .map(|staged| {
                            staged.map_or(StageOutcome::Allow, |staged| {
                                StageOutcome::Fill(staged, offset, false)
                            })
                        })
                        .map_err(anyhow::Error::from),
                    None => Err(anyhow::anyhow!("no account is signed in")),
                }
            }
        };
        if let Ok(StageOutcome::Fill(staged, _, _)) = &result {
            if let Some(entry) = self.hydrations.write().await.get_mut(path) {
                if entry.request_id == request_id {
                    entry.staged = Some(staged.clone());
                }
            }
        }
        result
    }

    /// Download a placeholder's content to a staging file for the helper.
    async fn stage_content(
        &self,
        path: &std::path::Path,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<std::path::PathBuf> {
        self.show_progress_soon(path.to_path_buf());

        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut last = None;
        for engine in engines {
            match engine.stage_content_cancellable(path, cancel.clone()).await {
                Ok(staged) => return Ok(staged),
                Err(Error::Other(message)) => last = Some(message),
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!(
            "no account owns {}{}",
            path.display(),
            last.map(|m| format!(" ({m})")).unwrap_or_default()
        )
    }

    async fn begin_hydration(
        &self,
        path: &std::path::Path,
        visible: bool,
        request_id: u64,
    ) -> HydrationStart {
        let guard = self.hydration_locks.acquire(path).await;
        if self.hydration_locks.is_releasing(path) {
            return HydrationStart::Releasing;
        }
        if gonedrive_core::hydrate::Placeholder::state(path)
            == gonedrive_core::hydrate::PlaceholderState::Hydrated
        {
            return HydrationStart::AlreadyMaterialized;
        }
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.hydrations.write().await.insert(
            path.to_path_buf(),
            InFlight {
                request_id,
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
                visible,
                cancel: cancel.clone(),
                staged: None,
                _path_guard: guard,
                finishing: false,
            },
        );
        HydrationStart::Started(cancel)
    }

    /// What was synced recently, newest first.
    pub async fn recent_activity(&self) -> Vec<Activity> {
        self.store
            .recent_activity(ACTIVITY_LIMIT as i64)
            .unwrap_or_default()
            .into_iter()
            .map(|row| Activity {
                kind: row.kind,
                name: row.name,
                path: row.path,
                detail: row.detail,
                when: row.when,
            })
            .collect()
    }

    /// Placeholders being filled right now.
    pub async fn active_hydrations(&self) -> Vec<Hydration> {
        self.hydrations
            .read()
            .await
            .values()
            .filter(|entry| entry.visible)
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

    /// Remove the exact unique hand-off file used by this request. The helper
    /// consumes an already-open descriptor and deliberately never unlinks an
    /// untrusted pathname; ownership of cleanup stays on this daemon.
    pub async fn cleanup_hydration_staging(&self, path: &std::path::Path, request_id: u64) {
        let staged = {
            let mut hydrations = self.hydrations.write().await;
            let Some(entry) = hydrations.get_mut(path) else {
                return;
            };
            if entry.request_id != request_id {
                tracing::debug!(
                    path = %path.display(),
                    request_id,
                    current = entry.request_id,
                    "ignoring stale hydration staging cleanup"
                );
                return;
            }
            entry.staged.take()
        };
        if let Some(staged) = staged {
            self.remove_staged_file(&staged).await;
        }
    }

    async fn remove_staged_file(&self, staged: &std::path::Path) {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            match engine.remove_staging_file(staged) {
                Ok(()) => return,
                Err(gonedrive_core::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    return;
                }
                Err(_) => continue,
            }
        }
        tracing::warn!(path = %staged.display(), "refusing to remove staging outside an account root");
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
    pub async fn finish_hydration(&self, path: &std::path::Path, request_id: u64) -> Result<()> {
        // Claim the completion before doing any filesystem or database work.
        // A final-range fast path in the helper can send duplicate `Filled`
        // messages; only the request that owns this entry may publish the
        // hydrated state and its activity event.  An absent entry is a stale
        // completion (for example one that arrived after a space release),
        // except for the helper's materialised-range fast path: that path
        // deliberately keeps its mark until this acknowledgement so the
        // release operation cannot lose protection in between.
        let fast_path_completion = {
            let mut hydrations = self.hydrations.write().await;
            match hydrations.get_mut(path) {
                None => true,
                Some(entry) if entry.request_id != request_id => {
                    tracing::debug!(
                        path = %path.display(),
                        request_id,
                        current = entry.request_id,
                        "ignoring stale hydration completion"
                    );
                    return Ok(());
                }
                Some(entry) if entry.finishing => {
                    tracing::debug!(
                        path = %path.display(),
                        request_id,
                        "ignoring duplicate hydration completion"
                    );
                    return Ok(());
                }
                Some(entry) => {
                    entry.finishing = true;
                    false
                }
            }
        };

        if fast_path_completion {
            // Serialize the fallback with a new request or a space release
            // before deciding which inode may be unwatched. The fast-path
            // event carries no daemon map entry, so a path-only unwatch without
            // this guard could retire a mark installed for a replacement inode.
            let _path_guard = self.hydration_locks.acquire(path).await;
            if !self.hydrations.read().await.contains_key(path)
                && gonedrive_core::hydrate::Placeholder::state(path)
                    == gonedrive_core::hydrate::PlaceholderState::Hydrated
            {
                if let Some(hsm) = self.hsm.read().await.clone() {
                    if let Err(error) = hsm.unwatch(path).await {
                        tracing::debug!(
                            path = %path.display(),
                            %error,
                            "could not retire a materialised-range mark"
                        );
                    }
                }
            } else {
                tracing::debug!(path = %path.display(), "ignoring stale hydration completion");
            }
            return Ok(());
        }

        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut result = Ok(());
        for engine in engines {
            if let Err(error) = engine.finish_hydration(path) {
                result = Err(error.into());
                break;
            }
        }
        if result.is_ok() {
            // Keep the in-flight entry (and its path guard) alive while the
            // kernel mark is retired. A stale completion must not be able to
            // orphan the current mark and trigger another callback later.
            if let Some(hsm) = self.hsm.read().await.clone() {
                if let Err(error) = hsm.unwatch(path).await {
                    tracing::warn!(path = %path.display(), %error, "could not retire hydration mark");
                }
            }
        }
        self.remove_hydration_if_current(path, request_id).await;
        if result.is_ok() {
            self.refresh_pending_status().await;
        }
        result
    }

    /// Release a failed per-path hydration and put its database state back to
    /// dehydrated so a later reader can retry it.
    pub async fn fail_hydration(&self, path: &std::path::Path, request_id: u64) -> Result<()> {
        if !self.claim_hydration_completion(path, request_id).await {
            return Ok(());
        }
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if engine.fail_hydration(path).is_ok() {
                break;
            }
        }
        self.remove_hydration_if_current(path, request_id).await;
        Ok(())
    }

    /// Put a placeholder's mtime back after a partial range fill moved it.
    pub async fn restore_mtime(&self, path: &std::path::Path, request_id: u64) -> Result<()> {
        if !self.claim_hydration_completion(path, request_id).await {
            return Ok(());
        }
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if engine.restore_mtime(path).is_ok() {
                break;
            }
        }
        self.remove_hydration_if_current(path, request_id).await;
        Ok(())
    }

    /// Claim one terminal helper callback. Keeping `finishing` set until the
    /// caller removes the entry prevents duplicate callbacks from racing the
    /// filesystem/database work or replacing the path guard underneath it.
    async fn claim_hydration_completion(&self, path: &std::path::Path, request_id: u64) -> bool {
        let mut hydrations = self.hydrations.write().await;
        let Some(entry) = hydrations.get_mut(path) else {
            tracing::debug!(
                path = %path.display(),
                request_id,
                "ignoring stale hydration completion"
            );
            return false;
        };
        if entry.request_id != request_id {
            tracing::debug!(
                path = %path.display(),
                request_id,
                current = entry.request_id,
                "ignoring stale hydration completion"
            );
            return false;
        }
        if entry.finishing {
            tracing::debug!(
                path = %path.display(),
                request_id,
                "ignoring duplicate hydration completion"
            );
            return false;
        }
        entry.finishing = true;
        true
    }

    async fn remove_hydration_if_current(&self, path: &std::path::Path, request_id: u64) {
        let mut hydrations = self.hydrations.write().await;
        if hydrations
            .get(path)
            .is_some_and(|entry| entry.request_id == request_id)
        {
            hydrations.remove(path);
        }
    }

    /// Fill a placeholder directly (FetchNow, the CLI, the context menu).
    ///
    /// The file may be under a kernel watch, and the daemon's writes are not
    /// exempt from it — writing into the holes would fire the very hook that
    /// asks us to hydrate, and a second download would race this one over the
    /// same bytes. So the watch comes off first; it goes back on only if the
    /// fill fails and the file is still empty.
    pub async fn hydrate(&self, path: &std::path::Path) -> Result<u64> {
        // FetchNow/context-menu downloads and fanotify hydration must not race
        // a user-requested space release on the same inode.
        let _path_guard = self.hydration_locks.acquire(path).await;
        if self.hydration_locks.is_releasing(path) {
            anyhow::bail!(
                "{} is being released; try again after it completes",
                path.display()
            );
        }
        let hsm = self.hsm.read().await.clone();
        if let Some(hsm) = &hsm {
            hsm.unwatch(path).await.with_context(|| {
                format!("could not suspend interception for {}", path.display())
            })?;
        }

        let result = {
            let _download_gate = self.download_gate.lock().await;
            self.hydrate_inner(path).await
        };

        if result.is_ok() {
            self.refresh_pending_status().await;
        }

        if result.is_err() {
            if let Some(hsm) = &hsm {
                if gonedrive_core::hydrate::Placeholder::state(path)
                    == gonedrive_core::hydrate::PlaceholderState::Dehydrated
                {
                    if let Err(error) = hsm.watch(path).await {
                        tracing::error!(path = %path.display(), %error, "could not restore interception");
                    }
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
        if client.is_none() {
            let mut hydrations = self.hydrations.write().await;
            let staged: Vec<_> = hydrations
                .values()
                .filter_map(|entry| entry.staged.clone())
                .collect();
            for entry in hydrations.values() {
                entry.cancel.store(true, Ordering::Relaxed);
            }
            hydrations.clear();
            drop(hydrations);
            for path in staged {
                self.remove_staged_file(&path).await;
            }
        }
        *self.hsm.write().await = client;
    }

    /// Keep a failed mark target visible while making reads fail closed. The
    /// helper restores its original mode only after the inode is protected.
    pub async fn lock_unwatched_placeholder(&self, path: &std::path::Path) -> Result<()> {
        let mut failures = crate::failure_summary::FailureSummary::default();
        for account in self.store.accounts()? {
            if !path.starts_with(&account.root_local_path) {
                continue;
            }
            match lock_unwatched_placeholder_for_account(&self.store, &account, path) {
                Ok(()) => return Ok(()),
                Err(gonedrive_core::Error::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    return Ok(())
                }
                Err(error) => failures.record(|| error.to_string()),
            }
        }
        if matches!(
            std::fs::symlink_metadata(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            return Ok(());
        }
        anyhow::bail!(
            "no account could safety-lock placeholder {}: {}",
            path.display(),
            failures.describe()
        )
    }

    /// Present the whole drive as a browsable placeholder tree.
    ///
    /// Runs on a blocking thread — it can touch a hundred thousand files — and
    /// registers every resulting placeholder with the helper. Safe to call
    /// repeatedly: existing files are left untouched.
    pub async fn materialize_tree(self: &Arc<Self>) -> Result<u64> {
        let hsm = self.require_hsm().await?;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?
            .clone();

        tracing::info!("materialising the drive as a placeholder tree");
        let worker_engine = engine.clone();
        let paths = tokio::task::spawn_blocking(move || {
            worker_engine.materialize_all(|done, total| {
                if done % 5000 == 0 || done == total {
                    tracing::info!(done, total, "materialising");
                }
                true
            })
        })
        .await??;

        let count = paths.len() as u64;
        self.protect_placeholders(&hsm, &paths).await?;

        // Materialising is a mode choice, not a one-shot repair. Persist it and
        // publish it to the live engine so later deltas create new/restored
        // entries without repeating this whole-tree pass at every start.
        if !self.config.on_demand {
            let mut config = (*self.config).clone();
            config.on_demand = true;
            config
                .save()
                .context("could not persist files-on-demand mode")?;
        }
        engine.enable_on_demand();
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

        // Install every mark before punching the first hole. The helper admits
        // the daemon's own pid, so keeping the mark through fallocate is safe;
        // publishing holes first and discovering fanotify_mark failed is not.
        let candidates = engine.subtree_local_files(relative)?;
        // Mark the whole candidate set before waiting on any path lock.  A
        // permission event that is already queued in fanotify can otherwise
        // arrive after the holes are punched and start a fresh hydration.
        let _release_barrier = self.hydration_locks.begin_release(&candidates);
        // Hold the guards until both the marks and the holes are durable.  This
        // makes a queued HSM completion wait instead of restoring the file
        // immediately after the release call returns.
        let result = async {
            let _path_guards = self.hydration_locks.acquire_many(&candidates).await;
            let mut newly_protected: Vec<(std::path::PathBuf, bool)> = Vec::new();
            for path in &candidates {
                let was_dehydrated = gonedrive_core::hydrate::Placeholder::state(path)
                    == gonedrive_core::hydrate::PlaceholderState::Dehydrated;
                if let Err(error) = hsm.watch(path).await {
                    if let Err(lock_error) = self.lock_unwatched_placeholder(path).await {
                        tracing::error!(
                            path = %path.display(),
                            error = %lock_error,
                            "could not safety-lock an unprotected placeholder"
                        );
                    }
                    for (protected, was_dehydrated) in newly_protected {
                        if !was_dehydrated {
                            let _ = hsm.unwatch(&protected).await;
                        }
                    }
                    return Err(error).with_context(|| {
                        format!("could not protect {} before freeing it", path.display())
                    });
                }
                newly_protected.push((path.clone(), was_dehydrated));
            }
            // A scheduler download may have removed its durable row just before
            // this call.  Serialise the actual transfer with the destructive
            // release so it cannot publish bytes after the holes are punched.
            let transfer_result = {
                let _download_gate = self.download_gate.lock().await;
                engine.free_space(relative)
            };

            // A queued fast-path read can retire a mark while the file is
            // still fully materialised. Reconcile the final state after the
            // holes are made so every surviving placeholder is protected even
            // when no daemon hydration callback was involved.
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
                self.protect_placeholders(&hsm, &dehydrated)
                    .await
                    .err()
            };
            // Files the safety checks skipped remain hydrated and should not
            // keep a permission mark. Successfully dehydrated files retain the
            // mark that was installed before their blocks were released.
            for path in &candidates {
                if gonedrive_core::hydrate::Placeholder::state(path)
                    != gonedrive_core::hydrate::PlaceholderState::Dehydrated
                {
                    if let Err(e) = hsm.unwatch(path).await {
                        tracing::warn!(path = %path.display(), error = %e, "could not remove unused mark");
                    }
                }
            }
            if let Some(error) = protection_error {
                tracing::error!(%error, "could not re-protect placeholders after release");
                if transfer_result.is_ok() {
                    return Err(error);
                }
            }
            Ok(transfer_result?)
        }
        .await;
        let outcome = result?;
        self.refresh_pending_status().await;
        Ok(outcome)
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

        // The chooser is a real user navigation boundary.  A partial folder
        // has only its Graph childCount in the database, so listing it without
        // this step would render an apparently empty directory.  Reuse the
        // same bounded population path as Nautilus: fetch the requested
        // directory and one lookahead level (A+1). If the network or helper
        // is unavailable, keep the cached listing path alive; the caller can
        // still show any rows already known and retry on the next navigation.
        if engine.folder_needs_population(relative)? {
            if let Err(error) = self.populate_folder(relative).await {
                tracing::debug!(
                    path = %relative.display(),
                    %error,
                    "could not populate folder for chooser listing; using cached metadata"
                );
            }
        }
        Ok(engine
            .list(relative)?
            .into_iter()
            .map(|entry| gonedrive_ipc::FolderEntry {
                name: entry.item.name,
                is_folder: entry.item.is_folder,
                selected: entry.pin.wants_content(),
                explicit: entry.explicit,
                bytes: entry.bytes,
                items: entry.items,
                items_known: entry.items_known,
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
        let outcome = engine.set_pin(relative, pin)?;
        self.refresh_pending_status().await;
        Ok(outcome)
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
        self.protect_placeholders(&hsm, &outcome.paths).await?;
        Ok(outcome)
    }

    /// Populate one directory, Cloud Files style: metadata is already local,
    /// its direct child inodes and one lookahead directory level are published
    /// and protected here.
    pub async fn populate_folder(
        &self,
        relative: &std::path::Path,
    ) -> Result<gonedrive_core::sync::FreeSpaceOutcome> {
        let hsm = self.require_hsm().await?;
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let engine = engines
            .first()
            .ok_or_else(|| anyhow::anyhow!("no account is signed in"))?;

        if relative.is_absolute()
            || relative
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            anyhow::bail!("folder population path must stay below the sync root");
        }
        let target = self.config.sync_root.join(relative);
        let outcome = engine.populate_folder(relative).await?;
        self.protect_placeholders(&hsm, &outcome.paths).await?;
        hsm.watch_directory_paths(&[target]).await?;
        Ok(outcome)
    }

    /// Protect a batch through the client's serialized, bounded registration
    /// path. Any file that cannot be marked is made unreadable before this
    /// returns; a zero-readable sparse file must never outlive an attempt.
    pub(super) async fn protect_placeholders(
        &self,
        hsm: &crate::hsm_client::HsmClient,
        paths: &[std::path::PathBuf],
    ) -> Result<()> {
        hsm.watch_paths(self, paths).await.map(|_| ())
    }

    /// Placeholders on disk that still need intercepting. This deliberately
    /// reads local account metadata instead of live engines: protection must
    /// be installed before network authentication has succeeded.
    pub async fn dehydrated_paths(&self) -> Result<Vec<std::path::PathBuf>> {
        Ok(self.placeholder_inventory().await?.0)
    }

    /// Existing mark targets and missing on-demand files, collected together
    /// so HSM startup never needs two database/path passes over a large drive.
    pub(crate) async fn placeholder_inventory(
        &self,
    ) -> Result<(
        Vec<std::path::PathBuf>,
        Vec<(gonedrive_core::db::Account, String)>,
    )> {
        let store = self.store.clone();
        let on_demand = self.config.on_demand;
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let mut missing = Vec::new();
            for account in store.accounts()? {
                let inventory = placeholder_inventory_for_account(&store, &account)?;
                out.extend(inventory.existing);
                if on_demand {
                    missing.extend(
                        inventory
                            .missing
                            .into_iter()
                            .map(|item_id| (account.clone(), item_id)),
                    );
                }
            }
            out.sort();
            out.dedup();
            Ok((out, missing))
        })
        .await
        .context("placeholder inventory worker failed")?
    }

    /// Populated directory boundaries that need process-aware direct-child
    /// open notifications. This remains local-only and works while Graph or
    /// the token service is offline.
    pub(crate) async fn population_watch_inventory(&self) -> Result<Vec<std::path::PathBuf>> {
        if !self.config.on_demand {
            return Ok(Vec::new());
        }
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut paths = Vec::new();
            for account in store.accounts()? {
                paths.extend(population_watch_directories_for_account(&store, &account)?);
            }
            paths.sort();
            paths.dedup();
            Ok(paths)
        })
        .await
        .context("directory population inventory worker failed")?
    }

    /// Recreate only placeholders found missing by the startup inventory, then
    /// protect each bounded batch before exposing more. Materialisation errors
    /// mean local content occupies the path and are left for reconciliation;
    /// mark errors remain fatal and are retried by the HSM supervisor.
    pub(crate) async fn restore_missing_placeholders(
        &self,
        hsm: &crate::hsm_client::HsmClient,
        missing: &[(gonedrive_core::db::Account, String)],
    ) -> Result<usize> {
        const RESTORE_BATCH: usize = 128;

        let mut restored = 0usize;
        let mut skipped = crate::failure_summary::FailureSummary::default();
        for chunk in missing.chunks(RESTORE_BATCH) {
            let store = self.store.clone();
            let chunk = chunk.to_vec();
            let results = tokio::task::spawn_blocking(move || {
                chunk
                    .into_iter()
                    .map(|(account, item_id)| {
                        let result =
                            materialise_placeholder_for_account(&store, &account, &item_id)
                                .map_err(|error| error.to_string());
                        (item_id, result)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .context("placeholder restore worker failed")?;
            let mut paths = Vec::with_capacity(results.len());
            for (item_id, result) in results {
                match result {
                    Ok(path) => paths.push(path),
                    Err(error) => skipped.record(|| format!("{item_id}: {error}")),
                }
            }
            if !paths.is_empty() {
                restored += hsm.watch_paths(self, &paths).await?;
            }
        }
        if !skipped.is_empty() {
            tracing::warn!(
                skipped = skipped.total(),
                examples = skipped.describe(),
                "could not rebuild some missing placeholders"
            );
        }
        Ok(restored)
    }

    pub(super) async fn register_pending_placeholders(
        &self,
        engines: &[Arc<Engine>],
    ) -> Result<usize> {
        let paths: Vec<_> = engines
            .iter()
            .flat_map(|engine| engine.take_placeholder_marks())
            .collect();
        let directories = self.population_watch_inventory().await?;
        if paths.is_empty() && directories.is_empty() {
            return Ok(0);
        }
        let Some(hsm) = self.hsm.read().await.clone() else {
            if paths.is_empty() {
                return Ok(0);
            }
            let mut cleanup_errors = crate::failure_summary::FailureSummary::default();
            for path in &paths {
                if let Err(error) = self.lock_unwatched_placeholder(path).await {
                    cleanup_errors.record(|| format!("{}: {error:#}", path.display()));
                }
            }
            let detail = if cleanup_errors.is_empty() {
                String::new()
            } else {
                format!(
                    "; cleanup failed for {} placeholder(s): {}",
                    cleanup_errors.total(),
                    cleanup_errors.describe()
                )
            };
            anyhow::bail!("files-on-demand helper is unavailable; safety-locked up to {} unprotected placeholder(s){detail}", paths.len());
        };
        self.protect_placeholders(&hsm, &paths).await?;
        let watched_directories = hsm.watch_directory_paths(&directories).await?;
        Ok(paths.len().saturating_add(watched_directories))
    }

    pub(super) async fn reconcile_placeholder_report(
        &self,
        placeholders: Vec<(std::path::PathBuf, u64, u64)>,
    ) -> Result<usize> {
        self.pending_placeholder_reports
            .lock()
            .await
            .remember(placeholders);
        self.reconcile_pending_placeholder_report().await
    }

    /// Replay the newest full local observation after the HSM becomes ready.
    /// The snapshot is cleared only if no newer scan replaced it meanwhile.
    pub(crate) async fn reconcile_pending_placeholder_report(&self) -> Result<usize> {
        let Some(snapshot) = self.pending_placeholder_reports.lock().await.snapshot() else {
            return Ok(0);
        };
        let Some(hsm) = self.hsm.read().await.clone() else {
            return Ok(0);
        };
        let reconciled = hsm.reconcile_observed(self, &snapshot).await?;
        self.pending_placeholder_reports
            .lock()
            .await
            .clear_if_current(&snapshot);
        Ok(reconciled)
    }
}

pub(crate) enum StageOutcome {
    Allow,
    Deny,
    Fill(std::path::PathBuf, u64, bool),
}

/// All context belonging to one fanotify read. Keeping the correlation ID
/// beside the reader policy prevents a completion from being accidentally
/// applied to a later request for the same path.
pub(crate) struct HydrationRequest<'a> {
    pub(crate) request_id: u64,
    pub(crate) path: &'a std::path::Path,
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) reader: &'a str,
    pub(crate) is_open: bool,
    pub(crate) capabilities: HydrationCapabilities,
}

enum HydrationStart {
    Started(Arc<std::sync::atomic::AtomicBool>),
    AlreadyMaterialized,
    Releasing,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HydrationCapabilities {
    pub(crate) partial: bool,
    pub(crate) file_manager_copy_detection: bool,
}
