//! Orchestration: pull remote changes, scan the local tree, drain the queue.
//!
//! The engine never performs a transfer inline. Everything that touches the
//! network beyond enumeration becomes a row in `pending_ops` first, so a
//! transfer that fails is retried with backoff and a transfer interrupted by a
//! restart is still queued when the daemon comes back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::OffsetDateTime;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::db::{Account, ItemRecord, PinState, Store};
use crate::error::{Error, Result};
use crate::graph::GraphClient;
use crate::hydrate::Placeholder;
use crate::sync::plan::LocalStat;
use crate::sync::rooted_fs::RootedFs;
use crate::sync::selection::{Listing, PinOutcome, Selection};

/// Bounds on how often a hydration reports progress: at least this much has to
/// have moved, so a small file still gets a few updates and a large one does
/// not get thousands.
pub(super) const PROGRESS_MIN_STEP: u64 = 64 * 1024;
pub(super) const PROGRESS_MAX_STEP: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueScope {
    /// Remote-to-local content queued by delta application.
    Downloads,
    /// Local-to-remote uploads and metadata mutations.
    Outbound,
    /// Outbound work except remote deletion, used while deletion discovery is
    /// incomplete. This keeps uploads flowing without outrunning the guard.
    OutboundNonDelete,
    /// All durable operations, including uploads and remote mutations.
    All,
}

