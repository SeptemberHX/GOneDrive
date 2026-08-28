//! Orchestration: pull remote changes, scan the local tree, drain the queue.
//!
//! The engine never performs a transfer inline. Everything that touches the
//! network beyond enumeration becomes a row in `pending_ops` first, so a
//! transfer that fails is retried with backoff and a transfer interrupted by a
//! restart is still queued when the daemon comes back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::db::{Account, ItemRecord, ItemState, OpKind, PendingOp, PinState, Store};
use crate::error::{Error, Result};
use crate::graph::GraphClient;
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::models::DriveItem;
use crate::names::conflict_copy_name;
use crate::sync::plan::{
    decide_local, decide_remote, partition_delta, record_from_remote, LocalAction, LocalStat,
    RemoteAction, SkipReason,
};
use crate::sync::selection::{available_bytes, human_bytes, Listing, PinOutcome, Selection};
use crate::sync::transfer::{self, UploadTarget};

/// Give up on an operation after this many failures and surface it to the user.
const MAX_OP_ATTEMPTS: u32 = 8;

/// Operations pulled off the queue per pass.
const QUEUE_BATCH: usize = 64;

/// Bounds on how often a hydration reports progress: at least this much has to
/// have moved, so a small file still gets a few updates and a large one does
/// not get thousands.
const PROGRESS_MIN_STEP: u64 = 64 * 1024;
const PROGRESS_MAX_STEP: u64 = 4 * 1024 * 1024;

/// Files the client itself creates, which must never be synced.
const INTERNAL_SUFFIXES: &[&str] = &[".gonedrive-partial"];

/// Markers that belong to this machine's desktop indexers, not to the drive.
/// Uploading one would push a local opt-out to every other device.
const INTERNAL_NAMES: &[&str] = &[".trackerignore", ".nomedia"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
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
    },
    Finished(SyncSummary),
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

