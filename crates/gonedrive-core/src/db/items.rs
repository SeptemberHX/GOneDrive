//! Remote item tree, derived paths, selection state, and resync generations.
//!
//! # Cycles
//!
//! Every path here is derived by walking `parent_id`, and nothing in SQLite
//! stops that column from describing a loop: a corrupted row, a half-applied
//! delta, a restored database. A recursive CTE that meets one never returns, so
//! a single bad row would hang the sync thread — and, through it, the daemon —
//! with no error and nothing in the log.
//!
//! Two defences, applied to every recursive query in this file. A walk *down*
//! the tree collects ids, so `UNION` rather than `UNION ALL` makes it
//! self-limiting: an id already visited is not visited again. A walk *up* the
//! tree carries a depth, and stops at 512 — far beyond any real drive, and
//! below the point where the query becomes expensive. `relative_path` then
//! reports the truncated chain as "no path", which is exactly how it already
//! reports an ancestor that has not arrived yet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::OffsetDateTime;

use crate::error::Result;
use crate::names::fold_case;

use super::{now, to_time, Account, ItemRecord, ItemState, OpKind, PinState, Store};

impl Store {
    // ---- items ------------------------------------------------------------

    pub fn upsert_item(&self, item: &ItemRecord) -> Result<()> {
        self.with_conn(|conn| upsert_item_on(conn, item))
    }

