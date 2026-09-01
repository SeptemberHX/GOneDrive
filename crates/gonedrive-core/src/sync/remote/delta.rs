//! Durable Graph delta collection, lazy first-run anchoring, and resync sweep.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::config::DownloadPolicy;
use crate::db::{Account, ItemState, OpKind, PinState};
use crate::error::{Error, Result};
use crate::models::{DriveItem, ParentReference};
use crate::sync::plan::{order_parents_first, partition_delta, record_from_remote, SkipReason};

use super::super::engine::{stat_of, Engine, SyncEvent, SyncSummary};

const APPLY_CHECKPOINT_ITEMS: usize = 1_000;

impl Engine {
    pub(super) async fn pull_remote(&self, summary: &mut SyncSummary) -> Result<()> {
        let mut account = self.account()?;
        {
            let _commit = self.remote_commit.lock().await;
            self.apply_deferred_remote(&account, summary).await?;
        }
        let existing = self.store.delta_batch(&self.account_id)?;

        // CFAPI-style partial population does not require a historical walk of
        // the whole drive. A truly fresh metadata-only account first anchors a
        // latest cursor, persists it, and then fills directories independently.
        // Accounts with existing metadata or an interrupted batch deliberately
        // retain the full-enumeration path so a new baseline cannot hide an old
        // remote deletion.
        let lazy_bootstrap = account.delta_link.is_none()
            && existing.is_none()
            && self.on_demand_enabled()
            && self.config.download_new == DownloadPolicy::Never
            && self.store.item_count(&self.account_id)? <= 1;
        if lazy_bootstrap {
            account = self.establish_lazy_baseline(&account).await?;
        }

        // Root population is resumable and independent of delta processing. A
        // transient listing/HSM failure leaves it Partial and is retried next
        // poll, while the durable change cursor still advances normally.
        if self.on_demand_enabled() && account.delta_link.is_some() && existing.is_none() {
            if let Err(error) = self.populate_folder(std::path::Path::new("")).await {
                summary.failed = summary.failed.saturating_add(1);
                tracing::warn!(%error, "root population failed; delta processing will continue");
                self.emit(SyncEvent::Failed {
                    name: "OneDrive".into(),
                    path: Some(account.root_local_path.clone()),
                    error: error.to_string(),
                });
            }
        }

        account = self.account()?;
        let initial_link = account.enum_link.clone().or(account.delta_link.clone());
        let mode = if let Some(batch) = &existing {
            batch.mode.clone()
        } else if account.delta_link.is_none() {
            let generation = self.store.begin_resync(&self.account_id)?;
            self.store
                .begin_delta_batch(&self.account_id, "initial", Some(generation))?;
            "initial".to_string()
        } else {
            self.store
                .begin_delta_batch(&self.account_id, "incremental", None)?;
            "incremental".to_string()
        };
        if mode != "initial" && mode != "incremental" {
            return Err(Error::other(format!(
                "cannot pull remote changes from a {mode} delta batch"
            )));
        }

        // A pre-v11 interrupted first pass is named `incremental`, but it still
        // has no accepted deltaLink. Resume it progressively instead of throwing
        // away the pages the older daemon already persisted.
        let progressive = account.delta_link.is_none();
        let batch = self
            .collect_delta_batch(
                &mode,
                initial_link.as_deref(),
                progressive,
                &account,
                summary,
            )
            .await?;
        let seen;
        {
            let _commit = self.remote_commit.lock().await;
            seen = self
                .apply_staged_delta(&account, summary, batch.generation)
                .await?;
            self.apply_deferred_remote(&account, summary).await?;
            if mode == "initial" {
                let generation = batch
                    .generation
                    .ok_or_else(|| Error::other("initial delta batch has no generation"))?;
                self.finish_resync(&account, generation, &seen, summary)?;
            }
            self.store
                .finish_delta_batch(&self.account_id, batch.delta_link.as_deref())?;
        }
        summary.enumerated = batch.item_count;
        Ok(())
    }