pub struct Engine {
    store: Store,
    graph: Arc<GraphClient>,
    config: Arc<Config>,
    account_id: String,
    events: broadcast::Sender<SyncEvent>,
    hostname: String,
    /// Names already reported as unsyncable, so a file that cannot be renamed
    /// is mentioned once rather than on every pass forever.
    reported_skips: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Directories whose timestamp has to be re-applied at the end of the pass.
    ///
    /// It cannot be done as each one is written, because mtime updates cascade:
    /// writing a file bumps its directory, and creating that directory bumped
    /// *its* parent. So the writes are recorded as they happen and the stamps
    /// applied once, deepest first, when the pass is done.
    dir_stamps: std::sync::Mutex<std::collections::HashSet<String>>,
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
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: SyncEvent) {
        // No subscribers is normal (the UI may be closed); losing the event is
        // fine because state lives in the database, not in the stream.
        let _ = self.events.send(event);
    }

    fn account(&self) -> Result<Account> {
        self.store
            .account(&self.account_id)?
            .ok_or_else(|| Error::other(format!("unknown account {}", self.account_id)))
    }

    /// One full pass: remote changes, then local changes, then the queue.
    ///
    /// The order matters. Pulling first means a local scan sees the freshest
    /// possible remote state, which keeps a file changed on both sides from
    /// being uploaded before we know the remote also moved.
    pub async fn sync_once(&self) -> Result<SyncSummary> {
        self.emit(SyncEvent::Started);
        let mut summary = SyncSummary::default();

        match self.pull_remote(&mut summary).await {
            Ok(()) => {}
            Err(Error::ResyncRequired) => {
                tracing::warn!("delta token rejected; re-enumerating the drive");
                self.store.set_delta_link(&self.account_id, None)?;
                self.full_resync(&mut summary).await?;
            }
            Err(e) => {
                self.emit(SyncEvent::Failed {
                    name: "delta".into(),
                    error: e.to_string(),
                });
                return Err(e);
            }
        }

        self.scan_local(&mut summary)?;
        self.drain_queue(&mut summary).await?;
        self.stamp_directories(&self.account()?)?;

        summary.still_queued = self.store.pending_count(&self.account_id)?;
        self.emit(SyncEvent::Finished(summary.clone()));
        Ok(summary)
    }

    // ---- remote -> local --------------------------------------------------

    async fn pull_remote(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        // An enumeration interrupted part-way resumes from where it stopped.
        // Restarting a 130 000-item drive from scratch on every attempt means a
        // first sync that is interrupted once never finishes at all.
        let mut link = account
            .enum_link
            .clone()
            .or_else(|| account.delta_link.clone());
        let mut items = 0u64;
        let mut pages = 0u64;

        loop {
            let page = self.graph.delta(link.as_deref()).await?;
            let (upserts, deletes) = partition_delta(page.value);

            items += (upserts.len() + deletes.len()) as u64;
            pages += 1;

            for item in &upserts {
                self.apply_remote(&account, item, summary, None)?;
            }
            for item in &deletes {
                self.apply_remote(&account, item, summary, None)?;
            }
            self.emit(SyncEvent::Enumerating { items, pages });

            match (page.next_link, page.delta_link) {
                (Some(next), _) => {
                    // Applying a page is idempotent, so recording the
                    // continuation after applying it is safe: a crash re-runs
                    // at most one page.
                    self.store.set_enum_link(&self.account_id, Some(&next))?;
                    link = Some(next);
                }
                (None, delta_link) => {
                    // The deltaLink is persisted only once the final page has
                    // landed. Saving it mid-enumeration would tell the next run
                    // that pages we never processed had already been handled.
                    self.store
                        .set_delta_link(&self.account_id, delta_link.as_deref())?;
                    self.store.set_enum_link(&self.account_id, None)?;
                    summary.enumerated = items;
                    return Ok(());
                }
            }
        }
    }

    /// Re-enumerate the whole drive after the service rejected our deltaLink.
    ///
    /// Anything the enumeration does not mention is gone remotely, which a
    /// plain re-run of delta cannot tell us — a fresh enumeration reports what
    /// exists, never what was removed.
    async fn full_resync(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        let generation = self.store.begin_resync(&self.account_id)?;
        let mut link: Option<String> = None;

        loop {
            let page = self.graph.delta(link.as_deref()).await?;
            let (upserts, deletes) = partition_delta(page.value);

            let seen: Vec<String> = upserts.iter().map(|i| i.id.clone()).collect();
            for item in &upserts {
                self.apply_remote(&account, item, summary, Some(generation))?;
            }
            self.store.mark_seen(&seen, generation)?;
            for item in &deletes {
                self.apply_remote(&account, item, summary, Some(generation))?;
            }

            if let Some(next) = page.next_link {
                link = Some(next);
                continue;
            }
            self.store
                .set_delta_link(&self.account_id, page.delta_link.as_deref())?;
            break;
        }

        // Sweep what the enumeration never mentioned.
        for stale in self.store.items_missed_by_resync(&self.account_id, generation)? {
            let Some(path) = self.store.local_path(&account, &stale.id)? else {
                self.store.delete_subtree(&stale.id)?;
                continue;
            };
            self.remove_local(&stale, &path, summary)?;
        }
        Ok(())
    }

    /// Apply one item from a delta page.
    fn apply_remote(
        &self,
        account: &Account,
        remote: &DriveItem,
        summary: &mut SyncSummary,
        generation: Option<i64>,
    ) -> Result<()> {
        let known = self.store.item(&remote.id)?;

        if remote.is_deleted() {
            let Some(known) = known else { return Ok(()) };
            let Some(path) = self.store.local_path(account, &known.id)? else {
                self.store.delete_subtree(&known.id)?;
                return Ok(());
            };
            let local = stat_of(&path);
            match decide_remote(remote, Some(&known), local.as_ref()) {
                RemoteAction::KeepLocalAndReupload => {
                    // Detach it from the remote item that no longer exists and
                    // re-offer the local content as something new.
                    self.store.delete_subtree(&known.id)?;
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Upload,
                        None,
                        Some(&path),
                        None,
                    )?;
                }
                _ => self.remove_local(&known, &path, summary)?,
            }
            return Ok(());
        }

        // The drive root anchors every path; record it and stop. Its pin stays
        // `Inherit` — see `ItemRecord::root`.
        if remote.is_root() {
            self.store
                .upsert_item(&ItemRecord::root(&self.account_id, &remote.id))?;
            return Ok(());
        }

        let Some(name) = remote.name.clone() else {
            return Ok(());
        };

        // The path comes from the *parent chain plus this name*, not from any
        // record we already hold, so a rename is seen at its new location.
        let Some(parent_id) = remote.parent_id() else {
            summary.skipped += 1;
            return Ok(());
        };
        let Some(parent_rel) = self.store.relative_path(parent_id)? else {
            // Ordered batches make this rare; it still happens across page
            // boundaries. The next pass picks it up.
            tracing::debug!(item = %remote.id, %parent_id, "parent not known yet");
            summary.skipped += 1;
            return Ok(());
        };
        let path = account.root_local_path.join(&parent_rel).join(&name);
        let local = stat_of(&path);

        // A dehydrated placeholder whose remote content changed is refreshed in
        // place — new size, new timestamp, new ctag — and stays a placeholder.
        // Its on-disk bytes are holes, not the user's edit, so the ordinary
        // conflict/download logic (which would see "local differs" and copy it
        // aside) must not run. The new content arrives when the file is next
        // opened. This is what makes a cloud change to an un-downloaded file
        // invisible to the user, as it should be.
        if !remote.is_folder()
            && !remote.is_deleted()
            && Placeholder::state(&path) == PlaceholderState::Dehydrated
        {
            if let Some(existing) = &known {
                let changed = existing.ctag.as_deref() != remote.ctag.as_deref()
                    || existing.size != remote.size.unwrap_or(0);
                if changed {
                    // Fall back to the known record time if the delta page
                    // omitted fileSystemInfo, so a refresh never stamps the
                    // placeholder with "now".
                    let mtime = remote
                        .effective_mtime()
                        .or_else(|| existing.remote_mtime);
                    let _ = Placeholder::create(
                        &path,
                        remote.size.unwrap_or(0),
                        &remote.id,
                        remote.ctag.as_deref(),
                        mtime,
                    );
                    tracing::debug!(name = %name, "refreshed placeholder from a remote change");
                }
            }
            let mut record =
                record_from_remote(&self.account_id, remote, ItemState::Dehydrated, PinState::Inherit);
            if let Some(existing) = &known {
                record.pin_state = existing.pin_state;
            }
            self.store.upsert_item(&record)?;
            if let Some(generation) = generation {
                self.store.mark_seen(&[record.id.clone()], generation)?;
            }
            return Ok(());
        }

        let action = decide_remote(remote, known.as_ref(), local.as_ref());
        let mut state = ItemState::Synced;

        match action {
            RemoteAction::CreateDirectory => {
                // Only inside a selected subtree. Building the full folder
                // structure for a drive the user has not chosen leaves tens of
                // thousands of empty directories behind for no benefit.
                if self.wants_content(known.as_ref(), parent_id)? {
                    std::fs::create_dir_all(&path)?;
                    self.mark_dir(&remote.id)?;
                } else {
                    summary.not_downloaded += 1;
                }
            }
            RemoteAction::Download { .. } => {
                // An item the user has not asked for is recorded but not
                // fetched. A drive is routinely larger than the disk under it,
                // so enumerating everything and downloading nothing is the only
                // safe thing to do before the user has chosen.
                if self.wants_content(known.as_ref(), parent_id)? {
                    state = ItemState::DirtyRemote;
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Download,
                        Some(&remote.id),
                        Some(&path),
                        None,
                    )?;
                } else {
                    state = ItemState::Dehydrated;
                    summary.not_downloaded += 1;
                    // In on-demand mode a newly-seen file becomes a placeholder
                    // right away, so it shows up in the file manager without
                    // waiting for a full re-materialisation.
                    if self.config.on_demand && !path.exists() {
                        if let Err(e) = Placeholder::create(
                            &path,
                            remote.size.unwrap_or(0),
                            &remote.id,
                            remote.ctag.as_deref(),
                            remote.effective_mtime(),
                        ) {
                            tracing::debug!(path = %path.display(), error = %e, "placeholder failed");
                        }
                    }
                }
            }
            RemoteAction::RestampMtime => {
                if let Some(mtime) = remote.effective_mtime() {
                    transfer::set_mtime(&path, mtime)?;
                }
            }
            RemoteAction::ConflictCopy => {
                let copy = self.make_conflict_copy(&path)?;
                summary.conflicts += 1;
                self.emit(SyncEvent::Conflict {
                    original: name.clone(),
                    copy: copy
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                });
                // The preserved copy goes up as a new file; the remote version
                // comes down under the original name. Neither is lost.
                self.store
                    .enqueue(&self.account_id, OpKind::Upload, None, Some(&copy), None)?;
                state = ItemState::DirtyRemote;
                self.store.enqueue(
                    &self.account_id,
                    OpKind::Download,
                    Some(&remote.id),
                    Some(&path),
                    None,
                )?;
            }
            RemoteAction::Skip(reason) => {
                if !matches!(reason, SkipReason::UpToDate) {
                    summary.skipped += 1;
                    self.emit(SyncEvent::Skipped {
                        name: name.clone(),
                        reason: format!("{reason:?}"),
                    });
                    return Ok(());
                }
            }
            RemoteAction::DeleteLocal | RemoteAction::KeepLocalAndReupload => {
                unreachable!("handled in the tombstone branch above")
            }
        }

        let mut record = record_from_remote(&self.account_id, remote, state, PinState::Inherit);
        if let Some(known) = &known {
            // Carry forward the user's retention choice and our last local
            // observation; the delta page knows nothing about either.
            record.pin_state = known.pin_state;
            record.local_mtime = known.local_mtime;
            record.local_size = known.local_size;
            record.local_ino = known.local_ino;
        }
        self.store.upsert_item(&record)?;

        if state == ItemState::Synced {
            if let Some(local) = stat_of(&path) {
                self.store.set_local_stat(
                    &record.id,
                    OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                    Some(local.size),
                    Some(local.ino),
                )?;
            }
        }
        if let Some(generation) = generation {
            self.store.mark_seen(&[record.id.clone()], generation)?;
        }
        Ok(())
    }

    /// Keep a file's extended attributes in step with what we know.
    ///
    /// The file manager reads these directly rather than asking the daemon, so
    /// they are what the user actually sees; letting them drift means wrong
    /// emblems on correct files.
    fn refresh_marks(&self, record: &ItemRecord, path: &Path) {
        if record.is_folder {
            return;
        }
        let Ok(pin) = self
            .store
            .effective_pin(&record.id, self.config.download_new.default_pin())
        else {
            return;
        };
        let wanted = pin.as_str();
        if Placeholder::pin(path).as_deref() != Some(wanted) {
            let _ = Placeholder::set_pin(path, wanted);
        }
        // A file with content but no state mark predates this client knowing
        // about placeholders; claim it so it renders correctly.
        if Placeholder::state(path) == PlaceholderState::Unmanaged
            && record.state == ItemState::Synced
        {
            let _ = Placeholder::mark_hydrated(path, &record.id, record.ctag.as_deref());
        }
    }

    /// Whether this item's content should be on disk.
    ///
    /// An explicit choice on the item itself wins; otherwise the answer comes
    /// from the nearest pinned ancestor, and failing that from the account's
    /// download policy.
    fn wants_content(&self, known: Option<&ItemRecord>, parent_id: &str) -> Result<bool> {
        let default = self.config.download_new.default_pin();
        let pin = match known.map(|k| k.pin_state) {
            Some(PinState::Inherit) | None => self.store.effective_pin(parent_id, default)?,
            Some(explicit) => explicit,
        };
        Ok(pin.wants_content())
    }

    /// Record that a directory — and every directory above it — was written
    /// to, so their timestamps can be restored at the end of the pass.
    ///
    /// The ancestors matter: creating `a/b/c` moves the mtime of `a/b` and of
    /// `a` too, and marking only the leaf leaves both of those showing the time
    /// of the sync rather than the time the service reported.
    fn mark_dir(&self, item_id: &str) -> Result<()> {
        let Ok(mut marked) = self.dir_stamps.lock() else {
            return Ok(());
        };
        let mut current = Some(item_id.to_string());
        while let Some(id) = current {
            if !marked.insert(id.clone()) {
                break; // this chain has already been walked
            }
            current = self.store.item(&id)?.and_then(|item| item.parent_id);
        }
        Ok(())
    }

    /// Apply the recorded directory timestamps, deepest first.
    ///
    /// Depth order is what makes one pass enough: stamping a parent before its
    /// children were finished would be undone by the child's own write.
    fn stamp_directories(&self, account: &Account) -> Result<()> {
        let ids: Vec<String> = match self.dir_stamps.lock() {
            Ok(mut marked) => marked.drain().collect(),
            Err(_) => return Ok(()),
        };

        let mut targets = Vec::new();
        for id in ids {
            let Some(record) = self.store.item(&id)? else {
                continue;
            };
            let (Some(mtime), Some(path)) =
                (record.remote_mtime, self.store.local_path(account, &id)?)
            else {
                continue;
            };
            if path.is_dir() {
                targets.push((path.components().count(), path, mtime));
            }
        }

        targets.sort_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
        for (_, path, mtime) in targets {
            if let Err(e) = transfer::set_mtime(&path, mtime) {
                tracing::debug!(path = %path.display(), error = %e, "could not stamp directory");
            }
        }
        Ok(())
    }

    fn remove_local(
        &self,
        known: &ItemRecord,
        path: &Path,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let result = if known.is_folder {
            // Only an empty directory is removed. A non-empty one means the
            // user put something there that the remote delete says nothing
            // about, and recursive deletion would destroy it.
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => summary.deleted_local += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not remove locally");
                summary.failed += 1;
            }
        }
        self.store.delete_subtree(&known.id)?;
        Ok(())
    }

    /// Move a local file aside under a conflict name, returning the new path.
    fn make_conflict_copy(&self, path: &Path) -> Result<PathBuf> {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let copy_name = conflict_copy_name(&name, &self.hostname, OffsetDateTime::now_utc());
        let copy = path.with_file_name(copy_name);
        std::fs::rename(path, &copy)?;
        tracing::info!(from = %path.display(), to = %copy.display(), "kept local copy");
        Ok(copy)
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
                    self.store.item_at_path(&self.account_id, &root_id, relative)
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

    // ---- local -> remote --------------------------------------------------

    /// Walk the sync root and queue whatever differs from our record.
    ///
    /// Runs on every pass, not just at startup: inotify is blind while the
    /// daemon is stopped, and its queue can overflow under heavy writes, so the
    /// scan is what makes the client eventually consistent regardless.
    pub fn scan_local(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        let Some(root_id) = account.root_item_id.clone() else {
            // Nothing has been enumerated yet; there is no tree to compare to.
            return Ok(());
        };
        if !account.root_local_path.exists() {
            std::fs::create_dir_all(&account.root_local_path)?;
            return Ok(());
        }

        // The sync root must be the same filesystem it was last time. If it is
        // not, the most likely cause is that it is not mounted and we are
        // looking at an empty mountpoint — every record would read as "deleted
        // locally" and the account would be wiped server-side. Record the
        // device the first time; refuse to scan (and so refuse to delete)
        // whenever it has changed.
        {
            use std::os::unix::fs::MetadataExt;
            let current_dev = std::fs::metadata(&account.root_local_path).map(|m| m.dev()).ok();
            match (account.root_dev, current_dev) {
                (Some(recorded), Some(now)) if recorded != now => {
                    let reason = format!(
                        "sync root {} is on a different filesystem than recorded \
                         (was dev {recorded}, now {now}) — refusing to scan so an \
                         unmounted root cannot delete the account",
                        account.root_local_path.display()
                    );
                    tracing::error!(%reason, "safety stop");
                    self.emit(SyncEvent::SafetyStop { reason });
                    return Ok(());
                }
                (None, Some(now)) => {
                    self.store.set_root_dev(&self.account_id, now)?;
                }
                _ => {}
            }
        }

        let by_inode = self.store.items_by_inode(&self.account_id)?;
        // `None` marks a directory that has no remote record yet — freshly
        // created locally. It is still walked: the far end of a rename often
        // lands inside exactly such a directory, and skipping it is how a
        // move used to decay into a remote delete plus a re-upload.
        let mut stack: Vec<(Option<String>, PathBuf)> =
            vec![(Some(root_id), account.root_local_path.clone())];
        // Decisions that need the *whole* walk before they are safe:
        // a rename that crosses directories shows up as "new file here" in one
        // directory and "record missing" in another, and acting on either half
        // early turns a move into a remote delete plus a full re-upload.
        let mut appeared: Vec<(std::path::PathBuf, LocalStat)> = Vec::new();
        let mut vanished: Vec<ItemRecord> = Vec::new();

        while let Some((parent_id, dir)) = stack.pop() {
            let entries = match read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::warn!(path = %dir.display(), error = %e, "cannot read directory");
                    continue;
                }
            };
            let known: HashMap<String, ItemRecord> = match &parent_id {
                Some(parent_id) => self
                    .store
                    .children(&self.account_id, parent_id)?
                    .into_iter()
                    .map(|item| (crate::names::fold_case(&item.name), item))
                    .collect(),
                None => HashMap::new(),
            };

            for (name, stat) in &entries {
                let record = known.get(&crate::names::fold_case(name));

                // A placeholder's bytes are not the user's content: dehydrated
                // means holes, hydrating means holes being filled. Judging
                // either as "locally modified" queues an upload of zeroes over
                // the real file in the cloud. Nothing about them is ours to
                // upload, so they only recurse (below) and refresh marks.
                let content_is_ours = record.is_none_or(|r| {
                    !matches!(r.state, ItemState::Dehydrated | ItemState::Hydrating)
                }) && (stat.is_dir
                    || Placeholder::state(&dir.join(name)) != PlaceholderState::Dehydrated);
                if !content_is_ours {
                    if let Some(record) = record {
                        if stat.is_dir {
                            stack.push((Some(record.id.clone()), dir.join(name)));
                        }
                    }
                    continue;
                }

                match decide_local(name, record, Some(stat)) {
                    LocalAction::CreateRemoteFolder => {
                        // A parent that is itself new has no id to record yet;
                        // the runner resolves it from the path once the parent
                        // exists — the queue preserves creation order.
                        self.store.enqueue(
                            &self.account_id,
                            OpKind::CreateRemoteFolder,
                            parent_id.as_deref(),
                            Some(&dir.join(name)),
                            None,
                        )?;
                        stack.push((None, dir.join(name)));
                    }
                    LocalAction::UploadNew => {
                        // Deferred: whether this is truly new or the far end
                        // of a rename is only knowable once the whole tree has
                        // been walked and the vanished records collected.
                        appeared.push((dir.join(name), *stat));
                    }
                    LocalAction::UploadModified => {
                        let id = record.map(|r| r.id.clone());
                        self.store.enqueue(
                            &self.account_id,
                            OpKind::Upload,
                            id.as_deref(),
                            Some(&dir.join(name)),
                            parent_id.as_deref(),
                        )?;
                        if let Some(id) = &id {
                            self.store.set_item_state(id, ItemState::DirtyLocal)?;
                        }
                    }
                    LocalAction::Skip(reason) => {
                        summary.skipped += 1;
                        let fresh = self
                            .reported_skips
                            .lock()
                            .map(|mut seen| seen.insert(name.clone()))
                            .unwrap_or(true);
                        if fresh {
                            self.emit(SyncEvent::Skipped {
                                name: name.clone(),
                                reason: format!("{reason:?}"),
                            });
                        }
                    }
                    LocalAction::Nothing | LocalAction::DeleteRemote => {}
                }

                if !stat.is_dir {
                    // The scan already stats every file, so keeping the file
                    // manager's marks in step costs one `getxattr` and a write
                    // only when they actually drifted.
                    if let Some(record) = record {
                        self.refresh_marks(record, &dir.join(name));
                    }
                }

                if stat.is_dir {
                    if let Some(record) = record {
                        // This walk has already paid for the `stat`, so noticing
                        // a directory whose timestamp drifted — from an earlier
                        // version, or an edit we did not make — is free here.
                        if record
                            .remote_mtime
                            .is_some_and(|t| t.unix_timestamp() != stat.mtime)
                        {
                            self.mark_dir(&record.id)?;
                        }
                        stack.push((Some(record.id.clone()), dir.join(name)));
                    }
                }
            }

            // Records with no matching entry on disk were deleted locally.
            let on_disk: std::collections::HashSet<String> = entries
                .iter()
                .map(|(name, _)| crate::names::fold_case(name))
                .collect();
            let Some(_parent_id) = parent_id else {
                continue; // a new directory has no records to have lost
            };
            for (folded, record) in &known {
                if on_disk.contains(folded) {
                    continue;
                }
                // A record that never had a local observation was never written
                // to disk in the first place — its download is still pending,
                // and deleting it remotely would be catastrophic.
                if record.local_ino.is_none() {
                    continue;
                }
                // Distinguish two kinds of "not wanted here". An *explicit*
                // exclusion ("don't keep this folder on this device") makes
                // local absence expected — deleting remotely would destroy
                // what the user chose not to sync. But an account-level
                // online-only *default* never expressed an opinion about this
                // item: it was on disk because the user put or fetched it
                // there, and its disappearance is the user deleting it. The
                // first guard here once ate every local deletion under the
                // default policy, silently.
                if self.store.explicitly_excluded(&record.id)? {
                    continue;
                }
                vanished.push(record.clone());
            }
        }

        // Pair the two halves of every rename before anything destructive:
        // a vanished record whose inode reappeared elsewhere is one PATCH,
        // not a remote delete plus a re-upload that forfeits the server-side
        // version history.
        let mut moved_ids = std::collections::HashSet::new();
        for (path, stat) in &appeared {
            // Parents are deliberately not recorded here: the runner resolves
            // them from the path, because a destination directory created in
            // this same pass has no remote id until its own queued creation
            // has run.
            let known = by_inode.get(&stat.ino).filter(|record| {
                vanished.iter().any(|v| v.id == record.id)
                    || self
                        .has_moved(&account, record, path.parent().unwrap_or(path), "")
                        .unwrap_or(false)
            });
            match known {
                Some(record) => {
                    moved_ids.insert(record.id.clone());
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::MoveRemote,
                        Some(&record.id),
                        Some(path),
                        None,
                    )?;
                }
                None => {
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Upload,
                        None,
                        Some(path),
                        None,
                    )?;
                }
            }
        }
        // Mass-deletion safety valve. Deleting a large fraction of the account
        // in one pass is far more likely to be an accident we should not
        // propagate — a mount that dropped without changing the device number,
        // a bind gone wrong, an `rm -rf` the user regrets — than a deliberate
        // change. Hold every deletion this pass and ask the user to confirm,
        // rather than execute an irreversible remote wipe.
        let doomed = vanished
            .iter()
            .filter(|r| !moved_ids.contains(&r.id))
            .count() as u64;
        if doomed > self.config.mass_delete_min {
            let total = self.store.item_count(&self.account_id)?.max(1);
            let fraction = doomed as f64 / total as f64;
            if fraction > self.config.mass_delete_fraction {
                let reason = format!(
                    "a single scan would delete {doomed} of {total} items \
                     ({:.0}%) — holding the deletions for confirmation instead \
                     of propagating what looks like an accidental mass loss",
                    fraction * 100.0
                );
                tracing::error!(%reason, "safety stop");
                self.emit(SyncEvent::SafetyStop { reason });
                summary.refused_deletions += doomed;
                return Ok(());
            }
        }

        for record in vanished {
            if moved_ids.contains(&record.id) {
                continue;
            }
            self.store.enqueue(
                &self.account_id,
                OpKind::DeleteRemote,
                Some(&record.id),
                None,
                None,
            )?;
        }
        Ok(())
    }

    /// Whether a known item really moved, rather than a new file coincidentally
    /// landing on a recycled inode.
    fn has_moved(
        &self,
        account: &Account,
        known: &ItemRecord,
        new_dir: &Path,
        new_name: &str,
    ) -> Result<bool> {
        let Some(old) = self.store.local_path(account, &known.id)? else {
            return Ok(false);
        };
        if old == new_dir.join(new_name) {
            return Ok(false);
        }
        // Inodes are recycled aggressively, so require that nothing remains at
        // the old path before believing this is the same file.
        Ok(!old.exists())
    }

    // ---- files on demand ---------------------------------------------------

    /// Fill a placeholder's content because something tried to read it.
    ///
    /// Called from the fanotify path with a process blocked on the other end,
    /// so it does the least work that unblocks the reader: fetch, write, mark.
    /// The whole file is fetched rather than the requested range — a reader
    /// asking for the first 4 KiB of a document is almost always about to ask
    /// for the rest, and one request beats hundreds.
    pub async fn hydrate(&self, path: &Path) -> Result<u64> {
        let item_id = Placeholder::item_id(path)
            .ok_or_else(|| Error::other(format!("{} is not a placeholder", path.display())))?;
        let record = self
            .store
            .item(&item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;

        if Placeholder::state(path) == PlaceholderState::Hydrated {
            return Ok(0);
        }

        tracing::info!(path = %path.display(), size = record.size, "hydrating on access");
        self.store.set_item_state(&item_id, ItemState::Hydrating)?;

        let response = self.graph.download(&item_id, None).await?;
        let mut stream = response.bytes_stream();
        let mut offset = 0u64;
        let name = record.name.clone();

        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            Placeholder::fill(path, offset, &chunk)?;
            offset += chunk.len() as u64;
            self.emit(SyncEvent::Progress {
                name: name.clone(),
                transferred: offset,
                total: Some(record.size),
                direction: Direction::Download,
            });
        }

        Placeholder::mark_hydrated(path, &item_id, record.ctag.as_deref())?;
        let pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::set_pin(path, pin.as_str());
        // The write moved the mtime; put the service's value back so the file
        // does not look locally modified the moment it is read.
        if let Some(mtime) = record.remote_mtime {
            transfer::set_mtime(path, mtime)?;
        }
        if let Some(local) = stat_of(path) {
            self.store.set_local_stat(
                &item_id,
                OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                Some(local.size),
                Some(local.ino),
            )?;
        }
        self.store.set_item_state(&item_id, ItemState::Synced)?;
        let account = self.account()?;
        let _ = self.refresh_folder_ancestors(&account, &item_id);
        Ok(offset)
    }

    /// Download a placeholder's content to a staging file beside it.
    ///
    /// Used on the files-on-demand path, where the daemon must **not** write to
    /// the placeholder itself: filling a hole faults the page in, which is a
    /// read, which re-enters the kernel hook that asked for the hydration in
    /// the first place. The privileged helper does the actual write through the
    /// descriptor the kernel handed it, which is exempt.
    ///
    /// The staging file is a sibling, so the helper's copy stays on one
    /// filesystem, and it carries the suffix the local scanner already ignores.
    pub async fn stage_content(&self, path: &Path) -> Result<PathBuf> {
        self.stage_content_cancellable(path, Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .await
    }

    /// As [`Self::stage_content`], but abandons the transfer when `cancel` is
    /// set.
    pub async fn stage_content_cancellable(
        &self,
        path: &Path,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<PathBuf> {
        let item_id = Placeholder::item_id(path)
            .ok_or_else(|| Error::other(format!("{} is not a placeholder", path.display())))?;
        let record = self
            .store
            .item(&item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;

        let staged = transfer::staging_sibling(path);

        tracing::info!(path = %path.display(), size = record.size, "staging content");
        self.store.set_item_state(&item_id, ItemState::Hydrating)?;

        self.emit(SyncEvent::HydrationStarted {
            path: path.to_path_buf(),
            name: record.name.clone(),
            size: record.size,
        });

        let events = self.events.clone();
        let reported = path.to_path_buf();
        let size = record.size;
        // The transfer reports every chunk, which for a 13 MB file is around a
        // thousand callbacks. Forwarding each one floods the bus for no
        // benefit: nobody can read a progress bar that updates a thousand
        // times. Emit on visible movement instead.
        let step = (size / 100).clamp(PROGRESS_MIN_STEP, PROGRESS_MAX_STEP);
        let mut last_sent = 0u64;

        let limit = self.config.download_limit_bytes_per_sec;
        let result =
            transfer::download_to_file_limited(&self.graph, &item_id, &staged, None, limit, &mut move |done, total| {
                let total = total.unwrap_or(size);
                if done == total || done.saturating_sub(last_sent) >= step {
                    last_sent = done;
                    let _ = events.send(SyncEvent::HydrationProgress {
                        path: reported.clone(),
                        transferred: done,
                        total,
                    });
                }
                !cancel.load(std::sync::atomic::Ordering::Relaxed)
            })
            .await;

        if let Err(e) = result {
            self.emit(SyncEvent::HydrationFinished {
                path: path.to_path_buf(),
                ok: false,
            });
            return Err(e);
        }
        Ok(staged)
    }

    /// Stage only a byte range of a placeholder, quietly.
    ///
    /// This is what makes browsing cheap: a file manager sniffing content
    /// types reads the first 16 KiB of each file, and answering that with a
    /// full download turns "open a folder" into "download the folder". The
    /// range is fetched, nothing else changes — no state transition, no
    /// progress events, no activity entry — and the kernel watch stays armed
    /// so a reader that continues past the window comes back for the rest.
    pub async fn stage_range(
        &self,
        path: &Path,
        offset: u64,
        length: u64,
    ) -> Result<PathBuf> {
        let item_id = Placeholder::item_id(path)
            .ok_or_else(|| Error::other(format!("{} is not a placeholder", path.display())))?;

        let staged = transfer::staging_sibling(path);

        // A range that is already real data on disk — a window some earlier
        // sniff filled — is served from the file itself. Browsing the same
        // folder twice must not fetch the same 16 KiB from the cloud twice.
        if let Some(bytes) = read_if_materialized(path, offset, length)? {
            tokio::fs::write(&staged, &bytes).await?;
            tracing::info!(
                path = %path.display(),
                offset,
                bytes = bytes.len(),
                "staged range from local data"
            );
            return Ok(staged);
        }

        let response = self
            .graph
            .download(&item_id, Some(offset..offset + length))
            .await?;
        let bytes = response.bytes().await?;
        tokio::fs::write(&staged, &bytes).await?;
        tracing::info!(
            path = %path.display(),
            offset,
            bytes = bytes.len(),
            "staged range"
        );
        Ok(staged)
    }

    /// Stage a byte range only if it is already real data on disk, never
    /// fetching. Returns `None` if any of the window is still a hole.
    ///
    /// This is what keeps folder browsing offline: a sniff of a window that was
    /// filled by an earlier real read is served locally; a sniff of an
    /// untouched placeholder returns `None`, and the caller lets the read see
    /// zeros rather than reaching for the network.
    pub async fn stage_range_if_local(
        &self,
        path: &Path,
        offset: u64,
        length: u64,
    ) -> Result<Option<PathBuf>> {
        let Some(bytes) = read_if_materialized(path, offset, length)? else {
            return Ok(None);
        };
        let staged = transfer::staging_sibling(path);
        tokio::fs::write(&staged, &bytes).await?;
        Ok(Some(staged))
    }

    /// Restore a placeholder's modification time to the record's after a
    /// partial range fill.
    ///
    /// A range fill pwrites content into the sparse file, and that write moves
    /// the mtime to the wall clock — so merely browsing a folder, or a
    /// thumbnailer sniffing a file's head, would leave every touched file
    /// stamped "now" instead of its real remote time. The file is still a
    /// placeholder (`full` fills finalise through `finish_hydration`, which
    /// already restores the time), so only the timestamp is corrected here.
    pub fn restore_mtime(&self, path: &Path) -> Result<()> {
        let Some(item_id) = Placeholder::item_id(path) else {
            return Ok(());
        };
        let Some(record) = self.store.item(&item_id)? else {
            return Ok(());
        };
        if let Some(mtime) = record.remote_mtime {
            // Only when it actually drifted: a browsing sniff that filled
            // nothing reports the same completion, and opening every sniffed
            // file for writing would make folder browsing crawl. A stat is
            // cheap and does not fault the placeholder.
            use std::os::unix::fs::MetadataExt;
            let drifted = std::fs::metadata(path)
                .map(|m| (m.mtime() - mtime.unix_timestamp()).abs() > 2)
                .unwrap_or(false);
            if drifted {
                transfer::set_mtime(path, mtime)?;
            }
        }
        Ok(())
    }

    /// Record that a placeholder the helper just filled is now complete.
    pub fn finish_hydration(&self, path: &Path) -> Result<()> {
        self.emit(SyncEvent::HydrationFinished {
            path: path.to_path_buf(),
            ok: true,
        });
        let Some(item_id) = Placeholder::item_id(path) else {
            return Ok(());
        };
        let Some(record) = self.store.item(&item_id)? else {
            return Ok(());
        };

        Placeholder::mark_hydrated(path, &item_id, record.ctag.as_deref())?;
        let pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::set_pin(path, pin.as_str());
        if let Some(mtime) = record.remote_mtime {
            transfer::set_mtime(path, mtime)?;
        }
        if let Some(local) = stat_of(path) {
            self.store.set_local_stat(
                &item_id,
                OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                Some(local.size),
                Some(local.ino),
            )?;
        }
        self.store.set_item_state(&item_id, ItemState::Synced)?;
        let account = self.account()?;
        self.refresh_folder_ancestors(&account, &item_id)?;
        Ok(())
    }

    /// Turn a downloaded file back into a placeholder, releasing its blocks.
    ///
    /// Refuses while the file has unsaved local changes: freeing space must
    /// never be a way to lose an edit.
    pub fn dehydrate(&self, path: &Path) -> Result<u64> {
        let account = self.account()?;
        let item_id = Placeholder::item_id(path)
            .or_else(|| {
                // Not marked yet — the usual case the first time, since the file
                // arrived through an ordinary download.
                self.store
                    .items_by_inode(&self.account_id)
                    .ok()?
                    .into_iter()
                    .find(|(_, item)| {
                        self.store
                            .local_path(&account, &item.id)
                            .ok()
                            .flatten()
                            .is_some_and(|p| p == path)
                    })
                    .map(|(_, item)| item.id)
            })
            .ok_or_else(|| Error::other(format!("{} is not a synced file", path.display())))?;

        let record = self
            .store
            .item(&item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;

        // The unsaved-changes guard protects *hydrated* files. A record
        // already in Dehydrated state owns none of its on-disk bytes — they
        // are cache from range fills, and a range fill also moves the mtime,
        // which is exactly what this guard keys on. Applying it here made
        // "free a file you merely looked at" impossible.
        if record.state != ItemState::Dehydrated {
            if let Some(local) = stat_of(path) {
                if crate::sync::plan::local_is_modified(&record, &local) {
                    return Err(Error::other(format!(
                        "{} has local changes that are not uploaded yet",
                        path.display()
                    )));
                }
            }
        }

        let mtime = record.remote_mtime;
        let freed = Placeholder::dehydrate(path, &item_id, record.ctag.as_deref())?;
        let pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::set_pin(path, pin.as_str());
        if let Some(mtime) = mtime {
            transfer::set_mtime(path, mtime)?;
        }
        self.store.set_item_state(&item_id, ItemState::Dehydrated)?;
        let _ = self.refresh_folder_ancestors(&account, &item_id);
        Ok(freed)
    }

    /// Create a placeholder for an item that has never been downloaded.
    pub fn materialise_placeholder(&self, item_id: &str) -> Result<PathBuf> {
        let account = self.account()?;
        let record = self
            .store
            .item(item_id)?
            .ok_or_else(|| Error::other(format!("unknown item {item_id}")))?;
        let path = self
            .store
            .local_path(&account, item_id)?
            .ok_or_else(|| Error::other(format!("no local path for {item_id}")))?;

        Placeholder::create(
            &path,
            record.size,
            item_id,
            record.ctag.as_deref(),
            record.remote_mtime,
        )?;
        self.store.set_item_state(item_id, ItemState::Dehydrated)?;
        Ok(path)
    }

    /// Files under a path that exist locally right now.
    ///
    /// The caller that is about to dehydrate them needs this list *first*, so
    /// it can lift their kernel watches before any file is written to: a
    /// dehydration writes into watched files, and a write into a watched file
    /// asks the watcher — us — whether to allow it.
    pub fn subtree_local_files(&self, relative: &Path) -> Result<Vec<PathBuf>> {
        let account = self.account()?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        let files = if target.is_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target]
        };
        let mut out = Vec::new();
        for file in files {
            if let Some(path) = self.store.local_path(&account, &file.id)? {
                if path.exists() {
                    out.push(path);
                }
            }
        }
        Ok(out)
    }

    /// Turn a downloaded subtree into placeholders, freeing its space.
    ///
    /// Files with unsaved local changes are left alone and reported: freeing
    /// space must never be a way to lose an edit.
    pub fn free_space(&self, relative: &Path) -> Result<FreeSpaceOutcome> {
        let account = self.account()?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        let files = if target.is_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target]
        };

        let mut outcome = FreeSpaceOutcome::default();
        for file in files {
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            if !path.exists() {
                continue;
            }
            match self.dehydrate(&path) {
                Ok(freed) => {
                    outcome.freed += freed;
                    outcome.files += 1;
                    outcome.paths.push(path);
                }
                Err(e) => outcome.skipped.push((path, e.to_string())),
            }
        }
        Ok(outcome)
    }

    /// Replace never-downloaded items in a subtree with placeholders.
    pub fn make_on_demand(&self, relative: &Path) -> Result<FreeSpaceOutcome> {
        let account = self.account()?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;
        let target = self
            .store
            .item_at_path(&self.account_id, &root_id, relative)?
            .ok_or_else(|| Error::other(format!("no such path: {}", relative.display())))?;

        let files = if target.is_folder {
            self.store.files_in_subtree(&target.id)?
        } else {
            vec![target]
        };

        let mut outcome = FreeSpaceOutcome::default();
        for file in files {
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            if path.exists() {
                // Already on disk: freeing it is `free_space`'s job, not this one.
                continue;
            }
            match self.materialise_placeholder(&file.id) {
                Ok(path) => {
                    outcome.files += 1;
                    outcome.paths.push(path);
                }
                Err(e) => outcome.skipped.push((path, e.to_string())),
            }
        }
        Ok(outcome)
    }

    /// Materialise the whole drive as a browsable placeholder tree.
    ///
    /// This is the difference between selective sync and true files-on-demand:
    /// every folder becomes a real (near-free) directory and every file a
    /// sparse placeholder, so the entire OneDrive is visible and navigable in
    /// any file manager, offline, with no per-folder network round-trip —
    /// exactly what Windows does. Content still arrives only when a file is
    /// opened.
    ///
    /// Idempotent and resumable: anything already on disk is left as it is, so
    /// a run interrupted half-way simply continues next time. `progress` is
    /// called periodically with (done, total) and returns false to stop.
    /// Returns the placeholder paths that now need intercepting.
    pub fn materialize_all(
        &self,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<Vec<PathBuf>> {
        let account = self.account()?;

        // Directories first, shallowest to deepest, so a child never precedes
        // its parent. Empty folders matter here — `make_on_demand` only ever
        // created directories as a side effect of placing a file, so a folder
        // with no files was invisible.
        let folders = self.store.all_folders_shallow_first(&self.account_id)?;
        for folder in &folders {
            if let Some(path) = self.store.local_path(&account, &folder.id)? {
                if !path.exists() {
                    std::fs::create_dir_all(&path)?;
                }
                // Badge the folder so the file manager shows online/local on
                // directories, not only files. Unconditional so a tree
                // materialised by an earlier version — its folders unbadged —
                // gets marked on the next run. The aggregate is refined below.
                if Placeholder::state(&path) == PlaceholderState::Unmanaged {
                    let _ = Placeholder::mark_dir(&path, PlaceholderState::Dehydrated, &folder.id);
                }
            }
        }

        let files = self.store.all_files(&self.account_id)?;
        let total = files.len() as u64;
        let mut done = 0u64;
        let mut watch = Vec::new();

        for file in files {
            done += 1;
            let Some(path) = self.store.local_path(&account, &file.id)? else {
                continue;
            };
            // Already present — downloaded, or a placeholder from a prior run.
            if path.exists() {
                // A file whose xattr claims it is downloaded while the content
                // is still a hole is a mislabelled placeholder from an earlier
                // bug: rebuild it as a proper placeholder with the right time,
                // otherwise it never reads on demand and shows a wrong stamp.
                if Placeholder::state(&path) == PlaceholderState::Hydrated {
                    use std::os::unix::fs::MetadataExt;
                    let hollow = std::fs::metadata(&path)
                        .map(|m| m.len() > 4096 && m.blocks() * 512 * 4 < m.len())
                        .unwrap_or(false);
                    if hollow {
                        if Placeholder::create(
                            &path,
                            file.size,
                            &file.id,
                            file.ctag.as_deref(),
                            file.remote_mtime,
                        )
                        .is_ok()
                        {
                            let _ = self.store.set_item_state(&file.id, ItemState::Dehydrated);
                            watch.push(path);
                        }
                        continue;
                    }
                }
                if Placeholder::state(&path) == PlaceholderState::Dehydrated {
                    // Self-heal a placeholder whose timestamp or size drifted
                    // from the record — a straggler created at a moment the
                    // remote time was not yet known keeps the materialisation
                    // time until corrected here, and re-materialise otherwise
                    // skips it forever.
                    if let Some(mtime) = file.remote_mtime {
                        use std::os::unix::fs::MetadataExt;
                        let stale = std::fs::metadata(&path)
                            .map(|m| {
                                (m.mtime() - mtime.unix_timestamp()).abs() > 2
                                    || m.len() != file.size
                            })
                            .unwrap_or(false);
                        if stale {
                            if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) != file.size {
                                // Size changed too: rebuild the placeholder.
                                let _ = Placeholder::create(
                                    &path,
                                    file.size,
                                    &file.id,
                                    file.ctag.as_deref(),
                                    Some(mtime),
                                );
                            } else {
                                let _ = transfer::set_mtime(&path, mtime);
                            }
                        }
                    }
                    watch.push(path);
                }
                continue;
            }
            match Placeholder::create(
                &path,
                file.size,
                &file.id,
                file.ctag.as_deref(),
                file.remote_mtime,
            ) {
                Ok(()) => {
                    self.store.set_item_state(&file.id, ItemState::Dehydrated)?;
                    watch.push(path);
                }
                Err(e) => tracing::warn!(path = %path.display(), error = %e, "placeholder failed"),
            }
            if done % 500 == 0 && !progress(done, total) {
                return Ok(watch);
            }
        }

        // Compute folder badges bottom-up: a folder is a check only if every
        // child is local. `folders` is shallowest-first, so reversing gives
        // deepest-first — each folder is decided after its subfolders are.
        for folder in folders.iter().rev() {
            let _ = self.recompute_folder_mark(&account, &folder.id);
        }

        // Directory timestamps last: creating children moved them off the
        // service's value.
        for folder in &folders {
            if let (Some(mtime), Some(path)) = (
                folder.remote_mtime,
                self.store.local_path(&account, &folder.id)?,
            ) {
                if path.is_dir() {
                    let _ = transfer::set_mtime(&path, mtime);
                }
            }
        }

        progress(total, total);
        Ok(watch)
    }

    /// Recompute a folder's badge from its direct children and return whether
    /// it changed.
    ///
    /// The rule the user expects, and Windows uses: a folder is "available on
    /// this device" (a check) only when *everything* inside is here — every
    /// file downloaded, every subfolder itself fully local — or when it was
    /// explicitly kept. A folder with some content still in the cloud shows the
    /// cloud, because opening it may still need the network. "Any one file is
    /// local" is not enough.
    fn recompute_folder_mark(&self, account: &Account, folder_id: &str) -> Result<bool> {
        let Some(path) = self.store.local_path(account, folder_id)? else {
            return Ok(false);
        };
        if !path.is_dir() {
            return Ok(false);
        }
        // An explicit "always keep on this device" wins outright.
        if self.store.explicitly_pinned(folder_id)? {
            let changed = Placeholder::state(&path) != PlaceholderState::Hydrated;
            let _ = Placeholder::mark_dir(&path, PlaceholderState::Hydrated, folder_id);
            let _ = Placeholder::set_pin(&path, "pinned");
            return Ok(changed);
        }

        let mut all_local = true;
        for child in self.store.children(&self.account_id, folder_id)? {
            let local = if child.is_folder {
                // A subfolder counts as local only if it, too, is fully local.
                self.store
                    .local_path(account, &child.id)?
                    .map(|p| Placeholder::state(&p) == PlaceholderState::Hydrated)
                    .unwrap_or(false)
            } else {
                child.state == ItemState::Synced && child.local_ino.is_some()
            };
            if !local {
                all_local = false;
                break;
            }
        }

        let target = if all_local {
            PlaceholderState::Hydrated
        } else {
            PlaceholderState::Dehydrated
        };
        let changed = Placeholder::state(&path) != target;
        if changed {
            let _ = Placeholder::mark_dir(&path, target, folder_id);
        }
        Ok(changed)
    }

    /// Recompute the badges of an item's ancestor folders after its state
    /// changed, stopping as soon as one is unchanged — nothing above a folder
    /// whose own badge did not move can have moved either.
    fn refresh_folder_ancestors(&self, account: &Account, item_id: &str) -> Result<()> {
        let mut current = self.store.item(item_id)?.and_then(|i| i.parent_id);
        while let Some(id) = current {
            let Some(folder) = self.store.item(&id)? else { break };
            if !folder.is_folder {
                break;
            }
            if !self.recompute_folder_mark(account, &id)? {
                break;
            }
            current = folder.parent_id;
        }
        Ok(())
    }

    /// Reconcile database states with what the files themselves say.
    ///
    /// A crash mid-hydration leaves the record claiming `Hydrating` while the
    /// file's own xattr — written last — still says dehydrated. The record is
    /// the copy that lies, so it is the one corrected. Run at start-up.
    pub fn repair_states(&self) -> Result<usize> {
        let account = self.account()?;
        let mut repaired = 0;
        for record in self.store.items_in_state(&self.account_id, ItemState::Hydrating)? {
            let Some(path) = self.store.local_path(&account, &record.id)? else {
                continue;
            };
            let state = match Placeholder::state(&path) {
                PlaceholderState::Hydrated => ItemState::Synced,
                // Missing or still holes: the content never arrived.
                _ => ItemState::Dehydrated,
            };
            self.store.set_item_state(&record.id, state)?;
            repaired += 1;
            tracing::info!(path = %path.display(), ?state, "repaired a stale hydrating record");
        }
        // A record claiming "locally modified" for a file whose own marks say
        // dehydrated is lying about ownership — freeing a file that was dirty
        // at the time leaves exactly this. Left alone, the scanner re-queues
        // an upload of it every single pass, forever, and the upload guard
        // refuses it every single pass, forever.
        for record in self.store.items_in_state(&self.account_id, ItemState::DirtyLocal)? {
            let Some(path) = self.store.local_path(&account, &record.id)? else {
                continue;
            };
            if Placeholder::state(&path) == PlaceholderState::Dehydrated {
                self.store.set_item_state(&record.id, ItemState::Dehydrated)?;
                repaired += 1;
                tracing::info!(path = %path.display(), "repaired a dirty record over a placeholder");
            }
        }
        Ok(repaired)
    }

    /// Give space back automatically, least-recently-used first.
    ///
    /// Only files whose effective pin is *not* "always keep" are candidates —
    /// the whole meaning of pinning is that pressure never takes those. Files
    /// with unsaved local changes are protected by [`Self::dehydrate`] as on
    /// every other path.
    pub fn auto_free(&self, keep_free: u64) -> Result<FreeSpaceOutcome> {
        let account = self.account()?;
        let mut outcome = FreeSpaceOutcome::default();

        let Some(free) = crate::sync::selection::available_bytes(&account.root_local_path) else {
            return Ok(outcome);
        };
        if free >= keep_free {
            return Ok(outcome);
        }
        let mut still_needed = keep_free - free;
        tracing::info!(
            free_bytes = free,
            target = keep_free,
            "low on space; releasing unpinned files"
        );

        let default = self.config.download_new.default_pin();
        let mut candidates = Vec::new();
        for record in self.store.items_in_state(&self.account_id, ItemState::Synced)? {
            if record.is_folder {
                continue;
            }
            if self
                .store
                .effective_pin(&record.id, default)?
                == crate::db::PinState::Pinned
            {
                continue;
            }
            let Some(path) = self.store.local_path(&account, &record.id)? else {
                continue;
            };
            let Ok(occupancy) = crate::hydrate::Occupancy::read(&path) else {
                continue;
            };
            if occupancy.allocated == 0 {
                continue; // already a placeholder
            }
            let atime = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.accessed().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            candidates.push((atime, occupancy.allocated, path));
        }
        // Oldest access first: the file nobody has opened in months goes
        // before the one from this morning.
        candidates.sort_by_key(|(atime, _, _)| *atime);

        for (_, allocated, path) in candidates {
            if still_needed == 0 {
                break;
            }
            match self.dehydrate(&path) {
                Ok(freed) => {
                    outcome.freed += freed;
                    outcome.files += 1;
                    outcome.paths.push(path);
                    still_needed = still_needed.saturating_sub(allocated);
                }
                Err(e) => outcome.skipped.push((path, e.to_string())),
            }
        }
        Ok(outcome)
    }

    /// Every placeholder that still needs intercepting, for registering with
    /// the privileged helper at start-up.
    pub fn dehydrated_paths(&self) -> Result<Vec<PathBuf>> {
        let account = self.account()?;
        let mut out = Vec::new();

        // The database narrows the search; the file's own xattr decides. A
        // hydration interrupted by a crash leaves the record saying
        // `Hydrating` while the file on disk is still empty, and trusting the
        // record there would leave that file unwatched — readable as zeroes,
        // silently, forever.
        for state in [
            ItemState::Dehydrated,
            ItemState::Hydrating,
            ItemState::Error,
        ] {
            for record in self.store.items_in_state(&self.account_id, state)? {
                if record.is_folder {
                    continue;
                }
                let Some(path) = self.store.local_path(&account, &record.id)? else {
                    continue;
                };
                if Placeholder::state(&path) == PlaceholderState::Dehydrated {
                    out.push(path);
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    // ---- the work queue ---------------------------------------------------

    /// Push local changes and finish queued transfers, without re-reading the
    /// remote file list.
    ///
    /// Neither uploading a file the user just edited nor downloading content
    /// they just chose needs a fresh enumeration — the metadata is already
    /// recorded. On a large drive an enumeration costs minutes, so making every
    /// transfer queue behind one turns a 30-second job into a coffee break.
    pub async fn transfer_only(&self) -> Result<SyncSummary> {
        self.emit(SyncEvent::Started);
        let mut summary = SyncSummary::default();
        self.scan_local(&mut summary)?;
        self.drain_queue(&mut summary).await?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        self.emit(SyncEvent::Finished(summary.clone()));
        Ok(summary)
    }

    async fn drain_queue(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        // Checked once per pass rather than per file: if the disk is nearly
        // full, downloads are held back as a group and resume by themselves
        // once space is freed. Uploads and deletes still run — they are how the
        // user gets space back.
        let space = available_bytes(&account.root_local_path);
        let downloads_allowed = match space {
            Some(free) if free < self.config.min_free_bytes => {
                tracing::error!(
                    free_bytes = free,
                    floor = self.config.min_free_bytes,
                    "not enough free disk space; downloads are paused"
                );
                self.emit(SyncEvent::Failed {
                    name: account.root_local_path.display().to_string(),
                    error: format!(
                        "only {} free; downloads are paused until at least {} is available",
                        human_bytes(free),
                        human_bytes(self.config.min_free_bytes)
                    ),
                });
                false
            }
            _ => true,
        };

        loop {
            let ops = self.store.due_ops(&self.account_id, QUEUE_BATCH)?;
            let ops: Vec<_> = ops
                .into_iter()
                .filter(|op| downloads_allowed || op.kind != OpKind::Download)
                .collect();
            if ops.is_empty() {
                return Ok(());
            }
            for op in ops {
                match self.run_op(&op, summary).await {
                    Ok(()) => self.store.complete_op(op.id)?,
                    Err(e) => self.record_failure(&op, e, summary)?,
                }
            }
        }
    }

    fn record_failure(&self, op: &PendingOp, error: Error, summary: &mut SyncSummary) -> Result<()> {
        let name = op
            .local_path
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .or_else(|| op.item_id.clone())
            .unwrap_or_default();

        // A permanent failure is not worth eight retries; only transient ones
        // get the full budget.
        let permanent = !error.is_transient();
        let attempts = self
            .store
            .fail_op(op.id, &error.to_string(), retry_delay(op.attempts))?;

        if permanent || attempts >= MAX_OP_ATTEMPTS {
            tracing::warn!(%name, error = %error, attempts, "giving up on operation");
            summary.failed += 1;
            self.emit(SyncEvent::Failed {
                name,
                error: error.to_string(),
            });
            if let Some(id) = &op.item_id {
                let _ = self.store.set_item_state(id, ItemState::Error);
            }
            self.store.complete_op(op.id)?;
        } else {
            tracing::debug!(%name, error = %error, attempts, "will retry");
        }
        Ok(())
    }

    async fn run_op(&self, op: &PendingOp, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        match op.kind {
            OpKind::Download => self.run_download(&account, op, summary).await,
            OpKind::Upload => self.run_upload(&account, op, summary).await,
            OpKind::CreateRemoteFolder => self.run_create_folder(op, summary).await,
            OpKind::DeleteRemote => self.run_delete_remote(op, summary).await,
            OpKind::MoveRemote => self.run_move_remote(op, summary).await,
        }
    }

    async fn run_download(
        &self,
        account: &Account,
        op: &PendingOp,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let item_id = op
            .item_id
            .as_deref()
            .ok_or_else(|| Error::other("download operation without an item id"))?;
        let Some(record) = self.store.item(item_id)? else {
            // The item was deleted between queueing and running.
            return Ok(());
        };
        let path = op
            .local_path
            .clone()
            .or(self.store.local_path(account, item_id)?)
            .ok_or_else(|| Error::other(format!("no local path for {item_id}")))?;

        self.store.set_item_state(item_id, ItemState::Hydrating)?;
        let name = record.name.clone();
        let events = self.events.clone();
        transfer::download_to_file_limited(
            &self.graph,
            item_id,
            &path,
            record.remote_mtime,
            self.config.download_limit_bytes_per_sec,
            &mut |done, total| {
                let _ = events.send(SyncEvent::Progress {
                    name: name.clone(),
                    transferred: done,
                    total,
                    direction: Direction::Download,
                });
                true
            },
        )
        .await?;

        if let Some(local) = stat_of(&path) {
            self.store.set_local_stat(
                item_id,
                OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                Some(local.size),
                Some(local.ino),
            )?;
        }
        self.store.set_item_state(item_id, ItemState::Synced)?;
        // Mark it for the file manager: an emblem should appear as soon as the
        // download lands, not only after the next pass.
        let pin = self
            .store
            .effective_pin(item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::mark_hydrated(&path, item_id, record.ctag.as_deref());
        let _ = Placeholder::set_pin(&path, pin.as_str());
        if let Some(parent) = &record.parent_id {
            self.mark_dir(parent)?;
        }
        summary.downloaded += 1;
        self.emit(SyncEvent::ItemDone {
            name: record.name,
            direction: Direction::Download,
        });
        Ok(())
    }

    async fn run_upload(
        &self,
        account: &Account,
        op: &PendingOp,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let path = op
            .local_path
            .clone()
            .ok_or_else(|| Error::other("upload operation without a local path"))?;
        if !path.exists() {
            // Deleted between queueing and running; the scan will notice.
            return Ok(());
        }
        // Checked against the file itself, not the database: the queue entry
        // may be hours old, and between then and now the file may have been
        // freed. Uploading a dehydrated placeholder uploads holes.
        match Placeholder::state(&path) {
            PlaceholderState::Dehydrated => {
                tracing::warn!(path = %path.display(), "refusing to upload a placeholder");
                return Ok(());
            }
            PlaceholderState::Hydrated | PlaceholderState::Unmanaged => {}
        }

        let name = path
            .file_name()
            .ok_or_else(|| Error::other(format!("{} has no file name", path.display())))?
            .to_string_lossy()
            .into_owned();

        let parent_id = match &op.payload {
            Some(id) => id.clone(),
            None => self.parent_id_for(account, &path)?,
        };
        // Resuming keys on the item id when we have one, so a re-queued upload
        // of the same file picks the session back up.
        let session_key = op.item_id.clone().unwrap_or_else(|| path.to_string_lossy().into_owned());

        let events = self.events.clone();
        let progress_name = name.clone();
        let item = transfer::upload_file(
            &self.graph,
            &self.store,
            &self.config,
            &path,
            UploadTarget {
                account_id: &self.account_id,
                parent_id: &parent_id,
                name: &name,
                // Present only for an item we already track. For anything
                // else the upload must create a new item and refuse to
                // clobber a file we have never seen.
                item_id: op.item_id.as_deref(),
                session_key: &session_key,
            },
            |done, total| {
                let _ = events.send(SyncEvent::Progress {
                    name: progress_name.clone(),
                    transferred: done,
                    total: Some(total),
                    direction: Direction::Upload,
                });
            },
        )
        .await?;

        // `Inherit`, never `Pinned`: stamping uploads as always-keep silently
        // converts everything the user ever created into pinned content. The
        // live symptom was a deleted folder resurrecting on every delta pass —
        // "pinned" told the applier its local absence was a hole to re-fill.
        let mut record =
            record_from_remote(&self.account_id, &item, ItemState::Synced, PinState::Inherit);
        record.parent_id = Some(parent_id);
        self.store.upsert_item(&record)?;
        if let Some(local) = stat_of(&path) {
            self.store.set_local_stat(
                &record.id,
                OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                Some(local.size),
                Some(local.ino),
            )?;
        }
        summary.uploaded += 1;
        self.emit(SyncEvent::ItemDone {
            name,
            direction: Direction::Upload,
        });
        Ok(())
    }

    async fn run_create_folder(&self, op: &PendingOp, summary: &mut SyncSummary) -> Result<()> {
        let path = op
            .local_path
            .clone()
            .ok_or_else(|| Error::other("folder creation without a local path"))?;
        // Queued without a parent id when the parent was itself new; by now
        // the queue has created it, so the path resolves.
        let parent_id = match op.item_id.as_deref() {
            Some(id) => id.to_string(),
            None => self.parent_id_for(&self.account()?, &path)?,
        };
        let parent_id = parent_id.as_str();
        let name = path
            .file_name()
            .ok_or_else(|| Error::other("folder path has no name"))?
            .to_string_lossy()
            .into_owned();

        let item = self.graph.create_folder(parent_id, &name).await?;
        let mut record =
            record_from_remote(&self.account_id, &item, ItemState::Synced, PinState::Inherit);
        record.parent_id = Some(parent_id.to_string());
        self.store.upsert_item(&record)?;
        if let Some(local) = stat_of(&path) {
            self.store
                .set_local_stat(&record.id, None, None, Some(local.ino))?;
        }
        summary.uploaded += 1;
        Ok(())
    }

    async fn run_delete_remote(&self, op: &PendingOp, summary: &mut SyncSummary) -> Result<()> {
        let item_id = op
            .item_id
            .as_deref()
            .ok_or_else(|| Error::other("delete operation without an item id"))?;
        match self.graph.delete(item_id).await {
            Ok(()) => {}
            // Already gone remotely: the outcome we wanted.
            Err(Error::Graph { status: 404, .. }) => {}
            Err(e) => return Err(e),
        }
        self.store.delete_subtree(item_id)?;
        summary.deleted_remote += 1;
        Ok(())
    }

    async fn run_move_remote(&self, op: &PendingOp, summary: &mut SyncSummary) -> Result<()> {
        let item_id = op
            .item_id
            .as_deref()
            .ok_or_else(|| Error::other("move operation without an item id"))?;
        let path = op
            .local_path
            .clone()
            .ok_or_else(|| Error::other("move operation without a local path"))?;
        // The parent is resolved from the path at run time, not from the
        // payload captured at queue time: when the destination directory was
        // created in the same pass, its remote id did not exist yet when this
        // operation was queued.
        let new_parent = self.parent_id_for(&self.account()?, &path)?;
        let new_name = path.file_name().map(|n| n.to_string_lossy().into_owned());

        let item = self
            .graph
            .move_item(item_id, Some(&new_parent), new_name.as_deref())
            .await?;
        let mut record =
            record_from_remote(&self.account_id, &item, ItemState::Synced, PinState::Inherit);
        record.parent_id = Some(new_parent);
        self.store.upsert_item(&record)?;
        if let Some(local) = stat_of(&path) {
            self.store.set_local_stat(
                &record.id,
                OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                Some(local.size),
                Some(local.ino),
            )?;
        }
        summary.uploaded += 1;
        Ok(())
    }

    /// The remote id of the folder a local path lives in.
    fn parent_id_for(&self, account: &Account, path: &Path) -> Result<String> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::other(format!("{} has no parent", path.display())))?;
        if parent == account.root_local_path {
            return account
                .root_item_id
                .clone()
                .ok_or_else(|| Error::other("the drive root has not been enumerated yet"));
        }
        let relative = parent
            .strip_prefix(&account.root_local_path)
            .map_err(|_| Error::other(format!("{} is outside the sync root", path.display())))?;

        // Walk down from the root, resolving one component at a time.
        let mut current = account
            .root_item_id
            .clone()
            .ok_or_else(|| Error::other("the drive root has not been enumerated yet"))?;
        for component in relative.components() {
            let name = component.as_os_str().to_string_lossy();
            let child = self
                .store
                .child_by_name(&self.account_id, &current, &name)?
                .ok_or_else(|| {
                    Error::other(format!(
                        "{} is not synced yet, so {} has nowhere to go",
                        name,
                        path.display()
                    ))
                })?;
            current = child.id;
        }
        Ok(current)
    }
}

/// Directory entries worth considering, with their stats.
///
/// Symlinks are read with `symlink_metadata` and then dropped: following them
/// would let a link out of the sync root pull arbitrary files into the account.
fn read_dir(dir: &Path) -> std::io::Result<Vec<(String, LocalStat)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if INTERNAL_SUFFIXES.iter().any(|s| name.ends_with(s))
            || INTERNAL_NAMES.contains(&name.as_str())
        {
            // Staging leftovers are pure cache: consumed within seconds when
            // things work, so one still here after fifteen minutes is an
            // orphan of a failed hand-off. The scan is already walking this
            // directory; the sweep costs one stat.
            if name.ends_with(".gonedrive-partial") {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > std::time::Duration::from_secs(900))
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            continue;
        }
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => continue, // vanished mid-scan
        };
        if !meta.is_file() && !meta.is_dir() {
            continue;
        }
        out.push((name, LocalStat::from_metadata(&meta)));
    }
    Ok(out)
}