    /// Commit a successful conditional mutation without overwriting a newer
    /// delta that may already have reached the database. The response is safe
    /// to apply while the current row is still the attempted version, or while
    /// a concurrent delta already installed this same response version.
    ///
    /// A mutation such as a rename changes the remote eTag. Advance other
    /// durable decisions made against the old version in the same transaction;
    /// otherwise a rename followed by a content upload would reject its own
    /// upload with `412 Precondition Failed`.
    pub fn upsert_item_after_mutation(
        &self,
        item: &ItemRecord,
        precondition: Option<&str>,
        replacing_existing: bool,
    ) -> Result<bool> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let current: Option<Option<String>> = tx
                .query_row(
                    "SELECT COALESCE(etag, ctag) FROM items WHERE id = ?1",
                    params![item.id],
                    |row| row.get(0),
                )
                .optional()?;
            let response_version = item.etag.as_deref().or(item.ctag.as_deref());
            let allowed = match current {
                // A create has no prior row by definition. For a replacement,
                // absence means a concurrent delta deleted the item; applying
                // this older response would resurrect a record the service has
                // already told us is gone.
                None => !replacing_existing,
                Some(current) => {
                    current.as_deref() == response_version
                        || (replacing_existing && current.as_deref() == precondition)
                }
            };
            if allowed {
                upsert_item_on(&tx, item)?;
                if replacing_existing {
                    tx.execute(
                        "UPDATE pending_ops
                            SET precondition = ?1
                          WHERE account_id = ?2 AND item_id = ?3
                            AND precondition IS ?4
                            AND kind IN ('upload', 'delete_remote', 'move_remote')",
                        params![response_version, item.account_id, item.id, precondition],
                    )?;
                }
            }
            tx.commit()?;
            Ok(allowed)
        })
    }

    /// Apply a batch atomically, so a crash mid-page cannot leave the tree
    /// half-updated with a deltaLink that says it was fully applied.
    pub fn upsert_items(&self, items: &[ItemRecord]) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            for item in items {
                upsert_item_on(&tx, item)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn item(&self, id: &str) -> Result<Option<ItemRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("{ITEM_COLUMNS} WHERE id = ?1"),
                    params![id],
                    row_to_item,
                )
                .optional()?)
        })
    }

    /// Find a child by name, case-insensitively — the way the service matches.
    pub fn child_by_name(
        &self,
        account_id: &str,
        parent_id: &str,
        name: &str,
    ) -> Result<Option<ItemRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!(
                        // Written against the expression the unique index is
                        // built on, so a large directory is one index probe
                        // rather than a scan of all its children.
                        "{ITEM_COLUMNS} WHERE account_id = ?1 AND IFNULL(parent_id, '') = ?2 AND name_folded = ?3"
                    ),
                    params![account_id, parent_id, fold_case(name)],
                    row_to_item,
                )
                .optional()?)
        })
    }

    pub fn children(&self, account_id: &str, parent_id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND parent_id = ?2 ORDER BY name"
            ))?;
            let rows = stmt.query_map(params![account_id, parent_id], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Whether this directory's direct children have been published locally.
    /// `None` identifies a tree created by a pre-partial-population release;
    /// the caller may inspect its xattr once and persist the adopted state.
    pub fn folder_population(&self, account_id: &str, item_id: &str) -> Result<Option<bool>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT populated FROM folder_population
                      WHERE account_id = ?1 AND item_id = ?2",
                    params![account_id, item_id],
                    |row| row.get::<_, i64>(0).map(|value| value != 0),
                )
                .optional()?)
        })
    }

    pub fn set_folder_population(
        &self,
        account_id: &str,
        item_id: &str,
        populated: bool,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO folder_population (account_id, item_id, populated)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(account_id, item_id) DO UPDATE SET
                   populated = excluded.populated",
                params![account_id, item_id, i64::from(populated)],
            )?;
            Ok(())
        })
    }

    /// Record the exact number of direct children observed after a complete
    /// Graph children pagination. A failed/incomplete page must not call this,
    /// otherwise a transient response could make a non-empty folder look
    /// empty until the next remote change.
    pub fn set_folder_child_count(
        &self,
        account_id: &str,
        item_id: &str,
        child_count: u64,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE items
                    SET child_count = ?3
                  WHERE account_id = ?1 AND id = ?2 AND is_folder = 1",
                params![
                    account_id,
                    item_id,
                    i64::try_from(child_count).unwrap_or(i64::MAX)
                ],
            )?;
            Ok(())
        })
    }

    /// Direct item count and direct-file bytes for every *known/populated*
    /// folder immediately below `parent_id`.
    ///
    /// A partial folder is intentionally omitted; callers use its remote
    /// `child_count` instead because its children have not been indexed yet.
    /// A recursive subtree query here would make opening a top-level folder
    /// proportional to the size of the entire drive.
    pub fn direct_child_summaries(
        &self,
        account_id: &str,
        parent_id: &str,
    ) -> Result<std::collections::HashMap<String, (u64, u64)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT folder.id,
                        COUNT(child.id),
                        COALESCE(SUM(CASE WHEN child.is_folder = 0 THEN child.size ELSE 0 END), 0)
                   FROM items folder
                   LEFT JOIN items child
                     ON child.account_id = folder.account_id
                    AND child.parent_id = folder.id
                   LEFT JOIN folder_population population
                     ON population.account_id = folder.account_id
                    AND population.item_id = folder.id
                  WHERE folder.account_id = ?1
                    AND folder.parent_id = ?2
                    AND folder.is_folder = 1
                    AND (population.populated IS NULL OR population.populated = 1)
                  GROUP BY folder.id",
            )?;
            let rows = stmt.query_map(params![account_id, parent_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, i64>(2)? as u64, row.get::<_, i64>(1)? as u64),
                ))
            })?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        })
    }

    pub fn items_in_state(&self, account_id: &str, state: ItemState) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND state = ?2"
            ))?;
            let rows = stmt.query_map(params![account_id, state.as_str()], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Files that can plausibly have a local placeholder. Deep metadata-only
    /// descendants of partial directories are excluded before any pathname is
    /// derived or opened, keeping HSM startup proportional to the published
    /// namespace instead of the whole cloud drive.
    pub fn placeholder_inventory_candidates(&self, account_id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "WITH RECURSIVE partial_tree(id) AS (
                     SELECT item_id FROM folder_population
                      WHERE account_id = ?1 AND populated = 0
                     UNION
                     SELECT child.id
                       FROM items child
                       JOIN partial_tree parent ON child.parent_id = parent.id
                      WHERE child.account_id = ?1 AND child.is_folder = 1
                 )
                 {ITEM_COLUMNS}
                   WHERE account_id = ?1
                     AND is_folder = 0
                     AND state IN ('dehydrated', 'hydrating', 'error')
                     AND (
                       local_ino IS NOT NULL
                       OR parent_id NOT IN (SELECT id FROM partial_tree)
                     )"
            ))?;
            let rows = stmt.query_map(params![account_id], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Populated directories whose direct partial children need open events.
    /// `local_ino` excludes metadata-only descendants learned from delta, so
    /// rebuilding ephemeral population boundaries is proportional to folders
    /// the user has actually browsed rather than the whole remote drive.
    pub fn population_watch_candidates(&self, account_id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS}
                  WHERE account_id = ?1
                    AND id IN (
                      SELECT items.id
                        FROM items
                        JOIN folder_population population
                          ON population.account_id = items.account_id
                         AND population.item_id = items.id
                       WHERE items.account_id = ?1
                         AND items.is_folder = 1
                         AND items.local_ino IS NOT NULL
                         AND population.populated = 1
                    )
                  ORDER BY id"
            ))?;
            let rows = stmt.query_map(params![account_id], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn set_item_state(&self, id: &str, state: ItemState) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE items SET state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, state.as_str(), now()],
            )?;
            Ok(())
        })
    }

    /// Mark an item as needing the remote version only if the database still
    /// holds the version against which a rejected mutation was attempted.
    pub fn mark_remote_race(&self, id: &str, precondition: Option<&str>) -> Result<bool> {
        self.with_conn(|conn| {
            Ok(conn.execute(
                "UPDATE items
                    SET state = 'dirty_remote', updated_at = ?3
                  WHERE id = ?1 AND COALESCE(etag, ctag) IS ?2",
                params![id, precondition, now()],
            )? > 0)
        })
    }

    /// Detach a local inode from an item only if that exact inode is still the
    /// recorded observation. A concurrent download may already have installed
    /// a newer inode, which this must not erase.
    pub fn clear_local_stat_if_inode(&self, id: &str, ino: u64) -> Result<bool> {
        self.with_conn(|conn| {
            Ok(conn.execute(
                "UPDATE items
                    SET local_mtime = NULL, local_size = NULL, local_ino = NULL, updated_at = ?3
                  WHERE id = ?1 AND local_ino = ?2",
                params![id, ino as i64, now()],
            )? > 0)
        })
    }

    pub fn set_pin_state(&self, id: &str, pin: PinState) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE items SET pin_state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, pin.as_str(), now()],
            )?;
            Ok(())
        })
    }

    /// Commit a successful user-requested space release.
    ///
    /// The item state, retention choice, and any stale download request are one
    /// durable decision. Leaving any one of them behind can make the next sync
    /// download a file whose blocks the user just released.
    pub fn commit_space_release(&self, account_id: &str, id: &str) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE items
                    SET state = 'dehydrated', pin_state = 'unpinned', updated_at = ?3
                  WHERE id = ?1 AND account_id = ?2",
                params![id, account_id, now()],
            )?;
            tx.execute(
                "DELETE FROM pending_ops
                  WHERE account_id = ?1 AND item_id = ?2 AND kind = 'download'",
                params![account_id, id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Commit content that has arrived through either a queued download or an
    /// on-demand read. Once the item is `synced`, another download for the same
    /// remote version is stale; retiring it in the same transaction prevents a
    /// crash or concurrent hydration from downloading the file twice.
    pub fn commit_hydration(
        &self,
        account_id: &str,
        id: &str,
        mtime: Option<OffsetDateTime>,
        size: Option<u64>,
        ino: Option<u64>,
        quickxor: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "UPDATE items
                    SET local_mtime = ?3, local_size = ?4, local_ino = ?5,
                        quickxor = COALESCE(?6, quickxor),
                        state = 'synced', updated_at = ?7
                  WHERE id = ?1 AND account_id = ?2",
                params![
                    id,
                    account_id,
                    mtime.map(|t| t.unix_timestamp()),
                    size.map(|v| v as i64),
                    ino.map(|v| v as i64),
                    quickxor,
                    now()
                ],
            )?;
            tx.execute(
                "DELETE FROM pending_ops
                  WHERE account_id = ?1 AND item_id = ?2 AND kind = 'download'",
                params![account_id, id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Commit a hydration only when this item has not already reached the
    /// synced state.  The helper can report the same `Filled` event more than
    /// once (for example when a final range read races the materialised-range
    /// fast path).  Making the state transition conditional gives concurrent
    /// completions one winner and prevents duplicate completion side effects.
    pub fn commit_hydration_once(
        &self,
        account_id: &str,
        id: &str,
        mtime: Option<OffsetDateTime>,
        size: Option<u64>,
        ino: Option<u64>,
        quickxor: Option<&str>,
    ) -> Result<bool> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let changed = tx.execute(
                "UPDATE items
                    SET local_mtime = ?3, local_size = ?4, local_ino = ?5,
                        quickxor = COALESCE(?6, quickxor),
                        state = 'synced', updated_at = ?7
                  WHERE id = ?1 AND account_id = ?2 AND state <> 'synced'",
                params![
                    id,
                    account_id,
                    mtime.map(|t| t.unix_timestamp()),
                    size.map(|v| v as i64),
                    ino.map(|v| v as i64),
                    quickxor,
                    now()
                ],
            )?;
            if changed > 0 {
                tx.execute(
                    "DELETE FROM pending_ops
                      WHERE account_id = ?1 AND item_id = ?2 AND kind = 'download'",
                    params![account_id, id],
                )?;
            }
            tx.commit()?;
            Ok(changed > 0)
        })
    }

    /// Record what we last observed on disk, so the next scan can tell an
    /// untouched file from a modified one without hashing it.
    pub fn set_local_stat(
        &self,
        id: &str,
        mtime: Option<OffsetDateTime>,
        size: Option<u64>,
        ino: Option<u64>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE items SET local_mtime = ?2, local_size = ?3, local_ino = ?4, updated_at = ?5
                   WHERE id = ?1",
                params![
                    id,
                    mtime.map(|t| t.unix_timestamp()),
                    size.map(|v| v as i64),
                    ino.map(|v| v as i64),
                    now()
                ],
            )?;
            Ok(())
        })
    }

    /// Delete an item and, for a folder, its whole subtree.
    ///
    /// Done with a recursive CTE rather than an application-level walk so a
    /// deleted 10 000-item folder is one statement inside one transaction.
    pub fn delete_subtree(&self, id: &str) -> Result<usize> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "WITH RECURSIVE doomed(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN doomed d ON i.parent_id = d.id
                 )
                 DELETE FROM pending_ops WHERE item_id IN (SELECT id FROM doomed)",
                params![id],
            )?;
            let n = tx.execute(
                "WITH RECURSIVE doomed(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN doomed d ON i.parent_id = d.id
                 )
                 DELETE FROM items WHERE id IN (SELECT id FROM doomed)",
                params![id],
            )?;
            tx.commit()?;
            Ok(n)
        })
    }

    /// Delete only the version against which Graph accepted a conditional
    /// request. A newer delta can win the race and keep its row intact.
    pub fn delete_subtree_if_precondition(
        &self,
        id: &str,
        precondition: Option<&str>,
    ) -> Result<bool> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let current: Option<Option<String>> = tx
                .query_row(
                    "SELECT COALESCE(etag, ctag) FROM items WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(current) = current else {
                tx.commit()?;
                return Ok(true);
            };
            if current.as_deref() != precondition {
                tx.commit()?;
                return Ok(false);
            }
            tx.execute(
                "WITH RECURSIVE doomed(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN doomed d ON i.parent_id = d.id
                 )
                 DELETE FROM pending_ops WHERE item_id IN (SELECT id FROM doomed)",
                params![id],
            )?;
            tx.execute(
                "WITH RECURSIVE doomed(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN doomed d ON i.parent_id = d.id
                 )
                 DELETE FROM items WHERE id IN (SELECT id FROM doomed)",
                params![id],
            )?;
            tx.commit()?;
            Ok(true)
        })
    }

    /// The item's path relative to the sync root.
    ///
    /// Returns `None` if the item is unknown or its parent chain is broken —
    /// which happens legitimately when a delta page arrives out of order and
    /// mentions a child before its parent.
    pub fn relative_path(&self, id: &str) -> Result<Option<PathBuf>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "WITH RECURSIVE chain(id, parent_id, name, depth) AS (
                     SELECT id, parent_id, name, 0 FROM items WHERE id = ?1
                     UNION ALL
                     SELECT i.id, i.parent_id, i.name, chain.depth + 1
                       FROM items i JOIN chain ON i.id = chain.parent_id
                      WHERE chain.depth < 512
                 )
                 SELECT name, parent_id FROM chain ORDER BY depth DESC",
            )?;
            let rows: Vec<(String, Option<String>)> = stmt
                .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            if rows.is_empty() {
                return Ok(None);
            }
            // The walk must terminate at the root (parent_id IS NULL); if it
            // does not, an ancestor is missing and the path is not yet known.
            if rows[0].1.is_some() {
                return Ok(None);
            }

            // Skip the root itself: its name is an artefact ("root"), and the
            // sync root directory supplies that level of the path.
            let mut path = PathBuf::new();
            for (name, _) in rows.iter().skip(1) {
                path.push(name);
            }
            Ok(Some(path))
        })
    }

    /// The item's absolute path on disk.
    pub fn local_path(&self, account: &Account, id: &str) -> Result<Option<PathBuf>> {
        Ok(self
            .relative_path(id)?
            .map(|rel| account.root_local_path.join(rel)))
    }

    /// Resolve an item's pin state by walking up to the nearest explicit choice.
    pub fn effective_pin(&self, id: &str, default: PinState) -> Result<PinState> {
        self.with_conn(|conn| {
            let found: Option<String> = conn
                .query_row(
                    "WITH RECURSIVE chain(id, parent_id, pin_state, depth) AS (
                         SELECT id, parent_id, pin_state, 0 FROM items WHERE id = ?1
                         UNION ALL
                         SELECT i.id, i.parent_id, i.pin_state, chain.depth + 1
                           FROM items i JOIN chain ON i.id = chain.parent_id
                          WHERE chain.depth < 512
                     )
                     SELECT pin_state FROM chain
                      WHERE pin_state <> 'inherit'
                      ORDER BY depth LIMIT 1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(found.map(|s| PinState::parse(&s)).unwrap_or(default))
        })
    }

    /// Whether the nearest explicit retention choice on this item's chain is
    /// an exclusion.
    ///
    /// This is a different question from "is content wanted": an account whose
    /// *default* is online-only still expects a local deletion to mean delete
    /// — the default never expressed an opinion about this item. Only a choice
    /// the user actually made ("don't keep this folder here") turns a missing
    /// local file into something other than a deletion.
    pub fn explicitly_excluded(&self, id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let found: Option<String> = conn
                .query_row(
                    "WITH RECURSIVE chain(id, parent_id, pin_state, depth) AS (
                         SELECT id, parent_id, pin_state, 0 FROM items WHERE id = ?1
                         UNION ALL
                         SELECT i.id, i.parent_id, i.pin_state, chain.depth + 1
                           FROM items i JOIN chain ON i.id = chain.parent_id
                          WHERE chain.depth < 512
                     )
                     SELECT pin_state FROM chain
                      WHERE pin_state <> 'inherit'
                      ORDER BY depth LIMIT 1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(found.as_deref() == Some("unpinned"))
        })
    }

    /// Whether the nearest explicit choice on this item's chain is "pinned".
    pub fn explicitly_pinned(&self, id: &str) -> Result<bool> {
        self.with_conn(|conn| {
            let found: Option<String> = conn
                .query_row(
                    "WITH RECURSIVE chain(id, parent_id, pin_state, depth) AS (
                         SELECT id, parent_id, pin_state, 0 FROM items WHERE id = ?1
                         UNION ALL
                         SELECT i.id, i.parent_id, i.pin_state, chain.depth + 1
                           FROM items i JOIN chain ON i.id = chain.parent_id
                          WHERE chain.depth < 512
                     )
                     SELECT pin_state FROM chain
                      WHERE pin_state <> 'inherit' ORDER BY depth LIMIT 1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(found.as_deref() == Some("pinned"))
        })
    }

    /// Every file in a subtree. Used when a newly pinned folder has to have its
    /// already-enumerated contents queued for download.
    pub fn files_in_subtree(&self, id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "WITH RECURSIVE tree(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 {ITEM_COLUMNS_CTE} WHERE id IN (SELECT id FROM tree) AND is_folder = 0"
            ))?;
            let rows = stmt.query_map(params![id], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Total remote size and file count of a subtree, so the user can be shown
    /// what a choice costs *before* making it.
    pub fn subtree_size(&self, id: &str) -> Result<(u64, u64)> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "WITH RECURSIVE tree(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 SELECT COALESCE(SUM(size), 0), COUNT(*) FROM items
                  WHERE id IN (SELECT id FROM tree) AND is_folder = 0",
                params![id],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )?)
        })
    }

    /// Number of records a remote delete of this item would remove, including
    /// folders. The mass-deletion guard must count the whole subtree: deleting
    /// one top-level folder can otherwise look like a harmless single item
    /// while actually erasing most of the account.
    pub fn subtree_item_count(&self, id: &str) -> Result<u64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "WITH RECURSIVE tree(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION
                     SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 SELECT COUNT(*) FROM tree",
                params![id],
                |row| row.get::<_, i64>(0),
            )? as u64)
        })
    }

    /// Every folder in an account, shallowest first.
    ///
    /// Ordering by the length of the derived path means a parent is always
    /// returned before its children, so materialising them in order never
    /// tries to create a directory inside one that does not exist yet.
    pub fn all_folders_shallow_first(&self, account_id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND is_folder = 1 AND parent_id IS NOT NULL"
            ))?;
            let rows = stmt.query_map(params![account_id], row_to_item)?;
            let mut folders: Vec<ItemRecord> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            // Depth by ancestor count; cheap because parent_id is indexed and
            // trees are shallow relative to their node count.
            let depth_of = |mut id: Option<String>| -> usize {
                let mut d = 0;
                while let Some(cur) = id {
                    match conn
                        .query_row(
                            "SELECT parent_id FROM items WHERE id = ?1",
                            params![cur],
                            |r| r.get::<_, Option<String>>(0),
                        )
                        .optional()
                    {
                        Ok(Some(p)) => {
                            id = p;
                            d += 1;
                        }
                        _ => break,
                    }
                }
                d
            };
            let mut keyed: Vec<(usize, ItemRecord)> = folders
                .drain(..)
                .map(|f| (depth_of(f.parent_id.clone()), f))
                .collect();
            keyed.sort_by_key(|(d, _)| *d);
            Ok(keyed.into_iter().map(|(_, f)| f).collect())
        })
    }

    /// Every file in an account.
    pub fn all_files(&self, account_id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND is_folder = 0"
            ))?;
            let rows = stmt.query_map(params![account_id], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Snapshot every item with one query. Used by the read-only full-scan
    /// verifier to build an in-memory expected tree without issuing one SQL
    /// query per pathname.
    pub fn all_items(&self, account_id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!("{ITEM_COLUMNS} WHERE account_id = ?1"))?;
            let rows = stmt.query_map(params![account_id], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Find an item by its path relative to the sync root.
    ///
    /// Resolved one component at a time through the case-folded index, so it
    /// matches names the way the service does rather than the way ext4 does.
    pub fn item_at_path(
        &self,
        account_id: &str,
        root_item_id: &str,
        relative: &Path,
    ) -> Result<Option<ItemRecord>> {
        let Some(mut current) = self.item(root_item_id)? else {
            return Ok(None);
        };
        for component in relative.components() {
            let name = component.as_os_str().to_string_lossy();
            if name == "." || name.is_empty() || name == "/" {
                continue;
            }
            match self.child_by_name(account_id, &current.id, &name)? {
                Some(child) => current = child,
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// When this record was last written, in seconds.
    ///
    /// Compared against an inode's birth time to tell a moved file from a new
    /// one that happens to have inherited a recycled inode number.
    pub fn item_recorded_at(&self, item_id: &str) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT updated_at FROM items WHERE id = ?1",
                    params![item_id],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    /// Rename a record in place, without touching anything else about it.
    ///
    /// Used when an item has to be parked under a reserved name so another
    /// item in the same delta batch can take the name it is vacating. The
    /// record must follow the inode or the next local scan reads the parked
    /// file as an unknown addition and the missing one as a deletion.
    pub fn rename_item_local(&self, item_id: &str, name: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE items SET name = ?2, name_folded = ?3, updated_at = ?4 WHERE id = ?1",
                params![item_id, name, fold_case(name), now()],
            )?;
            Ok(())
        })
    }

    /// Every item that currently claims a local inode, keyed by inode.
    ///
    /// Used to recognise a rename or move: the same inode showing up at a new
    /// path means the user moved the file, which is one PATCH remotely instead
    /// of a delete plus a full re-upload.
    pub fn items_by_inode(&self, account_id: &str) -> Result<HashMap<u64, ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND local_ino IS NOT NULL"
            ))?;
            let rows = stmt.query_map(params![account_id], row_to_item)?;
            let mut map = HashMap::new();
            for row in rows {
                let item = row?;
                if let Some(ino) = item.local_ino {
                    map.insert(ino, item);
                }
            }
            Ok(map)
        })
    }

    /// Begin a full re-enumeration, returning the generation to stamp items with.
    pub fn begin_resync(&self, account_id: &str) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET resync_gen = resync_gen + 1 WHERE id = ?1",
                params![account_id],
            )?;
            Ok(conn.query_row(
                "SELECT resync_gen FROM accounts WHERE id = ?1",
                params![account_id],
                |r| r.get(0),
            )?)
        })
    }

    pub fn mark_seen(&self, ids: &[String], generation: i64) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare("UPDATE items SET seen_gen = ?2 WHERE id = ?1")?;
                for id in ids {
                    stmt.execute(params![id, generation])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Items a full re-enumeration did not mention: they no longer exist
    /// remotely. Returned deepest-first so a folder is handled after its
    /// contents.
    ///
    /// The depth order is load-bearing, not cosmetic. Removing a parent
    /// folder before its child folder finds the parent non-empty, detaches
    /// the whole subtree from the store, and leaves the child directory on
    /// disk with no record — which the next local scan re-creates remotely.
    /// Files of equal depth still go before folders.
    pub fn items_missed_by_resync(
        &self,
        account_id: &str,
        generation: i64,
    ) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "WITH RECURSIVE depth(item_id, level) AS (
                     SELECT id, 0 FROM items WHERE account_id = ?1 AND parent_id IS NULL
                     UNION ALL
                     SELECT items.id, depth.level + 1
                       FROM items JOIN depth ON items.parent_id = depth.item_id
                      WHERE items.account_id = ?1 AND depth.level < 512
                 )
                 {ITEM_COLUMNS} LEFT JOIN depth ON depth.item_id = items.id
                 WHERE account_id = ?1 AND seen_gen < ?2 AND parent_id IS NOT NULL
                 ORDER BY COALESCE(depth.level, -1) DESC, is_folder ASC, id"
            ))?;
            let rows = stmt.query_map(params![account_id, generation], row_to_item)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Drop queued operations of one kind for a whole subtree.
    ///
    /// Excluding a folder has to withdraw work already queued for it, not just
    /// stop future work being queued — otherwise unpinning something appears to
    /// do nothing until the downloads it was meant to cancel have finished.
    pub fn cancel_ops_in_subtree(
        &self,
        account_id: &str,
        root_id: &str,
        kind: OpKind,
    ) -> Result<usize> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            let removed = tx.execute(
                "WITH RECURSIVE tree(id) AS (
                     SELECT id FROM items WHERE id = ?2
                     UNION
                     SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 DELETE FROM pending_ops
                  WHERE account_id = ?1 AND kind = ?3
                    AND item_id IN (SELECT id FROM tree)",
                params![account_id, root_id, kind.as_str()],
            )?;
            // Items left claiming they are mid-transfer would confuse the next
            // pass into thinking a download is still coming.
            tx.execute(
                "WITH RECURSIVE tree(id) AS (
                     SELECT id FROM items WHERE id = ?2
                     UNION
                     SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 UPDATE items SET state = 'dehydrated'
                  WHERE account_id = ?1
                    AND id IN (SELECT id FROM tree)
                    AND is_folder = 0
                    AND state IN ('dirty_remote', 'hydrating')
                    AND local_ino IS NULL",
                params![account_id, root_id],
            )?;
            tx.commit()?;
            Ok(removed)
        })
    }
}

