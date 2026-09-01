//! Local change detection.
//!
//! inotify is an optimisation, not the source of truth: its queue can overflow,
//! it sees nothing while the daemon is stopped, and the kernel caps how many
//! watches a user may hold. The periodic full scan is what guarantees
//! convergence — this just makes the common case feel immediate.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::event::{AccessKind, AccessMode, CreateKind, EventKind, ModifyKind, RenameMode};
use notify::{Event, RecursiveMode, Watcher};

const EVENT_QUEUE: usize = 4096;
const BATCH_QUEUE: usize = 64;

/// Files the client writes itself, whose events would otherwise wake the loop
/// once per chunk of every download.
const IGNORED_SUFFIXES: &[&str] = &[".gonedrive-partial"];

pub struct LocalWatcher {
    /// Dropping this stops the watch.
    _watcher: notify::RecommendedWatcher,
    open_writes: OpenWrites,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalChangeKind {
    CloseWrite,
    Create,
    Remove,
    Modify,
    Rename,
    Other,
}

#[derive(Debug)]
struct LocalChange {
    kind: LocalChangeKind,
    paths: Vec<PathBuf>,
}

/// One quiet-period worth of filesystem changes. `force_full` is set whenever
/// notify itself reported an error or either bounded queue overflowed.
#[derive(Debug)]
pub struct LocalChangeBatch {
    pub paths: Vec<PathBuf>,
    /// Exact file names whose last writable handle closed in this burst.
    /// The local reconciler uses this to distinguish a completed copy from a
    /// destination that merely has its final size but is still being filled.
    pub completed_writes: Vec<PathBuf>,
    pub force_full: bool,
}

/// Paths with a create/data event that have not reached close-write yet.
///
/// This state is updated in notify's callback before the bounded event queue,
/// so queue overflow cannot lose a close and leave reconciliation blocked
/// forever. The coordinator also consults it before applying a full-scan
/// report, preventing the periodic verifier from racing an application write.
#[derive(Debug, Clone, Default)]
pub struct OpenWrites {
    paths: Arc<Mutex<HashMap<PathBuf, usize>>>,
    changed: Arc<tokio::sync::Notify>,
}

impl OpenWrites {
    fn observe(&self, kind: &EventKind, paths: &[PathBuf]) {
        let mut open = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        match kind {
            EventKind::Create(CreateKind::File) => {
                for path in paths {
                    open.entry(path.clone()).or_insert(1);
                    changed = true;
                }
            }
            EventKind::Modify(ModifyKind::Data(_)) => {
                for path in paths {
                    // inotify reports a directory's child-list mutation as a
                    // data change on the directory itself. Directories never
                    // emit CloseWrite, so tracking one here would block every
                    // incremental scan below it forever. A metadata lookup is
                    // cheap relative to a write event and keeps the gate tied
                    // strictly to regular file content.
                    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
                        open.entry(path.clone()).or_insert(1);
                        changed = true;
                    }
                }
            }
            EventKind::Access(AccessKind::Close(AccessMode::Write)) | EventKind::Remove(_) => {
                for path in paths {
                    changed |= open.remove(path).is_some();
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() >= 2 => {
                if let Some(count) = open.remove(&paths[0]) {
                    open.insert(paths[1].clone(), count);
                    changed = true;
                }
            }
            _ => {}
        }
        drop(open);
        if changed {
            // `notify_one` retains a permit when the batching task has not
            // reached its wait yet, closing the check-then-sleep race.
            self.changed.notify_one();
        }
    }

    pub fn blocks(&self, scopes: &HashSet<PathBuf>) -> bool {
        let open = self.paths.lock().unwrap_or_else(|e| e.into_inner());
        open.keys().any(|path| {
            scopes
                .iter()
                .any(|scope| path.starts_with(scope) || scope.starts_with(path))
        })
    }

    pub fn is_idle(&self) -> bool {
        self.paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    async fn wait_for_change(&self) {
        self.changed.notified().await;
    }
}

impl LocalWatcher {
    /// Watch `root`, returning debounced dirty paths to the sync coordinator.
    ///
    /// Debouncing matters more than it looks: an application saving a document
    /// typically produces a create, several writes and a rename within a few
    /// hundred milliseconds, and syncing on each one would upload three
    /// half-written versions of the file.
    pub fn start(
        root: &Path,
        debounce: Duration,
    ) -> Result<(Self, tokio::sync::mpsc::Receiver<LocalChangeBatch>)> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<LocalChange>(EVENT_QUEUE);
        let (batch_tx, batch_rx) = tokio::sync::mpsc::channel(BATCH_QUEUE);
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflow = overflowed.clone();
        let open_writes = OpenWrites::default();
        let callback_writes = open_writes.clone();
        let batch_writes = open_writes.clone();

        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            match result {
                Ok(event) => {
                    let paths: Vec<_> = event
                        .paths
                        .into_iter()
                        .filter(|path| !is_internal(path))
                        .collect();
                    if paths.is_empty() {
                        return;
                    }
                    callback_writes.observe(&event.kind, &paths);
                    if !is_change(&event.kind) {
                        return;
                    }
                    let kind = classify(&event.kind);
                    tracing::debug!(?kind, ?paths, "local change");
                    if tx.try_send(LocalChange { kind, paths }).is_err() {
                        callback_overflow.store(true, Ordering::Release);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "filesystem watch error");
                    callback_overflow.store(true, Ordering::Release);
                    // Best effort wake-up. A full queue already guarantees the
                    // debounce task is active and will observe the flag.
                    let _ = tx.try_send(LocalChange {
                        kind: LocalChangeKind::Other,
                        paths: Vec::new(),
                    });
                }
            }
        })
        .context("could not create a filesystem watcher")?;

        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("could not watch {}", root.display()))?;

        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut dirty = HashSet::new();
                let mut completed_writes = HashSet::new();
                add_dirty_paths(&mut dirty, &mut completed_writes, first);
                // Collapse the burst: keep swallowing events until the tree has
                // been quiet for the debounce window. A quiet *open* writer is
                // not complete: wait for close-write rather than uploading the
                // stable-looking prefix it happens to have produced so far.
                loop {
                    match tokio::time::timeout(debounce, rx.recv()).await {
                        Ok(Some(change)) => {
                            add_dirty_paths(&mut dirty, &mut completed_writes, change)
                        }
                        Ok(None) => return,
                        Err(_) if batch_writes.blocks(&dirty) => {
                            batch_writes.wait_for_change().await
                        }
                        Err(_) => break,
                    }
                }
                let mut paths: Vec<_> = dirty.into_iter().collect();
                paths.sort();
                let mut completed_writes: Vec<_> = completed_writes.into_iter().collect();
                completed_writes.sort();
                let force_full = overflowed.swap(false, Ordering::AcqRel);
                if batch_tx
                    .send(LocalChangeBatch {
                        paths,
                        completed_writes,
                        force_full,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        Ok((
            Self {
                _watcher: watcher,
                open_writes,
            },
            batch_rx,
        ))
    }

    pub fn open_writes(&self) -> OpenWrites {
        self.open_writes.clone()
    }
}

fn classify(kind: &EventKind) -> LocalChangeKind {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => LocalChangeKind::CloseWrite,
        EventKind::Create(_) => LocalChangeKind::Create,
        EventKind::Remove(_) => LocalChangeKind::Remove,
        EventKind::Modify(ModifyKind::Name(_)) => LocalChangeKind::Rename,
        EventKind::Modify(_) => LocalChangeKind::Modify,
        EventKind::Any | EventKind::Other | EventKind::Access(_) => LocalChangeKind::Other,
    }
}