fn stat_of(path: &Path) -> Option<LocalStat> {
    // `symlink_metadata` so a symlink is never mistaken for its target.
    let meta = std::fs::symlink_metadata(path).ok()?;
    (meta.is_file() || meta.is_dir()).then(|| LocalStat::from_metadata(&meta))
}

/// The requested window, if every byte of it is already data rather than hole.
///
/// `SEEK_DATA`/`SEEK_HOLE` ask the filesystem directly which parts of a sparse
/// file are materialized, so this needs no bookkeeping of past fills.
fn read_if_materialized(path: &Path, offset: u64, length: u64) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;

    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let end = (offset + length).min(size);
    if offset >= end {
        return Ok(None);
    }

    // SAFETY: valid fd; lseek with SEEK_HOLE finds the next hole at or after
    // `offset`. If that hole starts at or beyond `end`, the window is data.
    let next_hole = unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, libc::SEEK_HOLE) };
    if next_hole < 0 || (next_hole as u64) < end {
        return Ok(None);
    }

    let mut buf = vec![0u8; (end - offset) as usize];
    file.read_exact_at(&mut buf, offset)?;
    Ok(Some(buf))
}

/// Backoff between retries of a queued operation: 2s, 4s, 8s … capped at 5min.
fn retry_delay(attempts: u32) -> Duration {
    Duration::from_secs(1u64 << (attempts + 1).min(8)).min(Duration::from_secs(300))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "linux".into())
}
