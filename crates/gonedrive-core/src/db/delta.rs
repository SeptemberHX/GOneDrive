//! Crash-safe Graph delta-page staging and cursor advancement.

use rusqlite::{params, OptionalExtension};

use crate::error::Result;

use super::{DeltaBatch, Store};

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
        items: &[(String, String)],
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
            for (item_id, payload) in items {
                tx.execute(
                    "INSERT INTO delta_staging (account_id, item_id, sequence, payload)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(account_id, item_id) DO UPDATE SET
                       sequence = excluded.sequence,
                       payload = excluded.payload",
                    params![account_id, item_id, sequence, payload],
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
