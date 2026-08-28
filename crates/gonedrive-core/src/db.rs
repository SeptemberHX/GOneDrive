//! The local state store: what we believe exists remotely, what exists
//! locally, and what work is outstanding.
//!
//! Two decisions shape the schema:
//!
//! * **Paths are derived, not stored.** An item knows only its parent and its
//!   name; the full path comes from a recursive CTE. Renaming a folder with
//!   50 000 descendants is then one UPDATE instead of 50 000.
//! * **Names are indexed case-folded.** OneDrive treats `Report.docx` and
//!   `report.docx` as the same item while ext4 does not. Without a
//!   case-insensitive uniqueness constraint the two silently fight forever, so
//!   the collision is caught at insert time by the database itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::names::fold_case;

const SCHEMA_VERSION: i64 = 3;

/// What we know about a file relative to the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// Local content matches the remote content we recorded.
    Synced,
    /// A placeholder: metadata is present, content is not. (Files-on-demand.)
    Dehydrated,
    /// Content is being filled in right now.
    Hydrating,
    /// Changed locally and not yet uploaded.
    DirtyLocal,
    /// Changed remotely and not yet downloaded.
    DirtyRemote,
    /// Changed on both sides; awaiting the conflict-copy resolution.
    Conflict,
    /// Repeatedly failed; surfaced in the UI's error centre.
    Error,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Synced => "synced",
            ItemState::Dehydrated => "dehydrated",
            ItemState::Hydrating => "hydrating",
            ItemState::DirtyLocal => "dirty_local",
            ItemState::DirtyRemote => "dirty_remote",
            ItemState::Conflict => "conflict",
            ItemState::Error => "error",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "dehydrated" => ItemState::Dehydrated,
            "hydrating" => ItemState::Hydrating,
            "dirty_local" => ItemState::DirtyLocal,
            "dirty_remote" => ItemState::DirtyRemote,
            "conflict" => ItemState::Conflict,
            "error" => ItemState::Error,
            _ => ItemState::Synced,
        }
    }
}

/// The user's retention choice, mirroring the Windows client's three states
/// plus an explicit "not chosen" that the other three are resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinState {
    /// No explicit choice: take whatever the nearest pinned ancestor says, or
    /// the account default. Every newly discovered item gets this, which is why
    /// choosing one folder is enough to choose its whole subtree — and why the
    /// default can stay "download nothing" without stamping a flag on each of
    /// a million rows.
    Inherit,
    /// "Always keep on this device."
    Pinned,
    /// Content is not wanted here. In M1 that means "never download"; once
    /// files-on-demand lands it also means "evictable".
    Unpinned,
    /// Downloaded on access, evicted under pressure. Needs M2.
    Auto,
}

impl PinState {
    pub fn as_str(self) -> &'static str {
        match self {
            PinState::Inherit => "inherit",
            PinState::Pinned => "pinned",
            PinState::Unpinned => "unpinned",
            PinState::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "pinned" => PinState::Pinned,
            "unpinned" => PinState::Unpinned,
            "auto" => PinState::Auto,
            _ => PinState::Inherit,
        }
    }

    /// Whether content belongs on disk under this (already resolved) state.
    pub fn wants_content(self) -> bool {
        matches!(self, PinState::Pinned)
    }
}

/// A unit of deferred work, durable across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Upload,
    Download,
    CreateRemoteFolder,
    DeleteRemote,
    MoveRemote,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Upload => "upload",
            OpKind::Download => "download",
            OpKind::CreateRemoteFolder => "create_remote_folder",
            OpKind::DeleteRemote => "delete_remote",
            OpKind::MoveRemote => "move_remote",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "upload" => OpKind::Upload,
            "download" => OpKind::Download,
            "create_remote_folder" => OpKind::CreateRemoteFolder,
            "delete_remote" => OpKind::DeleteRemote,
            "move_remote" => OpKind::MoveRemote,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: String,
    pub drive_id: String,
    pub root_item_id: Option<String>,
    pub display_name: Option<String>,
    pub address: Option<String>,
    pub root_local_path: PathBuf,
    pub delta_link: Option<String>,
    /// Continuation of an enumeration that has not finished yet.
    pub enum_link: Option<String>,
    pub quota_total: Option<u64>,
    pub quota_used: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ItemRecord {
    pub id: String,
    pub account_id: String,
    /// `None` only for the drive root.
    pub parent_id: Option<String>,
    pub name: String,
    pub is_folder: bool,
    pub size: u64,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub quickxor: Option<String>,
    pub sha1: Option<String>,
    pub remote_mtime: Option<OffsetDateTime>,
    pub local_mtime: Option<OffsetDateTime>,
    pub local_size: Option<u64>,
    pub local_ino: Option<u64>,
    pub state: ItemState,
    pub pin_state: PinState,
}

