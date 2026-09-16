//! Authentication and account status commands.

use std::sync::Arc;

use anyhow::{Context, Result};

use gonedrive_core::db::Store;
use gonedrive_core::sync::selection::{available_bytes, human_bytes};

use crate::login::{open_in_browser, Login};

use super::load_config;

pub(crate) async fn login() -> Result<()> {
    // Signing in beside a running daemon is the split brain this module warns
    // about: two processes would hold credentials for one account and take
    // turns rotating — and so invalidating — the same refresh token. The
    // daemon opens the browser itself and reports the outcome as a signal.
    if let Some(proxy) = super::running_daemon().await {
        return login_via_daemon(&proxy).await;
    }

    let config = load_config()?;
    gonedrive_core::config::ensure_dirs()?;
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let tokens = Arc::new(gonedrive_core::TokenStore::open().await.context(
        "could not reach the Secret Service; is gnome-keyring (or another keyring) running?",
    )?);

    let flow = Login::start(&config).await?;
    println!("Opening your browser to sign in.");
    println!("If nothing opens, visit this URL:\n\n  {}\n", flow.url);
    open_in_browser(&flow.url);

    // Same deadlines as the daemon's client: a flaky link must not hang the
    // token exchange forever.
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    let account_id = flow.complete(http, &config, &store, tokens).await?;
    // Persist the client id the sign-in just succeeded with. Baked-in and
    // environment ids are conveniences of the build that produced them; a
    // rebuild without the variable silently loses the bake, and the daemon
    // then refuses to start for no visible reason. The config file survives.
    if let Err(e) = config.save() {
        tracing::warn!(error = %e, "could not persist the configuration");
    }

    let account = store.account(&account_id)?;
    let who = account
        .as_ref()
        .and_then(|a| a.address.clone().or_else(|| a.display_name.clone()))
        .unwrap_or_else(|| account_id.clone());
    println!("Signed in as {who}.");
    println!("Sync folder: {}", config.sync_root.display());
    println!("\nStart syncing with: gonedrived run");
    Ok(())
}

pub(crate) fn status() -> Result<()> {
    let config = load_config()?;
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let accounts = store.accounts()?;

    if accounts.is_empty() {
        println!("No accounts signed in. Run `gonedrived login`.");
        return Ok(());
    }
    for account in accounts {
        let pending = store.pending_count(&account.id)?;
        println!("Account: {}", account.id);
        if let Some(address) = &account.address {
            println!("  Signed in as: {address}");
        }
        println!("  Sync folder:  {}", account.root_local_path.display());
        if let (Some(used), Some(total)) = (account.quota_used, account.quota_total) {
            println!(
                "  Quota:        {} of {}",
                human_bytes(used),
                human_bytes(total)
            );
        }
        if let Some(free) = available_bytes(&account.root_local_path) {
            println!("  Disk free:    {}", human_bytes(free));
        }
        println!(
            "  Enumerated:   {}",
            if account.delta_link.is_some() {
                "yes"
            } else {
                "not yet"
            }
        );
        println!("  Queued:       {pending}");
        println!(
            "  New content:  {}",
            match config.download_new {
                gonedrive_core::DownloadPolicy::Never =>
                    "not downloaded until you pin it (`gonedrived ls`, `gonedrived pin <path>`)",
                gonedrive_core::DownloadPolicy::Always => "downloaded automatically",
            }
        );
    }
    println!(
        "\nConfig: {}",
        gonedrive_core::config::config_file().display()
    );
    Ok(())
}

async fn login_via_daemon(proxy: &gonedrive_ipc::DaemonProxy<'static>) -> Result<()> {
    use futures_util::StreamExt;

    // Subscribed before the request, so a sign-in that completes quickly
    // cannot finish in the gap between the method returning and the listener
    // starting.
    let mut completed = proxy.receive_login_completed().await?;
    let mut failed = proxy.receive_login_failed().await?;

    let url = proxy.start_login().await?;
    println!("Opening your browser to sign in.");
    println!("If nothing opens, visit this URL:\n\n  {url}\n");

    tokio::select! {
        signal = completed.next() => {
            let account_id = signal
                .ok_or_else(|| anyhow::anyhow!("the daemon closed the sign-in stream"))?
                .args()?
                .account_id
                .to_string();
            let store = Store::open(&gonedrive_core::config::state_db_path())?;
            let who = store
                .account(&account_id)?
                .and_then(|account| account.address.or(account.display_name))
                .unwrap_or(account_id);
            println!("Signed in as {who}.");
            Ok(())
        }
        signal = failed.next() => {
            let message = signal
                .ok_or_else(|| anyhow::anyhow!("the daemon closed the sign-in stream"))?
                .args()?
                .message
                .to_string();
            anyhow::bail!("the daemon reports: {message}")
        }
    }
}

pub(crate) async fn logout(account_id: Option<String>) -> Result<()> {
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let accounts = store.accounts()?;

    let account_id = match (account_id, accounts.len()) {
        (Some(id), _) => id,
        (None, 1) => accounts[0].id.clone(),
        (None, 0) => anyhow::bail!("no accounts are signed in"),
        (None, _) => anyhow::bail!("several accounts are signed in; name the one to sign out"),
    };

    // Signing out in-process while a daemon is running is the split brain this
    // module warns about: the daemon's engine keeps transferring for an account
    // whose rows we just deleted, and its authenticator rotates the token —
    // re-creating the keyring entry this command removed.
    if let Some(proxy) = super::running_daemon().await {
        proxy.remove_account(&account_id).await?;
        println!("Signed out of {account_id}. Local files were left in place.");
        return Ok(());
    }

    let tokens = gonedrive_core::TokenStore::open().await?;
    tokens.delete(&account_id).await?;
    store.remove_account(&account_id)?;
    println!("Signed out of {account_id}. Local files were left in place.");
    Ok(())
}
