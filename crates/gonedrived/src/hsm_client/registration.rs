//! Placeholder mark registration, batching, reconciliation, and reconnect sets.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::Result;
use gonedrive_ipc::hsm::{Request, WatchOutcome, MAX_WATCH_BATCH};
use tokio::sync::{oneshot, Mutex};

use super::HsmClient;
use crate::app::App;
use crate::failure_summary::FailureSummary;

pub(super) type MarkResult = std::result::Result<Vec<WatchOutcome>, String>;
pub(super) type MarkWaiters = HashMap<u64, oneshot::Sender<MarkResult>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileIdentity {
    pub(super) dev: u64,
    pub(super) ino: u64,
}

pub(super) type WatchedFiles = HashMap<PathBuf, FileIdentity>;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct HelperFeatures {
    pub(super) received: bool,
    pub(super) batch_marks: bool,
    pub(super) mark_sweeps: bool,
    pub(super) directory_open_notifications: bool,
    pub(super) sync_roots: bool,
}

/// Leave room for JSON escaping and for the helper's result frame.
const WATCH_BATCH_TARGET_BYTES: usize = 48 * 1024;
const FEATURE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

impl HsmClient {
    pub(crate) async fn watched_count(&self) -> usize {
        self.watched.lock().await.len() + self.watched_directories.lock().await.len()
    }

    /// Ask the kernel to intercept reads of this placeholder.
    pub async fn watch(&self, path: &Path) -> Result<()> {
        let _registration = self.registration.lock().await;
        self.watch_unlocked(path).await
    }