/// Every write stamps the account's current resync generation. A full
/// re-enumeration can take minutes; an item uploaded or created meanwhile is
/// absent from the enumeration's snapshot, and without this stamp the sweep at
/// the end would read it as "no longer on the remote" and remove the local
/// file the user just added.
fn upsert_item_on(conn: &Connection, item: &ItemRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO items
           (id, account_id, parent_id, name, name_folded, is_folder, child_count, size, etag, ctag,
            quickxor, sha1, remote_mtime, local_mtime, local_size, local_ino,
            state, pin_state, updated_at, seen_gen)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,
                 COALESCE((SELECT resync_gen FROM accounts WHERE id = ?2), 0))
         ON CONFLICT(id) DO UPDATE SET
           seen_gen = MAX(items.seen_gen, excluded.seen_gen),
           parent_id = excluded.parent_id,
           name = excluded.name,
           name_folded = excluded.name_folded,
           is_folder = excluded.is_folder,
           child_count = CASE
               WHEN excluded.is_folder != 0
               THEN COALESCE(excluded.child_count, items.child_count)
               ELSE NULL
           END,
           size = excluded.size,
           etag = excluded.etag,
           ctag = excluded.ctag,
           quickxor = excluded.quickxor,
           sha1 = excluded.sha1,
           remote_mtime = excluded.remote_mtime,
           state = excluded.state,
           updated_at = excluded.updated_at",
        params![
            item.id,
            item.account_id,
            item.parent_id,
            item.name,
            fold_case(&item.name),
            item.is_folder as i64,
            item.child_count
                .map(|v| i64::try_from(v).unwrap_or(i64::MAX)),
            item.size as i64,
            item.etag,
            item.ctag,
            item.quickxor,
            item.sha1,
            item.remote_mtime.map(|t| t.unix_timestamp()),
            item.local_mtime.map(|t| t.unix_timestamp()),
            item.local_size.map(|v| v as i64),
            item.local_ino.map(|v| v as i64),
            item.state.as_str(),
            item.pin_state.as_str(),
            now(),
        ],
    )?;
    Ok(())
}

