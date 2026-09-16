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

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, Row};
use time::OffsetDateTime;

use crate::error::{Error, Result};

mod delta;
mod items;
mod pending;
mod uploads;

const SCHEMA_VERSION: i64 = 15;

const ACTIVITY_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS activity (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    kind     TEXT NOT NULL,
    name     TEXT NOT NULL,
    detail   TEXT NOT NULL DEFAULT '',
    when_ts  INTEGER NOT NULL
);
"#;

const DELTA_STAGING_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS delta_batches (
    account_id    TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    mode          TEXT NOT NULL,
    generation    INTEGER,
    item_count    INTEGER NOT NULL DEFAULT 0,
    page_count    INTEGER NOT NULL DEFAULT 0,
    next_sequence INTEGER NOT NULL DEFAULT 0,
    complete      INTEGER NOT NULL DEFAULT 0,
    delta_link    TEXT
);

CREATE TABLE IF NOT EXISTS delta_staging (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    item_id    TEXT NOT NULL,
    sequence   INTEGER NOT NULL,
    payload    TEXT NOT NULL,
    PRIMARY KEY (account_id, item_id)
);
CREATE INDEX IF NOT EXISTS delta_staging_order
    ON delta_staging(account_id, sequence);
"#;

/// One entry in the persisted activity log.
#[derive(Debug, Clone)]
pub struct ActivityRow {
    pub kind: String,
    pub name: String,
    pub path: String,
    pub detail: String,
    pub when: i64,
}

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
    /// `st_dev` of the sync root the last time it was scanned. `None` until the
    /// first scan records it. A later mismatch means the root is no longer the
    /// same filesystem — the guard against deleting a whole account because its
    /// mount vanished.
    pub root_dev: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ItemRecord {
    pub id: String,
    pub account_id: String,
    /// `None` only for the drive root.
    pub parent_id: Option<String>,
    pub name: String,
    pub is_folder: bool,
    /// Graph's direct child count for a folder. It is `None` when the remote
    /// response omitted the facet; it is never a recursive subtree total.
    pub child_count: Option<u64>,
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
    pub fn new(
        account_id: impl Into<String>,
        id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            parent_id: None,
            name: name.into(),
            is_folder: false,
            child_count: None,
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
    /// Remote version observed when local reconciliation made this decision.
    /// Conditional mutations must never replace this with a newer database
    /// value at execution time, or a concurrent delta could erase its own race
    /// protection.
    pub precondition: Option<String>,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
}

/// Result of atomically adding local delete decisions to the safety window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeleteGuardOutcome {
    /// Distinct remote records covered by pending and newly observed deletes.
    pub impacted: u64,
    pub total: u64,
    /// All pending deletes were withdrawn instead of allowing the window to
    /// cross the configured mass-deletion threshold.
    pub refused: bool,
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
    /// File identity captured when the session began. A session can only be
    /// resumed while this still matches, otherwise its committed prefix may
    /// belong to an older same-size version of the file.
    pub source_fingerprint: Option<String>,
}

