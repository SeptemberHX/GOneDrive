//! The D-Bus contract between `gonedrived` and its front ends.
//!
//! Kept in its own crate so the GTK application never links the sync engine.
//! That separation is the point of the split: the UI can crash, be restarted,
//! or be absent entirely without interrupting a transfer, and it never holds a
//! token or a database handle.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

pub mod reader_policy;

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
    /// OneDrive-relative display path captured when the activity happened.
    pub path: String,
    /// Extra context: the conflict copy's name, or the error.
    pub detail: String,
    /// Unix seconds.
    pub when: i64,
}

/// One durable queue entry shown while synchronization is waiting or retrying.
#[derive(Debug, Clone, Serialize, Deserialize, Type, PartialEq, Eq)]
pub struct PendingItem {
    /// One of `upload`, `download`, `create_remote_folder`, `delete_remote`,
    /// `move_remote`.
    pub kind: String,
    /// OneDrive-relative display path, or an honest fallback when unavailable.
    pub path: String,
    pub attempts: u32,
    /// Unix seconds; zero means immediately due.
    pub retry_at: i64,
    pub last_error: String,
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
    /// Bytes in direct file children, or the file's own size.
    pub bytes: u64,
    /// Direct children for a folder, or one for a file.
    pub items: u64,
}

/// What the daemon is doing right now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type, PartialEq, Eq)]
pub struct SyncStatus {
    /// One of `idle`, `syncing`, `paused`, `offline`, `keyring_locked`,
    /// `signed_out`, or `error`.
    pub state: String,
    /// Operations still queued.
    pub pending: u64,
    /// A bounded preview of the durable operations counted by `pending`.
    pub pending_items: Vec<PendingItem>,
    /// The file currently being transferred, if any.
    pub current_item: String,
    pub transferred: u64,
    pub total: u64,
    /// Most recent failure, or empty.
    pub last_error: String,
}

/// Completion of an explicitly requested remote sync generation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type, PartialEq, Eq)]
pub struct SyncRequestResult {
    pub ok: bool,
    /// Remote delta entries observed by the pass.
    pub changes: u64,
    /// Durable operations remaining after the delta was committed.
    pub queued: u64,
    pub error: String,
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

    /// Ask for a sync pass and return a generation that can be awaited through
    /// `sync_request_completed`.
    fn request_sync(&self) -> zbus::Result<u64>;

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
    fn sync_request_completed(generation: u64, result: SyncRequestResult) -> zbus::Result<()>;

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
    /// Maximum newline-delimited JSON frame accepted in either direction.
    pub const MAX_FRAME_SIZE: usize = 64 * 1024;
    /// Keep one batch bounded even when every path is very short. The encoded
    /// frame limit remains the final authority for unusually long paths.
    pub const MAX_WATCH_BATCH: usize = 256;

