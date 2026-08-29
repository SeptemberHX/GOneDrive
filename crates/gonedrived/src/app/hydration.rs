//! On-demand hydration, HSM registration and local retention controls.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use gonedrive_core::sync::engine::{Engine, FullScanReport};
use gonedrive_core::Error;
use gonedrive_ipc::{Activity, Hydration};

use super::{decide_stage, ensure_progress_window, App, InFlight, StagePlan, ACTIVITY_LIMIT};

impl App {
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
        let plan = decide_stage(is_open, reader, offset, length, size, allow_partial);
        if plan == StagePlan::AllowOpen {
            return Ok(None);
        }
        let cancel = self.begin_hydration(path).await;

        let result = match plan {
            // Let the open proceed untouched; the reads that follow drive
            // hydration. Keeps a media player streaming instead of pulling the
            // whole file just to read a header.
            StagePlan::AllowOpen => unreachable!(),
            // A sparse-aware copier (on open) or a consumer past the head:
            // give it the whole file before it looks.
            StagePlan::Full => self
                .stage_content(path, cancel)
                .await
                .map(|staged| Some((staged, 0, true))),
            // Browsing: serve the window only if it is already on disk from a
            // real earlier read; otherwise allow the read against the sparse
            // file with no network. The type comes from the name, as on Windows.
            StagePlan::BrowseLocal { offset, length } => {
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                if let Some(engine) = engines.first() {
                    if let Ok(Some(staged)) =
                        engine.stage_range_if_local(path, offset, length).await
                    {
                        Ok(Some((staged, offset, false)))
                    } else {
                        Ok(None)
                    }
                } else {
                    Ok(None)
                }
            }
            // The first head-read of a consumer: fetch just that window.
            StagePlan::Range { offset, length } => {
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                match engines.first() {
                    Some(engine) => engine
                        .stage_range(path, offset, length)
                        .await
                        .map(|staged| Some((staged, offset, false)))
                        .map_err(anyhow::Error::from),
                    None => Err(anyhow::anyhow!("no account is signed in")),
                }
            }
        };
        if result.is_err() {
            self.hydrations.write().await.remove(path);
        } else if let Ok(Some((staged, _, _))) = &result {
            if let Some(entry) = self.hydrations.write().await.get_mut(path) {
                entry.staged = Some(staged.clone());
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

    async fn begin_hydration(&self, path: &std::path::Path) -> Arc<std::sync::atomic::AtomicBool> {
        let guard = self.hydration_locks.acquire(path).await;
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
                staged: None,
                _path_guard: guard,
            },
        );
        cancel
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
    pub async fn cleanup_hydration_staging(&self, path: &std::path::Path) {
        let staged = self
            .hydrations
            .write()
            .await
            .get_mut(path)
            .and_then(|entry| entry.staged.take());
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
    pub async fn finish_hydration(&self, path: &std::path::Path) -> Result<()> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        let mut result = Ok(());
        for engine in engines {
            if let Err(error) = engine.finish_hydration(path) {
                result = Err(error.into());
                break;
            }
        }
        self.hydrations.write().await.remove(path);
        result
    }

    /// Release a failed per-path hydration and put its database state back to
    /// dehydrated so a later reader can retry it.
    pub async fn fail_hydration(&self, path: &std::path::Path) -> Result<()> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if engine.fail_hydration(path).is_ok() {
                break;
            }
        }
        self.hydrations.write().await.remove(path);
        Ok(())
    }

    /// Put a placeholder's mtime back after a partial range fill moved it.
    pub async fn restore_mtime(&self, path: &std::path::Path) -> Result<()> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if engine.restore_mtime(path).is_ok() {
                break;
            }
        }
        self.hydrations.write().await.remove(path);
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
            hsm.unwatch(path).await.with_context(|| {
                format!("could not suspend interception for {}", path.display())
            })?;
        }

        let result = self.hydrate_inner(path).await;

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

    /// Remove a dehydrated file that the helper could not mark. Leaving it on
    /// disk would expose sparse zeroes as real content; the remote record stays
    /// so a later successful materialisation can recreate it.
    pub async fn discard_unwatched_placeholder(&self, path: &std::path::Path) -> Result<()> {
        let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
        for engine in engines {
            if engine.discard_unwatched_placeholder(path).is_ok() {
                return Ok(());
            }
        }
        anyhow::bail!("no account owns {}", path.display())
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
        self.protect_placeholders(&hsm, &paths).await?;
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
        let mut newly_protected: Vec<(std::path::PathBuf, bool)> = Vec::new();
        for path in &candidates {
            let was_dehydrated = gonedrive_core::hydrate::Placeholder::state(path)
                == gonedrive_core::hydrate::PlaceholderState::Dehydrated;
            if let Err(error) = hsm.watch(path).await {
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
        let outcome = engine.free_space(relative)?;
        // Files the safety checks skipped remain hydrated and should not keep a
        // permission mark. Successfully dehydrated files retain the mark that
        // was installed before their blocks were released.
        for path in candidates {
            if gonedrive_core::hydrate::Placeholder::state(&path)
                != gonedrive_core::hydrate::PlaceholderState::Dehydrated
            {
                if let Err(e) = hsm.unwatch(&path).await {
                    tracing::warn!(path = %path.display(), error = %e, "could not remove unused mark");
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
        self.protect_placeholders(&hsm, &outcome.paths).await?;
        Ok(outcome)
    }

    /// Protect a batch, removing every not-yet-protected placeholder if one
    /// mark fails. A zero-readable sparse file must never outlive a partial
    /// registration attempt.
    async fn protect_placeholders(
        &self,
        hsm: &crate::hsm_client::HsmClient,
        paths: &[std::path::PathBuf],
    ) -> Result<()> {
        for (index, path) in paths.iter().enumerate() {
            if let Err(error) = hsm.watch(path).await {
                let mut cleanup_errors = Vec::new();
                for unprotected in &paths[index..] {
                    if let Err(cleanup_error) =
                        self.discard_unwatched_placeholder(unprotected).await
                    {
                        cleanup_errors.push(format!("{}: {cleanup_error}", unprotected.display()));
                    }
                }
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!("; cleanup also failed for {}", cleanup_errors.join(", "))
                };
                return Err(error).with_context(|| {
                    format!("could not protect placeholder {}{cleanup}", path.display())
                });
            }
        }
        Ok(())
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

    pub(super) async fn register_pending_placeholders(
        &self,
        engines: &[Arc<Engine>],
    ) -> Result<usize> {
        let paths: Vec<_> = engines
            .iter()
            .flat_map(|engine| engine.take_placeholder_marks())
            .collect();
        if paths.is_empty() {
            return Ok(0);
        }
        let Some(hsm) = self.hsm.read().await.clone() else {
            let mut cleanup_errors = Vec::new();
            for path in &paths {
                if let Err(error) = self.discard_unwatched_placeholder(path).await {
                    cleanup_errors.push(format!("{}: {error}", path.display()));
                }
            }
            let detail = if cleanup_errors.is_empty() {
                String::new()
            } else {
                format!("; cleanup failed for {}", cleanup_errors.join(", "))
            };
            anyhow::bail!("files-on-demand helper is unavailable; removed up to {} unprotected placeholder(s){detail}", paths.len());
        };
        self.protect_placeholders(&hsm, &paths).await?;
        Ok(paths.len())
    }

    pub(super) async fn reconcile_placeholder_report(
        &self,
        report: &FullScanReport,
    ) -> Result<usize> {
        let Some(hsm) = self.hsm.read().await.clone() else {
            return Ok(0);
        };
        hsm.reconcile_observed(self, &report.placeholders).await
    }
}
