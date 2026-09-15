//! Long-running daemon setup and shutdown supervision.

use std::time::Duration;

use anyhow::{Context, Result};

use crate::app::App;
use crate::commands::load_config;
use crate::watcher::LocalWatcher;
use crate::{hsm_client, service, watcher};

/// A transient mark failure must not leave the rest of a live helper session
/// permanently half-registered. This is intentionally much slower than normal
/// incremental registration and runs only after an error.
const HSM_REGISTRATION_RETRY: Duration = Duration::from_secs(30);

pub(crate) async fn run() -> Result<()> {
    let config_exists = gonedrive_core::config::config_file().exists();
    let mut config = load_config()?;
    apply_first_run_defaults(&mut config, config_exists);
    let app = App::new(config).await?;

    // The UI can sign in after this process has started. Establish the watch
    // before that happens so the first session has the same real-time local
    // change handling as every later restart.
    std::fs::create_dir_all(&app.config().sync_root).with_context(|| {
        format!(
            "could not create sync root {}",
            app.config().sync_root.display()
        )
    })?;

    // Linux desktop indexers do not understand partial cloud directories.
    // Process-aware HSM callbacks refuse to populate for them, while the
    // standard exclusion avoids the otherwise pointless recursive namespace
    // walk and permission-event traffic in the first place.
    if let Err(error) = crate::indexer::ensure_policy(
        &app.config().sync_root,
        app.config().exclude_desktop_indexer,
    ) {
        tracing::warn!(%error, "could not apply desktop-indexer policy");
    }

    // Claim the single-instance bus name FIRST, before touching credentials.
    // The front end detects the daemon by this name, so registering it up
    // front means a daemon that is up but not yet signed in — a locked keyring,
    // no network — shows as "running, needs attention" instead of "not
    // running", and a stuck credential read can never keep us off the bus. It
    // also settles the single-instance race before either candidate touches the
    // watcher, database or HSM session.
    let connection = service::serve(app.clone()).await?;
    tracing::info!(
        bus = gonedrive_ipc::SERVICE_NAME,
        "listening on the session bus"
    );

    let has_configured_account = !app.store.accounts()?.is_empty();
    let accounts = app.load_accounts().await?;
    if !has_configured_account {
        tracing::warn!("no accounts are signed in; run `gonedrived login` first");
    } else if accounts == 0 {
        tracing::warn!("the configured account is waiting for credentials to become available");
    }

    app.start_open_intent_monitor();

    // Files on demand needs the privileged helper. The connection is
    // supervised for the daemon's whole life: the helper restarts on
    // upgrades, and a daemon left orphaned means placeholders that read as
    // zeroes and free-space actions that fail — both learned the hard way.
    tokio::spawn(hsm_supervisor(app.clone()));

    // Keep the watcher alive for the daemon's lifetime; dropping it stops the
    // watch and the client silently falls back to poll-only.
    watcher::check_watch_limit(&app.config().sync_root);
    let (_watcher, local_changes, open_writes) = match LocalWatcher::start(
        &app.config().sync_root,
        Duration::from_millis(app.config().local_debounce_ms),
    ) {
        Ok((watcher, changes)) => {
            let open_writes = watcher.open_writes();
            (Some(watcher), Some(changes), open_writes)
        }
        Err(e) => {
            // Not fatal: the periodic scan still converges, just less
            // promptly, and saying so beats appearing to work perfectly.
            tracing::warn!(error = %e, "watching for local changes failed; \
                                        falling back to periodic scans only");
            (None, None, watcher::OpenWrites::default())
        }
    };

    app.clone()
        .run(local_changes, open_writes, shutdown_signal())
        .await;
    drop(connection);
    Ok(())
}

/// The desktop client promises a Windows-style browsable placeholder tree.
/// Keep one-shot CLI commands conservative, but make a brand-new daemon
/// profile honour that promise. Once a configuration exists its explicit
/// choice always wins; a successful login persists this first-run value.
fn apply_first_run_defaults(config: &mut gonedrive_core::config::Config, config_exists: bool) {
    if !config_exists {
        config.on_demand = true;
    }
}