#[derive(Debug, Clone)]
pub enum SyncEvent {
    Started,
    /// Reading the file list. Emitted per page, because a first pass over a
    /// large drive takes minutes and silence is indistinguishable from a hang.
    Enumerating {
        items: u64,
        pages: u64,
    },
    /// Something opened a placeholder and is blocked until its content
    /// arrives. Reported separately from ordinary sync progress because there
    /// is a user sitting in front of a frozen application waiting for it.
    HydrationStarted {
        path: PathBuf,
        name: String,
        size: u64,
    },
    HydrationProgress {
        path: PathBuf,
        transferred: u64,
        total: u64,
    },
    HydrationFinished {
        path: PathBuf,
        ok: bool,
    },
    Progress {
        name: String,
        transferred: u64,
        total: Option<u64>,
        direction: Direction,
    },
    ItemDone {
        name: String,
        direction: Direction,
    },
    /// An item was deleted — a local deletion propagated to the cloud, or a
    /// remote deletion applied locally.
    Deleted {
        name: String,
    },
    /// Both sides changed; the local copy was preserved under a new name.
    Conflict {
        original: String,
        copy: String,
    },
    Skipped {
        name: String,
        reason: String,
    },
    Failed {
        name: String,
        error: String,
    },
    /// A destructive step was refused because it looked like data loss rather
    /// than an intended change — the sync root on a different filesystem than
    /// recorded (an unmounted mountpoint), or a scan about to delete an
    /// alarming fraction of the account. Nothing was deleted; the user is
    /// asked to confirm.
    SafetyStop {
        reason: String,
        kind: SafetyStopKind,
    },
    Finished(SyncSummary),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyStopKind {
    General,
    MassDelete,
}

/// The result of freeing space or creating placeholders.
#[derive(Debug, Clone, Default)]
pub struct FreeSpaceOutcome {
    pub files: u64,
    pub freed: u64,
    /// Placeholders that now need intercepting by the privileged helper.
    pub paths: Vec<PathBuf>,
    /// Files deliberately left alone, with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub downloaded: u64,
    pub uploaded: u64,
    pub deleted_local: u64,
    pub deleted_remote: u64,
    pub conflicts: u64,
    pub skipped: u64,
    /// Enumerated but deliberately not fetched, because nothing pinned them.
    pub not_downloaded: u64,
    /// Items seen in this pass's enumeration.
    pub enumerated: u64,
    pub failed: u64,
    pub still_queued: u64,
    /// Remote deletions the safety valve held back this pass because they
    /// looked like an accidental mass loss. Non-zero means a `SafetyStop` was
    /// emitted and the deletions did not happen.
    pub refused_deletions: u64,
}

/// Result of the low-frequency, read-only whole-tree verification. Applying
/// `dirty_paths` through `scan_local_paths` revalidates every decision against
/// current disk and database state, so a stale discovery can never delete or
/// upload by itself.
#[derive(Debug, Default)]
pub struct FullScanReport {
    pub dirty_paths: Vec<PathBuf>,
    pub placeholders: Vec<(PathBuf, u64, u64)>,
    pub scanned_entries: u64,
}

pub struct Engine {
    // `pub(super)` so cohesive groups of methods can live in sibling modules of
    // `sync` (emblems, hydration, …) while still reaching the engine's state.
    // The fields stay invisible outside the `sync` module.
    pub(super) store: Store,
    pub(super) graph: Arc<GraphClient>,
    pub(super) config: Arc<Config>,
    pub(super) account_id: String,
    pub(super) events: broadcast::Sender<SyncEvent>,
    pub(super) hostname: String,
    /// Names already reported as unsyncable, so a file that cannot be renamed
    /// is mentioned once rather than on every pass forever.
    pub(super) reported_skips: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Directories whose timestamp has to be re-applied at the end of the pass.
    ///
    /// It cannot be done as each one is written, because mtime updates cascade:
    /// writing a file bumps its directory, and creating that directory bumped
    /// *its* parent. So the writes are recorded as they happen and the stamps
    /// applied once, deepest first, when the pass is done.
    pub(super) dir_stamps: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Exact paths created as new placeholders since the last coordinator
    /// pass. This avoids re-enumerating every dehydrated record after delta.
    pub(super) placeholder_marks: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// Set only after the user resumes a mass-deletion safety stop. It permits
    /// one fresh scan to requeue the confirmed wave and is never persisted.
    pub(super) mass_delete_approval: std::sync::atomic::AtomicBool,
}

impl Engine {
    pub fn new(
        store: Store,
        graph: Arc<GraphClient>,
        config: Arc<Config>,
        account_id: String,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            store,
            graph,
            config,
            account_id,
            events,
            hostname: hostname(),
            reported_skips: std::sync::Mutex::new(std::collections::HashSet::new()),
            dir_stamps: std::sync::Mutex::new(std::collections::HashSet::new()),
            placeholder_marks: std::sync::Mutex::new(std::collections::HashSet::new()),
            mass_delete_approval: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    pub fn approve_next_mass_delete(&self) {
        self.mass_delete_approval
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn emit(&self, event: SyncEvent) {
        // No subscribers is normal (the UI may be closed); losing the event is
        // fine because state lives in the database, not in the stream.
        let _ = self.events.send(event);
    }

    pub(super) fn account(&self) -> Result<Account> {
        self.store
            .account(&self.account_id)?
            .ok_or_else(|| Error::other(format!("unknown account {}", self.account_id)))
    }

    pub(super) fn rooted(&self, account: &Account) -> Result<RootedFs> {
        RootedFs::open(&account.root_local_path)
    }

    pub(super) fn create_placeholder_at(
        &self,
        account: &Account,
        path: &Path,
        size: u64,
        item_id: &str,
        ctag: Option<&str>,
        mtime: Option<OffsetDateTime>,
    ) -> Result<()> {
        // New placeholders never replace a path that appeared after discovery.
        // Existing placeholders are refreshed through their already-validated
        // descriptor at the call site.
        let file = self.rooted(account)?.create_new_file(path)?;
        Placeholder::create_file(&file, size, item_id, ctag, mtime)
    }

    /// Resolve a placeholder beneath this account's root and bind its xattr id
    /// to the database path before any content or metadata operation.
    pub(super) fn open_owned_placeholder(
        &self,
        path: &Path,
        write: bool,
    ) -> Result<(Account, ItemRecord, std::fs::File)> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let file = if write {
            rooted.open_write(path)?
        } else {
            rooted.open_read(path)?
        };
        if !file.metadata()?.is_file() {
            return Err(Error::other(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let item_id = Placeholder::item_id_file(&file)
            .ok_or_else(|| Error::other(format!("{} is not a placeholder", path.display())))?;
        let record = self
            .store
            .item(&item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;
        if record.account_id != self.account_id
            || self.store.local_path(&account, &item_id)?.as_deref() != Some(path)
        {
            return Err(Error::other(format!(
                "{} is not the path of item {item_id} in this account",
                path.display()
            )));
        }
        Ok((account, record, file))
    }

    /// One full pass: remote changes, then local changes, then the queue.
    ///
    /// The order matters. Pulling first means a local scan sees the freshest
    /// possible remote state, which keeps a file changed on both sides from
    /// being uploaded before we know the remote also moved.
    pub async fn sync_once(&self) -> Result<SyncSummary> {
        self.emit(SyncEvent::Started);
        let mut summary = SyncSummary::default();

        self.remote_phase(&mut summary).await?;
        self.scan_local(&mut summary)?;
        self.drain_queue(&mut summary).await?;
        self.stamp_directories(&self.account()?)?;

        summary.still_queued = self.store.pending_count(&self.account_id)?;
        self.emit(SyncEvent::Finished(summary.clone()));
        Ok(summary)
    }

    /// Pull and apply only the remote delta. Transfers are deliberately left in
    /// the durable queue so the daemon coordinator can run them in short slices
    /// and give the next delta deadline priority.
    pub async fn sync_remote(&self) -> Result<SyncSummary> {
        self.emit(SyncEvent::Started);
        let mut summary = SyncSummary::default();
        self.remote_phase(&mut summary).await?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        self.emit(SyncEvent::Finished(summary.clone()));
        Ok(summary)
    }

    /// Browse what has been enumerated, with each entry's effective pin.
    pub fn list(&self, relative: &Path) -> Result<Vec<Listing>> {
        self.selection().list(relative)
    }

    /// Choose whether a path's content is kept on this device.
    pub fn set_pin(&self, relative: &Path, pin: PinState) -> Result<PinOutcome> {
        let outcome = self.selection().set_pin(relative, pin)?;
        // A pin/unpin changes what a folder should show: pinned turns it into a
        // "always keep" badge outright, unpinning may drop it back to cloud.
        // Recompute the target folder and let the change ripple up.
        if let Ok(account) = self.account() {
            if let Some(root_id) = account.root_item_id.clone() {
                if let Ok(Some(target)) =
                    self.store
                        .item_at_path(&self.account_id, &root_id, relative)
                {
                    if target.is_folder {
                        let _ = self.recompute_folder_mark(&account, &target.id);
                    }
                    let _ = self.refresh_folder_ancestors(&account, &target.id);
                }
            }
        }
        Ok(outcome)
    }

    fn selection(&self) -> Selection {
        Selection::new(
            self.store.clone(),
            self.config.clone(),
            self.account_id.clone(),
        )
    }
}

pub(super) fn stat_of(path: &Path) -> Option<LocalStat> {
    // `symlink_metadata` so a symlink is never mistaken for its target.
    let meta = std::fs::symlink_metadata(path).ok()?;
    (meta.is_file() || meta.is_dir()).then(|| LocalStat::from_metadata(&meta))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "linux".into())
}
