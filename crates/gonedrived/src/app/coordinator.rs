//! Priority sync scheduling and explicit-sync completion barriers.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use gonedrive_core::sync::engine::{Engine, FullScanReport, QueueScope, SyncSummary};
use gonedrive_core::Error;
use gonedrive_ipc::SyncRequestResult;

use super::{App, DaemonEvent};

const TRANSFER_SLICE: Duration = Duration::from_secs(5);

struct TransferBatch {
    scope: QueueScope,
    results: Vec<std::result::Result<SyncSummary, Error>>,
    exhausted: bool,
}

struct LocalScanBatch {
    engines: Vec<Arc<Engine>>,
    results: Vec<std::result::Result<SyncSummary, Error>>,
}

struct FullScanBatch {
    result: Result<Vec<(Arc<Engine>, FullScanReport)>>,
}

struct ExplicitSync {
    target_generation: u64,
    result: SyncRequestResult,
}

impl App {
    fn complete_explicit_sync(
        &self,
        completed_generation: &mut u64,
        target_generation: u64,
        result: SyncRequestResult,
    ) {
        while *completed_generation < target_generation {
            *completed_generation += 1;
            self.emit(DaemonEvent::SyncRequestCompleted {
                generation: *completed_generation,
                result: result.clone(),
            });
        }
    }

    async fn consume_full_scan(
        &self,
        result: Result<Vec<(Arc<Engine>, FullScanReport)>>,
        dirty_paths: &mut HashSet<std::path::PathBuf>,
    ) -> Option<bool> {
        let reports = match result {
            Ok(reports) => reports,
            Err(error) => {
                tracing::warn!(%error, "full local verification failed");
                return None;
            }
        };
        let mut found_dirty = false;
        for (_, report) in reports {
            tracing::info!(
                scanned = report.scanned_entries,
                dirty_scopes = report.dirty_paths.len(),
                placeholders = report.placeholders.len(),
                "full local verification finished"
            );
            if let Err(error) = self.reconcile_placeholder_report(report.placeholders).await {
                tracing::warn!(%error, "placeholder reconciliation failed");
            }
            found_dirty |= !report.dirty_paths.is_empty();
            dirty_paths.extend(report.dirty_paths);
        }
        Some(found_dirty)
    }

    async fn consume_transfer(&self, batch: TransferBatch) -> (QueueScope, bool) {
        let mut did_work = false;
        let mut still_queued = 0u64;
        for result in batch.results {
            match result {
                Ok(summary) => {
                    did_work |= summary.downloaded > 0
                        || summary.uploaded > 0
                        || summary.deleted_local > 0
                        || summary.deleted_remote > 0;
                    still_queued = still_queued.saturating_add(summary.still_queued);
                }
                Err(error) => {
                    tracing::warn!(%error, scope = ?batch.scope, "transfer slice failed");
                    self.report_core_error(&error).await;
                }
            }
        }
        (
            batch.scope,
            still_queued > 0 && (did_work || batch.exhausted),
        )
    }

    async fn consume_local_scan(&self, result: Result<LocalScanBatch>) -> bool {
        let batch = match result {
            Ok(batch) => batch,
            Err(error) => {
                tracing::error!(%error, "incremental local scan task failed");
                self.set_state("error", Some(error.to_string())).await;
                return false;
            }
        };
        let mut scans_complete = true;
        for result in batch.results {
            if let Err(error) = result {
                scans_complete = false;
                tracing::error!(%error, "incremental local scan failed");
                self.set_state("error", Some(error.to_string())).await;
            }
        }
        if let Err(error) = self.register_pending_placeholders(&batch.engines).await {
            tracing::error!(%error, "could not register changed placeholders");
            self.set_state("error", Some(error.to_string())).await;
        }
        scans_complete
    }

