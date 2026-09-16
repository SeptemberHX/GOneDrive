//! The D-Bus service front ends talk to.

use std::sync::Arc;

use anyhow::{Context, Result};
use gonedrive_ipc::{
    AccountInfo, Activity, FolderEntry, Hydration, SyncRequestResult, SyncStatus, OBJECT_PATH,
    SERVICE_NAME,
};
use zbus::object_server::SignalEmitter;
use zbus::{connection, interface};

use crate::app::{App, DaemonEvent};
use crate::login::{open_in_browser, Login};

pub struct DaemonService {
    app: Arc<App>,
}

/// Why a caller's path was refused by [`confine_to_root`].
#[derive(Debug, PartialEq)]
enum ConfineError {
    /// The path is not inside the sync root at all.
    Outside,
    /// The path *is* the sync root: a single-shot call that would act on the
    /// whole drive, which these entry points deliberately refuse.
    WholeRoot,
}

/// Confine a caller-supplied absolute path to the sync root, returning it
/// relative to the root. Refuses anything outside the root, and refuses the
/// root itself so a one-click menu action can never sweep the entire drive.
///
/// Pure, so the safety guard can be tested without a running daemon.
fn confine_to_root(
    root: &std::path::Path,
    path: &str,
) -> std::result::Result<std::path::PathBuf, ConfineError> {
    let relative = std::path::Path::new(path)
        .strip_prefix(root)
        .map_err(|_| ConfineError::Outside)?;
    if relative.as_os_str().is_empty() {
        return Err(ConfineError::WholeRoot);
    }
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ConfineError::Outside);
    }
    Ok(relative.to_path_buf())
}

/// One line the CLI prints and the settings dialog shows.
pub(crate) fn describe_mode_switch(switch: &crate::app::ModeSwitch) -> String {
    let mut line = match switch.mode {
        "on-demand" => "Files on demand is on.".to_string(),
        "full" => "Full sync is on: the whole drive will be downloaded.".to_string(),
        other => format!("Sync mode is now {other}."),
    };
    if let Some(conversion) = &switch.conversion {
        line.push_str(&format!(
            " Kept {} folder(s) with local content; {} never-opened folder(s) now load on first open ({} placeholder(s) removed).",
            conversion.folders_kept, conversion.folders_pruned, conversion.placeholders_removed
        ));
    }
    if switch.restarting {
        line.push_str(" The daemon is restarting to apply it.");
    } else if switch.policy_changed {
        line.push_str(" Restart gonedrived to apply it.");
    }
    line
}

impl DaemonService {
    /// Turn an absolute path from a file manager into one relative to the sync
    /// root, refusing anything outside it.
    async fn relative(&self, path: &str) -> zbus::fdo::Result<std::path::PathBuf> {
        let accounts = self
            .app
            .accounts()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let root = accounts
            .first()
            .map(|a| std::path::PathBuf::from(&a.root_path))
            .ok_or_else(|| zbus::fdo::Error::Failed("no account is signed in".into()))?;

        confine_to_root(&root, path).map_err(|e| match e {
            ConfineError::Outside => {
                zbus::fdo::Error::InvalidArgs(format!("{path} is outside {}", root.display()))
            }
            // The sync root itself is off limits to these single-shot calls.
            // The audit log has a real entry where one context-menu click asked
            // to dehydrate the entire drive — an operation that big gets a place
            // where its scope is unmistakable (the panel's folder page, entry by
            // entry), not a menu item next to "open".
            ConfineError::WholeRoot => zbus::fdo::Error::AccessDenied(
                "refusing to act on the whole sync folder at once; \
                 choose folders individually in the GOnedrive panel"
                    .into(),
            ),
        })
    }

    /// Browsing the root is safe and necessary; unlike mutation actions this
    /// accepts the empty relative path while retaining the same confinement.
    async fn browse_relative(&self, path: &str) -> zbus::fdo::Result<std::path::PathBuf> {
        let accounts = self
            .app
            .accounts()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let root = accounts
            .first()
            .map(|a| std::path::PathBuf::from(&a.root_path))
            .ok_or_else(|| zbus::fdo::Error::Failed("no account is signed in".into()))?;
        match confine_to_root(&root, path) {
            Ok(relative) => Ok(relative),
            Err(ConfineError::WholeRoot) => Ok(std::path::PathBuf::new()),
            Err(ConfineError::Outside) => Err(zbus::fdo::Error::InvalidArgs(format!(
                "{path} is outside {}",
                root.display()
            ))),
        }
    }
}