impl ItemRecord {
    /// The record for a drive root.
    ///
    /// One constructor rather than three call sites, because the root's pin
    /// state decides the pin state of *every* item beneath it: making it
    /// `Pinned` silently turns "download nothing by default" into "download the
    /// entire drive". It must stay `Inherit` so the account-level policy is
    /// what actually applies.
    pub fn root(account_id: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            is_folder: true,
            state: ItemState::Synced,
            pin_state: PinState::Inherit,
            ..Self::new(account_id, id, "root")
        }
    }

    /// A minimal record for a newly-seen remote item.
    pub fn new(account_id: impl Into<String>, id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            parent_id: None,
            name: name.into(),
            is_folder: false,
            size: 0,
            etag: None,
            ctag: None,
            quickxor: None,
            sha1: None,
            remote_mtime: None,
            local_mtime: None,
            local_size: None,
            local_ino: None,
            state: ItemState::DirtyRemote,
            pin_state: PinState::Inherit,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingOp {
    pub id: i64,
    pub account_id: String,
    pub kind: OpKind,
    pub item_id: Option<String>,
    pub local_path: Option<PathBuf>,
    pub payload: Option<String>,
    pub attempts: u32,
    pub last_error: Option<String>,
}

/// A resumable upload we can pick back up after a restart.
#[derive(Debug, Clone)]
pub struct UploadSessionRecord {
    pub key: String,
    pub account_id: String,
    pub upload_url: String,
    pub expires_at: Option<OffsetDateTime>,
    pub committed_offset: u64,
    pub total_size: u64,
}

/// Handle to the state database.
///
/// SQLite calls here are short and local, so a plain mutex is cheaper than
/// moving every query onto a blocking thread pool.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL lets the delta applier write while the UI reads.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| Error::other("the state database mutex was poisoned"))?;
        f(&conn)
    }

    fn migrate(&self) -> Result<()> {
        self.with_conn(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if version >= SCHEMA_VERSION {
                return Ok(());
            }
            if version < 1 {
                conn.execute_batch(SCHEMA)?;
            }
            if version < 2 {
                // Where a part-finished enumeration left off, so an interrupted
                // first pass over a large drive resumes instead of restarting.
                conn.execute_batch("ALTER TABLE accounts ADD COLUMN enum_link TEXT")?;
            }
            if version < 3 {
                // An earlier build stamped every *upload* as `pinned`. That
                // turned a later local deletion into "a pinned file vanished —
                // put it back", resurrecting folders, and made those items read
                // as explicitly-excluded so deletions were vetoed. The upload
                // path is fixed; this scrubs the records it already poisoned.
                // Genuine user pins are re-established the moment the user
                // pins something, so clearing the ambiguous ones is safe.
                let scrubbed = conn.execute(
                    "UPDATE items SET pin_state = 'inherit' WHERE pin_state = 'pinned'",
                    [],
                )?;
                if scrubbed > 0 {
                    tracing::info!(scrubbed, "cleared pin flags an old upload path set wrongly");
                }
            }
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            Ok(())
        })
    }

    // ---- accounts ---------------------------------------------------------

    pub fn upsert_account(&self, account: &Account) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO accounts
                   (id, drive_id, root_item_id, display_name, address, root_local_path,
                    delta_link, quota_total, quota_used)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                   drive_id = excluded.drive_id,
                   root_item_id = COALESCE(excluded.root_item_id, accounts.root_item_id),
                   display_name = excluded.display_name,
                   address = excluded.address,
                   root_local_path = excluded.root_local_path,
                   quota_total = excluded.quota_total,
                   quota_used = excluded.quota_used",
                params![
                    account.id,
                    account.drive_id,
                    account.root_item_id,
                    account.display_name,
                    account.address,
                    account.root_local_path.to_string_lossy(),
                    account.delta_link,
                    account.quota_total.map(|v| v as i64),
                    account.quota_used.map(|v| v as i64),
                ],
            )?;
            Ok(())
        })
    }

    pub fn account(&self, id: &str) -> Result<Option<Account>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, drive_id, root_item_id, display_name, address, root_local_path,
                            delta_link, quota_total, quota_used, enum_link
                       FROM accounts WHERE id = ?1",
                    params![id],
                    row_to_account,
                )
                .optional()?)
        })
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, drive_id, root_item_id, display_name, address, root_local_path,
                        delta_link, quota_total, quota_used, enum_link
                   FROM accounts ORDER BY id",
            )?;
            let rows = stmt.query_map([], row_to_account)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

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

    pub fn set_quota(&self, account_id: &str, total: Option<u64>, used: Option<u64>) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET quota_total = ?2, quota_used = ?3 WHERE id = ?1",
                params![account_id, total.map(|v| v as i64), used.map(|v| v as i64)],
            )?;
            Ok(())
        })
    }

    pub fn remove_account(&self, account_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            // Items, pending ops and upload sessions cascade.
            conn.execute("DELETE FROM accounts WHERE id = ?1", params![account_id])?;
            Ok(())
        })
    }

    // ---- items ------------------------------------------------------------

    pub fn upsert_item(&self, item: &ItemRecord) -> Result<()> {
        self.with_conn(|conn| upsert_item_on(conn, item))
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
                .query_row(&format!("{ITEM_COLUMNS} WHERE id = ?1"), params![id], row_to_item)
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
                        "{ITEM_COLUMNS} WHERE account_id = ?1 AND parent_id = ?2 AND name_folded = ?3"
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

    pub fn items_in_state(&self, account_id: &str, state: ItemState) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND state = ?2"
            ))?;
            let rows = stmt.query_map(params![account_id, state.as_str()], row_to_item)?;
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

    pub fn set_pin_state(&self, id: &str, pin: PinState) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE items SET pin_state = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, pin.as_str(), now()],
            )?;
            Ok(())
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
            let n = conn.execute(
                "WITH RECURSIVE doomed(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION ALL
                     SELECT i.id FROM items i JOIN doomed d ON i.parent_id = d.id
                 )
                 DELETE FROM items WHERE id IN (SELECT id FROM doomed)",
                params![id],
            )?;
            Ok(n)
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

    /// Every file in a subtree. Used when a newly pinned folder has to have its
    /// already-enumerated contents queued for download.
    pub fn files_in_subtree(&self, id: &str) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "WITH RECURSIVE tree(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION ALL
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
                     UNION ALL
                     SELECT i.id FROM items i JOIN tree t ON i.parent_id = t.id
                 )
                 SELECT COALESCE(SUM(size), 0), COUNT(*) FROM items
                  WHERE id IN (SELECT id FROM tree) AND is_folder = 0",
                params![id],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
            )?)
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
            let mut folders: Vec<ItemRecord> =
                rows.collect::<std::result::Result<Vec<_>, _>>()?;
            // Depth by ancestor count; cheap because parent_id is indexed and
            // trees are shallow relative to their node count.
            let depth_of = |mut id: Option<String>| -> usize {
                let mut d = 0;
                while let Some(cur) = id {
                    match conn
                        .query_row("SELECT parent_id FROM items WHERE id = ?1", params![cur], |r| {
                            r.get::<_, Option<String>>(0)
                        })
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
            conn.execute(
                "INSERT INTO pending_ops
                   (account_id, kind, item_id, local_path, payload, next_attempt_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)
                 ON CONFLICT(account_id, kind, dedup_key) DO UPDATE SET
                   payload = excluded.payload,
                   next_attempt_at = 0,
                   attempts = 0,
                   last_error = NULL",
                params![
                    account_id,
                    kind.as_str(),
                    item_id,
                    local_path.map(|p| p.to_string_lossy().into_owned()),
                    payload,
                    now(),
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Operations whose backoff has elapsed, oldest first.
    pub fn due_ops(&self, account_id: &str, limit: usize) -> Result<Vec<PendingOp>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, account_id, kind, item_id, local_path, payload, attempts, last_error
                   FROM pending_ops
                  WHERE account_id = ?1 AND next_attempt_at <= ?2
                  ORDER BY id LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![account_id, now(), limit as i64], |row| {
                Ok(PendingOp {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    kind: OpKind::parse(&row.get::<_, String>(2)?).unwrap_or(OpKind::Upload),
                    item_id: row.get(3)?,
                    local_path: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
                    payload: row.get(5)?,
                    attempts: row.get::<_, i64>(6)? as u32,
                    last_error: row.get(7)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    pub fn complete_op(&self, id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM pending_ops WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    /// Record a failure and schedule the retry.
    pub fn fail_op(&self, id: i64, error: &str, retry_in: std::time::Duration) -> Result<u32> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE pending_ops
                    SET attempts = attempts + 1, last_error = ?2, next_attempt_at = ?3
                  WHERE id = ?1",
                params![id, error, now() + retry_in.as_secs() as i64],
            )?;
            let attempts: i64 = conn.query_row(
                "SELECT attempts FROM pending_ops WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            Ok(attempts as u32)
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
                let mut stmt =
                    tx.prepare("UPDATE items SET seen_gen = ?2 WHERE id = ?1")?;
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
    pub fn items_missed_by_resync(
        &self,
        account_id: &str,
        generation: i64,
    ) -> Result<Vec<ItemRecord>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{ITEM_COLUMNS} WHERE account_id = ?1 AND seen_gen < ?2 AND parent_id IS NOT NULL
                 ORDER BY is_folder ASC"
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
                     UNION ALL
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
                     UNION ALL
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

    // ---- upload sessions --------------------------------------------------

    pub fn save_upload_session(&self, session: &UploadSessionRecord) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO upload_sessions
                   (key, account_id, upload_url, expires_at, committed_offset, total_size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(key) DO UPDATE SET
                   upload_url = excluded.upload_url,
                   expires_at = excluded.expires_at,
                   committed_offset = excluded.committed_offset,
                   total_size = excluded.total_size",
                params![
                    session.key,
                    session.account_id,
                    session.upload_url,
                    session.expires_at.map(|t| t.unix_timestamp()),
                    session.committed_offset as i64,
                    session.total_size as i64,
                ],
            )?;
            Ok(())
        })
    }

    pub fn upload_session(&self, key: &str) -> Result<Option<UploadSessionRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT key, account_id, upload_url, expires_at, committed_offset, total_size
                       FROM upload_sessions WHERE key = ?1",
                    params![key],
                    |row| {
                        Ok(UploadSessionRecord {
                            key: row.get(0)?,
                            account_id: row.get(1)?,
                            upload_url: row.get(2)?,
                            expires_at: row
                                .get::<_, Option<i64>>(3)?
                                .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok()),
                            committed_offset: row.get::<_, i64>(4)? as u64,
                            total_size: row.get::<_, i64>(5)? as u64,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn set_upload_offset(&self, key: &str, offset: u64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE upload_sessions SET committed_offset = ?2 WHERE key = ?1",
                params![key, offset as i64],
            )?;
            Ok(())
        })
    }

    pub fn delete_upload_session(&self, key: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM upload_sessions WHERE key = ?1", params![key])?;
            Ok(())
        })
    }
}

fn upsert_item_on(conn: &Connection, item: &ItemRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO items
           (id, account_id, parent_id, name, name_folded, is_folder, size, etag, ctag,
            quickxor, sha1, remote_mtime, local_mtime, local_size, local_ino,
            state, pin_state, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
         ON CONFLICT(id) DO UPDATE SET
           parent_id = excluded.parent_id,
           name = excluded.name,
           name_folded = excluded.name_folded,
           is_folder = excluded.is_folder,
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
const ITEM_COLUMNS: &str = "SELECT id, account_id, parent_id, name, is_folder, size, etag, ctag,
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
        size: row.get::<_, i64>(5)? as u64,
        etag: row.get(6)?,
        ctag: row.get(7)?,
        quickxor: row.get(8)?,
        sha1: row.get(9)?,
        remote_mtime: to_time(row.get(10)?),
        local_mtime: to_time(row.get(11)?),
        local_size: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        local_ino: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        state: ItemState::parse(&row.get::<_, String>(14)?),
        pin_state: PinState::parse(&row.get::<_, String>(15)?),
    })
}

