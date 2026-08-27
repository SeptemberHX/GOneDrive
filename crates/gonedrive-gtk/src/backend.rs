//! The bridge between the GTK main loop and the daemon.
//!
//! GTK is single-threaded and GLib-driven; zbus wants an async runtime. Rather
//! than trying to marry the two executors, the D-Bus client lives on its own
//! thread with its own Tokio runtime and communicates through channels. The UI
//! side then only ever touches widgets from the main thread, which is the one
//! rule GTK does not forgive breaking.

use std::time::Duration;

use futures_util::StreamExt;
use gonedrive_ipc::{AccountInfo, Activity, DaemonProxy, FolderEntry, Hydration, SyncStatus};

use crate::tray::{GonedriveTray, TrayAction};

/// Everything the UI can be told.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// The daemon is reachable; here is the current picture.
    Connected {
        accounts: Vec<AccountInfo>,
        status: SyncStatus,
    },
    /// The daemon is not running or went away.
    Disconnected(String),
    Status(SyncStatus),
    /// A sign-in has begun; show the URL in case the browser did not open.
    LoginStarted(String),
    LoginCompleted,
    LoginFailed(String),
    Conflict { original: String, copy: String },
    ItemFailed { name: String, error: String },
    /// The recent-activity list, on request.
    Activity(Vec<Activity>),
    /// One level of the drive, for the folder chooser.
    FolderListing {
        path: String,
        entries: Vec<FolderEntry>,
    },
    /// Something opened a placeholder and is waiting for its content.
    HydrationStarted(Hydration),
    HydrationProgress(Hydration),
    HydrationFinished { path: String, ok: bool },
}

/// Everything the UI can ask for.
#[derive(Debug, Clone)]
pub enum Command {
    StartLogin,
    SyncNow,
    SetPaused(bool),
    RemoveAccount(String),
    Refresh,
    CancelHydration(String),
    /// Fetch the recent-activity list.
    GetActivity,
    /// Browse one level of the drive.
    ListFolder(String),
    /// Keep (or stop keeping) a path on this device. `keep: false` also frees
    /// the space, matching what unticking a folder means on Windows.
    SetKeep { path: String, keep: bool },
}

/// How long to wait before retrying a connection to the daemon.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// Start the backend thread, returning the ends the UI talks through.
///
/// `with_tray` also puts a status-notifier item on the bus; the progress
/// window does not want one.
pub fn spawn(
    with_tray: bool,
) -> (
    async_channel::Sender<Command>,
    async_channel::Receiver<UiEvent>,
    async_channel::Receiver<TrayAction>,
) {
    let (command_tx, command_rx) = async_channel::unbounded::<Command>();
    let (ui_tx, ui_rx) = async_channel::unbounded::<UiEvent>();
    let (tray_tx, tray_rx) = async_channel::unbounded::<TrayAction>();

    let commands_for_tray = command_tx.clone();
    std::thread::Builder::new()
        .name("gonedrive-dbus".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = ui_tx.send_blocking(UiEvent::Disconnected(e.to_string()));
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(
                &runtime,
                run(command_rx, ui_tx, tray_tx, commands_for_tray, with_tray),
            );
        })
        .expect("could not start the D-Bus thread");

    (command_tx, ui_rx, tray_rx)
}