    /// Persist a current delta cursor before any partial directory listing.
    /// This ordering is the safety invariant that closes the children-pagination
    /// race without making first login enumerate six figures of old metadata.
    async fn establish_lazy_baseline(&self, account: &Account) -> Result<Account> {
        let page = self.graph.latest_delta().await?;
        if page.next_link.is_some() || !page.value.is_empty() {
            return Err(Error::other(
                "Graph returned items while establishing a latest delta cursor",
            ));
        }
        let delta_link = page
            .delta_link
            .as_deref()
            .ok_or_else(|| Error::other("Graph did not return a latest delta cursor"))?;
        let root_id = account
            .root_item_id
            .as_deref()
            .ok_or_else(|| Error::other("the account has no drive root id"))?;
        self.store
            .upsert_item(&crate::db::ItemRecord::root(&self.account_id, root_id))?;
        self.store
            .set_folder_population(&self.account_id, root_id, false)?;
        // Cursor last: a crash before this write simply obtains a fresh baseline
        // on restart; a crash after it sees the durable Partial marker and knows
        // the root children listing still has to be resumed.
        self.store
            .set_delta_link(&self.account_id, Some(delta_link))?;
        self.account()
    }

    /// Fetch and apply one folder's direct children, following Graph's opaque
    /// continuation links. This is intentionally not treated as a complete
    /// snapshot: the background delta owns deletion/reconciliation semantics.
    pub(crate) async fn fetch_folder_children(
        &self,
        account: &Account,
        parent_id: &str,
        summary: &mut SyncSummary,
    ) -> Result<u64> {
        // Hold the commit order across pagination. Delta requests may continue
        // collecting in parallel, but their newer changes cannot be published
        // before this directory snapshot has landed.
        let _commit = self.remote_commit.lock().await;
        let mut next = None;
        let mut count = 0u64;
        let mut pages = 0u64;
        loop {
            let page = self.graph.children(parent_id, next.as_deref()).await?;
            pages = pages.saturating_add(1);
            for mut item in page.value {
                if item.is_deleted() {
                    continue;
                }
                if item.parent_id().is_none() {
                    item.parent_reference = Some(ParentReference {
                        drive_id: Some(account.drive_id.clone()),
                        id: Some(parent_id.to_string()),
                        path: None,
                    });
                }
                count = count.saturating_add(1);
                if item.is_package() {
                    self.store.upsert_remote_exclusion(
                        &self.account_id,
                        &item.id,
                        Some(parent_id),
                        "package",
                        None,
                    )?;
                    summary.skipped = summary.skipped.saturating_add(1);
                    self.emit(SyncEvent::Skipped {
                        name: item.name.clone().unwrap_or_else(|| item.id.clone()),
                        reason: format!("{:?}", SkipReason::Package),
                    });
                    continue;
                }
                if let Err(error) = self.apply_remote(account, &item, summary) {
                    if !is_item_scoped_error(&error) {
                        return Err(error);
                    }
                    self.defer_item_apply_failure(account, &item, &error, summary)?;
                }
            }
            self.protect_pending_placeholders().await?;
            self.emit(SyncEvent::Enumerating {
                items: count,
                pages,
            });
            match page.next_link {
                Some(link) => next = Some(link),
                None => {
                    // Only a complete pagination is an authoritative local
                    // count. Partial/error pages leave the previous Graph
                    // summary intact and will be retried on the next open.
                    self.store
                        .set_folder_child_count(&self.account_id, parent_id, count)?;
                    self.apply_deferred_remote(account, summary).await?;
                    return Ok(count);
                }
            }
        }
    }

    /// Re-enumerate the whole drive after the service rejected our deltaLink.
    /// Anything absent from the completed fresh enumeration is gone remotely.
    pub(super) async fn full_resync(&self, summary: &mut SyncSummary) -> Result<()> {
        let account = self.account()?;
        let existing = self.store.delta_batch(&self.account_id)?;
        let (generation, initial_link) = match existing {
            Some(batch) if batch.mode == "full" => {
                let generation = batch
                    .generation
                    .ok_or_else(|| Error::other("full delta batch has no generation"))?;
                (generation, account.enum_link.clone())
            }
            Some(_) => {
                self.store.abort_delta_batch(&self.account_id)?;
                let generation = self.store.begin_resync(&self.account_id)?;
                self.store
                    .begin_delta_batch(&self.account_id, "full", Some(generation))?;
                (generation, None)
            }
            None => {
                let generation = self.store.begin_resync(&self.account_id)?;
                self.store
                    .begin_delta_batch(&self.account_id, "full", Some(generation))?;
                (generation, None)
            }
        };
        let batch = self
            .collect_delta_batch("full", initial_link.as_deref(), false, &account, summary)
            .await?;
        {
            let _commit = self.remote_commit.lock().await;
            let seen = self
                .apply_staged_delta(&account, summary, Some(generation))
                .await?;
            self.finish_resync(&account, generation, &seen, summary)?;
            self.store
                .finish_delta_batch(&self.account_id, batch.delta_link.as_deref())?;
        }
        summary.enumerated = batch.item_count;
        Ok(())
    }