    /// Priority coordinator. Remote delta deadlines always run before local
    /// reconciliation and durable transfers. Full-tree discovery is read-only
    /// and stays on a blocking worker while this loop continues polling.
    pub async fn run(
        self: Arc<Self>,
        mut local_changes: Option<tokio::sync::mpsc::Receiver<crate::watcher::LocalChangeBatch>>,
        open_writes: crate::watcher::OpenWrites,
        shutdown: impl std::future::Future<Output = ()>,
    ) {
        let base = Duration::from_secs(self.config().poll_interval_secs);
        let ceiling = Duration::from_secs(self.config().max_poll_interval_secs);
        let full_interval = Duration::from_secs(self.config().full_scan_interval_secs);
        let mut retry_interval = base;
        let mut next_remote = Instant::now();
        let mut next_full = Instant::now();
        let mut force_remote = false;
        // Set only by an explicit wake (SyncNow, a login, an unlock). Watcher
        // batches also force a remote pass, but must not re-probe a locked
        // keyring: that prompts the user on every local file change.
        let mut explicit_wake = false;
        let mut remote_required_for_local = false;
        let mut dirty_paths = HashSet::new();
        let mut completed_writes = HashSet::new();
        let mut download_work = false;
        let mut queue_work = false;
        let mut discovery_running = false;
        let mut discovery_again = false;
        let mut delete_verification_ready = false;
        let mut download_transfer_running = false;
        let mut outbound_transfer_running = false;
        let mut local_scan_running = false;
        let mut completed_generation = 0u64;
        let mut explicit_sync: Option<ExplicitSync> = None;
        let (discovery_tx, mut discovery_rx) = tokio::sync::mpsc::channel::<FullScanBatch>(1);
        let (transfer_tx, mut transfer_rx) = tokio::sync::mpsc::channel(2);
        let (local_scan_tx, mut local_scan_rx) = tokio::sync::mpsc::channel(1);
        let auto_free_running = Arc::new(AtomicBool::new(false));
        tokio::pin!(shutdown);

        loop {
            if self.is_paused() {
                // A request made while paused, or a request whose scan raised a
                // safety stop, must receive a terminal result. Otherwise the
                // CLI waits forever for a generation the coordinator can no
                // longer advance.
                let requested = self.requested_sync_generation.load(Ordering::Acquire);
                let tracked = explicit_sync
                    .as_ref()
                    .map_or(completed_generation, |sync| sync.target_generation);
                let target = requested.max(tracked);
                if target > completed_generation {
                    let reason = {
                        let status = self.status.read().await;
                        if status.last_error.is_empty() {
                            "sync is paused".to_string()
                        } else {
                            status.last_error.clone()
                        }
                    };
                    let mut result = explicit_sync
                        .take()
                        .map(|sync| sync.result)
                        .unwrap_or_default();
                    result.ok = false;
                    result.error = reason;
                    self.complete_explicit_sync(&mut completed_generation, target, result);
                }
                tokio::select! {
                    biased;
                    _ = &mut shutdown => {
                        tracing::info!("shutting down");
                        return;
                    }
                    Some(batch) = discovery_rx.recv(), if discovery_running => {
                        discovery_running = false;
                        let verification = self.consume_full_scan(batch.result, &mut dirty_paths).await;
                        delete_verification_ready = verification.is_some();
                        if verification == Some(true) {
                            remote_required_for_local = true;
                            force_remote = true;
                        }
                        next_full = if std::mem::take(&mut discovery_again) {
                            Instant::now()
                        } else if verification.is_none() {
                            Instant::now() + base.min(full_interval)
                        } else {
                            Instant::now() + full_interval
                        };
                    }
                    Some(batch) = transfer_rx.recv(), if download_transfer_running || outbound_transfer_running => {
                        let (scope, more) = self.consume_transfer(batch).await;
                        match scope {
                            QueueScope::Downloads => {
                                download_transfer_running = false;
                                download_work |= more;
                            }
                            QueueScope::Outbound | QueueScope::OutboundNonDelete => {
                                outbound_transfer_running = false;
                                queue_work |= more;
                            }
                            QueueScope::All => {
                                download_transfer_running = false;
                                outbound_transfer_running = false;
                                download_work |= more;
                                queue_work |= more;
                            }
                        }
                    }
                    Some(result) = local_scan_rx.recv(), if local_scan_running => {
                        local_scan_running = false;
                        let scan_complete = self.consume_local_scan(result).await;
                        queue_work |= scan_complete;
                        if !scan_complete {
                            delete_verification_ready = false;
                            if discovery_running { discovery_again = true; }
                            else { next_full = Instant::now(); }
                        }
                    }
                    batch = receive_local_change(&mut local_changes) => {
                        match batch {
                            Some(batch) => {
                                dirty_paths.extend(batch.paths);
                                completed_writes.extend(batch.completed_writes);
                                remote_required_for_local = true;
                                force_remote = true;
                                if batch.force_full {
                                    if discovery_running { discovery_again = true; }
                                    else { next_full = Instant::now(); }
                                }
                            }
                            None => local_changes = None,
                        }
                    }
                    _ = self.wake.notified() => {
                        force_remote = true;
                        explicit_wake = true;
                        if self.force_full_scan.swap(false, Ordering::AcqRel) {
                            if discovery_running { discovery_again = true; }
                            else { next_full = Instant::now(); }
                        }
                    }
                }
                continue;
            }

            // SyncNow is a remote freshness request, not a request to walk the
            // whole local tree. It drains work already reported by the watcher
            // and the fresh delta; startup, the periodic verifier, and watcher
            // overflow retain ownership of full discovery.
            let explicit_quiescent = explicit_sync.is_some()
                && sync_barrier_ready(
                    local_scan_running,
                    download_transfer_running,
                    outbound_transfer_running,
                    remote_required_for_local,
                    !dirty_paths.is_empty(),
                    download_work,
                    queue_work,
                );
            if explicit_quiescent {
                let pending = self.store.accounts().and_then(|accounts| {
                    accounts.into_iter().try_fold(0u64, |total, account| {
                        self.store
                            .pending_count(&account.id)
                            .map(|count| total.saturating_add(count))
                    })
                });
                match pending {
                    Ok(0) => {
                        let mut explicit = explicit_sync.take().expect("checked above");
                        explicit.result.queued = 0;
                        self.complete_explicit_sync(
                            &mut completed_generation,
                            explicit.target_generation,
                            explicit.result,
                        );
                        continue;
                    }
                    Ok(pending) => {
                        // Everything still queued is sitting out a retry
                        // delay: nothing more can happen until those timers
                        // fire. The explicit pass is as complete as it can
                        // be, so report what remains rather than holding the
                        // caller until a backed-off transfer finally lands.
                        let due = self.store.accounts().and_then(|accounts| {
                            accounts.into_iter().try_fold(0u64, |total, account| {
                                self.store
                                    .due_pending_count(&account.id)
                                    .map(|count| total.saturating_add(count))
                            })
                        });
                        if matches!(due, Ok(0)) {
                            let mut explicit = explicit_sync.take().expect("checked above");
                            explicit.result.queued = pending;
                            self.complete_explicit_sync(
                                &mut completed_generation,
                                explicit.target_generation,
                                explicit.result,
                            );
                            continue;
                        }
                    }
                    Err(error) => {
                        let message = format!("could not inspect the operation queue: {error}");
                        self.set_state("error", Some(message.clone())).await;
                        let mut explicit = explicit_sync.take().expect("checked above");
                        explicit.result.ok = false;
                        explicit.result.error = message;
                        self.complete_explicit_sync(
                            &mut completed_generation,
                            explicit.target_generation,
                            explicit.result,
                        );
                        continue;
                    }
                }
            }

            // Remote is the highest-priority mutation. Any local work waits
            // for a successful fresh delta, preserving the pull-before-upload
            // invariant; If-Match remains the final race guard at Graph.
            //
            // Let an already-started outbound slice publish its Graph result
            // before pulling again. A resumable create becomes visible in
            // delta just before run_upload commits the returned item locally;
            // observing our own half-committed upload in that window used to
            // manufacture a conflict copy. The scheduler yields at five-second
            // chunk boundaries; an in-flight HTTP chunk may overrun that target,
            // but polling resumes immediately after its result is durable.
            let remote_due = force_remote || Instant::now() >= next_remote;
            if remote_poll_ready(remote_due, outbound_transfer_running) {
                let mut changes = 0u64;
                let mut queued = 0u64;
                let mut failure = None;

                // A transient token-endpoint failure must recover on the
                // ordinary remote timer. A locked keyring is different: retry
                // that only after an explicit wake, otherwise Secret Service
                // can prompt the user on every timer tick.
                let retry_missing = {
                    let status = self.status.read().await;
                    missing_account_retry_due(explicit_wake, &status.state)
                };
                if retry_missing {
                    if let Err(error) = self.attach_missing_accounts().await {
                        tracing::warn!(error = ?error, "could not restore configured account");
                        failure = Some(self.report_anyhow_error(&error).await);
                    }
                }

                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                if failure.is_none() {
                    for engine in &engines {
                        match engine.sync_remote().await {
                            Ok(summary) => {
                                changes = changes.saturating_add(summary.enumerated);
                                queued = queued.saturating_add(summary.still_queued);
                            }
                            Err(error) => {
                                failure = Some(self.report_core_error(&error).await);
                                break;
                            }
                        }
                    }
                }
                if let Err(error) = self.register_pending_placeholders(&engines).await {
                    self.set_state("error", Some(error.to_string())).await;
                    failure.get_or_insert_with(|| error.to_string());
                }

                let success = failure.is_none();
                if success {
                    retry_interval = base;
                    next_remote = Instant::now() + base;
                    remote_required_for_local = false;
                    download_work = true;
                    queue_work = true;
                    if auto_free_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let app = self.clone();
                        let running = auto_free_running.clone();
                        tokio::spawn(async move {
                            app.auto_free_if_low().await;
                            running.store(false, Ordering::Release);
                        });
                    }
                } else {
                    next_remote = Instant::now() + retry_interval;
                    retry_interval = ceiling.min(retry_interval.saturating_mul(2));
                    remote_required_for_local = true;
                    download_work = false;
                    queue_work = false;
                }
                force_remote = false;
                explicit_wake = false;

                let target = self.requested_sync_generation.load(Ordering::Acquire);
                let newest_tracked = explicit_sync
                    .as_ref()
                    .map_or(completed_generation, |explicit| explicit.target_generation);
                if target > newest_tracked {
                    let result = SyncRequestResult {
                        ok: success,
                        changes,
                        queued,
                        error: failure.clone().unwrap_or_default(),
                    };
                    if success {
                        explicit_sync = Some(ExplicitSync {
                            target_generation: target,
                            result,
                        });
                    } else {
                        self.complete_explicit_sync(&mut completed_generation, target, result);
                    }
                } else if !success {
                    if let Some(mut explicit) = explicit_sync.take() {
                        explicit.result.ok = false;
                        explicit.result.error = failure.unwrap_or_default();
                        self.complete_explicit_sync(
                            &mut completed_generation,
                            explicit.target_generation,
                            explicit.result,
                        );
                    }
                }
                continue;
            }

            if !discovery_running && open_writes.is_idle() && Instant::now() >= next_full {
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                let tx = discovery_tx.clone();
                discovery_running = true;
                next_full = Instant::now() + full_interval;
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let mut reports = Vec::new();
                        for engine in engines {
                            let report = engine.discover_local_changes()?;
                            reports.push((engine, report));
                        }
                        Ok::<_, anyhow::Error>(reports)
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                    let _ = tx.send(FullScanBatch { result }).await;
                });
                continue;
            }

            if download_work && !download_transfer_running {
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                let deadline = Instant::now() + TRANSFER_SLICE;
                let tx = transfer_tx.clone();
                let download_gate = self.download_gate.clone();
                download_work = false;
                download_transfer_running = true;
                tokio::spawn(async move {
                    let _download_gate = download_gate.lock().await;
                    let mut results = Vec::with_capacity(engines.len());
                    for engine in engines {
                        results.push(engine.transfer_slice(deadline, QueueScope::Downloads).await);
                    }
                    let _ = tx
                        .send(TransferBatch {
                            scope: QueueScope::Downloads,
                            results,
                            exhausted: Instant::now() >= deadline,
                        })
                        .await;
                });
                continue;
            }

            if !dirty_paths.is_empty()
                && !remote_required_for_local
                && !local_scan_running
                && !open_writes.blocks(&dirty_paths)
            {
                let paths: Vec<_> = dirty_paths.drain().collect();
                let completed_writes = std::mem::take(&mut completed_writes);
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                let tx = local_scan_tx.clone();
                local_scan_running = true;
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        let results = engines
                            .iter()
                            .map(|engine| engine.sync_local_change_batch(&paths, &completed_writes))
                            .collect();
                        LocalScanBatch { engines, results }
                    })
                    .await
                    .map_err(anyhow::Error::from);
                    let _ = tx.send(result).await;
                });
                continue;
            }

            if queue_work && !outbound_transfer_running && !remote_required_for_local {
                let engines: Vec<_> = self.engines.read().await.values().cloned().collect();
                let deadline = Instant::now() + TRANSFER_SLICE;
                // A full verification may still discover more roots from the
                // same deletion wave. Keep uploads and moves responsive, but
                // never let a persisted DELETE outrun that complete view.
                let scope = outbound_scope(
                    discovery_running,
                    local_scan_running,
                    delete_verification_ready,
                );
                let tx = transfer_tx.clone();
                queue_work = false;
                outbound_transfer_running = true;
                tokio::spawn(async move {
                    let mut results = Vec::with_capacity(engines.len());
                    for engine in engines {
                        results.push(engine.transfer_slice(deadline, scope).await);
                    }
                    let _ = tx
                        .send(TransferBatch {
                            scope,
                            results,
                            exhausted: Instant::now() >= deadline,
                        })
                        .await;
                });
                continue;
            }

            let timer = if discovery_running || !open_writes.is_idle() {
                next_remote
            } else {
                next_remote.min(next_full)
            };
            let remote_due = force_remote || Instant::now() >= next_remote;
            tokio::select! {
                biased;
                _ = &mut shutdown => {
                    tracing::info!("shutting down");
                    return;
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(timer)),
                    if !remote_due || !outbound_transfer_running => {}
                Some(batch) = discovery_rx.recv(), if discovery_running => {
                    discovery_running = false;
                    let verification = self.consume_full_scan(batch.result, &mut dirty_paths).await;
                    delete_verification_ready = verification.is_some();
                    if verification == Some(true) {
                        remote_required_for_local = true;
                        force_remote = true;
                    }
                    next_full = if std::mem::take(&mut discovery_again) {
                        Instant::now()
                    } else if verification.is_none() {
                        Instant::now() + base.min(full_interval)
                    } else {
                        Instant::now() + full_interval
                    };
                }
                Some(batch) = transfer_rx.recv(), if download_transfer_running || outbound_transfer_running => {
                    let (scope, more) = self.consume_transfer(batch).await;
                    match scope {
                        QueueScope::Downloads => {
                            download_transfer_running = false;
                            download_work |= more;
                        }
                        QueueScope::Outbound | QueueScope::OutboundNonDelete => {
                            outbound_transfer_running = false;
                            queue_work |= more;
                        }
                        QueueScope::All => {
                            download_transfer_running = false;
                            outbound_transfer_running = false;
                            download_work |= more;
                            queue_work |= more;
                        }
                    }
                }
                Some(result) = local_scan_rx.recv(), if local_scan_running => {
                    local_scan_running = false;
                    let scan_complete = self.consume_local_scan(result).await;
                    queue_work |= scan_complete;
                    if !scan_complete {
                        delete_verification_ready = false;
                        if discovery_running { discovery_again = true; }
                        else { next_full = Instant::now(); }
                    }
                }
                batch = receive_local_change(&mut local_changes) => {
                    match batch {
                        Some(batch) => {
                            dirty_paths.extend(batch.paths);
                            completed_writes.extend(batch.completed_writes);
                            remote_required_for_local = true;
                            force_remote = true;
                            if batch.force_full {
                                if discovery_running { discovery_again = true; }
                                else { next_full = Instant::now(); }
                            }
                        }
                        None => local_changes = None,
                    }
                }
                _ = self.wake.notified() => {
                    force_remote = true;
                    explicit_wake = true;
                    if self.force_full_scan.swap(false, Ordering::AcqRel) {
                        if discovery_running { discovery_again = true; }
                        else { next_full = Instant::now(); }
                    }
                }
            }
        }
    }
}

