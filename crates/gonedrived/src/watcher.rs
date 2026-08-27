//! Local change detection.
//!
//! inotify is an optimisation, not the source of truth: its queue can overflow,
//! it sees nothing while the daemon is stopped, and the kernel caps how many
//! watches a user may hold. The periodic full scan is what guarantees
//! convergence — this just makes the common case feel immediate.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecursiveMode, Watcher};
use tokio::sync::Notify;

/// Files the client writes itself, whose events would otherwise wake the loop
/// once per chunk of every download.
const IGNORED_SUFFIXES: &[&str] = &[".gonedrive-partial"];

pub struct LocalWatcher {
    /// Dropping this stops the watch.
    _watcher: notify::RecommendedWatcher,
}

impl LocalWatcher {
    /// Watch `root`, waking `notify` once changes have been quiet for
    /// `debounce`.
    ///
    /// Debouncing matters more than it looks: an application saving a document
    /// typically produces a create, several writes and a rename within a few
    /// hundred milliseconds, and syncing on each one would upload three
    /// half-written versions of the file.
    pub fn start(root: &Path, debounce: Duration, wake: Arc<Notify>) -> Result<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();

        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            match result {
                Ok(event) => {
                    if !is_change(&event.kind) {
                        return;
                    }
                    if event.paths.iter().any(|p| is_internal(p)) {
                        return;
                    }
                    tracing::debug!(kind = ?event.kind, paths = ?event.paths, "local change");
                    // Unbounded, so this never blocks the inotify reader thread.
                    let _ = tx.send(());
                }
                Err(e) => tracing::warn!(error = %e, "filesystem watch error"),
            }
        })
        .context("could not create a filesystem watcher")?;

        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("could not watch {}", root.display()))?;

        tokio::spawn(async move {
            while rx.recv().await.is_some() {
                // Collapse the burst: keep swallowing events until the tree has
                // been quiet for the debounce window.
                loop {
                    match tokio::time::timeout(debounce, rx.recv()).await {
                        Ok(Some(())) => continue,
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
                wake.notify_one();
            }
        });

        Ok(Self { _watcher: watcher })
    }
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