/// A durable Graph delta enumeration being assembled across pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaBatch {
    pub mode: String,
    pub generation: Option<i64>,
    pub item_count: u64,
    pub page_count: u64,
    pub complete: bool,
    pub delta_link: Option<String>,
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
            // Schema changes and user_version advance as one unit. A process
            // dying after ALTER TABLE but before the pragma must not leave a
            // database that attempts the same ALTER forever on next startup.
            let tx = conn.unchecked_transaction()?;
            if version < 1 {
                tx.execute_batch(SCHEMA)?;
            }
            if version < 2 {
                // Where a part-finished enumeration left off, so an interrupted
                // first pass over a large drive resumes instead of restarting.
                tx.execute_batch("ALTER TABLE accounts ADD COLUMN enum_link TEXT")?;
            }
            if version < 3 {
                // An earlier build stamped every *upload* as `pinned`. That
                // turned a later local deletion into "a pinned file vanished —
                // put it back", resurrecting folders, and made those items read
                // as explicitly-excluded so deletions were vetoed. The upload
                // path is fixed; this scrubs the records it already poisoned.
                // Genuine user pins are re-established the moment the user
                // pins something, so clearing the ambiguous ones is safe.
                let scrubbed = tx.execute(
                    "UPDATE items SET pin_state = 'inherit' WHERE pin_state = 'pinned'",
                    [],
                )?;
                if scrubbed > 0 {
                    tracing::info!(scrubbed, "cleared pin flags an old upload path set wrongly");
                }
            }
            if version < 4 {
                // The device the sync root lived on when we last scanned it.
                // A mismatch means the root is not the filesystem it was — most
                // likely unmounted, leaving an empty mountpoint — and a scan
                // that trusted the empty tree would delete the whole account
                // server-side. Recorded on first scan, checked on every scan.
                tx.execute_batch("ALTER TABLE accounts ADD COLUMN root_dev INTEGER")?;
            }
            if version < 5 {
                // The activity log, persisted so it survives a restart instead
                // of living only in the front end's memory.
                tx.execute_batch(ACTIVITY_TABLE)?;
            }
            if version < 6 {
                tx.execute_batch("ALTER TABLE upload_sessions ADD COLUMN source_fingerprint TEXT")?;
            }
            if version < 7 {
                tx.execute_batch(DELTA_STAGING_TABLES)?;
            }
            if version < 8 {
                tx.execute_batch(
                    "ALTER TABLE pending_ops ADD COLUMN precondition TEXT;
                     UPDATE pending_ops
                        SET precondition = (
                            SELECT COALESCE(items.etag, items.ctag)
                              FROM items WHERE items.id = pending_ops.item_id
                        )
                      WHERE item_id IS NOT NULL
                        AND kind IN ('upload', 'delete_remote', 'move_remote');",
                )?;
            }
            if version < 9 {
                // Item ids and local paths are unique only within an account.
                // A global `key` primary key allowed one account's resumable
                // upload URL to replace or resume another account's session.
                tx.execute_batch(
                    "ALTER TABLE upload_sessions RENAME TO upload_sessions_v8;
                     CREATE TABLE upload_sessions (
                         key                TEXT NOT NULL,
                         account_id         TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                         upload_url         TEXT NOT NULL,
                         expires_at         INTEGER,
                         committed_offset   INTEGER NOT NULL DEFAULT 0,
                         total_size         INTEGER NOT NULL,
                         source_fingerprint TEXT,
                         PRIMARY KEY (account_id, key)
                     );
                     INSERT INTO upload_sessions
                         (key, account_id, upload_url, expires_at, committed_offset,
                          total_size, source_fingerprint)
                     SELECT key, account_id, upload_url, expires_at, committed_offset,
                            total_size, source_fingerprint
                       FROM upload_sessions_v8;
                     DROP TABLE upload_sessions_v8;",
                )?;
            }
            if version < 10 {
                tx.execute_batch("ALTER TABLE activity ADD COLUMN path TEXT NOT NULL DEFAULT '';")?;
            }
            if version < 11 {
                // Initial delta enumeration can take minutes on a large drive.
                // Remember which final-per-id rows have already been applied so
                // each page can become visible without reprocessing every older
                // page. A replacement occurrence resets `applied` in
                // `stage_delta_page`, preserving Graph's last-occurrence rule.
                tx.execute_batch(
                    "ALTER TABLE delta_staging
                         ADD COLUMN applied INTEGER NOT NULL DEFAULT 0;
                     ALTER TABLE delta_staging
                         ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
                     CREATE INDEX delta_staging_pending
                         ON delta_staging(account_id, applied, deleted, sequence);

                     CREATE TABLE remote_exclusions (
                         account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                         item_id    TEXT NOT NULL,
                         parent_id  TEXT,
                         reason     TEXT NOT NULL,
                         seen_gen   INTEGER NOT NULL DEFAULT 0,
                         PRIMARY KEY (account_id, item_id)
                     );
                     CREATE INDEX remote_exclusions_by_parent
                         ON remote_exclusions(account_id, parent_id);",
                )?;
                // Existing v10 batches already contain serialized DriveItems.
                // Derive tombstones with a structured parser; leaving every old
                // row at the column default would let an incomplete legacy
                // batch apply remote deletions as progressive upserts.
                let tombstones = {
                    let mut stmt =
                        tx.prepare("SELECT account_id, item_id, payload FROM delta_staging")?;
                    let rows = stmt.query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?;
                    let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
                    rows.into_iter()
                        .filter_map(|(account_id, item_id, payload)| {
                            serde_json::from_str::<serde_json::Value>(&payload)
                                .ok()?
                                .get("deleted")
                                .is_some_and(|facet| !facet.is_null())
                                .then_some((account_id, item_id))
                        })
                        .collect::<Vec<_>>()
                };
                for (account_id, item_id) in tombstones {
                    tx.execute(
                        "UPDATE delta_staging SET deleted = 1
                          WHERE account_id = ?1 AND item_id = ?2",
                        params![account_id, item_id],
                    )?;
                }
            }
            if version < 12 {
                // Directory placeholders are independently populated. Keeping
                // this state in SQLite as well as an xattr lets delta decide
                // whether to publish a child without opening the directory;
                // an open would itself look like a user browse to inotify and
                // recursively populate the tree during a background pass.
                tx.execute_batch(
                    "CREATE TABLE folder_population (
                         account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                         item_id    TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                         populated INTEGER NOT NULL CHECK (populated IN (0, 1)),
                         PRIMARY KEY (account_id, item_id)
                     );",
                )?;
            }
            if version < 13 {
                // A delta item can arrive without an ancestor that this lazy
                // client has indexed yet. Keeping the exact payload lets the
                // global cursor advance without losing the item, while local
                // reconciliation can freeze that remote identity until its
                // parent chain becomes known and the change is replayed.
                tx.execute_batch(
                    "CREATE TABLE deferred_remote_changes (
                         account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                         item_id    TEXT NOT NULL,
                         parent_id  TEXT,
                         payload    TEXT NOT NULL,
                         updated_at INTEGER NOT NULL,
                         PRIMARY KEY (account_id, item_id)
                     );
                     CREATE INDEX deferred_remote_by_parent
                         ON deferred_remote_changes(account_id, parent_id);",
                )?;
            }
            if version < 14 {
                // A lazy tree does not index a folder's children until that
                // folder is opened. Keep Graph's direct childCount on the
                // folder itself so a parent listing can show a useful count
                // without recursively fetching or publishing the subtree.
                // The small migration fixture used by older tests has no
                // `items` table; tolerate that partial schema and let the
                // base schema create the column when a real store is opened.
                let has_items: bool = tx.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_master
                          WHERE type = 'table' AND name = 'items'
                     )",
                    [],
                    |row| row.get(0),
                )?;
                if has_items {
                    let columns = {
                        let mut stmt = tx.prepare("PRAGMA table_info(items)")?;
                        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                        rows.collect::<std::result::Result<Vec<_>, _>>()?
                    };
                    if !columns.iter().any(|name| name == "child_count") {
                        tx.execute_batch("ALTER TABLE items ADD COLUMN child_count INTEGER")?;
                    }
                }
            }
            if version < 15 {
                // See the note beside the index in the base schema: every
                // by-item deletion was a full queue scan. `IF NOT EXISTS`
                // because a store created at the current version already has
                // it from the base schema.
                let has_pending_ops: bool = tx.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_master
                          WHERE type = 'table' AND name = 'pending_ops'
                     )",
                    [],
                    |row| row.get(0),
                )?;
                if has_pending_ops {
                    tx.execute_batch(
                        "CREATE INDEX IF NOT EXISTS pending_ops_by_item
                             ON pending_ops(account_id, item_id);",
                    )?;
                }
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
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
                            delta_link, quota_total, quota_used, enum_link, root_dev
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
                        delta_link, quota_total, quota_used, enum_link, root_dev
                   FROM accounts ORDER BY id",
            )?;
            let rows = stmt.query_map([], row_to_account)?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Record the device the sync root lives on, so a later scan can tell that
    /// the root has become a different filesystem (an unmounted mountpoint).
    pub fn set_root_dev(&self, account_id: &str, dev: u64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE accounts SET root_dev = ?2 WHERE id = ?1",
                params![account_id, dev as i64],
            )?;
            Ok(())
        })
    }

    /// Append an activity entry and trim the log to its most recent `keep`
    /// rows, so it survives a restart without growing without bound.
    pub fn push_activity(
        &self,
        kind: &str,
        name: &str,
        path: &str,
        detail: &str,
        when: i64,
        keep: i64,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO activity (kind, name, path, detail, when_ts)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![kind, name, path, detail, when],
            )?;
            conn.execute(
                "DELETE FROM activity WHERE id NOT IN
                   (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)",
                params![keep],
            )?;
            Ok(())
        })
    }

    /// The most recent activity entries, newest first.
    pub fn recent_activity(&self, limit: i64) -> Result<Vec<ActivityRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT kind, name, path, detail, when_ts
                   FROM activity ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                Ok(ActivityRow {
                    kind: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    detail: row.get(3)?,
                    when: row.get(4)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Update the service-reported quota totals for an account.
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
        root_dev: row.get::<_, Option<i64>>(10)?.map(|v| v as u64),
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
    -- Graph's direct folder childCount. NULL means the service did not include
    -- a summary yet; it is deliberately not a recursive subtree count.
    child_count  INTEGER,
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
-- Completing a hydration, releasing space, cancelling a queued delete and
-- dropping a subtree all delete by item. Without this they scanned the whole
-- queue each time, which on a whole-drive pin is one scan of n rows per one of
-- n downloads.
CREATE INDEX IF NOT EXISTS pending_ops_by_item ON pending_ops(account_id, item_id);

CREATE TABLE IF NOT EXISTS upload_sessions (
    key              TEXT NOT NULL,
    account_id       TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    upload_url       TEXT NOT NULL,
    expires_at       INTEGER,
    committed_offset INTEGER NOT NULL DEFAULT 0,
    total_size       INTEGER NOT NULL,
    PRIMARY KEY (account_id, key)
);
"#;

#[cfg(test)]
mod tests;
