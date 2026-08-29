//! Local change detection.
//!
//! inotify is an optimisation, not the source of truth: its queue can overflow,
//! it sees nothing while the daemon is stopped, and the kernel caps how many
//! watches a user may hold. The periodic full scan is what guarantees
//! convergence — this just makes the common case feel immediate.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecursiveMode, Watcher};

const EVENT_QUEUE: usize = 4096;
const BATCH_QUEUE: usize = 64;

/// Files the client writes itself, whose events would otherwise wake the loop
/// once per chunk of every download.
const IGNORED_SUFFIXES: &[&str] = &[".gonedrive-partial"];

pub struct LocalWatcher {
    /// Dropping this stops the watch.
    _watcher: notify::RecommendedWatcher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalChangeKind {
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
    pub force_full: bool,
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

        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            match result {
                Ok(event) => {
                    if !is_change(&event.kind) {
                        return;
                    }
                    let paths: Vec<_> = event
                        .paths
                        .into_iter()
                        .filter(|path| !is_internal(path))
                        .collect();
                    if paths.is_empty() {
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
                add_dirty_paths(&mut dirty, first);
                // Collapse the burst: keep swallowing events until the tree has
                // been quiet for the debounce window.
                loop {
                    match tokio::time::timeout(debounce, rx.recv()).await {
                        Ok(Some(change)) => add_dirty_paths(&mut dirty, change),
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                let mut paths: Vec<_> = dirty.into_iter().collect();
                paths.sort();
                let force_full = overflowed.swap(false, Ordering::AcqRel);
                if batch_tx
                    .send(LocalChangeBatch { paths, force_full })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        Ok((Self { _watcher: watcher }, batch_rx))
    }
}

fn classify(kind: &EventKind) -> LocalChangeKind {
    match kind {
        EventKind::Create(_) => LocalChangeKind::Create,
        EventKind::Remove(_) => LocalChangeKind::Remove,
        EventKind::Modify(ModifyKind::Name(_)) => LocalChangeKind::Rename,
        EventKind::Modify(_) => LocalChangeKind::Modify,
        EventKind::Any | EventKind::Other | EventKind::Access(_) => LocalChangeKind::Other,
    }
}

fn add_dirty_paths(dirty: &mut HashSet<PathBuf>, change: LocalChange) {
    // Rename events carry both old and new names. For every change scan the
    // entry and its parent; this pairs cross-directory moves in one batch and
    // notices deletion of the old name even when that path no longer exists.
    for path in change.paths {
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

    #[test]
    fn rename_batches_both_names_and_both_parents() {
        let old = PathBuf::from("/sync/old/a.txt");
        let new = PathBuf::from("/sync/new/a.txt");
        let mut dirty = HashSet::new();
        add_dirty_paths(
            &mut dirty,
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
}