/// Keep the daemon attached to the files-on-demand helper.
async fn hsm_supervisor(app: std::sync::Arc<App>) {
    let socket = hsm_client::HsmClient::default_socket();
    let mut reported_off = false;
    loop {
        match hsm_client::HsmClient::connect(app.clone(), &socket).await {
            Ok(client) => {
                reported_off = false;
                let client = std::sync::Arc::new(client);
                // Publish the authenticated connection before the cold mark
                // sweep. A remote delta may create a new placeholder while the
                // old inventory is still registering; it must queue behind the
                // same registration lock instead of seeing `None` and deleting
                // the just-created sparse file as unprotected.
                app.set_hsm(Some(client.clone())).await;
                let mut registered = register_hsm_placeholders(&app, &client, true).await;
                registered &= replay_placeholder_report(&app).await;
                while !registered {
                    tokio::select! {
                        _ = client.closed() => break,
                        _ = tokio::time::sleep(HSM_REGISTRATION_RETRY) => {
                            registered = register_hsm_placeholders(&app, &client, false).await;
                            registered &= replay_placeholder_report(&app).await;
                        }
                    }
                }
                if registered {
                    client.closed().await;
                }
                app.set_hsm(None).await;
                tracing::warn!("files on demand lost the helper; reconnecting");
            }
            Err(e) => {
                // Reported once, not every five seconds, and loudly the first
                // time: placeholders on disk read as zeroes until this works.
                if !reported_off {
                    reported_off = true;
                    tracing::warn!("files on demand is OFF: {e:#}");
                    if let Ok(paths) = app.dehydrated_paths().await {
                        if !paths.is_empty() {
                            tracing::error!(
                                count = paths.len(),
                                "placeholders on disk have nothing to fill them \
                                 until the helper is running"
                            );
                        }
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn register_hsm_placeholders(
    app: &App,
    client: &hsm_client::HsmClient,
    initial: bool,
) -> bool {
    match client.watch_all(app).await {
        Ok(registered_now) => {
            let protected_total = client.watched_count().await;
            if initial {
                tracing::info!(protected_total, registered_now, "files on demand is active");
            } else {
                tracing::info!(
                    protected_total,
                    registered_now,
                    "files-on-demand registration recovered"
                );
            }
            true
        }
        Err(error) => {
            tracing::warn!(
                %error,
                retry_secs = HSM_REGISTRATION_RETRY.as_secs(),
                "could not register placeholders; will retry"
            );
            false
        }
    }
}

async fn replay_placeholder_report(app: &App) -> bool {
    match app.reconcile_pending_placeholder_report().await {
        Ok(0) => true,
        Ok(n) => {
            tracing::info!(
                placeholders = n,
                "reconciled placeholders observed while HSM was starting"
            );
            true
        }
        Err(error) => {
            tracing::warn!(
                %error,
                retry_secs = HSM_REGISTRATION_RETRY.as_secs(),
                "could not replay placeholder scan; will retry"
            );
            false
        }
    }
}

/// Resolve when the process is asked to stop.
async fn shutdown_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use gonedrive_core::config::Config;

    use super::apply_first_run_defaults;

    const USER_UNIT: &str = include_str!("../../../data/gonedrived.service");

    #[test]
    fn a_new_daemon_profile_starts_in_files_on_demand_mode() {
        let mut config = Config::default();

        apply_first_run_defaults(&mut config, false);

        assert!(config.on_demand);
    }

    #[test]
    fn an_explicit_selective_sync_setting_is_preserved() {
        let mut config = Config {
            on_demand: false,
            ..Config::default()
        };

        apply_first_run_defaults(&mut config, true);

        assert!(!config.on_demand);
    }

    #[test]
    fn user_unit_keeps_openat2_create_available() {
        assert!(
            !USER_UNIT
                .lines()
                .any(|line| line.trim() == "RestrictSUIDSGID=true"),
            "RestrictSUIDSGID makes systemd reject RootedFs openat2(O_CREAT) calls"
        );
    }

    #[test]
    fn user_unit_bounds_journal_writes() {
        assert!(USER_UNIT.contains("LogRateLimitIntervalSec=1min"));
        assert!(USER_UNIT.contains("LogRateLimitBurst=30"));
    }
}