async fn run(
    commands: async_channel::Receiver<Command>,
    ui: async_channel::Sender<UiEvent>,
    actions: async_channel::Sender<TrayAction>,
    tray_commands: async_channel::Sender<Command>,
    with_tray: bool,
) {
    // The tray registers in the background. Registration talks to the shell's
    // StatusNotifierWatcher, and on a shell that is slow, absent, or mid-
    // restart that handshake can stall — it must never gate the daemon
    // connection, or a wedged tray turns into a blank panel.
    let tray: TrayHandle = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let with_tray = with_tray && std::env::var_os("GONEDRIVE_NO_TRAY").is_none();
    if with_tray {
        // The tray lives on its own thread with its own runtime. Sharing this
        // thread's executor stalls its registration — and a status icon that
        // may or may not appear depending on scheduling is worse than none.
        // The handle is just a channel sender, so updates cross over freely.
        let slot = tray.clone();
        let actions_for_tray = actions.clone();
        let commands_for_item = tray_commands.clone();
        std::thread::Builder::new()
            .name("gonedrive-tray".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        tracing::warn!("tray thread has no runtime: {e}");
                        return;
                    }
                };
                runtime.block_on(async move {
                    // Registration is retried for the process's whole life.
                    // The StatusNotifierWatcher lives in the shell, and shells
                    // drop it — an extension crash, a restart — so "no tray
                    // right now" is a condition to wait out, not a verdict.
                    loop {
                        let item = GonedriveTray::new(
                            actions_for_tray.clone(),
                            commands_for_item.clone(),
                        );
                        match item.start().await {
                            Ok(handle) => {
                                tracing::info!("tray registered");
                                *slot.lock().await = Some(handle);
                                // Service tasks live on this runtime; park to
                                // keep them alive.
                                std::future::pending::<()>().await;
                            }
                            Err(e) => {
                                tracing::info!(
                                    "no system tray yet ({e}); retrying in 30s"
                                );
                                tokio::time::sleep(Duration::from_secs(30)).await;
                            }
                        }
                    }
                });
            })
            .expect("could not start the tray thread");
    }

    loop {
        match connect(&ui, &tray).await {
            Ok(proxy) => {
                // Returns when the connection drops, so the outer loop
                // reconnects — a daemon restart should not require restarting
                // the application.
                serve(&proxy, &commands, &ui, &tray).await;
                let _ = ui
                    .send(UiEvent::Disconnected("the daemon stopped".into()))
                    .await;
            }
            Err(e) => {
                if let Some(handle) = tray.lock().await.as_ref() {
                    handle
                        .update(|tray| {
                            tray.set_status(SyncStatus {
                                state: "offline".into(),
                                ..Default::default()
                            });
                        })
                        .await;
                }
                let _ = ui.send(UiEvent::Disconnected(e.to_string())).await;
            }
        }
        if commands.is_closed() {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

type TrayHandle = std::sync::Arc<tokio::sync::Mutex<Option<ksni::Handle<GonedriveTray>>>>;

async fn connect(
    ui: &async_channel::Sender<UiEvent>,
    tray: &TrayHandle,
) -> zbus::Result<DaemonProxy<'static>> {
    let connection = zbus::Connection::session().await?;
    let proxy = DaemonProxy::new(&connection).await?;
    // Any call proves the daemon is actually there; merely building a proxy
    // does not, because D-Bus resolves the name lazily.
    let accounts = proxy.list_accounts().await?;
    let status = proxy.status().await?;
    if let Some(handle) = tray.lock().await.as_ref() {
        let first = accounts.first().cloned();
        let current = status.clone();
        handle
            .update(move |tray| {
                tray.set_account(first);
                tray.set_status(current);
            })
            .await;
    }
    let _ = ui.send(UiEvent::Connected { accounts, status }).await;

    // Anything already in flight, so a window that opened late shows the
    // transfer it was opened *for* rather than an empty list.
    if let Ok(active) = proxy.active_hydrations().await {
        for hydration in active {
            let _ = ui.send(UiEvent::HydrationStarted(hydration)).await;
        }
    }
    Ok(proxy)
}

async fn serve(
    proxy: &DaemonProxy<'static>,
    commands: &async_channel::Receiver<Command>,
    ui: &async_channel::Sender<UiEvent>,
    tray: &TrayHandle,
) {
    let Ok(mut status_changed) = proxy.receive_status_changed().await else {
        return;
    };
    let Ok(mut login_completed) = proxy.receive_login_completed().await else {
        return;
    };
    let Ok(mut login_failed) = proxy.receive_login_failed().await else {
        return;
    };
    let Ok(mut conflicts) = proxy.receive_conflict_detected().await else {
        return;
    };
    let Ok(mut failures) = proxy.receive_item_failed().await else {
        return;
    };
    let Ok(mut hydration_started) = proxy.receive_hydration_started().await else {
        return;
    };
    let Ok(mut hydration_progress) = proxy.receive_hydration_progress().await else {
        return;
    };
    let Ok(mut hydration_finished) = proxy.receive_hydration_finished().await else {
        return;
    };

    loop {
        tokio::select! {
            Some(signal) = status_changed.next() => {
                if let Ok(args) = signal.args() {
                    if let Some(handle) = tray.lock().await.as_ref() {
                        let status = args.status.clone();
                        handle.update(move |tray| tray.set_status(status)).await;
                    }
                    let _ = ui.send(UiEvent::Status(args.status)).await;
                }
            }
            Some(_) = login_completed.next() => {
                let _ = ui.send(UiEvent::LoginCompleted).await;
                if let Ok(accounts) = proxy.list_accounts().await {
                    if let Ok(status) = proxy.status().await {
                        let _ = ui.send(UiEvent::Connected { accounts, status }).await;
                    }
                }
            }
            Some(signal) = login_failed.next() => {
                if let Ok(args) = signal.args() {
                    let _ = ui.send(UiEvent::LoginFailed(args.message.to_string())).await;
                }
            }
            Some(signal) = conflicts.next() => {
                if let Ok(args) = signal.args() {
                    let _ = ui.send(UiEvent::Conflict {
                        original: args.original.to_string(),
                        copy: args.copy.to_string(),
                    }).await;
                }
            }
            Some(signal) = failures.next() => {
                if let Ok(args) = signal.args() {
                    let _ = ui.send(UiEvent::ItemFailed {
                        name: args.name.to_string(),
                        error: args.error.to_string(),
                    }).await;
                }
            }
            Some(signal) = hydration_started.next() => {
                if let Ok(args) = signal.args() {
                    let _ = ui.send(UiEvent::HydrationStarted(args.hydration)).await;
                }
            }
            Some(signal) = hydration_progress.next() => {
                if let Ok(args) = signal.args() {
                    let _ = ui.send(UiEvent::HydrationProgress(args.hydration)).await;
                }
            }
            Some(signal) = hydration_finished.next() => {
                if let Ok(args) = signal.args() {
                    let _ = ui.send(UiEvent::HydrationFinished {
                        path: args.path.to_string(),
                        ok: args.ok,
                    }).await;
                }
            }
            command = commands.recv() => {
                match command {
                    Ok(command) => {
                        if !handle(proxy, command, ui).await {
                            return; // the daemon went away mid-call
                        }
                    }
                    Err(_) => return, // the UI is gone
                }
            }
            else => return,
        }
    }
}

/// Run one command. Returns false when the daemon is no longer reachable.
async fn handle(
    proxy: &DaemonProxy<'static>,
    command: Command,
    ui: &async_channel::Sender<UiEvent>,
) -> bool {
    match command {
        Command::StartLogin => match proxy.start_login().await {
            Ok(url) => {
                let _ = ui.send(UiEvent::LoginStarted(url)).await;
                true
            }
            Err(e) => {
                let _ = ui.send(UiEvent::LoginFailed(e.to_string())).await;
                // A failed sign-in is not a lost connection.
                !is_disconnected(&e)
            }
        },
        Command::SyncNow => proxy.sync_now().await.is_ok(),
        Command::CancelHydration(path) => {
            // A refusal here just means it already finished.
            let _ = proxy.cancel_hydration(&path).await;
            true
        }
        Command::ListFolder(path) => match proxy.list_folder(&path).await {
            Ok(entries) => {
                let _ = ui.send(UiEvent::FolderListing { path, entries }).await;
                true
            }
            Err(_) => false,
        },
        Command::SetKeep { path, keep } => {
            let ok = if keep {
                proxy.set_pin_state(&path, "pinned").await.is_ok()
            } else {
                // Deselecting means "not on this device": stop future
                // downloads AND give the space back, the way unticking a
                // folder behaves on Windows.
                let a = proxy.set_pin_state(&path, "unpinned").await.is_ok();
                let b = proxy.free_up_space(&path).await.is_ok();
                a && b
            };
            ok
        }
        Command::GetActivity => match proxy.recent_activity().await {
            Ok(items) => {
                let _ = ui.send(UiEvent::Activity(items)).await;
                true
            }
            Err(_) => false,
        },
        Command::SetPaused(paused) => proxy.set_paused(paused).await.is_ok(),
        Command::RemoveAccount(id) => {
            let ok = proxy.remove_account(&id).await.is_ok();
            if ok {
                if let (Ok(accounts), Ok(status)) =
                    (proxy.list_accounts().await, proxy.status().await)
                {
                    let _ = ui.send(UiEvent::Connected { accounts, status }).await;
                }
            }
            ok
        }
        Command::Refresh => match (proxy.list_accounts().await, proxy.status().await) {
            (Ok(accounts), Ok(status)) => {
                let _ = ui.send(UiEvent::Connected { accounts, status }).await;
                true
            }
            _ => false,
        },
    }
}

/// Distinguish "the daemon refused this" from "the daemon is gone".
///
/// Only the latter should tear down the connection and reconnect; a refused
/// sign-in is an ordinary answer.
fn is_disconnected(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::InputOutput(_))
}