fn row_to_account(row: &Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        drive_id: row.get(1)?,
        root_item_id: row.get(2)?,
        display_name: row.get(3)?,
        address: row.get(4)?,
        root_local_path: PathBuf::from(row.get::<_, String>(5)?),
        delta_link: row.get(6)?,
        enum_link: row.get(9)?,
        quota_total: row.get::<_, Option<i64>>(7)?.map(|v| v as u64),
        quota_used: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
    })
}

fn to_time(secs: Option<i64>) -> Option<OffsetDateTime> {
    secs.and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok())
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id              TEXT PRIMARY KEY,
    drive_id        TEXT NOT NULL,
    root_item_id    TEXT,
    display_name    TEXT,
    address         TEXT,
    root_local_path TEXT NOT NULL,
    delta_link      TEXT,
    quota_total     INTEGER,
    quota_used      INTEGER,
    resync_gen      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS items (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    parent_id    TEXT,
    name         TEXT NOT NULL,
    name_folded  TEXT NOT NULL,
    is_folder    INTEGER NOT NULL,
    size         INTEGER NOT NULL DEFAULT 0,
    etag         TEXT,
    ctag         TEXT,
    quickxor     TEXT,
    sha1         TEXT,
    remote_mtime INTEGER,
    local_mtime  INTEGER,
    local_size   INTEGER,
    local_ino    INTEGER,
    state        TEXT NOT NULL,
    pin_state    TEXT NOT NULL,
    updated_at   INTEGER NOT NULL,
    -- Stamped during a full re-enumeration. Anything still carrying an older
    -- generation afterwards was deleted remotely while we were not watching.
    seen_gen     INTEGER NOT NULL DEFAULT 0
);

