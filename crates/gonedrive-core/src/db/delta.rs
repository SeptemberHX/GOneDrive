//! Crash-safe Graph delta-page staging and cursor advancement.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;

use super::{now, DeltaBatch, Store};

impl Store {
    /// Persist the deltaLink returned by the last successful enumeration.
    pub fn set_delta_link(&self, account_id: &str, link: Option<&str>) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET delta_link = ?2 WHERE id = ?1",
                params![account_id, link],
            )?;
            Ok(())
        })
    }

    /// How many items the account has on record -- the denominator for the
    /// mass-deletion safety valve.
    pub fn item_count(&self, account_id: &str) -> Result<u64> {
        self.with_conn(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM items WHERE account_id = ?1",
                params![account_id],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        })
    }

    /// Remember where an unfinished enumeration got to.
    pub fn set_enum_link(&self, account_id: &str, link: Option<&str>) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET enum_link = ?2 WHERE id = ?1",
                params![account_id, link],
            )?;
            Ok(())
        })
    }

    // ---- remote changes waiting for a lazy parent ------------------------

    pub fn defer_remote_change(
        &self,
        account_id: &str,
        item_id: &str,
        parent_id: Option<&str>,
        payload: &str,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO deferred_remote_changes
                   (account_id, item_id, parent_id, payload, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, item_id) DO UPDATE SET
                   parent_id = excluded.parent_id,
                   payload = excluded.payload,
                   updated_at = excluded.updated_at",
                params![account_id, item_id, parent_id, payload, now()],
            )?;
            Ok(())
        })
    }

    pub fn deferred_remote_payloads(&self, account_id: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT payload FROM deferred_remote_changes
                  WHERE account_id = ?1 ORDER BY updated_at, item_id",
            )?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn deferred_remote_ids(
        &self,
        account_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT item_id FROM deferred_remote_changes WHERE account_id = ?1")?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn remove_deferred_remote(&self, account_id: &str, item_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM deferred_remote_changes
                  WHERE account_id = ?1 AND item_id = ?2",
                params![account_id, item_id],
            )?;
            Ok(())
        })
    }

    pub fn delete_deferred_remote_subtree(&self, account_id: &str, item_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "WITH RECURSIVE doomed(id) AS (
                     SELECT ?2
                     UNION ALL
                     SELECT child.item_id
                       FROM deferred_remote_changes child
                       JOIN doomed parent ON child.parent_id = parent.id
                      WHERE child.account_id = ?1
                 )
                 DELETE FROM deferred_remote_changes
                  WHERE account_id = ?1 AND item_id IN (SELECT id FROM doomed)",
                params![account_id, item_id],
            )?;
            Ok(())
        })
    }

    // ---- durable delta assembly -------------------------------------------

    pub fn delta_batch(&self, account_id: &str) -> Result<Option<DeltaBatch>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT mode, generation, item_count, page_count, complete, delta_link
                       FROM delta_batches WHERE account_id = ?1",
                    params![account_id],
                    |row| {
                        Ok(DeltaBatch {
                            mode: row.get(0)?,
                            generation: row.get(1)?,
                            item_count: row.get::<_, i64>(2)? as u64,
                            page_count: row.get::<_, i64>(3)? as u64,
                            complete: row.get::<_, i64>(4)? != 0,
                            delta_link: row.get(5)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn begin_delta_batch(
        &self,
        account_id: &str,
        mode: &str,
        generation: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM delta_staging WHERE account_id = ?1",
                params![account_id],
            )?;
            tx.execute(
                "DELETE FROM delta_batches WHERE account_id = ?1",
                params![account_id],
            )?;
            tx.execute(
                "INSERT INTO delta_batches (account_id, mode, generation)
                 VALUES (?1, ?2, ?3)",
                params![account_id, mode, generation],
            )?;
            tx.execute(
                "UPDATE accounts SET enum_link = NULL WHERE id = ?1",
                params![account_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Add one page atomically with the continuation needed for the next page.
    /// Repeated item ids replace the earlier occurrence, preserving Graph's
    /// documented "last occurrence wins" rule.
    pub fn stage_delta_page(
        &self,
        account_id: &str,
        items: &[(String, String, bool)],
        next_link: Option<&str>,
        delta_link: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut sequence: i64 = tx.query_row(
                "SELECT next_sequence FROM delta_batches WHERE account_id = ?1",
                params![account_id],
                |row| row.get(0),
            )?;
            for (item_id, payload, deleted) in items {
                tx.execute(
                    "INSERT INTO delta_staging
                       (account_id, item_id, sequence, payload, applied, deleted)
                     VALUES (?1, ?2, ?3, ?4, 0, ?5)
                     ON CONFLICT(account_id, item_id) DO UPDATE SET
                       sequence = excluded.sequence,
                       payload = excluded.payload,
                       applied = 0,
                       deleted = excluded.deleted",
                    params![account_id, item_id, sequence, payload, i64::from(*deleted)],
                )?;
                sequence += 1;
            }
            tx.execute(
                "UPDATE delta_batches SET
                   item_count = item_count + ?2,
                   page_count = page_count + 1,
                   next_sequence = ?3,
                   complete = ?4,
                   delta_link = ?5
                 WHERE account_id = ?1",
                params![
                    account_id,
                    items.len() as i64,
                    sequence,
                    i64::from(next_link.is_none()),
                    delta_link,
                ],
            )?;
            tx.execute(
                "UPDATE accounts SET enum_link = ?2 WHERE id = ?1",
                params![account_id, next_link],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn staged_delta_payloads(&self, account_id: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT payload FROM delta_staging
                  WHERE account_id = ?1 ORDER BY sequence",
            )?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Final-per-id upserts that have not reached the filesystem/database tree.
    pub fn unapplied_delta_upserts(&self, account_id: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT payload FROM delta_staging
                  WHERE account_id = ?1 AND applied = 0 AND deleted = 0
                  ORDER BY sequence",
            )?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Final-per-id tombstones. These are deliberately read only after the
    /// complete deltaLink has arrived, so a partial enumeration cannot delete.
    pub fn unapplied_delta_deletes(&self, account_id: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT payload FROM delta_staging
                  WHERE account_id = ?1 AND applied = 0 AND deleted = 1
                  ORDER BY sequence DESC",
            )?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Mark a successfully applied slice together. A crash before this commit
    /// merely replays an idempotent slice; a crash afterwards resumes at the
    /// next one.
    pub fn mark_staged_applied(&self, account_id: &str, item_ids: &[String]) -> Result<()> {
        if item_ids.is_empty() {
            return Ok(());
        }
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare(
                    "UPDATE delta_staging SET applied = 1
                      WHERE account_id = ?1 AND item_id = ?2",
                )?;
                for item_id in item_ids {
                    stmt.execute(params![account_id, item_id])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn staged_upsert_ids(&self, account_id: &str) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT item_id FROM delta_staging
                  WHERE account_id = ?1 AND deleted = 0 ORDER BY sequence",
            )?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    // ---- unsupported remote container trees -------------------------------

    /// OneNote packages are Graph containers but not ordinary filesystem
    /// folders. Persisting their ids lets later incremental child changes stay
    /// excluded even when the package itself is absent from that delta page.
    pub fn remote_is_excluded(&self, account_id: &str, item_id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM remote_exclusions
                      WHERE account_id = ?1 AND item_id = ?2",
                    params![account_id, item_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    pub fn remote_exclusion_ids(
        &self,
        account_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT item_id FROM remote_exclusions WHERE account_id = ?1")?;
            let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn upsert_remote_exclusion(
        &self,
        account_id: &str,
        item_id: &str,
        parent_id: Option<&str>,
        reason: &str,
        generation: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO remote_exclusions
                   (account_id, item_id, parent_id, reason, seen_gen)
                 VALUES (?1, ?2, ?3, ?4, COALESCE(?5, 0))
                 ON CONFLICT(account_id, item_id) DO UPDATE SET
                   parent_id = excluded.parent_id,
                   reason = excluded.reason,
                   seen_gen = CASE WHEN ?5 IS NULL
                                   THEN remote_exclusions.seen_gen
                                   ELSE excluded.seen_gen END",
                params![account_id, item_id, parent_id, reason, generation],
            )?;
            Ok(())
        })
    }

    pub fn remove_remote_exclusion(&self, account_id: &str, item_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM remote_exclusions
                  WHERE account_id = ?1 AND item_id = ?2",
                params![account_id, item_id],
            )?;
            Ok(())
        })
    }

    pub fn delete_remote_exclusion_subtree(&self, account_id: &str, item_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "WITH RECURSIVE subtree(id) AS (
                     SELECT item_id FROM remote_exclusions
                      WHERE account_id = ?1 AND item_id = ?2
                     UNION ALL
                     SELECT child.item_id
                       FROM remote_exclusions child
                       JOIN subtree parent ON child.parent_id = parent.id
                      WHERE child.account_id = ?1
                 )
                 DELETE FROM remote_exclusions
                  WHERE account_id = ?1 AND item_id IN (SELECT id FROM subtree)",
                params![account_id, item_id],
            )?;
            Ok(())
        })
    }

    pub fn prune_remote_exclusions(&self, account_id: &str, generation: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM remote_exclusions
                  WHERE account_id = ?1 AND seen_gen != ?2",
                params![account_id, generation],
            )?;
            Ok(())
        })
    }

    /// Advance the cursor only after every staged filesystem change succeeded.
    pub fn finish_delta_batch(&self, account_id: &str, delta_link: Option<&str>) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE accounts SET delta_link = ?2, enum_link = NULL WHERE id = ?1",
                params![account_id, delta_link],
            )?;
            tx.execute(
                "DELETE FROM delta_staging WHERE account_id = ?1",
                params![account_id],
            )?;
            tx.execute(
                "DELETE FROM delta_batches WHERE account_id = ?1",
                params![account_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn abort_delta_batch(&self, account_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM delta_staging WHERE account_id = ?1",
                params![account_id],
            )?;
            tx.execute(
                "DELETE FROM delta_batches WHERE account_id = ?1",
                params![account_id],
            )?;
            tx.execute(
                "UPDATE accounts SET enum_link = NULL WHERE id = ?1",
                params![account_id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }
}
