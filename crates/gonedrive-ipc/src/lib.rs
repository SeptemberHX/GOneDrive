//! The D-Bus contract between `gonedrived` and its front ends.
//!
//! Kept in its own crate so the GTK application never links the sync engine.
//! That separation is the point of the split: the UI can crash, be restarted,
//! or be absent entirely without interrupting a transfer, and it never holds a
//! token or a database handle.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

pub const SERVICE_NAME: &str = "org.gonedrive.Daemon";
pub const OBJECT_PATH: &str = "/org/gonedrive/Daemon";
pub const INTERFACE: &str = "org.gonedrive.Daemon1";

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct AccountInfo {
    pub id: String,
    pub display_name: String,
    pub address: String,
    pub root_path: String,
    /// Bytes. Zero when the service has not reported a quota.
    pub quota_total: u64,
    pub quota_used: u64,
}

/// A placeholder being filled because something tried to read it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Hydration {
    /// Absolute path of the file being filled.
    pub path: String,
    pub name: String,
    pub transferred: u64,
    /// Zero when the size is not yet known.
    pub total: u64,
}

/// Something the daemon finished doing, for the activity list.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct Activity {
    /// One of `downloaded`, `uploaded`, `deleted`, `conflict`, `failed`.
    pub kind: String,
    pub name: String,
    /// Extra context: the conflict copy's name, or the error.
    pub detail: String,
    /// Unix seconds.
    pub when: i64,
}

/// One entry of a browsable folder listing, for the folder chooser.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct FolderEntry {
    pub name: String,
    pub is_folder: bool,
    /// Whether content under this entry is kept on this device (resolved
    /// through inheritance).
    pub selected: bool,
    /// Whether the choice was made on this entry itself rather than inherited.
    pub explicit: bool,
    /// Total remote bytes beneath it.
    pub bytes: u64,
    pub files: u64,
}

/// What the daemon is doing right now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type, PartialEq, Eq)]
pub struct SyncStatus {
    /// One of `idle`, `syncing`, `paused`, `offline`, `signed_out`, `error`.
    pub state: String,
    /// Operations still queued.
    pub pending: u64,
    /// The file currently being transferred, if any.
    pub current_item: String,
    pub transferred: u64,
    pub total: u64,
    /// Most recent failure, or empty.
    pub last_error: String,
}

impl SyncStatus {
    pub fn idle(pending: u64) -> Self {
        Self {
            state: "idle".into(),
            pending,
            ..Default::default()
        }
    }
}

#[zbus::proxy(
    interface = "org.gonedrive.Daemon1",
    default_service = "org.gonedrive.Daemon",
    default_path = "/org/gonedrive/Daemon"
)]
pub trait Daemon {
    /// Accounts the daemon is syncing.
    fn list_accounts(&self) -> zbus::Result<Vec<AccountInfo>>;

    /// Begin an interactive login, returning the URL to open in a browser.
    ///
    /// The daemon also tries to open it itself; the URL is returned so the UI
    /// can show a "didn't open? copy this link" affordance, which is the only
    /// recourse on a machine with no portal and no `xdg-open`.
    fn start_login(&self) -> zbus::Result<String>;

    /// Sign out and forget the account's credentials. Local files are kept.
    fn remove_account(&self, account_id: &str) -> zbus::Result<()>;

    fn status(&self) -> zbus::Result<SyncStatus>;

    /// Ask for a sync pass immediately rather than waiting for the poll timer.
    fn sync_now(&self) -> zbus::Result<()>;

    fn set_paused(&self, paused: bool) -> zbus::Result<()>;

    /// Choose whether a path's content is kept on this device.
    ///
    /// Paths are absolute, because the callers are file managers handing over
    /// what the user right-clicked. `pin` is one of `pinned`, `unpinned`,
    /// `inherit`.
    fn set_pin_state(&self, path: &str, pin: &str) -> zbus::Result<()>;

    /// Turn a path back into a placeholder, freeing its space. Folders apply
    /// to everything beneath them.
    fn free_up_space(&self, path: &str) -> zbus::Result<u64>;

    /// Download a placeholder now rather than waiting for something to read it.
    fn fetch_now(&self, path: &str) -> zbus::Result<u64>;

    /// Create placeholders for never-downloaded files under a path. Returns
    /// how many were created.
    fn make_on_demand(&self, path: &str) -> zbus::Result<u64>;

    /// Present the whole drive as a browsable placeholder tree (true
    /// files-on-demand). Returns how many placeholders are now watched.
    fn materialize_tree(&self) -> zbus::Result<u64>;

