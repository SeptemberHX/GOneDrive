//! Durable pending-operation queue and mass-deletion transaction guard.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

use super::{now, DeleteGuardOutcome, OpKind, PendingOp, Store};

impl Store {
    // ---- pending operations ----------------------------------------------

    pub fn enqueue(
        &self,
        account_id: &str,
        kind: OpKind,
        item_id: Option<&str>,
        local_path: Option<&Path>,
        payload: Option<&str>,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            enqueue_pending(conn, account_id, kind, item_id, local_path, payload, None)
        })
    }

    /// Queue a remote mutation using the version observed when the local scan
    /// made its decision. A later delta may update the item before this row is
    /// executed; retaining the original value makes Graph reject that race.
    pub fn enqueue_conditional(
        &self,
        account_id: &str,
        kind: OpKind,
        item_id: &str,
        local_path: Option<&Path>,
        payload: Option<&str>,
        precondition: Option<&str>,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            enqueue_pending(
                conn,
                account_id,
                kind,
                Some(item_id),
                local_path,
                payload,
                precondition,
            )
        })
    }

    /// Accumulate conditional remote deletes in one durable quiet window.
    ///
    /// Scoped watcher scans may observe one filesystem deletion wave as many
    /// small batches. Count the union of already-pending and new delete roots,
    /// including their descendants, before any of them becomes executable.
    /// Crossing the guard atomically withdraws the whole window.
    pub fn enqueue_guarded_deletes(
        &self,
        account_id: &str,
        deletes: &[(&str, Option<&str>)],
        mass_delete_min: u64,
        mass_delete_fraction: f64,
        settle: std::time::Duration,
    ) -> Result<DeleteGuardOutcome> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS delete_guard_roots (
                     id TEXT PRIMARY KEY
                 );
                 DELETE FROM delete_guard_roots;",
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO delete_guard_roots(id)
                 SELECT item_id FROM pending_ops
                  WHERE account_id = ?1 AND kind = 'delete_remote'
                    AND item_id IS NOT NULL",
                params![account_id],
            )?;
            let mut new_deletes = Vec::new();
            {
                let mut insert =
                    tx.prepare("INSERT OR IGNORE INTO delete_guard_roots(id) VALUES (?1)")?;
                for &(item_id, precondition) in deletes {
                    if insert.execute(params![item_id])? != 0 {
                        new_deletes.push((item_id, precondition));
                    }
                }
            }

            let impacted: i64 = tx.query_row(
                "WITH RECURSIVE tree(id) AS (
                     SELECT item.id
                       FROM items AS item
                       JOIN delete_guard_roots AS root ON root.id = item.id
                      WHERE item.account_id = ?1
                     UNION
                     SELECT child.id
                       FROM items AS child
                       JOIN tree AS parent ON child.parent_id = parent.id
                      WHERE child.account_id = ?1
                 )
                 SELECT COUNT(*) FROM tree",
                params![account_id],
                |row| row.get(0),
            )?;
            let total: i64 = tx.query_row(
                "SELECT COUNT(*) FROM items WHERE account_id = ?1",
                params![account_id],
                |row| row.get(0),
            )?;
            let impacted = impacted as u64;
            let total = (total as u64).max(1);
            let refused =
                impacted > mass_delete_min && impacted as f64 / total as f64 > mass_delete_fraction;

            if refused {
                tx.execute(
                    "DELETE FROM pending_ops
                      WHERE account_id = ?1 AND kind = 'delete_remote'",
                    params![account_id],
                )?;
            } else {
                let window_changed = !new_deletes.is_empty();
                for (item_id, precondition) in new_deletes {
                    enqueue_pending(
                        &tx,
                        account_id,
                        OpKind::DeleteRemote,
                        Some(item_id),
                        None,
                        None,
                        precondition,
                    )?;
                }
                if window_changed {
                    let delay = i64::try_from(settle.as_secs()).unwrap_or(i64::MAX);
                    let not_before = now().saturating_add(delay);
                    tx.execute(
                        "UPDATE pending_ops
                            SET next_attempt_at = MAX(next_attempt_at, ?2)
                          WHERE account_id = ?1 AND kind = 'delete_remote'",
                        params![account_id, not_before],
                    )?;
                }
            }
            tx.execute("DELETE FROM delete_guard_roots", [])?;
            tx.commit()?;
            Ok(DeleteGuardOutcome {
                impacted,
                total,
                refused,
            })
        })
    }

    /// Queue a metadata-only move as one transaction with withdrawing a stale
    /// source-side delete and rebasing every descendant operation.
    pub fn enqueue_move(
        &self,
        account_id: &str,
        item_id: &str,
        old_path: &Path,
        new_path: &Path,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let precondition = prepare_local_move(&tx, account_id, item_id, old_path, new_path)?;
            enqueue_pending(
                &tx,
                account_id,
                OpKind::MoveRemote,
                Some(item_id),
                Some(new_path),
                None,
                precondition.as_deref(),
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Queue a metadata move and the content upload that belongs with it as
    /// one durable decision. A crash must never leave an edited rename with
    /// only its move half recorded. The source-side delete and every queued
    /// descendant path are updated in the same transaction.
    pub fn enqueue_move_and_upload(
        &self,
        account_id: &str,
        item_id: &str,
        old_path: &Path,
        new_path: &Path,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let precondition = prepare_local_move(&tx, account_id, item_id, old_path, new_path)?;
            enqueue_pending(
                &tx,
                account_id,
                OpKind::MoveRemote,
                Some(item_id),
                Some(new_path),
                None,
                precondition.as_deref(),
            )?;
            enqueue_pending(
                &tx,
                account_id,
                OpKind::Upload,
                Some(item_id),
                Some(new_path),
                None,
                precondition.as_deref(),
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Rewrite durable work after a remote folder/file move changed its local
    /// absolute path. Operations keyed by item id otherwise keep trying the old
    /// name forever; path-keyed new uploads are coalesced with any row already
    /// queued for the destination.
    pub fn rebase_pending_paths(
        &self,
        account_id: &str,
        old_path: &Path,
        new_path: &Path,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            rebase_pending_paths_on(&tx, account_id, old_path, new_path)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Operations whose backoff has elapsed, oldest first.
    pub fn due_ops(&self, account_id: &str, limit: usize) -> Result<Vec<PendingOp>> {
        self.due_ops_where(account_id, limit, None, false)
    }

    /// Downloads due now, oldest first. A remote delta uses this view so cloud
    /// changes are not stuck behind an unrelated backlog of local uploads.
    pub fn due_download_ops(&self, account_id: &str, limit: usize) -> Result<Vec<PendingOp>> {
        self.due_ops_where(account_id, limit, Some(true), false)
    }

    /// Local-to-remote work due now. Kept disjoint from downloads so a slow
    /// upload cannot prevent a fresh delta from materialising remote changes.
    pub fn due_outbound_ops(&self, account_id: &str, limit: usize) -> Result<Vec<PendingOp>> {
        self.due_ops_where(account_id, limit, Some(false), false)
    }

    /// Outbound work that cannot remove cloud data. Used while a full local
    /// verification is still discovering deletion roots: uploads and moves
    /// keep flowing, but deletes wait until the complete tree is evaluated.
    pub fn due_non_delete_outbound_ops(
        &self,
        account_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingOp>> {
        self.due_ops_where(account_id, limit, Some(false), true)
    }

    fn due_ops_where(
        &self,
        account_id: &str,
        limit: usize,
        downloads: Option<bool>,
        exclude_remote_deletes: bool,
    ) -> Result<Vec<PendingOp>> {
        self.with_conn(|conn| {
            let filter = match (downloads, exclude_remote_deletes) {
                (Some(false), true) => {
                    "AND kind NOT IN ('download', 'delete_remote')
                     AND NOT EXISTS (
                         SELECT 1 FROM pending_ops AS download
                          WHERE download.account_id = pending_ops.account_id
                            AND download.item_id = pending_ops.item_id
                            AND download.kind = 'download'
                     )"
                }
                (Some(true), _) => "AND kind = 'download'",
                (Some(false), false) => {
                    "AND kind <> 'download'
                     AND NOT EXISTS (
                         SELECT 1 FROM pending_ops AS download
                          WHERE download.account_id = pending_ops.account_id
                            AND download.item_id = pending_ops.item_id
                            AND download.kind = 'download'
                     )"
                }
                (None, _) => "",
            };
            let sql = format!(
                "SELECT id, account_id, kind, item_id, local_path, payload, precondition,
                        attempts, next_attempt_at, last_error
                   FROM pending_ops
                  WHERE account_id = ?1 AND next_attempt_at <= ?2 {filter}
                  ORDER BY CASE WHEN kind = 'download' THEN 0 ELSE 1 END, id LIMIT ?3"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![account_id, now(), limit as i64], row_to_pending)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// A bounded snapshot of all durable work, including delayed retries.
    pub fn pending_ops(&self, account_id: &str, limit: usize) -> Result<Vec<PendingOp>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, account_id, kind, item_id, local_path, payload, precondition,
                        attempts, next_attempt_at, last_error
                   FROM pending_ops
                  WHERE account_id = ?1
                  ORDER BY CASE WHEN kind = 'download' THEN 0 ELSE 1 END,
                           next_attempt_at, id
                  LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![account_id, limit as i64], row_to_pending)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn complete_op(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Reload a queued operation immediately before execution. Queue rows can
    /// be cancelled or have their conditional version advanced after a due
    /// batch was selected, notably when a move and upload target the same item.
    pub fn due_pending_op(&self, id: i64) -> Result<Option<PendingOp>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, account_id, kind, item_id, local_path, payload, precondition,
                            attempts, next_attempt_at, last_error
                       FROM pending_ops
                      WHERE id = ?1 AND next_attempt_at <= ?2",
                    params![id, now()],
                    row_to_pending,
                )
                .optional()?)
        })
    }

    /// Withdraw stale outbound decisions for one item without touching a newer
    /// decision that a concurrent local reconciliation recorded with a fresh
    /// precondition. Downloads are owned by the remote worker and stay queued.
    pub fn cancel_outbound_ops_for_version(
        &self,
        account_id: &str,
        item_id: &str,
        precondition: Option<&str>,
    ) -> Result<usize> {
        self.with_conn(|conn| {
            Ok(conn.execute(
                "DELETE FROM pending_ops
                  WHERE account_id = ?1 AND item_id = ?2 AND precondition IS ?3
                    AND kind IN ('upload', 'delete_remote', 'move_remote')",
                params![account_id, item_id, precondition],
            )?)
        })
    }

    /// A locally visible item is no longer a deletion candidate. Withdraw the
    /// delayed delete immediately so a later disappearance starts a fresh
    /// quiet-window generation instead of inheriting the old deadline.
    pub fn cancel_pending_delete(&self, account_id: &str, item_id: &str) -> Result<usize> {
        self.with_conn(|conn| {
            Ok(conn.execute(
                "DELETE FROM pending_ops
                  WHERE account_id = ?1 AND item_id = ?2 AND kind = 'delete_remote'",
                params![account_id, item_id],
            )?)
        })
    }

    /// Record a failure and schedule the retry.
    pub fn fail_op(
        &self,
        id: i64,
        error: &str,
        retry_in: std::time::Duration,
    ) -> Result<Option<u32>> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE pending_ops
                    SET attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3
                  WHERE id = ?1",
                params![id, error, now() + retry_in.as_secs() as i64],
            )?;
            if changed == 0 {
                return Ok(None);
            }
            let attempts: i64 = conn.query_row(
                "SELECT attempts FROM pending_ops WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            Ok(Some(attempts as u32))
        })
    }

    pub fn pending_count(&self, account_id: &str) -> Result<u64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM pending_ops WHERE account_id = ?1",
                params![account_id],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }

    /// Queued operations whose retry delay has elapsed. Zero with a non-zero
    /// [`Self::pending_count`] means everything left is waiting out a backoff.
    pub fn due_pending_count(&self, account_id: &str) -> Result<u64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM pending_ops WHERE account_id = ?1 AND next_attempt_at <= ?2",
                params![account_id, now()],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }
}