fn add_dirty_paths(
    dirty: &mut HashSet<PathBuf>,
    completed_writes: &mut HashSet<PathBuf>,
    change: LocalChange,
) {
    // Rename events carry both old and new names. For every change scan the
    // entry and its parent; this pairs cross-directory moves in one batch and
    // notices deletion of the old name even when that path no longer exists.
    for path in change.paths {
        match change.kind {
            LocalChangeKind::Create | LocalChangeKind::Modify => {
                completed_writes.remove(&path);
            }
            LocalChangeKind::CloseWrite | LocalChangeKind::Rename => {
                completed_writes.insert(path.clone());
            }
            LocalChangeKind::Remove | LocalChangeKind::Other => {}
        }
        if let Some(parent) = path.parent() {
            dirty.insert(parent.to_path_buf());
        }
        dirty.insert(path);
    }
    tracing::trace!(kind = ?change.kind, scopes = dirty.len(), "batched local change");
}

/// Whether an event means the tree actually changed.
///
/// Reads are not changes, and this matters more than it sounds: the periodic
/// local scan opens every directory it walks, which the kernel reports as
/// `Access(Open)`. Treating those as changes makes the scan wake the loop that
/// runs the scan — the daemon then syncs continuously, and the status flips
/// between "syncing" and "up to date" forever.
///
/// Attribute-only changes are ignored for the same reason: this client writes
/// extended attributes and timestamps as part of a normal pass, and OneDrive
/// has nothing to sync from them anyway.
fn is_change(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => true,
        // `Any`/`Other` cover backends that do not classify; the periodic scan
        // is the backstop, but waking is the safer guess.
        EventKind::Any | EventKind::Other => true,
    }
}

