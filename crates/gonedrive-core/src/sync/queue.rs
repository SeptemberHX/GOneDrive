//! Durable transfer and remote-mutation queue execution.

use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::db::{Account, ItemState, OpKind, PendingOp, PinState};
use crate::error::{Error, Result};
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::sync::plan::{record_from_remote, LocalStat};
use crate::sync::selection::{available_bytes, human_bytes};
use crate::sync::transfer::{self, UploadTarget};

use super::engine::{
    stat_of, Direction, Engine, QueueScope, SafetyStopKind, SyncEvent, SyncPhase, SyncSummary,
};

/// Operations pulled off the queue per pass.
const QUEUE_BATCH: usize = 64;

impl Engine {
    // ---- the work queue ---------------------------------------------------

    /// Push local changes and finish queued transfers, without re-reading the
    /// remote file list.
    ///
    /// Neither uploading a file the user just edited nor downloading content
    /// they just chose needs a fresh enumeration — the metadata is already
    /// recorded. On a large drive an enumeration costs minutes, so making every
    /// transfer queue behind one turns a 30-second job into a coffee break.
    pub async fn transfer_only(&self) -> Result<SyncSummary> {
        let phase = SyncPhase::start(self);
        let mut summary = SyncSummary::default();
        self.scan_local(&mut summary)?;
        self.drain_queue(&mut summary).await?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        phase.finish(&summary);
        Ok(summary)
    }

    /// Run durable work until `deadline`, yielding cleanly between streamed
    /// download chunks and resumable upload chunks. A yielded operation stays
    /// queued without consuming a retry attempt.
    pub async fn transfer_slice(
        &self,
        deadline: Instant,
        scope: QueueScope,
    ) -> Result<SyncSummary> {
        let phase = SyncPhase::start(self);
        let mut summary = SyncSummary::default();
        self.drain_queue_until(&mut summary, Some(deadline), scope)
            .await?;
        self.stamp_directories(&self.account()?)?;
        summary.still_queued = self.store.pending_count(&self.account_id)?;
        phase.finish(&summary);
        Ok(summary)
    }

    pub(super) async fn drain_queue(&self, summary: &mut SyncSummary) -> Result<()> {
        self.drain_queue_until(summary, None, QueueScope::All).await
    }