/// Note the deliberate omission of `local_*` from the UPDATE clause above:
/// a delta page describes the *remote* item and must not clobber what we
/// observed on disk. Local stats are written only by `set_local_stat`.
const ITEM_COLUMNS: &str =
    "SELECT id, account_id, parent_id, name, is_folder, child_count, size, etag, ctag,
        quickxor, sha1, remote_mtime, local_mtime, local_size, local_ino, state, pin_state
   FROM items";

/// The same column list, for queries that supply their own `WHERE` against a CTE.
const ITEM_COLUMNS_CTE: &str = ITEM_COLUMNS;

fn row_to_item(row: &Row<'_>) -> rusqlite::Result<ItemRecord> {
    Ok(ItemRecord {
        id: row.get(0)?,
        account_id: row.get(1)?,
        parent_id: row.get(2)?,
        name: row.get(3)?,
        is_folder: row.get::<_, i64>(4)? != 0,
        child_count: row.get::<_, Option<i64>>(5)?.map(|v| v.max(0) as u64),
        size: row.get::<_, i64>(6)? as u64,
        etag: row.get(7)?,
        ctag: row.get(8)?,
        quickxor: row.get(9)?,
        sha1: row.get(10)?,
        remote_mtime: to_time(row.get(11)?),
        local_mtime: to_time(row.get(12)?),
        local_size: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        local_ino: row.get::<_, Option<i64>>(14)?.map(|v| v as u64),
        state: ItemState::parse(&row.get::<_, String>(15)?),
        pin_state: PinState::parse(&row.get::<_, String>(16)?),
    })
}