fn row_to_pending(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingOp> {
    Ok(PendingOp {
        id: row.get(0)?,
        account_id: row.get(1)?,
        kind: OpKind::parse(&row.get::<_, String>(2)?).unwrap_or(OpKind::Upload),
        item_id: row.get(3)?,
        local_path: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
        payload: row.get(5)?,
        precondition: row.get(6)?,
        attempts: row.get::<_, i64>(7)? as u32,
        next_attempt_at: row.get(8)?,
        last_error: row.get(9)?,
    })
}

fn prepare_local_move(
    conn: &Connection,
    account_id: &str,
    item_id: &str,
    old_path: &Path,
    new_path: &Path,
) -> Result<Option<String>> {
    let precondition: Option<String> = conn
        .query_row(
            "SELECT COALESCE(etag, ctag) FROM items
              WHERE account_id = ?1 AND id = ?2",
            params![account_id, item_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    conn.execute(
        "WITH RECURSIVE moved_tree(id) AS (
             SELECT id FROM items WHERE account_id = ?1 AND id = ?2
             UNION ALL
             SELECT child.id
               FROM items AS child
               JOIN moved_tree AS parent ON child.parent_id = parent.id
              WHERE child.account_id = ?1
         )
         DELETE FROM pending_ops
          WHERE account_id = ?1 AND kind = 'delete_remote'
            AND item_id IN (SELECT id FROM moved_tree)",
        params![account_id, item_id],
    )?;
    rebase_pending_paths_on(conn, account_id, old_path, new_path)?;
    Ok(precondition)
}

fn rebase_pending_paths_on(
    conn: &Connection,
    account_id: &str,
    old_path: &Path,
    new_path: &Path,
) -> Result<()> {
    let old = old_path.to_string_lossy().into_owned();
    let new = new_path.to_string_lossy().into_owned();
    conn.execute(
        "UPDATE OR IGNORE pending_ops
            SET local_path = ?3 || substr(local_path, length(?2) + 1)
          WHERE account_id = ?1
            AND (local_path = ?2 OR substr(local_path, 1, length(?2) + 1) = ?2 || '/')",
        params![account_id, old, new],
    )?;
    // A row left at the old path collided with equivalent work already queued
    // at the destination; the destination row is the one to keep.
    conn.execute(
        "DELETE FROM pending_ops
          WHERE account_id = ?1
            AND (local_path = ?2 OR substr(local_path, 1, length(?2) + 1) = ?2 || '/')",
        params![account_id, old],
    )?;
    conn.execute(
        "UPDATE OR IGNORE upload_sessions
            SET key = ?3 || substr(key, length(?2) + 1)
          WHERE account_id = ?1
            AND (key = ?2 OR substr(key, 1, length(?2) + 1) = ?2 || '/')",
        params![account_id, old, new],
    )?;
    conn.execute(
        "DELETE FROM upload_sessions
          WHERE account_id = ?1
            AND (key = ?2 OR substr(key, 1, length(?2) + 1) = ?2 || '/')",
        params![account_id, old],
    )?;
    Ok(())
}

fn enqueue_pending(
    conn: &Connection,
    account_id: &str,
    kind: OpKind,
    item_id: Option<&str>,
    local_path: Option<&Path>,
    payload: Option<&str>,
    precondition: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO pending_ops
           (account_id, kind, item_id, local_path, payload, precondition,
            next_attempt_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)
         ON CONFLICT(account_id, kind, dedup_key) DO UPDATE SET
           payload = excluded.payload,
           local_path = excluded.local_path,
           precondition = excluded.precondition,
           next_attempt_at = 0,
           attempts = 0,
           last_error = NULL",
        params![
            account_id,
            kind.as_str(),
            item_id,
            local_path.map(|p| p.to_string_lossy().into_owned()),
            payload,
            precondition,
            now(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}