    async fn drain_queue_until(
        &self,
        summary: &mut SyncSummary,
        deadline: Option<Instant>,
        scope: QueueScope,
    ) -> Result<()> {
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
                    path: Some(account.root_local_path.clone()),
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
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(());
            }
            let ops = match scope {
                QueueScope::Downloads => {
                    self.store.due_download_ops(&self.account_id, QUEUE_BATCH)?
                }
                QueueScope::Outbound => {
                    self.store.due_outbound_ops(&self.account_id, QUEUE_BATCH)?
                }
                QueueScope::OutboundNonDelete => self
                    .store
                    .due_non_delete_outbound_ops(&self.account_id, QUEUE_BATCH)?,
                QueueScope::All => self.store.due_ops(&self.account_id, QUEUE_BATCH)?,
            };
            let ops: Vec<_> = ops
                .into_iter()
                .filter(|op| downloads_allowed || op.kind != OpKind::Download)
                .collect();
            if ops.is_empty() {
                return Ok(());
            }
            for listed in ops {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Ok(());
                }
                // A preceding mutation in this batch may have advanced the
                // item's eTag, while the delta worker may have cancelled the
                // operation altogether. Never execute the stale batch copy.
                let Some(op) = self.store.due_pending_op(listed.id)? else {
                    continue;
                };
                match self.run_op(&op, summary, deadline).await {
                    Ok(()) => self.store.complete_op(op.id)?,
                    Err(Error::Cancelled) if deadline.is_some() => return Ok(()),
                    // Credentials are shared account state, not a bad file.
                    // Leave the operation untouched and let the coordinator
                    // expose the actionable account status to the user.
                    Err(e) if is_account_credential_error(&e) => return Err(e),
                    Err(e) => self.record_failure(&op, e, summary)?,
                }
            }
        }
    }

    fn record_failure(
        &self,
        op: &PendingOp,
        error: Error,
        summary: &mut SyncSummary,
    ) -> Result<()> {
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
        let Some(attempts) =
            self.store
                .fail_op(op.id, &error.to_string(), retry_delay(op.attempts))?
        else {
            // A concurrent delta made this operation obsolete while its HTTP
            // request was in flight. There is no failed work left to retry.
            return Ok(());
        };

        // A transient failure — the link dropped, the service is throttling or
        // briefly 5xx-ing — is never a reason to abandon a transfer. On a train
        // the signal comes back and the file must still sync, so transient ops
        // keep retrying with capped backoff (every few minutes) until they
        // succeed or the item changes underneath them, rather than being given
        // up after a fixed count that a long tunnel would exhaust. Only a
        // *permanent* error — a rejected name, a gone item, a local permission
        // problem — gives up.
        if permanent {
            tracing::warn!(%name, error = %error, attempts, "giving up on operation");
            summary.failed += 1;
            self.emit(SyncEvent::Failed {
                name,
                path: op.local_path.clone().or_else(|| {
                    op.item_id.as_deref().and_then(|id| {
                        self.account()
                            .ok()
                            .and_then(|account| self.store.local_path(&account, id).ok().flatten())
                    })
                }),
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

    async fn run_op(
        &self,
        op: &PendingOp,
        summary: &mut SyncSummary,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let account = self.account()?;
        match op.kind {
            OpKind::Download => self.run_download(&account, op, summary, deadline).await,
            OpKind::Upload => self.run_upload(&account, op, summary, deadline).await,
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
        deadline: Option<Instant>,
    ) -> Result<()> {
        let rooted = self.rooted(account)?;
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

        // Upgrade/crash recovery: an on-demand read may already have delivered
        // this exact remote version while the durable scheduler still held its
        // download row. Verify the bytes before retiring that stale row; a
        // locally edited hydrated file must continue into conflict handling.
        if record.state == ItemState::Synced {
            if let Ok(existing) = rooted.open_read(&path) {
                if Placeholder::state_file(&existing) == PlaceholderState::Hydrated
                    && self.disposable_local_version(&record, &existing).is_some()
                {
                    tracing::info!(path = %path.display(), "retiring a stale completed download");
                    return Ok(());
                }
            }
        }

        // A download overwrites whatever is on disk. If that file holds edits
        // that were never uploaded — an in-place change the size+mtime
        // heuristic missed, so the remote was judged the newer side — preserve
        // the local copy under a conflict name first and re-offer it, rather
        // than let the remote silently win. This is the conflict guarantee,
        // applied where a remote change races a hidden local edit.
        let mut syncing_file = None;
        let expected_destination = if rooted.metadata(&path)?.is_some() {
            let existing = rooted.open_read(&path)?;
            if self.disposable_local_version(&record, &existing).is_some() {
                // The marker itself changes ctime, so install it on the same
                // verified inode and capture the guarded version afterwards.
                let _ = Placeholder::set_syncing_file(&existing, true);
                let version = transfer::FileVersion::capture_file(&existing)?;
                syncing_file = Some(existing);
                version
            } else {
                let copy = self.make_conflict_copy(&path)?;
                self.emit(SyncEvent::Conflict {
                    original: record.name.clone(),
                    copy: copy
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    path: path.clone(),
                });
                summary.conflicts += 1;
                self.store
                    .enqueue(&self.account_id, OpKind::Upload, None, Some(&copy), None)?;
                transfer::FileVersion::capture_rooted(&rooted, &path)?
            }
        } else {
            transfer::FileVersion::capture_rooted(&rooted, &path)?
        };

        self.store.set_item_state(item_id, ItemState::Hydrating)?;
        let name = record.name.clone();
        let events = self.events.clone();
        let result = transfer::download_to_file_limited_rooted(
            &rooted,
            &self.graph,
            transfer::DownloadSpec {
                item_id,
                dest: &path,
                expected_destination: Some(expected_destination),
                etag: record.etag.as_deref(),
                mtime: record.remote_mtime,
                rate_limit: self.config.download_limit_bytes_per_sec,
            },
            &mut |done, total| {
                let _ = events.send(SyncEvent::Progress {
                    name: name.clone(),
                    transferred: done,
                    total,
                    direction: Direction::Download,
                });
                deadline.is_none_or(|deadline| Instant::now() < deadline)
            },
        )
        .await;
        if let Some(file) = &syncing_file {
            let _ = Placeholder::set_syncing_file(file, false);
        }
        let downloaded = match result {
            Err(Error::Cancelled) if deadline.is_some() => {
                self.store.set_item_state(item_id, ItemState::DirtyRemote)?;
                return Err(Error::Cancelled);
            }
            result => result?,
        };

        // Publish placeholder metadata, then make sure nobody rewrote the new
        // inode before we declare it synced. xattrs change ctime, so compare
        // only the properties that identify the bytes themselves.
        let pin = self
            .store
            .effective_pin(item_id, self.config.download_new.default_pin())?;
        let published = rooted.open_write(&path)?;
        let _ = Placeholder::mark_hydrated_file(&published, item_id, record.ctag.as_deref());
        let _ = Placeholder::set_pin_file(&published, pin.as_str());
        let current_version = transfer::FileVersion::capture_rooted(&rooted, &path)?;
        if !current_version.same_content_as(downloaded.version) {
            let copy = self.make_conflict_copy(&path)?;
            self.store
                .enqueue(&self.account_id, OpKind::Upload, None, Some(&copy), None)?;
            summary.conflicts += 1;
            self.emit(SyncEvent::Conflict {
                original: record.name.clone(),
                copy: copy
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                path: path.clone(),
            });
            return Err(transfer::file_changed_error(
                &path,
                "finishing the download",
            ));
        }

        let local = LocalStat::from_metadata(&published.metadata()?);
        self.store.commit_hydration(
            &account.id,
            item_id,
            OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
            Some(local.size),
            Some(local.ino),
        )?;
        // The file manager marks were written before the stability check above,
        // so the synced emblem and database state become visible together.
        if let Some(parent) = &record.parent_id {
            self.mark_dir(parent)?;
        }
        summary.downloaded += 1;
        self.emit(SyncEvent::ItemDone {
            name: record.name,
            path,
            direction: Direction::Download,
        });
        Ok(())
    }

    async fn run_upload(
        &self,
        account: &Account,
        op: &PendingOp,
        summary: &mut SyncSummary,
        deadline: Option<Instant>,
    ) -> Result<()> {
        let rooted = self.rooted(account)?;
        let path = op
            .local_path
            .clone()
            .ok_or_else(|| Error::other("upload operation without a local path"))?;
        let Some(_) = rooted.metadata(&path)? else {
            // Deleted between queueing and running; the scan will notice.
            return Ok(());
        };
        // Checked against the file itself, not the database: the queue entry
        // may be hours old, and between then and now the file may have been
        // freed. Uploading a dehydrated placeholder uploads holes.
        let source_file = rooted.open_read(&path)?;
        match Placeholder::state_file(&source_file) {
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
        let session_key = op
            .item_id
            .clone()
            .unwrap_or_else(|| path.to_string_lossy().into_owned());

        let events = self.events.clone();
        let progress_name = name.clone();
        // Bind the transient marker to the inode already checked above. A path
        // replacement must not make cleanup mutate the replacement instead.
        let _ = Placeholder::set_syncing_file(&source_file, true);
        // Derive the stability token from the descriptor that will be hashed
        // and marked, rather than a path lookup that could name a replaced
        // inode by the time the upload starts.
        let source_version = transfer::FileVersion::capture_file(&source_file)?;
        let result = transfer::upload_file_rooted(
            &rooted,
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
                etag: op.precondition.as_deref(),
                session_key: &session_key,
            },
            |done, total| {
                let _ = events.send(SyncEvent::Progress {
                    name: progress_name.clone(),
                    transferred: done,
                    total: Some(total),
                    direction: Direction::Upload,
                });
                deadline.is_none_or(|deadline| Instant::now() < deadline)
            },
        )
        .await;
        let _ = Placeholder::set_syncing_file(&source_file, false);

        let item = match result {
            Ok(item) => item,
            // If-Match rejected: the remote changed since our last pull, so our
            // upload would overwrite an edit we never saw. Preserve our local
            // copy under a conflict name and re-offer it; forget the stale
            // record so the next delta brings the remote version down fresh.
            Err(Error::Graph { status: 412, .. }) => {
                if let Some(id) = op.item_id.as_deref() {
                    self.store.cancel_outbound_ops_for_version(
                        &self.account_id,
                        id,
                        op.precondition.as_deref(),
                    )?;
                    if let Some(local) = stat_of(&path) {
                        self.store.clear_local_stat_if_inode(id, local.ino)?;
                    }
                    self.store
                        .mark_remote_race(id, op.precondition.as_deref())?;
                }
                if rooted.metadata(&path)?.is_some() {
                    let copy = self.make_conflict_copy(&path)?;
                    self.emit(SyncEvent::Conflict {
                        original: name.clone(),
                        copy: copy
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        path: path.clone(),
                    });
                    summary.conflicts += 1;
                    self.store.enqueue(
                        &self.account_id,
                        OpKind::Upload,
                        None,
                        Some(&copy),
                        None,
                    )?;
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        if !transfer::FileVersion::capture_rooted(&rooted, &path)?.same_content_as(source_version) {
            return Err(transfer::file_changed_error(&path, "uploading"));
        }
        let content_hash = source_file.try_clone().ok().and_then(|mut file| {
            file.seek(SeekFrom::Start(0)).ok()?;
            crate::hash::quick_xor_reader(&mut file).ok()
        });
        if !transfer::FileVersion::capture_rooted(&rooted, &path)?.same_content_as(source_version) {
            return Err(transfer::file_changed_error(&path, "hashing after upload"));
        }

        // `Inherit`, never `Pinned`: stamping uploads as always-keep silently
        // converts everything the user ever created into pinned content. The
        // live symptom was a deleted folder resurrecting on every delta pass —
        // "pinned" told the applier its local absence was a hole to re-fill.
        let mut record = record_from_remote(
            &self.account_id,
            &item,
            ItemState::Synced,
            PinState::Inherit,
        );
        record.parent_id = Some(parent_id);
        let committed = self.store.upsert_item_after_mutation(
            &record,
            op.precondition.as_deref(),
            op.item_id.is_some(),
        )?;
        if !committed {
            tracing::debug!(item_id = %record.id, "newer delta already committed upload result");
            summary.uploaded += 1;
            return Ok(());
        }
        if let Ok(meta) = source_file.metadata() {
            let local = LocalStat::from_metadata(&meta);
            if let Err(error) = self.store.set_local_stat(
                &record.id,
                OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
                Some(local.size),
                Some(local.ino),
            ) {
                // The service has already committed the upload. Retrying a
                // brand-new create could only collide with that same item;
                // leave reconciliation to the next local/delta pass instead.
                tracing::warn!(path = %path.display(), %error, "could not record uploaded local version");
            }
        }

        let default_pin = self.config.download_new.default_pin();
        let pin = match self.store.effective_pin(&record.id, default_pin) {
            Ok(pin) => pin,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not resolve uploaded file retention policy");
                default_pin
            }
        };
        let marks = Placeholder::mark_hydrated_file_with_hash(
            &source_file,
            &record.id,
            record.ctag.as_deref(),
            content_hash.as_deref(),
        )
        .and_then(|()| Placeholder::set_pin_file(&source_file, pin.as_str()));
        if let Err(error) = marks {
            // As above, a local metadata failure must not turn a committed
            // remote create into a duplicate retry. A later scan repairs marks.
            tracing::warn!(path = %path.display(), %error, "could not publish uploaded placeholder marks");
        }
        summary.uploaded += 1;
        self.emit(SyncEvent::ItemDone {
            name,
            path,
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
        let mut record = record_from_remote(
            &self.account_id,
            &item,
            ItemState::Synced,
            PinState::Inherit,
        );
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
        // The name for the activity log, read before the record is gone.
        let name = self
            .store
            .item(item_id)?
            .map(|record| record.name)
            .unwrap_or_default();
        let activity_path = self.store.local_path(&self.account()?, item_id)?;
        if let Some(path) = &activity_path {
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    tracing::info!(path = %path.display(), "local delete was undone; cancelling remote delete");
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match self.graph.delete(item_id, op.precondition.as_deref()).await {
            Ok(()) => {}
            // Already gone remotely: the outcome we wanted.
            Err(Error::Graph { status: 404, .. }) => {}
            // A remote edit won after the local delete was decided. Preserve
            // that newer version; the next delta recreates it locally.
            Err(Error::Graph { status: 412, .. }) => {
                self.store
                    .mark_remote_race(item_id, op.precondition.as_deref())?;
                self.emit(SyncEvent::SafetyStop {
                    reason: format!(
                        "remote item {name} changed while its local deletion was pending; keeping the newer remote version"
                    ),
                    kind: SafetyStopKind::General,
                });
                summary.conflicts += 1;
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        if !self
            .store
            .delete_subtree_if_precondition(item_id, op.precondition.as_deref())?
        {
            tracing::debug!(
                item_id,
                "newer delta retained a concurrently deleted item record"
            );
        }
        summary.deleted_remote += 1;
        self.emit(SyncEvent::Deleted {
            name,
            path: activity_path,
        });
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

        let item = match self
            .graph
            .move_item(
                item_id,
                Some(&new_parent),
                new_name.as_deref(),
                op.precondition.as_deref(),
            )
            .await
        {
            Ok(item) => item,
            Err(Error::Graph { status: 412, .. }) => {
                self.store.cancel_outbound_ops_for_version(
                    &self.account_id,
                    item_id,
                    op.precondition.as_deref(),
                )?;
                if !path.exists() {
                    self.store
                        .mark_remote_race(item_id, op.precondition.as_deref())?;
                    return Ok(());
                }
                let local = stat_of(&path);
                let copy = self.make_conflict_copy(&path)?;
                if let Some(local) = local {
                    self.store.clear_local_stat_if_inode(item_id, local.ino)?;
                }
                self.store
                    .mark_remote_race(item_id, op.precondition.as_deref())?;
                self.scan_local_paths(std::slice::from_ref(&copy), summary)?;
                summary.conflicts += 1;
                self.emit(SyncEvent::Conflict {
                    original: name_for_path(&path),
                    copy: name_for_path(&copy),
                    path: path.clone(),
                });
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut record = record_from_remote(
            &self.account_id,
            &item,
            ItemState::Synced,
            PinState::Inherit,
        );
        record.parent_id = Some(new_parent);
        let committed =
            self.store
                .upsert_item_after_mutation(&record, op.precondition.as_deref(), true)?;
        if !committed {
            tracing::debug!(item_id, "newer delta already committed move result");
            summary.uploaded += 1;
            return Ok(());
        }
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

/// Backoff between retries of a queued operation: 2s, 4s, 8s … capped at 5min.
fn retry_delay(attempts: u32) -> Duration {
    Duration::from_secs(1u64 << (attempts + 1).min(8)).min(Duration::from_secs(300))
}

fn name_for_path(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn is_account_credential_error(error: &Error) -> bool {
    matches!(
        error,
        Error::AuthRequired(_) | Error::Keyring(_) | Error::KeyringLocked(_)
    )
}

#[cfg(test)]
mod tests {
    use super::is_account_credential_error;
    use crate::Error;

    #[test]
    fn credential_failures_escape_the_file_retry_queue() {
        assert!(is_account_credential_error(&Error::AuthRequired(
            "expired".into()
        )));
        assert!(is_account_credential_error(&Error::Keyring(
            "service unavailable".into()
        )));
        assert!(is_account_credential_error(&Error::KeyringLocked(
            "login collection".into()
        )));
        assert!(!is_account_credential_error(&Error::other("bad name")));
    }
}