    /// Result for one path in a batched mark request. Results have the same
    /// order as the request paths, avoiding another copy of every long path in
    /// the helper's reply.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct WatchOutcome {
        pub ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub dev: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ino: Option<u64>,
    }

    /// Sent by the daemon to the helper.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Request {
        /// Intercept reads of this placeholder until it is hydrated.
        Watch { id: u64, path: String },
        /// Register several placeholders in one bounded round trip.
        WatchBatch { id: u64, paths: Vec<String> },
        /// Start a complete mark-set declaration after reconnecting. Existing
        /// marks remain armed until the matching commit succeeds.
        BeginWatchSet { id: u64 },
        /// Remove marks not observed since [`Request::BeginWatchSet`].
        CommitWatchSet { id: u64 },
        /// Abandon an incomplete declaration without changing existing marks.
        AbortWatchSet { id: u64 },
        /// Stop intercepting: the file is hydrated, gone, or no longer ours.
        Unwatch { id: u64, path: String },
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
            /// True for an open event: the read has not started yet. A
            /// sparse-aware copier issues no read at all, so its only chance to
            /// get real content is to hydrate fully here, on the open.
            #[serde(default)]
            is_open: bool,
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
            /// Whether [`Request::WatchBatch`] is understood. Missing means
            /// false, so a new daemon remains compatible with an old helper.
            #[serde(default)]
            batch_marks: bool,
            /// Whether a complete reconnect registration can retire stale
            /// marks without an unprotected interval.
            #[serde(default)]
            mark_sweeps: bool,
            /// The helper can distinguish a real Nautilus copy from file-type
            /// sniffing by observing the copy's writable destination descriptor.
            #[serde(default)]
            file_manager_copy_detection: bool,
        },
        /// Completion of a watch/unwatch request. A request id makes concurrent
        /// registrations unambiguous and lets the daemon refuse to create a
        /// readable-zero placeholder when fanotify_mark failed.
        WatchResult {
            id: u64,
            ok: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            message: Option<String>,
            /// Identity of the descriptor the helper actually marked. The
            /// daemon must cache this value rather than stat the pathname
            /// again after acknowledgement, which would reintroduce a rename
            /// race on its side of the protocol.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            dev: Option<u64>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            ino: Option<u64>,
        },
        /// Completion of a [`Request::WatchBatch`]. A top-level error covers a
        /// malformed/oversized request; otherwise there is one result per path.
        WatchBatchResult {
            id: u64,
            ok: bool,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            message: Option<String>,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            results: Vec<WatchOutcome>,
        },
        /// Retained for protocol compatibility with older helpers. New watch
        /// operations use [`Event::WatchResult`].
        Ok,
        Error {
            message: String,
        },
        Pong {
            version: String,
        },
    }

    #[cfg(test)]
    mod tests {
        use super::{Event, Request, WatchOutcome};

        #[test]
        fn watch_acknowledgements_keep_their_request_id() {
            let request = serde_json::to_string(&Request::Watch {
                id: 42,
                path: "/tmp/placeholder".into(),
            })
            .unwrap();
            let parsed: Request = serde_json::from_str(&request).unwrap();
            assert!(matches!(parsed, Request::Watch { id: 42, .. }));

            let event = serde_json::to_string(&Event::WatchResult {
                id: 42,
                ok: false,
                message: Some("unsupported filesystem".into()),
                dev: None,
                ino: None,
            })
            .unwrap();
            let parsed: Event = serde_json::from_str(&event).unwrap();
            assert!(matches!(
                parsed,
                Event::WatchResult {
                    id: 42,
                    ok: false,
                    ..
                }
            ));
        }

        #[test]
        fn batch_marks_preserve_result_order_without_repeating_paths() {
            let request = Request::WatchBatch {
                id: 7,
                paths: vec!["/tmp/a".into(), "/tmp/b".into()],
            };
            let encoded = serde_json::to_string(&request).unwrap();
            assert!(!encoded.contains("watch_results"));

            let event = Event::WatchBatchResult {
                id: 7,
                ok: true,
                message: None,
                results: vec![
                    WatchOutcome {
                        ok: true,
                        message: None,
                        dev: Some(1),
                        ino: Some(2),
                    },
                    WatchOutcome {
                        ok: false,
                        message: Some("unsupported".into()),
                        dev: None,
                        ino: None,
                    },
                ],
            };
            let encoded = serde_json::to_string(&event).unwrap();
            let parsed: Event = serde_json::from_str(&encoded).unwrap();
            assert!(matches!(
                parsed,
                Event::WatchBatchResult {
                    id: 7,
                    ok: true,
                    results,
                    ..
                } if results.len() == 2 && results[0].ino == Some(2)
            ));
        }

        #[test]
        fn old_feature_replies_default_batch_marks_off() {
            let parsed: Event =
                serde_json::from_str(r#"{"op":"features","partial_hydration":true}"#).unwrap();
            assert!(matches!(
                parsed,
                Event::Features {
                    partial_hydration: true,
                    batch_marks: false,
                    mark_sweeps: false,
                    file_manager_copy_detection: false,
                }
            ));
        }

        #[test]
        fn mark_set_controls_round_trip_with_request_ids() {
            for request in [
                Request::BeginWatchSet { id: 1 },
                Request::CommitWatchSet { id: 2 },
                Request::AbortWatchSet { id: 3 },
            ] {
                let encoded = serde_json::to_string(&request).unwrap();
                let parsed: Request = serde_json::from_str(&encoded).unwrap();
                assert!(matches!(
                    parsed,
                    Request::BeginWatchSet { .. }
                        | Request::CommitWatchSet { .. }
                        | Request::AbortWatchSet { .. }
                ));
            }
        }
    }
}