/// Decide whether an unattached account may be retried without user input.
/// Only `offline` is self-healing. A locked keyring could repeatedly display
/// an unlock prompt, and a rejected refresh token needs a real sign-in.
pub(super) fn missing_account_retry_due(explicit_wake: bool, state: &str) -> bool {
    explicit_wake || state == "offline"
}

/// A delta must not observe a remote mutation that an outbound worker has
/// completed at Graph but has not committed to the local store yet.
pub(super) fn remote_poll_ready(remote_due: bool, outbound_running: bool) -> bool {
    remote_due && !outbound_running
}

pub(super) fn outbound_scope(
    discovery_running: bool,
    local_scan_running: bool,
    delete_verification_ready: bool,
) -> QueueScope {
    if discovery_running || local_scan_running || !delete_verification_ready {
        QueueScope::OutboundNonDelete
    } else {
        QueueScope::Outbound
    }
}

pub(super) fn sync_barrier_ready(
    local_scan_running: bool,
    download_running: bool,
    outbound_running: bool,
    remote_required: bool,
    has_dirty_paths: bool,
    download_work: bool,
    queue_work: bool,
) -> bool {
    !local_scan_running
        && !download_running
        && !outbound_running
        && !remote_required
        && !has_dirty_paths
        && !download_work
        && !queue_work
}

async fn receive_local_change(
    receiver: &mut Option<tokio::sync::mpsc::Receiver<crate::watcher::LocalChangeBatch>>,
) -> Option<crate::watcher::LocalChangeBatch> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}
