//! The interactive sign-in flow, shared by the CLI and the D-Bus service.

use std::sync::Arc;

use anyhow::{Context, Result};
use gonedrive_core::auth::{PendingLogin, LOGIN_TIMEOUT};
use gonedrive_core::config::Config;
use gonedrive_core::db::{Account, ItemRecord, Store};
use gonedrive_core::graph::GraphClient;
use gonedrive_core::{Authenticator, TokenStore};

/// A login waiting for the user to finish in their browser.
pub struct Login {
    pending: PendingLogin,
    pub url: String,
}

impl Login {
    pub async fn start(config: &Config) -> Result<Self> {
        let pending = PendingLogin::start(&config.client_id)
            .await
            .context("could not start the sign-in flow")?;
        let url = pending.authorize_url.to_string();
        Ok(Self { pending, url })
    }

    /// Wait for the browser redirect, then set the account up.
    ///
    /// Returns the account id. Everything the daemon needs afterwards — the
    /// refresh token in the keyring, the account row, the drive root — is in
    /// place by the time this returns, so a caller can start syncing straight
    /// away.
    pub async fn complete(
        self,
        http: reqwest::Client,
        config: &Config,
        store: &Store,
        tokens: Arc<TokenStore>,
    ) -> Result<String> {
        let token_set = self
            .pending
            .complete(&http, LOGIN_TIMEOUT)
            .await
            .context("the sign-in did not complete")?;

        // Identify the account before persisting anything, so a failure here
        // leaves no half-configured account behind.
        let probe = GraphClient::new(
            http.clone(),
            gonedrive_core::TokenSource::Static(token_set.access_token.clone()),
        );
        let user = probe.me().await.context("could not read the signed-in user")?;
        let drive = probe.drive().await.context("could not read the drive")?;
        let root = probe.root().await.context("could not read the drive root")?;

        let account_id = user.id.clone();
        Authenticator::adopt(
            http,
            config.client_id.clone(),
            account_id.clone(),
            tokens,
            token_set,
        )
        .await
        .context("could not store the refresh token in the keyring")?;

        let quota = drive.quota;
        store.upsert_account(&Account {
            id: account_id.clone(),
            drive_id: drive.id,
            root_item_id: Some(root.id.clone()),
            display_name: user.display_name.clone(),
            address: user.address().map(str::to_string),
            root_local_path: config.sync_root.clone(),
            delta_link: None,
            enum_link: None,
            quota_total: quota.and_then(|q| q.total),
            quota_used: quota.and_then(|q| q.used),
        })?;

        store.upsert_item(&ItemRecord::root(&account_id, &root.id))?;

        std::fs::create_dir_all(&config.sync_root)?;

        // Persist the client id the sign-in just succeeded with. Baked-in and
        // environment ids are conveniences of the build that produced them; a
        // rebuild without the variable silently loses the bake, and the daemon
        // then refuses to start for no visible reason. The config file is the
        // copy that survives.
        config.save().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not persist the configuration");
        });
        Ok(account_id)
    }
}

/// Open a URL in the user's browser, best-effort.
///
/// Failure is not fatal: the caller always shows the URL as well, which is the
/// only thing that works over SSH or in a minimal session.
pub fn open_in_browser(url: &str) -> bool {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}