-- OneDrive is case-insensitive; ext4 is not. IFNULL keeps the root (whose
-- parent is NULL) inside the constraint, since SQLite treats NULLs in a UNIQUE
-- index as distinct from each other.
CREATE UNIQUE INDEX IF NOT EXISTS items_unique_child
    ON items(account_id, IFNULL(parent_id, ''), name_folded);
CREATE INDEX IF NOT EXISTS items_by_parent ON items(account_id, parent_id);
CREATE INDEX IF NOT EXISTS items_by_state  ON items(account_id, state);
CREATE INDEX IF NOT EXISTS items_by_ino    ON items(account_id, local_ino);

CREATE TABLE IF NOT EXISTS pending_ops (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL,
    item_id         TEXT,
    local_path      TEXT,
    payload         TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    -- Queueing the same work twice is common (an editor writes a file three
    -- times in a second); this collapses those into one row.
    dedup_key       TEXT GENERATED ALWAYS AS (IFNULL(item_id, IFNULL(local_path, ''))) STORED
);
CREATE UNIQUE INDEX IF NOT EXISTS pending_ops_dedup
    ON pending_ops(account_id, kind, dedup_key);
CREATE INDEX IF NOT EXISTS pending_ops_due ON pending_ops(account_id, next_attempt_at);