    /// Commit absence information only after a complete fresh enumeration.
    fn finish_resync(
        &self,
        account: &Account,
        generation: i64,
        seen: &[String],
        summary: &mut SyncSummary,
    ) -> Result<()> {
        self.store.mark_seen(seen, generation)?;
        for stale in self
            .store
            .items_missed_by_resync(&self.account_id, generation)?
        {
            let Some(path) = self.store.local_path(account, &stale.id)? else {
                self.store.delete_subtree(&stale.id)?;
                continue;
            };
            // Honour the tombstone guarantee during a sweep too: a local edit
            // that never reached Graph is detached and re-offered, never erased.
            if !stale.is_folder {
                if let Some(local) = stat_of(&path) {
                    if crate::sync::plan::local_is_modified(&stale, &local) {
                        self.store.delete_subtree(&stale.id)?;
                        self.store.enqueue(
                            &self.account_id,
                            OpKind::Upload,
                            None,
                            Some(&path),
                            None,
                        )?;
                        continue;
                    }
                }
            }
            self.remove_local(&stale, &path, summary)?;
        }
        self.store
            .prune_remote_exclusions(&self.account_id, generation)?;
        Ok(())
    }

    /// Persist every page before applying it. Initial enumeration progressively
    /// exposes resolved upserts; later modes retain whole-batch application.
    /// Tombstones and cursor advancement always wait for the complete feed.
    async fn collect_delta_batch(
        &self,
        expected_mode: &str,
        initial_link: Option<&str>,
        progressive: bool,
        account: &Account,
        summary: &mut SyncSummary,
    ) -> Result<crate::db::DeltaBatch> {
        let mut exclusions = if progressive {
            self.store.remote_exclusion_ids(&self.account_id)?
        } else {
            Default::default()
        };
        if progressive {
            let generation = self
                .store
                .delta_batch(&self.account_id)?
                .and_then(|batch| batch.generation);
            let _commit = self.remote_commit.lock().await;
            self.apply_staged_upserts(account, summary, generation, false, &mut exclusions)
                .await?;
            self.protect_pending_placeholders().await?;
        }
        loop {
            let batch = self
                .store
                .delta_batch(&self.account_id)?
                .ok_or_else(|| Error::other("delta batch disappeared"))?;
            if batch.mode != expected_mode {
                return Err(Error::other(format!(
                    "cannot resume {} delta batch as {expected_mode}",
                    batch.mode
                )));
            }
            if batch.complete {
                return Ok(batch);
            }

            let current_account = self.account()?;
            let link = current_account
                .enum_link
                .as_deref()
                .or(if batch.page_count == 0 {
                    initial_link
                } else {
                    None
                });
            if batch.page_count > 0 && link.is_none() {
                return Err(Error::other(
                    "incomplete delta batch has no continuation link",
                ));
            }
            let page = self.graph.delta(link).await?;
            let payloads = page
                .value
                .iter()
                .map(|item| {
                    Ok((
                        item.id.clone(),
                        serde_json::to_string(item)?,
                        item.is_deleted(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            self.store.stage_delta_page(
                &self.account_id,
                &payloads,
                page.next_link.as_deref(),
                page.delta_link.as_deref(),
            )?;
            let current = self
                .store
                .delta_batch(&self.account_id)?
                .ok_or_else(|| Error::other("delta batch disappeared after staging"))?;
            if progressive {
                let _commit = self.remote_commit.lock().await;
                self.apply_staged_upserts(
                    account,
                    summary,
                    current.generation,
                    false,
                    &mut exclusions,
                )
                .await?;
                self.protect_pending_placeholders().await?;
            }
            self.emit(SyncEvent::Enumerating {
                items: current.item_count,
                pages: current.page_count,
            });
        }
    }

    /// Apply every final-per-id row, then process tombstones deepest-first.
    async fn apply_staged_delta(
        &self,
        account: &Account,
        summary: &mut SyncSummary,
        generation: Option<i64>,
    ) -> Result<Vec<String>> {
        let mut exclusions = self.store.remote_exclusion_ids(&self.account_id)?;
        self.apply_staged_upserts(account, summary, generation, true, &mut exclusions)
            .await?;
        self.protect_pending_placeholders().await?;

        let items = self
            .store
            .unapplied_delta_deletes(&self.account_id)?
            .into_iter()
            .map(|payload| serde_json::from_str::<DriveItem>(&payload).map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        let (_, deletes) = partition_delta(items);
        let mut applied = Vec::with_capacity(deletes.len());
        for item in &deletes {
            self.store
                .delete_deferred_remote_subtree(&self.account_id, &item.id)?;
            let excluded = self.store.remote_is_excluded(&self.account_id, &item.id)?;
            if excluded {
                self.store
                    .delete_remote_exclusion_subtree(&self.account_id, &item.id)?;
            }
            if !excluded || self.store.item(&item.id)?.is_some() {
                if let Err(error) = self.apply_remote(account, item, summary) {
                    if !is_item_scoped_error(&error) {
                        return Err(error);
                    }
                    self.defer_item_apply_failure(account, item, &error, summary)?;
                }
            }
            applied.push(item.id.clone());
        }
        self.store.mark_staged_applied(&self.account_id, &applied)?;
        self.store.staged_upsert_ids(&self.account_id)
    }

    /// Apply only upserts whose parent chain is already known. Unknown parents
    /// remain staged while pages arrive, then move into a durable deferred table
    /// so one sparse-namespace dependency cannot stop the global delta cursor.
    async fn apply_staged_upserts(
        &self,
        account: &Account,
        summary: &mut SyncSummary,
        generation: Option<i64>,
        require_resolved: bool,
        exclusions: &mut HashSet<String>,
    ) -> Result<()> {
        let items = self
            .store
            .unapplied_delta_upserts(&self.account_id)?
            .into_iter()
            .map(|payload| serde_json::from_str::<DriveItem>(&payload).map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        let mut applied = Vec::with_capacity(items.len());
        let mut unresolved: Vec<(DriveItem, String, String)> = Vec::new();

        for item in order_parents_first(items) {
            let payload = serde_json::to_string(&item)?;

            if item.is_package() {
                self.store
                    .delete_deferred_remote_subtree(&self.account_id, &item.id)?;
                self.store.upsert_remote_exclusion(
                    &self.account_id,
                    &item.id,
                    item.parent_id(),
                    "package",
                    generation,
                )?;
                summary.skipped += 1;
                self.emit(SyncEvent::Skipped {
                    name: item.name.clone().unwrap_or_else(|| item.id.clone()),
                    reason: format!("{:?}", SkipReason::Package),
                });
                exclusions.insert(item.id.clone());
                applied.push(item.id);
                self.commit_staged_slice(&mut applied, false).await?;
                continue;
            }

            if item.is_root() {
                self.apply_remote(account, &item, summary)?;
                self.store
                    .remove_deferred_remote(&self.account_id, &item.id)?;
                applied.push(item.id);
                self.commit_staged_slice(&mut applied, false).await?;
                continue;
            }

            let Some(parent_id) = item.parent_id() else {
                let reason = format!("remote item {} has no parent", item.id);
                unresolved.push((item, payload, reason));
                continue;
            };
            if exclusions.contains(parent_id) {
                self.store
                    .delete_deferred_remote_subtree(&self.account_id, &item.id)?;
                self.store.upsert_remote_exclusion(
                    &self.account_id,
                    &item.id,
                    Some(parent_id),
                    "package-descendant",
                    generation,
                )?;
                exclusions.insert(item.id.clone());
                applied.push(item.id);
                self.commit_staged_slice(&mut applied, false).await?;
                continue;
            }
            if self.store.relative_path(parent_id)?.is_none() {
                let reason = format!(
                    "remote item {} ({}) is waiting for unknown parent {parent_id}",
                    item.id,
                    item.name.as_deref().unwrap_or("unnamed")
                );
                unresolved.push((item, payload, reason));
                continue;
            }

            if exclusions.remove(&item.id) {
                self.store
                    .remove_remote_exclusion(&self.account_id, &item.id)?;
            }
            if let Err(error) = self.apply_remote(account, &item, summary) {
                if !is_item_scoped_error(&error) {
                    return Err(error);
                }
                // Keep the success path lean: a first enumeration can contain
                // hundreds of thousands of items, so the extra DB/path probes
                // used to prove this safe are paid only after an actual error.
                let safe_to_record_without_local =
                    self.failed_new_remote_target_is_absent(account, &item)?;
                if safe_to_record_without_local && !matches!(error, Error::Db(_)) {
                    // Metadata and local materialisation are separate facts. A
                    // new item whose target was definitely absent cannot hide
                    // local content; retain the remote record in Error state so
                    // the UI can show it and the placeholder repair path can
                    // retry it, while the delta cursor remains free to advance.
                    let record = record_from_remote(
                        &self.account_id,
                        &item,
                        ItemState::Error,
                        PinState::Inherit,
                    );
                    self.store.upsert_item(&record)?;
                }
                self.defer_item_apply_failure(account, &item, &error, summary)?;
                applied.push(item.id);
                self.commit_staged_slice(&mut applied, false).await?;
                continue;
            }
            self.store
                .remove_deferred_remote(&self.account_id, &item.id)?;
            applied.push(item.id);
            self.commit_staged_slice(&mut applied, false).await?;
        }
        self.commit_staged_slice(&mut applied, true).await?;

        if require_resolved && !unresolved.is_empty() {
            for (item, payload, reason) in unresolved {
                self.store.defer_remote_change(
                    &self.account_id,
                    &item.id,
                    item.parent_id(),
                    &payload,
                )?;
                summary.failed = summary.failed.saturating_add(1);
                tracing::warn!(item_id = %item.id, %reason, "deferred remote item without a known parent");
                self.emit(SyncEvent::Failed {
                    name: item.name.clone().unwrap_or_else(|| item.id.clone()),
                    path: None,
                    error: reason,
                });
                applied.push(item.id);
            }
            self.commit_staged_slice(&mut applied, true).await?;
        }
        Ok(())
    }

    /// Replay exact delta payloads that were previously missing an ancestor.
    /// A successful parent in this pass can unlock its children later in the
    /// same ordered vector; anything still unresolved remains durable without
    /// blocking the global delta cursor.
    async fn apply_deferred_remote(
        &self,
        account: &Account,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let payloads = self.store.deferred_remote_payloads(&self.account_id)?;
        if payloads.is_empty() {
            return Ok(());
        }
        let items = payloads
            .into_iter()
            .map(|payload| serde_json::from_str::<DriveItem>(&payload).map_err(Error::from))
            .collect::<Result<Vec<_>>>()?;
        let mut applied = 0usize;
        for item in order_parents_first(items) {
            if item.is_deleted() {
                match self.apply_remote(account, &item, summary) {
                    Ok(()) => {
                        self.store
                            .delete_deferred_remote_subtree(&self.account_id, &item.id)?;
                        applied += 1;
                    }
                    Err(error) if is_item_scoped_error(&error) => {
                        tracing::warn!(item_id = %item.id, %error, "deferred remote deletion is still not locally representable");
                    }
                    Err(error) => return Err(error),
                }
                continue;
            }
            if item.is_package() {
                self.store.upsert_remote_exclusion(
                    &self.account_id,
                    &item.id,
                    item.parent_id(),
                    "package",
                    None,
                )?;
                self.store
                    .delete_deferred_remote_subtree(&self.account_id, &item.id)?;
                applied += 1;
                continue;
            }
            let Some(parent_id) = item.parent_id() else {
                continue;
            };
            if self.store.relative_path(parent_id)?.is_none() {
                continue;
            }
            match self.apply_remote(account, &item, summary) {
                Ok(()) => {
                    self.store
                        .remove_deferred_remote(&self.account_id, &item.id)?;
                    applied += 1;
                }
                Err(error) if is_item_scoped_error(&error) => {
                    tracing::warn!(item_id = %item.id, %error, "deferred remote item is still not locally representable");
                }
                Err(error) => return Err(error),
            }
        }
        self.protect_pending_placeholders().await?;
        if applied > 0 {
            tracing::info!(applied, "replayed deferred remote items");
        }
        Ok(())
    }

    async fn commit_staged_slice(&self, applied: &mut Vec<String>, force: bool) -> Result<()> {
        if applied.is_empty() || (!force && applied.len() < APPLY_CHECKPOINT_ITEMS) {
            return Ok(());
        }
        self.store.mark_staged_applied(&self.account_id, applied)?;
        applied.clear();
        self.protect_pending_placeholders().await
    }

    /// A failed new remote item can be represented in Error state only when no
    /// local record or filesystem entry existed before the attempt. Every item-
    /// scoped failure is also deferred for replay; local reconciliation freezes
    /// both its id and target path until that replay succeeds.
    fn failed_new_remote_target_is_absent(
        &self,
        account: &Account,
        item: &DriveItem,
    ) -> Result<bool> {
        if item.is_root() || item.is_deleted() || self.store.item(&item.id)?.is_some() {
            return Ok(false);
        }
        let Some(path) = self.remote_item_path(account, item)? else {
            return Ok(false);
        };
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            // A component that exceeds this filesystem's byte limit cannot
            // conceal an existing entry under the exact same name.
            Err(error) if error.raw_os_error() == Some(libc::ENAMETOOLONG) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    fn remote_item_path(&self, account: &Account, item: &DriveItem) -> Result<Option<PathBuf>> {
        let (Some(parent_id), Some(name)) = (item.parent_id(), item.name.as_deref()) else {
            return Ok(None);
        };
        Ok(self
            .store
            .relative_path(parent_id)?
            .map(|parent| account.root_local_path.join(parent).join(name)))
    }

    fn report_item_apply_failure(
        &self,
        account: &Account,
        item: &DriveItem,
        error: &Error,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let name = item.name.clone().unwrap_or_else(|| item.id.clone());
        let path = self.remote_item_path(account, item)?.or_else(|| {
            self.store
                .item(&item.id)
                .ok()
                .flatten()
                .and_then(|known| self.store.local_path(account, &known.id).ok().flatten())
        });
        summary.failed = summary.failed.saturating_add(1);
        tracing::warn!(item_id = %item.id, %name, error = %error, "remote item could not be applied locally");
        self.emit(SyncEvent::Failed {
            name,
            path,
            error: error.to_string(),
        });
        Ok(())
    }

    fn defer_item_apply_failure(
        &self,
        account: &Account,
        item: &DriveItem,
        error: &Error,
        summary: &mut SyncSummary,
    ) -> Result<()> {
        let known_parent = self.store.item(&item.id)?.and_then(|known| known.parent_id);
        let payload = serde_json::to_string(item)?;
        self.store.defer_remote_change(
            &self.account_id,
            &item.id,
            item.parent_id().or(known_parent.as_deref()),
            &payload,
        )?;
        self.report_item_apply_failure(account, item, error, summary)
    }
}

fn is_item_scoped_error(error: &Error) -> bool {
    match error {
        Error::InvalidName { .. } | Error::Other(_) => true,
        Error::Io(error) => {
            matches!(
                error.kind(),
                std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
                    | std::io::ErrorKind::AlreadyExists
                    | std::io::ErrorKind::InvalidFilename
                    | std::io::ErrorKind::InvalidInput
                    | std::io::ErrorKind::NotADirectory
                    | std::io::ErrorKind::IsADirectory
                    | std::io::ErrorKind::DirectoryNotEmpty
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::WouldBlock
            ) || matches!(
                error.raw_os_error(),
                Some(
                    libc::ENAMETOOLONG
                        | libc::ENOTDIR
                        | libc::EISDIR
                        | libc::ENOTEMPTY
                        | libc::ELOOP
                )
            )
        }
        Error::Db(rusqlite::Error::SqliteFailure(error, _)) => {
            error.code == rusqlite::ErrorCode::ConstraintViolation
        }
        _ => false,
    }
}
