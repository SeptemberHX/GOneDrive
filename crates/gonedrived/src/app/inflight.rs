//! The hydrations this daemon has in flight, and the per-path locks that
//! serialise them.
//!
//! Kept apart from `App` so the bookkeeping can be exercised on its own.
//! `App` cannot be built in a test — its constructor talks to the Secret
//! Service — which is why a placeholder whose fill failed could leak its path
//! guard and its `Hydrating` row for a whole session before anyone noticed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use gonedrive_ipc::Hydration;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

/// A placeholder currently being filled.
pub(super) struct InFlight {
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
pub(super) struct PathLocks {
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
pub(super) struct ReleaseBarrier<'a> {
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
    pub(super) async fn acquire(&self, path: &std::path::Path) -> OwnedMutexGuard<()> {
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
    pub(super) async fn acquire_many(
        &self,
        paths: &[std::path::PathBuf],
    ) -> Vec<OwnedMutexGuard<()>> {
        let mut ordered = paths.to_vec();
        ordered.sort();
        ordered.dedup();
        let mut guards = Vec::with_capacity(ordered.len());
        for path in ordered {
            guards.push(self.acquire(&path).await);
        }
        guards
    }

    pub(super) fn begin_release(&self, paths: &[std::path::PathBuf]) -> ReleaseBarrier<'_> {
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

    pub(super) fn is_releasing(&self, path: &std::path::Path) -> bool {
        self.releasing
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(path)
    }
}

/// Whether a hydration may start, and if so how to cancel it.
pub(super) enum HydrationStart {
    Started(Arc<AtomicBool>),
    AlreadyMaterialized,
    Releasing,
}

/// Every hydration this daemon is waiting on, keyed by path.
#[derive(Default)]
pub(super) struct InFlightHydrations {
    entries: RwLock<HashMap<PathBuf, InFlight>>,
    locks: PathLocks,
}

impl InFlightHydrations {
    /// Register a hydration and take its path guard, unless the path is
    /// already materialised or being released.
    pub(super) async fn begin(
        &self,
        path: &Path,
        visible: bool,
        request_id: u64,
    ) -> HydrationStart {
        let guard = self.locks.acquire(path).await;
        if self.locks.is_releasing(path) {
            return HydrationStart::Releasing;
        }
        if gonedrive_core::hydrate::Placeholder::state(path)
            == gonedrive_core::hydrate::PlaceholderState::Hydrated
        {
            return HydrationStart::AlreadyMaterialized;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.entries.write().await.insert(
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

    /// Replace the progress an entry reports, if one is still registered.
    pub(super) async fn set_progress(&self, path: &Path, hydration: Hydration) {
        if let Some(entry) = self.entries.write().await.get_mut(path) {
            entry.hydration = hydration;
        }
    }

    /// Advance an entry's byte counts and return what to publish.
    pub(super) async fn advance(
        &self,
        path: &Path,
        transferred: u64,
        total: u64,
    ) -> Option<Hydration> {
        let mut entries = self.entries.write().await;
        let entry = entries.get_mut(path)?;
        entry.hydration.transferred = transferred;
        entry.hydration.total = total;
        Some(entry.hydration.clone())
    }

    /// Drop an entry that no terminal handshake has claimed.
    ///
    /// The handshake keeps the path guard until its work is done, so an
    /// asynchronous engine event must not pull that guard out from under it.
    pub(super) async fn remove_unclaimed(&self, path: &Path) {
        let mut entries = self.entries.write().await;
        if !entries.get(path).is_some_and(|entry| entry.finishing) {
            entries.remove(path);
        }
    }

    pub(super) async fn contains(&self, path: &Path) -> bool {
        self.entries.read().await.contains_key(path)
    }

    /// The fills a progress window should show.
    pub(super) async fn visible(&self) -> Vec<Hydration> {
        self.entries
            .read()
            .await
            .values()
            .filter(|entry| entry.visible)
            .map(|entry| entry.hydration.clone())
            .collect()
    }

    /// Ask the transfer to stop. False when nothing is in flight for `path`.
    pub(super) async fn cancel(&self, path: &Path) -> bool {
        match self.entries.read().await.get(path) {
            Some(entry) => {
                entry.cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Record where this request staged its bytes, if it still owns the entry.
    pub(super) async fn set_staged(&self, path: &Path, request_id: u64, staged: PathBuf) {
        if let Some(entry) = self.entries.write().await.get_mut(path) {
            if entry.request_id == request_id {
                entry.staged = Some(staged);
            }
        }
    }

    /// Take back the staging path this request registered.
    pub(super) async fn take_staged(&self, path: &Path, request_id: u64) -> Option<PathBuf> {
        let mut entries = self.entries.write().await;
        let entry = entries.get_mut(path)?;
        if entry.request_id != request_id {
            tracing::debug!(
                path = %path.display(),
                request_id,
                current = entry.request_id,
                "ignoring stale hydration staging cleanup"
            );
            return None;
        }
        entry.staged.take()
    }

    /// Claim the one terminal callback for this request. `finishing` stays set
    /// until the caller removes the entry, so a duplicate callback cannot race
    /// the filesystem and database work or replace the path guard under it.
    pub(super) async fn claim_completion(&self, path: &Path, request_id: u64) -> bool {
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get_mut(path) else {
            tracing::debug!(path = %path.display(), request_id, "ignoring stale hydration completion");
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
            tracing::debug!(path = %path.display(), request_id, "ignoring duplicate hydration completion");
            return false;
        }
        entry.finishing = true;
        true
    }

    /// Claim the terminal callback, distinguishing "no entry" from "not
    /// yours". An absent entry is the helper's materialised-range fast path,
    /// which keeps its mark until this acknowledgement so a concurrent release
    /// cannot lose protection in between — not a stale completion to ignore.
    pub(super) async fn claim_completion_or_fast_path(
        &self,
        path: &Path,
        request_id: u64,
    ) -> Option<bool> {
        let mut entries = self.entries.write().await;
        match entries.get_mut(path) {
            None => Some(true),
            Some(entry) if entry.request_id != request_id => {
                tracing::debug!(
                    path = %path.display(),
                    request_id,
                    current = entry.request_id,
                    "ignoring stale hydration completion"
                );
                None
            }
            Some(entry) if entry.finishing => {
                tracing::debug!(path = %path.display(), request_id, "ignoring duplicate hydration completion");
                None
            }
            Some(entry) => {
                entry.finishing = true;
                Some(false)
            }
        }
    }

    pub(super) async fn remove_if_current(&self, path: &Path, request_id: u64) {
        let mut entries = self.entries.write().await;
        if entries
            .get(path)
            .is_some_and(|entry| entry.request_id == request_id)
        {
            entries.remove(path);
        }
    }

    /// Cancel and forget everything under `root`, returning each abandoned
    /// path with whatever it had staged. Passing `None` abandons all of them.
    pub(super) async fn abandon(&self, root: Option<&Path>) -> Vec<(PathBuf, Option<PathBuf>)> {
        let mut entries = self.entries.write().await;
        let doomed: Vec<_> = entries
            .keys()
            .filter(|path| root.is_none_or(|root| path.starts_with(root)))
            .cloned()
            .collect();
        let mut abandoned = Vec::with_capacity(doomed.len());
        for path in doomed {
            if let Some(entry) = entries.remove(&path) {
                entry.cancel.store(true, Ordering::Relaxed);
                abandoned.push((path, entry.staged));
            }
        }
        abandoned
    }

    // ---- the path locks, which outlive any one entry ----------------------

    pub(super) async fn acquire(&self, path: &Path) -> OwnedMutexGuard<()> {
        self.locks.acquire(path).await
    }

    pub(super) async fn acquire_many(&self, paths: &[PathBuf]) -> Vec<OwnedMutexGuard<()>> {
        self.locks.acquire_many(paths).await
    }

    pub(super) fn is_releasing(&self, path: &Path) -> bool {
        self.locks.is_releasing(path)
    }

    pub(super) fn begin_release(&self, paths: &[PathBuf]) -> ReleaseBarrier<'_> {
        self.locks.begin_release(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_placeholder(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[tokio::test]
    async fn abandoning_a_hydration_releases_the_path_for_the_next_reader() {
        // The failure this covers cost a whole session once: a fill that
        // errored left its entry and its path guard behind, so every later
        // read of that file waited out the helper's five-minute timeout.
        let dir = tempfile::tempdir().unwrap();
        let path = temp_placeholder(&dir, "report.bin");
        let inflight = InFlightHydrations::default();

        assert!(matches!(
            inflight.begin(&path, true, 1).await,
            HydrationStart::Started(_)
        ));
        assert!(inflight.contains(&path).await);

        let abandoned = inflight.abandon(None).await;
        assert_eq!(abandoned.len(), 1);
        assert!(!inflight.contains(&path).await);

        // The guard is gone, so a second reader gets in rather than blocking.
        let next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            inflight.begin(&path, true, 2),
        )
        .await
        .expect("the path guard must have been released");
        assert!(matches!(next, HydrationStart::Started(_)));
    }

    #[tokio::test]
    async fn only_the_request_that_owns_an_entry_may_complete_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_placeholder(&dir, "report.bin");
        let inflight = InFlightHydrations::default();
        inflight.begin(&path, true, 7).await;

        assert!(
            !inflight.claim_completion(&path, 6).await,
            "a late callback from an older request is refused"
        );
        assert!(inflight.claim_completion(&path, 7).await);
        assert!(
            !inflight.claim_completion(&path, 7).await,
            "fanotify can report the same completion twice"
        );

        inflight.remove_if_current(&path, 6).await;
        assert!(inflight.contains(&path).await, "a stale id removes nothing");
        inflight.remove_if_current(&path, 7).await;
        assert!(!inflight.contains(&path).await);
    }

    #[tokio::test]
    async fn abandoning_is_scoped_to_one_account_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a")).unwrap();
        std::fs::create_dir_all(dir.path().join("b")).unwrap();
        let mine = temp_placeholder(&dir, "a/mine.bin");
        let theirs = temp_placeholder(&dir, "b/theirs.bin");
        let inflight = InFlightHydrations::default();
        inflight.begin(&mine, true, 1).await;
        inflight.begin(&theirs, true, 2).await;

        let abandoned = inflight.abandon(Some(&dir.path().join("a"))).await;
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].0, mine);
        assert!(!inflight.contains(&mine).await);
        assert!(
            inflight.contains(&theirs).await,
            "another account's fills are untouched"
        );
    }

    #[tokio::test]
    async fn a_release_in_progress_turns_a_late_read_away() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_placeholder(&dir, "report.bin");
        let inflight = InFlightHydrations::default();

        let _barrier = inflight.begin_release(std::slice::from_ref(&path));
        assert!(matches!(
            inflight.begin(&path, true, 1).await,
            HydrationStart::Releasing
        ));
    }

    #[tokio::test]
    async fn staging_is_only_recorded_and_reclaimed_by_its_own_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = temp_placeholder(&dir, "report.bin");
        let staged = dir.path().join(".report.bin.gonedrive-partial");
        let inflight = InFlightHydrations::default();
        inflight.begin(&path, true, 3).await;

        inflight.set_staged(&path, 2, staged.clone()).await;
        assert_eq!(inflight.take_staged(&path, 3).await, None);

        inflight.set_staged(&path, 3, staged.clone()).await;
        assert_eq!(inflight.take_staged(&path, 2).await, None);
        assert_eq!(inflight.take_staged(&path, 3).await, Some(staged));
    }
}
