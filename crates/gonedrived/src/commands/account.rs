//! Authentication and account status commands.

use std::sync::Arc;

use anyhow::{Context, Result};

use gonedrive_core::db::Store;
use gonedrive_core::sync::selection::{available_bytes, human_bytes};

use crate::login::{open_in_browser, Login};

use super::load_config;

pub(crate) async fn login() -> Result<()> {
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

pub(crate) async fn logout(account_id: Option<String>) -> Result<()> {
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let accounts = store.accounts()?;

    let account_id = match (account_id, accounts.len()) {
        (Some(id), _) => id,
        (None, 1) => accounts[0].id.clone(),
        (None, 0) => anyhow::bail!("no accounts are signed in"),
        (None, _) => anyhow::bail!("several accounts are signed in; name the one to sign out"),
    };

    let tokens = gonedrive_core::TokenStore::open().await?;
    tokens.delete(&account_id).await?;
    store.remove_account(&account_id)?;
    println!("Signed out of {account_id}. Local files were left in place.");
    Ok(())
}
