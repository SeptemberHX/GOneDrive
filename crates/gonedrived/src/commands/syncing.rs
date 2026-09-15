//! One-shot sync and queued-transfer commands.

use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;

use crate::app::{App, DaemonEvent};

use super::{load_config, running_daemon};

pub(crate) async fn sync_once() -> Result<()> {
    if let Some(proxy) = running_daemon().await {
        println!("Asking the running daemon to sync…");
        return sync_via_daemon(&proxy).await;
    }
    let config = load_config()?;
    let app = App::new(config).await?;
    if app.load_accounts().await? == 0 {
        anyhow::bail!("no accounts are signed in; run `gonedrived login` first");
    }

    // Remote requests and queued transfers can still take time on a slow
    // connection. Surface progress even though files-on-demand no longer
    // performs a historical whole-drive enumeration on first login.
    let progress = spawn_progress(&app);

    app.sync_all().await;
    progress.abort();
    println!();

    let status = app.status().await;
    if !status.last_error.is_empty() {
        println!("Finished with errors: {}", status.last_error);
    }
    println!("{} operation(s) still queued.", status.pending);
    Ok(())
}

pub(crate) async fn transfer() -> Result<()> {
    if let Some(proxy) = running_daemon().await {
        println!("Asking the running daemon to transfer…");
        return sync_via_daemon(&proxy).await;
    }
    let config = load_config()?;
    let app = App::new(config).await?;
    if app.load_accounts().await? == 0 {
        anyhow::bail!("no accounts are signed in; run `gonedrived login` first");
    }
    let progress = spawn_progress(&app);
    app.transfer_all().await?;
    progress.abort();
    println!();

    let status = app.status().await;
    if !status.last_error.is_empty() {
        println!("Finished with errors: {}", status.last_error);
    }
    println!("{} operation(s) still queued.", status.pending);
    Ok(())
}

/// Echo the daemon's own event stream to the terminal.
fn spawn_progress(app: &std::sync::Arc<App>) -> tokio::task::JoinHandle<()> {
    let mut events = app.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                DaemonEvent::Status(status) => match status.state.as_str() {
                    "enumerating" => print!("\r  reading file list: {}    ", status.current_item),
                    "downloading" | "uploading" => print!(
                        "\r  {} {}    ",
                        status.state,
                        truncate(&status.current_item, 48)
                    ),
                    _ => continue,
                },
                DaemonEvent::ItemFailed { name, error } => {
                    println!("\r  failed: {name}: {error}");
                    continue;
                }
                DaemonEvent::Conflict { original, copy } => {
                    println!("\r  conflict: {original} kept locally as {copy}");
                    continue;
                }
                DaemonEvent::SafetyStop { reason } => {
                    println!("\r  SAFETY STOP: {reason}");
                    continue;
                }
                _ => continue,
            }
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    })
}

/// Keep a progress line from wrapping the terminal.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Ask the daemon for a pass and wait until it settles.
async fn sync_via_daemon(proxy: &gonedrive_ipc::DaemonProxy<'static>) -> Result<()> {
    // Subscribe before requesting so a very fast empty delta cannot complete
    // in the gap between the method return and signal registration.
    let mut completions = proxy.receive_sync_request_completed().await?;
    let generation = proxy.request_sync().await?;

    loop {
        let Some(signal) = completions.next().await else {
            anyhow::bail!("the daemon closed the sync completion stream");
        };
        let args = signal.args()?;
        if args.generation != generation {
            continue;
        }
        if !args.result.ok {
            anyhow::bail!("the daemon reports: {}", args.result.error);
        }
        println!(
            "Remote changes checked ({} change(s), {} operation(s) queued).",
            args.result.changes, args.result.queued
        );
        break;
    }

    let mut last = String::new();
    loop {
        let status = proxy.status().await?;
        if status.state != last {
            match status.state.as_str() {
                "enumerating" => println!("  reading the file list…"),
                "downloading" | "uploading" => {
                    println!("  {} {}", status.state, status.current_item)
                }
                _ => {}
            }
            last = status.state.clone();
        }
        match status.state.as_str() {
            "idle" if status.pending == 0 => {
                println!("Done.");
                return Ok(());
            }
            "keyring_locked" | "signed_out" | "error" => {
                anyhow::bail!("the daemon reports: {}", status.last_error)
            }
            "offline" => anyhow::bail!(
                "the daemon is offline and will retry on its own: {}",
                status.last_error
            ),
            "paused" => anyhow::bail!("the daemon paused for safety: {}", status.last_error),
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}