    /// What was synced recently, newest first.
    ///
    /// Kept by the daemon rather than the front end so the list survives the
    /// window being closed and reopened.
    fn recent_activity(&self) -> zbus::Result<Vec<Activity>>;

    /// Browse one level of the drive, for the folder chooser. `path` is
    /// relative to the sync root; empty means the root itself.
    fn list_folder(&self, path: &str) -> zbus::Result<Vec<FolderEntry>>;

    /// Placeholders currently being filled.
    ///
    /// Lets a progress window that started late show what is already running,
    /// rather than an empty list until the next signal.
    fn active_hydrations(&self) -> zbus::Result<Vec<Hydration>>;

    /// Abandon a hydration. The process waiting on the read gets an I/O error,
    /// which is the honest outcome of the user saying "stop".
    fn cancel_hydration(&self, path: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn status_changed(status: SyncStatus) -> zbus::Result<()>;

    #[zbus(signal)]
    fn login_completed(account_id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn login_failed(message: &str) -> zbus::Result<()>;

    /// Both sides changed; the local copy was preserved under `copy`.
    #[zbus(signal)]
    fn conflict_detected(original: &str, copy: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn item_failed(name: &str, error: &str) -> zbus::Result<()>;

    /// Something opened a placeholder and is blocked until it is filled.
    #[zbus(signal)]
    fn hydration_started(hydration: Hydration) -> zbus::Result<()>;

    #[zbus(signal)]
    fn hydration_progress(hydration: Hydration) -> zbus::Result<()>;

    #[zbus(signal)]
    fn hydration_finished(path: &str, ok: bool) -> zbus::Result<()>;
}

/// The protocol between `gonedrived` and the privileged fanotify helper.
///
/// Newline-delimited JSON over a unix socket. The helper runs as root but is
/// deliberately ignorant: it holds no tokens, makes no network calls, and knows
/// only paths and byte ranges. Everything that could leak an account lives on
/// the unprivileged side.
pub mod hsm {
    use serde::{Deserialize, Serialize};

    fn default_true() -> bool {
        true
    }

    /// Default socket path. The directory is root-owned; the socket itself is
    /// mode 0666 and every connection is checked with `SO_PEERCRED`.
    pub const SOCKET_PATH: &str = "/run/gonedrive/hsm.sock";

    /// Sent by the daemon to the helper.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Request {
        /// Intercept reads of this placeholder until it is hydrated.
        Watch { path: String },
        /// Stop intercepting: the file is hydrated, gone, or no longer ours.
        Unwatch { path: String },
        /// Answer to a [`Event::Hydrate`].
        ///
        /// `source` names a file holding the content. The helper copies it into
        /// the placeholder **through the descriptor the kernel handed it**,
        /// because writes from any other descriptor re-enter the hook: filling
        /// a hole faults the page in, which is itself a read. The daemon
        /// therefore downloads, and the helper writes.
        ///
        /// `ok: false` (with no source) makes the blocked reader see an I/O
        /// error rather than silently reading zeroes.
        Hydrated {
            id: u64,
            ok: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            source: Option<String>,
            /// Where in the placeholder the staged bytes belong.
            #[serde(default)]
            offset: u64,
            /// Whole file, or only the requested range. A partial fill leaves
            /// the file dehydrated and its watch armed: a 16 KiB sniff costs
            /// 16 KiB, and only a reader that goes further pays for the rest.
            #[serde(default = "default_true")]
            full: bool,
        },
        /// Liveness probe, also used to confirm the helper's version.
        Ping,
        /// Ask what the helper can do. A helper that predates this variant
        /// answers with a parse error, which reads as "no optional features" —
        /// exactly right: nothing optional may be used until the helper
        /// explicitly says it understands it.
        Features,
    }

    /// Sent by the helper to the daemon.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Event {
        /// A process is about to read `path` and is blocked until the daemon
        /// answers. The range is what the reader actually asked for.
        Hydrate {
            id: u64,
            path: String,
            offset: u64,
            length: u64,
            /// `comm` of the process whose read is blocked — the answer to
            /// "what keeps downloading my files", written into the daemon log.
            #[serde(default)]
            reader: String,
        },
        /// The content was written and the reader released. The daemon
        /// finalises its own bookkeeping on seeing this — but only for a
        /// `full` fill; a range fill changes nothing about the file's state.
        Filled {
            id: u64,
            ok: bool,
            path: String,
            #[serde(default = "default_true")]
            full: bool,
        },
        /// Answer to [`Request::Features`].
        Features {
            #[serde(default)]
            partial_hydration: bool,
        },
        Ok,
        Error { message: String },
        Pong { version: String },
    }
}