    async fn watch_unlocked(&self, path: &Path) -> Result<()> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let mut outcomes = self
            .request_marks(
                id,
                Request::Watch {
                    id,
                    path: path.to_string_lossy().into_owned(),
                },
            )
            .await?;
        if outcomes.len() != 1 {
            anyhow::bail!(
                "helper returned {} results for one watch request",
                outcomes.len()
            );
        }
        let identity = identity_from_outcome(outcomes.remove(0))?;
        self.watched
            .lock()
            .await
            .insert(path.to_path_buf(), identity);
        Ok(())
    }

    /// Stop intercepting: the file is hydrated, deleted, or no longer ours.
    pub async fn unwatch(&self, path: &Path) -> Result<()> {
        let _registration = self.registration.lock().await;
        if !self.watched.lock().await.contains_key(path) {
            return Ok(());
        }
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let outcomes = self
            .request_marks(
                id,
                Request::Unwatch {
                    id,
                    path: path.to_string_lossy().into_owned(),
                },
            )
            .await?;
        if outcomes.len() != 1 || !outcomes[0].ok {
            anyhow::bail!(
                "helper rejected unwatch: {}",
                outcomes
                    .first()
                    .and_then(|outcome| outcome.message.as_deref())
                    .unwrap_or("invalid acknowledgement")
            );
        }
        self.watched.lock().await.remove(path);
        Ok(())
    }

    /// Stop listening for opens after a directory's direct child list has
    /// become complete.
    pub async fn unwatch_directory(&self, path: &Path) -> Result<()> {
        let _registration = self.registration.lock().await;
        if !self.watched_directories.lock().await.contains_key(path) {
            return Ok(());
        }
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let outcomes = self
            .request_marks(
                id,
                Request::Unwatch {
                    id,
                    path: path.to_string_lossy().into_owned(),
                },
            )
            .await?;
        if outcomes.len() != 1 || !outcomes[0].ok {
            anyhow::bail!(
                "helper rejected directory unwatch: {}",
                outcomes
                    .first()
                    .and_then(|outcome| outcome.message.as_deref())
                    .unwrap_or("invalid acknowledgement")
            );
        }
        self.watched_directories.lock().await.remove(path);
        Ok(())
    }

    async fn request_marks(&self, id: u64, request: Request) -> Result<Vec<WatchOutcome>> {
        const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

        let (tx, rx) = oneshot::channel();
        self.pending_marks.lock().await.insert(id, tx);
        if let Err(error) = self.send(&request).await {
            self.pending_marks.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(ACK_TIMEOUT, rx).await {
            Ok(Ok(Ok(outcomes))) => Ok(outcomes),
            Ok(Ok(Err(message))) => anyhow::bail!(message),
            Ok(Err(_)) => anyhow::bail!("helper dropped watch request {id}"),
            Err(_) => {
                self.pending_marks.lock().await.remove(&id);
                anyhow::bail!(
                    "helper did not acknowledge watch request {id} within {ACK_TIMEOUT:?}"
                )
            }
        }
    }

    async fn negotiated_features(&self) -> HelperFeatures {
        let mut features = self.features.clone();
        if !features.borrow().received {
            let _ = tokio::time::timeout(FEATURE_WAIT, async {
                while features.changed().await.is_ok() {
                    if features.borrow().received {
                        break;
                    }
                }
            })
            .await;
        }
        let negotiated = *features.borrow();
        negotiated
    }

    async fn batch_marks_available(&self) -> bool {
        self.negotiated_features().await.batch_marks
    }

    /// Tell the helper which tree it may touch on our behalf.
    ///
    /// A helper that enforces roots refuses every mark and every staged file
    /// outside them, so this precedes the first watch on a new connection.
    /// Returns whether the helper understood the declaration; an older helper
    /// is served unconfined, as before.
    pub(super) async fn register_sync_roots(&self, root: &Path) -> Result<bool> {
        if !self.negotiated_features().await.sync_roots {
            return Ok(false);
        }
        self.request_control(|id| Request::RegisterRoots {
            id,
            paths: vec![root.to_string_lossy().into_owned()],
        })
        .await?;
        Ok(true)
    }

    /// Register every placeholder currently on disk.
    pub async fn watch_all(&self, app: &App) -> Result<usize> {
        let (paths, missing) = app.placeholder_inventory().await?;
        let directories = app.population_watch_inventory().await?;
        let features = self.negotiated_features().await;
        let sweep = features.mark_sweeps
            && self.watched.lock().await.is_empty()
            && self.watched_directories.lock().await.is_empty();
        if sweep {
            self.request_control(|id| Request::BeginWatchSet { id })
                .await?;
        } else {
            self.prune_stale_watches().await?;
            self.prune_stale_directory_watches().await?;
        }

        let paths = if sweep {
            paths
        } else {
            let watched = self.watched.lock().await;
            paths
                .into_iter()
                .filter(|path| !watched.contains_key(path))
                .collect()
        };
        let watched = match self.watch_paths(app, &paths).await {
            Ok(watched) => watched,
            Err(error) => {
                if sweep {
                    let _ = self
                        .request_control(|id| Request::AbortWatchSet { id })
                        .await;
                }
                return Err(error);
            }
        };
        if !directories.is_empty() && !features.directory_open_notifications {
            if sweep {
                self.request_control(|id| Request::CommitWatchSet { id })
                    .await?;
            }
            let restored = app.restore_missing_placeholders(self, &missing).await?;
            anyhow::bail!(
                "the installed files-on-demand helper is too old for lazy directory population; update and restart gonedrive-hsm (protected {watched} file placeholder(s), restored {restored})"
            );
        }
        let directories = if sweep {
            directories
        } else {
            let watched = self.watched_directories.lock().await;
            directories
                .into_iter()
                .filter(|path| !watched.contains_key(path))
                .collect()
        };
        let watched_directories = match self.watch_directory_paths(&directories).await {
            Ok(watched) => watched,
            Err(error) => {
                if sweep {
                    let _ = self
                        .request_control(|id| Request::AbortWatchSet { id })
                        .await;
                }
                return Err(error);
            }
        };
        if sweep {
            self.request_control(|id| Request::CommitWatchSet { id })
                .await?;
        }
        let restored = app.restore_missing_placeholders(self, &missing).await?;
        Ok(watched
            .saturating_add(watched_directories)
            .saturating_add(restored))
    }

    /// Register only placeholders created or replaced by the just-finished
    /// coordinator phase.
    pub async fn watch_paths(&self, app: &App, paths: &[PathBuf]) -> Result<usize> {
        // Materialisation and watcher reconciliation can discover the same
        // batch. Keep cleanup ownership with one registration pass at a time.
        let _registration = self.registration.lock().await;
        let use_batches = self.batch_marks_available().await;
        let mut watched = 0usize;
        let mut failures = FailureSummary::default();
        let mut cleanup_failures = FailureSummary::default();
        let mut cursor = 0usize;
        while cursor < paths.len() {
            let end = if use_batches {
                next_watch_batch_end(paths, cursor)
            } else {
                cursor + 1
            };
            let chunk = &paths[cursor..end];
            let outcomes = match self.request_watch_chunk(chunk, use_batches, false).await {
                Ok(outcomes) if outcomes.len() == chunk.len() => outcomes,
                Ok(outcomes) => {
                    let error = format!(
                        "helper returned {} results for {} paths",
                        outcomes.len(),
                        chunk.len()
                    );
                    safety_lock_tail(app, &paths[cursor..], &mut cleanup_failures).await;
                    anyhow::bail!(
                        "{error}; safety-locked the unconfirmed registration tail{}",
                        cleanup_suffix(&cleanup_failures)
                    );
                }
                Err(error) => {
                    // This client has already completed its authenticated Pong
                    // handshake. A transport failure now means the protection
                    // state of the unsent tail is unknown, so fail closed. A
                    // connection rejected during startup never reaches here.
                    safety_lock_tail(app, &paths[cursor..], &mut cleanup_failures).await;
                    anyhow::bail!(
                        "helper registration transport failed: {error:#}; safety-locked the unconfirmed registration tail{}",
                        cleanup_suffix(&cleanup_failures)
                    );
                }
            };

            let mut accepted = Vec::new();
            for (path, outcome) in chunk.iter().zip(outcomes) {
                match identity_from_outcome(outcome) {
                    Ok(identity) => {
                        accepted.push((path.clone(), identity));
                        watched += 1;
                    }
                    Err(error) => {
                        failures.record(|| format!("{}: {error:#}", path.display()));
                        if let Err(lock_error) = app.lock_unwatched_placeholder(path).await {
                            cleanup_failures
                                .record(|| format!("{}: {lock_error:#}", path.display()));
                        }
                    }
                }
            }
            if !accepted.is_empty() {
                self.watched.lock().await.extend(accepted);
            }
            cursor = end;
        }
        if failures.is_empty() {
            return Ok(watched);
        }

        tracing::warn!(
            watched,
            failed = failures.total(),
            cleanup_failed = cleanup_failures.total(),
            examples = failures.describe(),
            cleanup_examples = cleanup_failures.describe(),
            "placeholder registration failed"
        );
        let cleanup = if cleanup_failures.is_empty() {
            "; each rejected placeholder was safety-locked".to_string()
        } else {
            format!(
                "; failed to safety-lock {} placeholder(s): {}",
                cleanup_failures.total(),
                cleanup_failures.describe()
            )
        };
        anyhow::bail!(
            "failed to register {} placeholder(s); registration examples: {}{}",
            failures.total(),
            failures.describe(),
            cleanup
        )
    }

    /// Register populated parents as direct-child enumeration boundaries.
    /// Individual path failures are logged and retried later; unlike an
    /// unmarked sparse file, an unmarked directory cannot expose file bytes.
    pub async fn watch_directory_paths(&self, paths: &[PathBuf]) -> Result<usize> {
        if paths.is_empty() {
            return Ok(0);
        }
        let _registration = self.registration.lock().await;
        let paths: Vec<_> = {
            let watched = self.watched_directories.lock().await;
            paths
                .iter()
                .filter(|path| !watched.contains_key(path.as_path()))
                .cloned()
                .collect()
        };
        if paths.is_empty() {
            return Ok(0);
        }
        let features = self.negotiated_features().await;
        if !features.directory_open_notifications {
            anyhow::bail!("the files-on-demand helper does not support directory-open marks");
        }
        let use_batches = features.batch_marks;
        let mut watched_count = 0usize;
        let mut failures = FailureSummary::default();
        let mut cursor = 0usize;
        while cursor < paths.len() {
            let end = if use_batches {
                next_watch_batch_end(&paths, cursor)
            } else {
                cursor + 1
            };
            let chunk = &paths[cursor..end];
            let outcomes = self.request_watch_chunk(chunk, use_batches, true).await?;
            if outcomes.len() != chunk.len() {
                anyhow::bail!(
                    "helper returned {} results for {} directory marks",
                    outcomes.len(),
                    chunk.len()
                );
            }
            let mut accepted = Vec::new();
            for (path, outcome) in chunk.iter().zip(outcomes) {
                match identity_from_outcome(outcome) {
                    Ok(identity) => {
                        accepted.push((path.clone(), identity));
                        watched_count += 1;
                    }
                    Err(error) => {
                        failures.record(|| format!("{}: {error:#}", path.display()));
                    }
                }
            }
            self.watched_directories.lock().await.extend(accepted);
            cursor = end;
        }
        if !failures.is_empty() {
            tracing::warn!(
                watched = watched_count,
                failed = failures.total(),
                examples = failures.describe(),
                "directory-population registration was incomplete"
            );
        }
        Ok(watched_count)
    }

    async fn request_watch_chunk(
        &self,
        paths: &[PathBuf],
        use_batches: bool,
        directories: bool,
    ) -> Result<Vec<WatchOutcome>> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let request = watch_request(id, paths, use_batches, directories);
        self.request_marks(id, request).await
    }

    async fn request_control(&self, request: impl FnOnce(u64) -> Request) -> Result<()> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let outcomes = self.request_marks(id, request(id)).await?;
        if outcomes.len() == 1 && outcomes[0].ok {
            Ok(())
        } else {
            anyhow::bail!(
                "helper rejected mark-set operation: {}",
                outcomes
                    .first()
                    .and_then(|outcome| outcome.message.as_deref())
                    .unwrap_or("invalid acknowledgement")
            )
        }
    }

    /// Reconcile identities found by the low-frequency read-only verifier.
    pub async fn reconcile_observed(
        &self,
        app: &App,
        placeholders: &[(PathBuf, u64, u64)],
    ) -> Result<usize> {
        let mut changed = Vec::new();
        {
            let watched = self.watched.lock().await;
            for (path, dev, ino) in placeholders {
                if gonedrive_core::hydrate::Placeholder::state(path)
                    != gonedrive_core::hydrate::PlaceholderState::Dehydrated
                {
                    continue;
                }
                let identity = FileIdentity {
                    dev: *dev,
                    ino: *ino,
                };
                if watched.get(path) != Some(&identity) {
                    changed.push(path.clone());
                }
            }
        }
        // A rename keeps the inode mark. Refresh the new path before pruning
        // the old one so there is no unprotected interval.
        let registered = self.watch_paths(app, &changed).await?;
        self.prune_stale_watches().await?;
        Ok(registered)
    }

    async fn prune_stale_watches(&self) -> Result<()> {
        let stale: Vec<_> = self
            .watched
            .lock()
            .await
            .keys()
            .filter(|path| {
                let state = gonedrive_core::hydrate::Placeholder::state(path);
                state != gonedrive_core::hydrate::PlaceholderState::Dehydrated
                    && !is_safety_locked_file(path)
            })
            .cloned()
            .collect();
        let mut failures = FailureSummary::default();
        for path in stale {
            if let Err(error) = self.unwatch(&path).await {
                failures.record(|| format!("{}: {error:#}", path.display()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "failed to remove {} stale placeholder mark(s): {}",
                failures.total(),
                failures.describe()
            )
        }
    }

    async fn prune_stale_directory_watches(&self) -> Result<()> {
        let stale: Vec<_> = self
            .watched_directories
            .lock()
            .await
            .keys()
            .filter(|path| {
                gonedrive_core::hydrate::Placeholder::directory_population(path)
                    != gonedrive_core::hydrate::DirectoryPopulation::Populated
            })
            .cloned()
            .collect();
        let mut failures = FailureSummary::default();
        for path in stale {
            if let Err(error) = self.unwatch_directory(&path).await {
                failures.record(|| format!("{}: {error:#}", path.display()));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "failed to remove {} stale directory mark(s): {}",
                failures.total(),
                failures.describe()
            )
        }
    }
}

fn watch_request(id: u64, paths: &[PathBuf], use_batches: bool, directories: bool) -> Request {
    let encoded_paths = || {
        paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    };
    if directories && use_batches {
        Request::WatchDirectoryBatch {
            id,
            paths: encoded_paths(),
        }
    } else if directories {
        Request::WatchDirectory {
            id,
            path: paths[0].to_string_lossy().into_owned(),
        }
    } else if use_batches {
        Request::WatchBatch {
            id,
            paths: encoded_paths(),
        }
    } else {
        Request::Watch {
            id,
            path: paths[0].to_string_lossy().into_owned(),
        }
    }
}

fn next_watch_batch_end(paths: &[PathBuf], start: usize) -> usize {
    let mut end = start;
    let mut encoded_bytes = 128usize;
    while end < paths.len() && end - start < MAX_WATCH_BATCH {
        let path = paths[end].to_string_lossy();
        let path_bytes = serde_json::to_string(path.as_ref())
            .expect("serializing a string cannot fail")
            .len()
            + 1;
        if end > start && encoded_bytes.saturating_add(path_bytes) > WATCH_BATCH_TARGET_BYTES {
            break;
        }
        encoded_bytes = encoded_bytes.saturating_add(path_bytes);
        end += 1;
    }
    end.max(start + 1)
}

fn identity_from_outcome(outcome: WatchOutcome) -> Result<FileIdentity> {
    if !outcome.ok {
        anyhow::bail!(
            "{}",
            outcome
                .message
                .unwrap_or_else(|| "fanotify mark failed".into())
        );
    }
    match (outcome.dev, outcome.ino) {
        (Some(dev), Some(ino)) => Ok(FileIdentity { dev, ino }),
        _ => anyhow::bail!("helper did not return the marked file identity"),
    }
}

async fn safety_lock_tail(app: &App, paths: &[PathBuf], failures: &mut FailureSummary) {
    for path in paths {
        if let Err(error) = app.lock_unwatched_placeholder(path).await {
            failures.record(|| format!("{}: {error:#}", path.display()));
        }
    }
}

fn cleanup_suffix(failures: &FailureSummary) -> String {
    if failures.is_empty() {
        String::new()
    } else {
        format!(
            "; failed to lock {} path(s): {}",
            failures.total(),
            failures.describe()
        )
    }
}

fn is_safety_locked_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_file() && metadata.permissions().mode() & 0o7777 == 0
    })
}

