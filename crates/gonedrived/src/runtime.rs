//! Long-running daemon setup and shutdown supervision.

use std::time::Duration;

use anyhow::Result;

use crate::app::App;
use crate::commands::load_config;
use crate::watcher::LocalWatcher;
use crate::{hsm_client, service, watcher};

pub(crate) async fn run() -> Result<()> {
    let config = load_config()?;
    let app = App::new(config).await?;

    let accounts = app.load_accounts().await?;
    if accounts == 0 {
        tracing::warn!("no accounts are signed in; run `gonedrived login` first");
    }

    // Files on demand needs the privileged helper. The connection is
    // supervised for the daemon's whole life: the helper restarts on
    // upgrades, and a daemon left orphaned means placeholders that read as
    // zeroes and free-space actions that fail — both learned the hard way.
    tokio::spawn(hsm_supervisor(app.clone()));

    // In on-demand mode the whole drive is presented as placeholders once the
    // helper is up, so the file manager shows the full tree. Deferred to a
    // task so start-up is not blocked by materialising a large drive.
    if app.config.on_demand {
        let app = app.clone();
        tokio::spawn(async move {
            // Give the helper a moment to connect, then materialise.
            for _ in 0..30 {
                if app.on_demand_ready().await {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            if let Err(e) = app.materialize_tree().await {
                tracing::warn!(error = %e, "could not materialise the placeholder tree");
            }
        });
    }

    let connection = service::serve(app.clone()).await?;
    tracing::info!(
        bus = gonedrive_ipc::SERVICE_NAME,
        "listening on the session bus"
    );

    // Keep the watcher alive for the daemon's lifetime; dropping it stops the
    // watch and the client silently falls back to poll-only.
    let (_watcher, local_changes) = if accounts > 0 {
        watcher::check_watch_limit(&app.config.sync_root);
        match LocalWatcher::start(
            &app.config.sync_root,
            Duration::from_millis(app.config.local_debounce_ms),
        ) {
            Ok((watcher, changes)) => (Some(watcher), Some(changes)),
            Err(e) => {
                // Not fatal: the periodic scan still converges, just less
                // promptly, and saying so beats appearing to work perfectly.
                tracing::warn!(error = %e, "watching for local changes failed; \
                                            falling back to periodic scans only");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    app.clone().run(local_changes, shutdown_signal()).await;
    drop(connection);
    Ok(())
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
                match client.watch_all(&app).await {
                    Ok(n) => tracing::info!(placeholders = n, "files on demand is active"),
                    Err(e) => tracing::warn!(error = %e, "could not register placeholders"),
                }
                app.set_hsm(Some(client.clone())).await;

                client.closed().await;
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