#[interface(name = "org.gonedrive.Daemon1")]
impl DaemonService {
    async fn list_accounts(&self) -> zbus::fdo::Result<Vec<AccountInfo>> {
        self.app
            .accounts()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Start an interactive sign-in and return the URL to visit.
    ///
    /// Returns as soon as the URL exists rather than waiting for the user, so
    /// the caller is not blocked on a D-Bus method for however long someone
    /// takes to type a password. Completion arrives as a signal.
    async fn start_login(&self) -> zbus::fdo::Result<String> {
        // One at a time. A second sign-in completes against the same account,
        // and attaching an account that is already active is refused, so the
        // user was shown a failure for a sign-in that had in fact worked.
        if self
            .app
            .login_in_flight
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(zbus::fdo::Error::Failed(
                "a sign-in is already waiting for the browser".into(),
            ));
        }
        let guard = LoginGuard(self.app.login_in_flight.clone());
        let login = Login::start(&self.app.config())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        let url = login.url.clone();

        let app = self.app.clone();
        tokio::spawn(async move {
            // Held until the flow ends, whichever way it ends.
            let _guard = guard;
            let result = login
                .complete(
                    app.http.clone(),
                    &app.config(),
                    &app.store,
                    app.tokens.clone(),
                )
                .await;
            match result {
                Ok(account_id) => {
                    // Persist the client id the sign-in just succeeded with —
                    // from the *live* configuration, not the snapshot taken
                    // before the browser round trip. A mode switch made while
                    // the user was typing a password used to be undone here.
                    if let Err(e) = app.config().save() {
                        tracing::warn!(error = %e, "could not persist the configuration");
                    }
                    if let Err(e) = app.attach_account(&account_id).await {
                        app.notify_login_failed(&e.to_string());
                        return;
                    }
                    app.notify_login_completed(&account_id);
                    app.wake.notify_one();
                }
                Err(e) => app.notify_login_failed(&e.to_string()),
            }
        });

        open_in_browser(&url);
        Ok(url)
    }