pub(super) async fn fail_pending_marks(pending: &Mutex<MarkWaiters>, message: &str) {
    let requests: Vec<_> = pending
        .lock()
        .await
        .drain()
        .map(|(_, sender)| sender)
        .collect();
    for sender in requests {
        let _ = sender.send(Err(message.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_batches_are_count_and_frame_bounded() {
        let paths: Vec<_> = (0..1000)
            .map(|index| PathBuf::from(format!("/tmp/cloud/{index:04}/file.bin")))
            .collect();
        let mut cursor = 0;
        while cursor < paths.len() {
            let end = next_watch_batch_end(&paths, cursor);
            assert!(end > cursor);
            assert!(end - cursor <= MAX_WATCH_BATCH);
            let request = Request::WatchBatch {
                id: 1,
                paths: paths[cursor..end]
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            };
            assert!(serde_json::to_string(&request).unwrap().len() <= WATCH_BATCH_TARGET_BYTES);
            cursor = end;
        }
    }

    #[test]
    fn json_escaped_paths_still_respect_the_batch_target() {
        let component = "quote-\\\"-".repeat(100);
        let paths: Vec<_> = (0..MAX_WATCH_BATCH)
            .map(|index| PathBuf::from(format!("/tmp/{component}/{index}")))
            .collect();
        let end = next_watch_batch_end(&paths, 0);
        let request = Request::WatchBatch {
            id: u64::MAX,
            paths: paths[..end]
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        };
        assert!(serde_json::to_string(&request).unwrap().len() <= WATCH_BATCH_TARGET_BYTES);
    }

    #[test]
    fn a_success_without_an_inode_is_not_accepted() {
        let error = identity_from_outcome(WatchOutcome {
            ok: true,
            message: None,
            dev: Some(1),
            ino: None,
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("identity"), "{error}");
    }

    #[test]
    fn directory_boundaries_use_the_negotiated_directory_protocol() {
        let paths = vec![PathBuf::from("/sync"), PathBuf::from("/sync/Opened")];
        assert!(matches!(
            watch_request(3, &paths, true, true),
            Request::WatchDirectoryBatch { id: 3, paths } if paths.len() == 2
        ));
        assert!(matches!(
            watch_request(4, &paths[..1], false, true),
            Request::WatchDirectory { id: 4, .. }
        ));
        assert!(matches!(
            watch_request(5, &paths, true, false),
            Request::WatchBatch { id: 5, .. }
        ));
    }

    #[test]
    fn a_mode_zero_recovery_target_is_not_pruned_as_unmanaged() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("locked-placeholder");
        std::fs::write(&path, []).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
        assert!(is_safety_locked_file(&path));
    }
}