CREATE TABLE IF NOT EXISTS upload_sessions (
    key              TEXT PRIMARY KEY,
    account_id       TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    upload_url       TEXT NOT NULL,
    expires_at       INTEGER,
    committed_offset INTEGER NOT NULL DEFAULT 0,
    total_size       INTEGER NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_account() -> (Store, Account) {
        let store = Store::open_in_memory().unwrap();
        let account = Account {
            id: "acct".into(),
            drive_id: "drive1".into(),
            root_item_id: Some("ROOT".into()),
            display_name: Some("Test User".into()),
            address: Some("t@example.com".into()),
            root_local_path: PathBuf::from("/home/u/OneDrive"),
            delta_link: None,
            enum_link: None,
            quota_total: Some(1024),
            quota_used: Some(10),
        };
        store.upsert_account(&account).unwrap();

        let mut root = ItemRecord::new("acct", "ROOT", "root");
        root.is_folder = true;
        root.state = ItemState::Synced;
        store.upsert_item(&root).unwrap();
        (store, account)
    }

    fn folder(id: &str, parent: &str, name: &str) -> ItemRecord {
        let mut item = ItemRecord::new("acct", id, name);
        item.parent_id = Some(parent.into());
        item.is_folder = true;
        item
    }

    fn file(id: &str, parent: &str, name: &str) -> ItemRecord {
        let mut item = ItemRecord::new("acct", id, name);
        item.parent_id = Some(parent.into());
        item
    }

    #[test]
    fn migration_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store.migrate().unwrap();
        store.migrate().unwrap();
        assert!(store.accounts().unwrap().is_empty());
    }

    #[test]
    fn upsert_updates_remote_fields_but_not_local_stats() {
        let (store, _) = store_with_account();
        let mut item = file("F1", "ROOT", "a.txt");
        item.size = 10;
        store.upsert_item(&item).unwrap();
        store
            .set_local_stat(
                "F1",
                Some(OffsetDateTime::from_unix_timestamp(1000).unwrap()),
                Some(10),
                Some(42),
            )
            .unwrap();

        // A later delta page carries no local knowledge; it must not erase ours.
        let mut updated = file("F1", "ROOT", "a.txt");
        updated.size = 20;
        updated.ctag = Some("c2".into());
        store.upsert_item(&updated).unwrap();

        let got = store.item("F1").unwrap().unwrap();
        assert_eq!(got.size, 20);
        assert_eq!(got.ctag.as_deref(), Some("c2"));
        assert_eq!(got.local_ino, Some(42));
        assert_eq!(got.local_size, Some(10));
    }

    #[test]
    fn relative_path_walks_the_parent_chain() {
        let (store, account) = store_with_account();
        store.upsert_item(&folder("D1", "ROOT", "Documents")).unwrap();
        store.upsert_item(&folder("D2", "D1", "Reports")).unwrap();
        store.upsert_item(&file("F1", "D2", "q3.docx")).unwrap();

        assert_eq!(
            store.relative_path("F1").unwrap().unwrap(),
            PathBuf::from("Documents/Reports/q3.docx")
        );
        // The root maps to the sync root itself, not to a "root" directory.
        assert_eq!(store.relative_path("ROOT").unwrap().unwrap(), PathBuf::new());
        assert_eq!(
            store.local_path(&account, "F1").unwrap().unwrap(),
            PathBuf::from("/home/u/OneDrive/Documents/Reports/q3.docx")
        );
    }

    #[test]
    fn relative_path_is_none_when_an_ancestor_is_missing() {
        let (store, _) = store_with_account();
        // A delta page can mention a child before its parent has arrived.
        store.upsert_item(&file("F1", "NOT_YET", "orphan.txt")).unwrap();
        assert!(store.relative_path("F1").unwrap().is_none());
        assert!(store.relative_path("NOBODY").unwrap().is_none());
    }

    #[test]
    fn renaming_a_folder_moves_its_whole_subtree() {
        let (store, _) = store_with_account();
        store.upsert_item(&folder("D1", "ROOT", "Old")).unwrap();
        store.upsert_item(&file("F1", "D1", "a.txt")).unwrap();

        let mut renamed = folder("D1", "ROOT", "New");
        store.upsert_item(&renamed).unwrap();
        assert_eq!(
            store.relative_path("F1").unwrap().unwrap(),
            PathBuf::from("New/a.txt")
        );

        // Moving the folder elsewhere moves the child too — still one UPDATE.
        store.upsert_item(&folder("D2", "ROOT", "Archive")).unwrap();
        renamed.parent_id = Some("D2".into());
        store.upsert_item(&renamed).unwrap();
        assert_eq!(
            store.relative_path("F1").unwrap().unwrap(),
            PathBuf::from("Archive/New/a.txt")
        );
    }

    #[test]
    fn case_insensitive_names_collide_the_way_onedrive_does() {
        let (store, _) = store_with_account();
        store.upsert_item(&file("F1", "ROOT", "Report.docx")).unwrap();

        // A *different* item with the same case-folded name is a real conflict
        // and must be refused rather than silently creating a second row.
        let err = store.upsert_item(&file("F2", "ROOT", "report.docx"));
        assert!(err.is_err(), "expected a uniqueness violation");

        // Lookup ignores case, matching how the service resolves names.
        let found = store.child_by_name("acct", "ROOT", "REPORT.DOCX").unwrap();
        assert_eq!(found.unwrap().id, "F1");
    }

    #[test]
    fn same_name_in_different_folders_is_fine() {
        let (store, _) = store_with_account();
        store.upsert_item(&folder("D1", "ROOT", "a")).unwrap();
        store.upsert_item(&folder("D2", "ROOT", "b")).unwrap();
        store.upsert_item(&file("F1", "D1", "notes.txt")).unwrap();
        store.upsert_item(&file("F2", "D2", "notes.txt")).unwrap();
        assert_eq!(store.children("acct", "D1").unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_folder_removes_its_descendants() {
        let (store, _) = store_with_account();
        store.upsert_item(&folder("D1", "ROOT", "Docs")).unwrap();
        store.upsert_item(&folder("D2", "D1", "Sub")).unwrap();
        store.upsert_item(&file("F1", "D2", "deep.txt")).unwrap();
        store.upsert_item(&file("F2", "ROOT", "keep.txt")).unwrap();

        assert_eq!(store.delete_subtree("D1").unwrap(), 3);
        assert!(store.item("F1").unwrap().is_none());
        assert!(store.item("F2").unwrap().is_some());
    }

    #[test]
    fn batch_upsert_is_atomic() {
        let (store, _) = store_with_account();
        let batch = vec![
            file("F1", "ROOT", "one.txt"),
            file("F2", "ROOT", "two.txt"),
            // Collides case-insensitively with F1, so the batch must roll back.
            file("F3", "ROOT", "ONE.TXT"),
        ];
        assert!(store.upsert_items(&batch).is_err());
        assert!(store.item("F1").unwrap().is_none());
        assert!(store.item("F2").unwrap().is_none());
    }

    #[test]
    fn queued_work_is_deduplicated_per_item() {
        let (store, _) = store_with_account();
        store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();

        // An editor saving three times in a second must not produce three uploads.
        for _ in 0..3 {
            store
                .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
                .unwrap();
        }
        assert_eq!(store.pending_count("acct").unwrap(), 1);

        // A different kind of work on the same item is a separate operation.
        store
            .enqueue("acct", OpKind::Download, Some("F1"), None, None)
            .unwrap();
        assert_eq!(store.pending_count("acct").unwrap(), 2);
    }

    #[test]
    fn failed_ops_back_off_and_come_back_when_due() {
        let (store, _) = store_with_account();
        store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();
        store
            .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
            .unwrap();

        let op = store.due_ops("acct", 10).unwrap().remove(0);
        let attempts = store
            .fail_op(op.id, "network down", std::time::Duration::from_secs(3600))
            .unwrap();
        assert_eq!(attempts, 1);
        // Still queued, but not yet due.
        assert_eq!(store.pending_count("acct").unwrap(), 1);
        assert!(store.due_ops("acct", 10).unwrap().is_empty());

        // Re-queueing the same work clears the backoff, which is what a fresh
        // local edit should do.
        store
            .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
            .unwrap();
        assert_eq!(store.due_ops("acct", 10).unwrap().len(), 1);

        store.complete_op(op.id).unwrap();
        assert_eq!(store.pending_count("acct").unwrap(), 0);
    }

    #[test]
    fn upload_sessions_survive_for_resume() {
        let (store, _) = store_with_account();
        let session = UploadSessionRecord {
            key: "F1".into(),
            account_id: "acct".into(),
            upload_url: "https://upload".into(),
            expires_at: None,
            committed_offset: 0,
            total_size: 100 * 1024 * 1024,
        };
        store.save_upload_session(&session).unwrap();
        store.set_upload_offset("F1", 20 * 1024 * 1024).unwrap();

        let got = store.upload_session("F1").unwrap().unwrap();
        assert_eq!(got.committed_offset, 20 * 1024 * 1024);
        assert_eq!(got.total_size, 100 * 1024 * 1024);

        store.delete_upload_session("F1").unwrap();
        assert!(store.upload_session("F1").unwrap().is_none());
    }

    #[test]
    fn removing_an_account_cascades() {
        let (store, _) = store_with_account();
        store.upsert_item(&file("F1", "ROOT", "a.txt")).unwrap();
        store
            .enqueue("acct", OpKind::Upload, Some("F1"), None, None)
            .unwrap();

        store.remove_account("acct").unwrap();
        assert!(store.item("F1").unwrap().is_none());
        assert_eq!(store.pending_count("acct").unwrap(), 0);
    }

    #[test]
    fn delta_link_round_trips_verbatim() {
        let (store, _) = store_with_account();
        // Opaque and full of characters we must not mangle.
        let link = "https://graph.microsoft.com/v1.0/me/drive/root/delta?token=aTEuMDs...%3d%3d";
        store.set_delta_link("acct", Some(link)).unwrap();
        assert_eq!(store.account("acct").unwrap().unwrap().delta_link.as_deref(), Some(link));

        store.set_delta_link("acct", None).unwrap();
        assert!(store.account("acct").unwrap().unwrap().delta_link.is_none());
    }
}