fn is_internal(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Warn when the kernel's watch budget is too small for the tree.
///
/// Exceeding it makes `notify` silently miss changes in whole subdirectories,
/// which presents to the user as "sync just doesn't work for that folder" —
/// worth an explicit message rather than a mystery.
pub fn check_watch_limit(root: &Path) {
    let limit: usize = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8192);
    let directories = count_directories(root, limit);
    if directories >= limit {
        tracing::warn!(
            limit,
            "the sync folder has at least as many directories as the inotify watch limit; \
             local changes may be missed until the next full scan. \
             Raise it with: sudo sysctl fs.inotify.max_user_watches={}",
            limit * 4
        );
    }
}

/// Count directories, stopping early once the limit is reached — walking a
/// million-entry tree just to produce a warning would be worse than the warning.
fn count_directories(root: &Path, limit: usize) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                count += 1;
                if count >= limit {
                    return count;
                }
                stack.push(entry.path());
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::RenameMode;
    use std::io::Write;

    #[test]
    fn rename_batches_both_names_and_both_parents() {
        let old = PathBuf::from("/sync/old/a.txt");
        let new = PathBuf::from("/sync/new/a.txt");
        let mut dirty = HashSet::new();
        add_dirty_paths(
            &mut dirty,
            &mut HashSet::new(),
            LocalChange {
                kind: classify(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))),
                paths: vec![old.clone(), new.clone()],
            },
        );

        assert!(dirty.contains(&old));
        assert!(dirty.contains(&new));
        assert!(dirty.contains(Path::new("/sync/old")));
        assert!(dirty.contains(Path::new("/sync/new")));
    }

    #[test]
    fn staging_files_and_metadata_only_events_are_ignored() {
        assert!(is_internal(Path::new("/sync/.a.gonedrive-partial")));
        assert!(!is_change(&EventKind::Modify(ModifyKind::Metadata(
            notify::event::MetadataKind::Any,
        ))));
    }

    #[test]
    fn writable_handles_block_only_their_own_scopes_until_close() {
        let dir = tempfile::tempdir().unwrap();
        let scope_a = dir.path().join("A");
        let scope_b = dir.path().join("B");
        std::fs::create_dir_all(&scope_a).unwrap();
        let path = scope_a.join("writing.bin");
        std::fs::write(&path, b"in progress").unwrap();
        let tracker = OpenWrites::default();
        tracker.observe(
            &EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            std::slice::from_ref(&path),
        );

        assert!(tracker.blocks(&HashSet::from([scope_a])));
        assert!(!tracker.blocks(&HashSet::from([scope_b])));

        tracker.observe(
            &EventKind::Access(AccessKind::Close(AccessMode::Write)),
            std::slice::from_ref(&path),
        );
        assert!(tracker.is_idle());
    }

    #[test]
    fn directory_content_changes_do_not_become_permanent_open_writes() {
        let dir = tempfile::tempdir().unwrap();
        let tracker = OpenWrites::default();
        tracker.observe(
            &EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            &[dir.path().to_path_buf()],
        );

        assert!(tracker.is_idle());
        assert!(!tracker.blocks(&HashSet::from([dir.path().join("child.txt")])));
    }

    #[test]
    fn close_write_marks_the_exact_file_complete() {
        let path = PathBuf::from("/sync/A/file.bin");
        let mut dirty = HashSet::new();
        let mut completed = HashSet::new();
        add_dirty_paths(
            &mut dirty,
            &mut completed,
            LocalChange {
                kind: LocalChangeKind::CloseWrite,
                paths: vec![path.clone()],
            },
        );

        assert!(dirty.contains(Path::new("/sync/A")));
        assert!(completed.contains(&path));
    }

    #[tokio::test]
    async fn a_real_open_writer_is_not_published_before_close() {
        let dir = tempfile::tempdir().unwrap();
        let (_watcher, mut batches) =
            LocalWatcher::start(dir.path(), Duration::from_millis(40)).unwrap();
        let path = dir.path().join("slow-copy.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"unfinished prefix").unwrap();
        file.flush().unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(180), batches.recv())
                .await
                .is_err(),
            "an open destination must not reach reconciliation"
        );

        drop(file);
        let batch = tokio::time::timeout(Duration::from_secs(2), batches.recv())
            .await
            .expect("close-write batch timed out")
            .expect("watcher stopped");
        assert!(batch.paths.contains(&path));
        assert!(batch.completed_writes.contains(&path));
    }

    #[tokio::test]
    async fn opening_a_partial_directory_is_not_a_local_change() {
        let root = tempfile::tempdir().unwrap();
        let partial = root.path().join("Cloud folder");
        std::fs::create_dir(&partial).unwrap();
        xattr::set(
            &partial,
            gonedrive_core::hydrate::XATTR_POPULATION,
            b"partial",
        )
        .unwrap();
        let (_watcher, mut batches) =
            LocalWatcher::start(root.path(), Duration::from_millis(40)).unwrap();

        let _entries = std::fs::read_dir(&partial).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), batches.recv())
                .await
                .is_err(),
            "directory reads must not enter local reconciliation"
        );
    }
}