    /// Sign out. Local files are deliberately left in place — removing an
    /// account is not a request to delete the user's documents.
    async fn remove_account(&self, account_id: &str) -> zbus::fdo::Result<()> {
        self.app.detach_account(account_id).await;
        self.app
            .tokens
            .delete(account_id)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        self.app
            .store
            .remove_account(account_id)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn status(&self) -> SyncStatus {
        self.app.status().await
    }

    async fn sync_now(&self) -> zbus::fdo::Result<()> {
        let _ = self.app.request_sync();
        Ok(())
    }

    async fn request_sync(&self) -> zbus::fdo::Result<u64> {
        Ok(self.app.request_sync())
    }

    async fn set_paused(&self, paused: bool) -> zbus::fdo::Result<()> {
        self.app.set_paused(paused).await;
        Ok(())
    }

    async fn set_pin_state(&self, path: &str, pin: &str) -> zbus::fdo::Result<()> {
        tracing::info!(%path, %pin, "SetPinState requested");
        let relative = self.relative(path).await?;
        let pin = gonedrive_core::db::PinState::parse(pin);
        let outcome = self
            .app
            .set_pin(&relative, pin)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        tracing::info!(
            path = %relative.display(),
            queued_files = outcome.files,
            queued_bytes = outcome.bytes,
            cancelled = outcome.cancelled,
            "SetPinState done"
        );
        self.app.wake.notify_one();
        Ok(())
    }

    async fn free_up_space(&self, path: &str) -> zbus::fdo::Result<u64> {
        tracing::info!(%path, "FreeUpSpace requested");
        let relative = self.relative(path).await?;
        let outcome = self
            .app
            .free_space(&relative)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        tracing::info!(
            path = %relative.display(),
            files = outcome.files,
            freed = outcome.freed,
            kept = outcome.skipped.len(),
            "FreeUpSpace done"
        );
        Ok(outcome.freed)
    }

    async fn list_folder(&self, path: &str) -> zbus::fdo::Result<Vec<FolderEntry>> {
        // Unlike the action methods, an empty path is legal here: it names the
        // sync root, which is exactly where the chooser starts.
        let relative = std::path::PathBuf::from(path);
        self.app
            .list_folder(&relative)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn recent_activity(&self) -> Vec<Activity> {
        self.app.recent_activity().await
    }

    async fn active_hydrations(&self) -> Vec<Hydration> {
        self.app.active_hydrations().await
    }

    async fn cancel_hydration(&self, path: &str) -> zbus::fdo::Result<()> {
        if self.app.cancel_hydration(std::path::Path::new(path)).await {
            Ok(())
        } else {
            Err(zbus::fdo::Error::Failed(format!(
                "{path} is not being downloaded"
            )))
        }
    }

    async fn make_on_demand(&self, path: &str) -> zbus::fdo::Result<u64> {
        let relative = self.relative(path).await?;
        let outcome = self
            .app
            .make_on_demand(&relative)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        Ok(outcome.files)
    }

    async fn populate_folder(&self, path: &str) -> zbus::fdo::Result<u64> {
        let relative = self.browse_relative(path).await?;
        let outcome = self
            .app
            .populate_folder(&relative)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        if !outcome.skipped.is_empty() {
            tracing::warn!(
                path = %relative.display(),
                skipped = outcome.skipped.len(),
                "some directory entries could not be populated"
            );
        }
        Ok(outcome.files)
    }

    async fn materialize_tree(&self) -> zbus::fdo::Result<u64> {
        tracing::info!("MaterializeTree requested");
        self.app
            .materialize_tree()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))
    }

    async fn sync_mode(&self) -> zbus::fdo::Result<String> {
        Ok(self.app.sync_mode().as_str().to_string())
    }

    async fn set_sync_mode(&self, mode: &str) -> zbus::fdo::Result<String> {
        tracing::info!(%mode, "SetSyncMode requested");
        let mode = crate::app::SyncMode::parse(mode).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!(
                "unknown sync mode {mode:?}; use on-demand or full"
            ))
        })?;
        let switch = self
            .app
            .set_sync_mode(mode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("{e:#}")))?;
        Ok(describe_mode_switch(&switch))
    }

    async fn fetch_now(&self, path: &str) -> zbus::fdo::Result<u64> {
        tracing::info!(%path, "FetchNow requested");
        let relative = self.relative(path).await?;
        let root = self
            .app
            .accounts()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?
            .into_iter()
            .next()
            .map(|account| std::path::PathBuf::from(account.root_path))
            .ok_or_else(|| zbus::fdo::Error::Failed("no account is signed in".into()))?;
        self.app
            .hydrate(&root.join(relative))
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    #[zbus(signal)]
    async fn status_changed(emitter: &SignalEmitter<'_>, status: SyncStatus) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn sync_request_completed(
        emitter: &SignalEmitter<'_>,
        generation: u64,
        result: SyncRequestResult,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn login_completed(emitter: &SignalEmitter<'_>, account_id: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn login_failed(emitter: &SignalEmitter<'_>, message: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn conflict_detected(
        emitter: &SignalEmitter<'_>,
        original: &str,
        copy: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn item_failed(emitter: &SignalEmitter<'_>, name: &str, error: &str) -> zbus::Result<()>;

    /// A destructive step was refused as likely accidental data loss. Nothing
    /// was deleted; the user should look and confirm.
    #[zbus(signal)]
    async fn safety_stop(emitter: &SignalEmitter<'_>, reason: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn hydration_started(
        emitter: &SignalEmitter<'_>,
        hydration: Hydration,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn hydration_progress(
        emitter: &SignalEmitter<'_>,
        hydration: Hydration,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn hydration_finished(
        emitter: &SignalEmitter<'_>,
        path: &str,
        ok: bool,
    ) -> zbus::Result<()>;
}

/// Publish the service on the session bus and start forwarding daemon events
/// to it as signals.
pub async fn serve(app: Arc<App>) -> Result<zbus::Connection> {
    let connection = connection::Builder::session()
        .context("could not connect to the session bus")?
        // zbus permits replacing an existing owner by default. A second
        // daemon must fail here instead of competing for the database, local
        // watcher and privileged helper while silently stealing the UI name.
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .name(SERVICE_NAME)
        .context("another GOnedrive daemon already owns the bus name")?
        .serve_at(OBJECT_PATH, DaemonService { app: app.clone() })
        .context("could not publish the daemon object")?
        .build()
        .await
        .context("could not start the D-Bus service")?;

    let emitter = SignalEmitter::new(&connection, OBJECT_PATH)
        .context("could not build a signal emitter")?
        .to_owned();
    let mut events = app.subscribe();

    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let result = match event {
                        DaemonEvent::Status(status) => {
                            DaemonService::status_changed(&emitter, status).await
                        }
                        DaemonEvent::LoginCompleted(id) => {
                            DaemonService::login_completed(&emitter, &id).await
                        }
                        DaemonEvent::LoginFailed(message) => {
                            DaemonService::login_failed(&emitter, &message).await
                        }
                        DaemonEvent::SyncRequestCompleted { generation, result } => {
                            DaemonService::sync_request_completed(&emitter, generation, result)
                                .await
                        }
                        DaemonEvent::Conflict { original, copy } => {
                            DaemonService::conflict_detected(&emitter, &original, &copy).await
                        }
                        DaemonEvent::ItemFailed { name, error } => {
                            DaemonService::item_failed(&emitter, &name, &error).await
                        }
                        DaemonEvent::SafetyStop { reason } => {
                            DaemonService::safety_stop(&emitter, &reason).await
                        }
                        DaemonEvent::HydrationStarted(hydration) => {
                            DaemonService::hydration_started(&emitter, hydration).await
                        }
                        DaemonEvent::HydrationProgress(hydration) => {
                            DaemonService::hydration_progress(&emitter, hydration).await
                        }
                        DaemonEvent::HydrationFinished { path, ok } => {
                            DaemonService::hydration_finished(&emitter, &path, ok).await
                        }
                    };
                    if let Err(e) = result {
                        tracing::debug!(error = %e, "could not emit signal");
                    }
                }
                // A slow or absent listener is not the daemon's problem.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    Ok(connection)
}

/// Clears the sign-in flag however the flow ends, including an early return.
struct LoginGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for LoginGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::{confine_to_root, ConfineError};
    use std::path::{Path, PathBuf};

    const ROOT: &str = "/home/u/OneDrive";

    #[test]
    fn a_path_inside_the_root_is_confined_to_a_relative_one() {
        assert_eq!(
            confine_to_root(Path::new(ROOT), "/home/u/OneDrive/Docs/a.txt"),
            Ok(PathBuf::from("Docs/a.txt"))
        );
    }

    #[test]
    fn a_path_outside_the_root_is_refused() {
        // The classic escape: a sibling that shares a name prefix, and an
        // absolute path elsewhere entirely.
        assert_eq!(
            confine_to_root(Path::new(ROOT), "/home/u/OneDrive-evil/x"),
            Err(ConfineError::Outside)
        );
        assert_eq!(
            confine_to_root(Path::new(ROOT), "/etc/passwd"),
            Err(ConfineError::Outside)
        );
        assert_eq!(
            confine_to_root(Path::new(ROOT), "/home/u"),
            Err(ConfineError::Outside)
        );
        assert_eq!(
            confine_to_root(Path::new(ROOT), "/home/u/OneDrive/a/../../outside"),
            Err(ConfineError::Outside)
        );
    }

    #[test]
    fn the_sync_root_itself_is_refused_so_one_click_cannot_sweep_the_drive() {
        assert_eq!(
            confine_to_root(Path::new(ROOT), ROOT),
            Err(ConfineError::WholeRoot)
        );
        // A trailing slash is still the root.
        assert_eq!(
            confine_to_root(Path::new(ROOT), "/home/u/OneDrive/"),
            Err(ConfineError::WholeRoot)
        );
    }
}
