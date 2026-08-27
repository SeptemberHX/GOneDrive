//! Persistence for refresh tokens, and the live access-token cache built on it.
//!
//! Refresh tokens go into the Secret Service (GNOME Keyring / KWallet) via
//! `oo7`, never into the state database — the database is a plain file that
//! backups and sync tools will happily copy elsewhere.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::auth::{self, TokenSet};
use crate::error::{Error, Result};

/// Attribute identifying this application's items in the Secret Service.
const APP_ATTR: &str = "gonedrive";

/// Stores one refresh token per account in the user's keyring.
pub struct TokenStore {
    keyring: oo7::Keyring,
}

impl TokenStore {
    pub async fn open() -> Result<Self> {
        let keyring = oo7::Keyring::new()
            .await
            .map_err(|e| Error::Keyring(format!("opening the keyring failed: {e}")))?;
        Ok(Self { keyring })
    }

    fn attributes(account_id: &str) -> HashMap<&str, &str> {
        HashMap::from([("application", APP_ATTR), ("account", account_id)])
    }

    pub async fn save(&self, account_id: &str, refresh_token: &str) -> Result<()> {
        self.keyring
            .create_item(
                &format!("GOnedrive refresh token ({account_id})"),
                &Self::attributes(account_id),
                refresh_token,
                // Replace rather than accumulate: the service rotates the token
                // on every refresh, so stale copies are worse than useless.
                true,
            )
            .await
            .map_err(|e| Error::Keyring(format!("storing the refresh token failed: {e}")))
    }

    pub async fn load(&self, account_id: &str) -> Result<Option<String>> {
        let items = self
            .keyring
            .search_items(&Self::attributes(account_id))
            .await
            .map_err(|e| Error::Keyring(format!("searching the keyring failed: {e}")))?;

        let Some(item) = items.first() else {
            return Ok(None);
        };
        let secret = item
            .secret()
            .await
            .map_err(|e| Error::Keyring(format!("reading the refresh token failed: {e}")))?;
        let token = String::from_utf8(secret.to_vec())
            .map_err(|_| Error::Keyring("the stored refresh token is not valid UTF-8".into()))?;
        Ok(Some(token))
    }

    pub async fn delete(&self, account_id: &str) -> Result<()> {
        self.keyring
            .delete(&Self::attributes(account_id))
            .await
            .map_err(|e| Error::Keyring(format!("deleting the refresh token failed: {e}")))
    }
}

/// Hands out a valid access token, refreshing behind the caller's back.
///
/// Every Graph request goes through [`Authenticator::access_token`]. The mutex
/// means a burst of concurrent transfers performs one refresh, not one each.
pub struct Authenticator {
    http: reqwest::Client,
    client_id: String,
    account_id: String,
    store: Arc<TokenStore>,
    tokens: Mutex<TokenSet>,
}

impl Authenticator {
    /// Take ownership of tokens from a just-completed interactive login and
    /// write the refresh token to the keyring.
    pub async fn adopt(
        http: reqwest::Client,
        client_id: String,
        account_id: String,
        store: Arc<TokenStore>,
        tokens: TokenSet,
    ) -> Result<Self> {
        store.save(&account_id, &tokens.refresh_token).await?;
        Ok(Self {
            http,
            client_id,
            account_id,
            store,
            tokens: Mutex::new(tokens),
        })
    }

    /// Rebuild from the keyring on daemon start.
    ///
    /// Performs one refresh immediately, which doubles as a check that the
    /// stored token still works — better to surface a dead account at startup
    /// than at the first upload.
    pub async fn restore(
        http: reqwest::Client,
        client_id: String,
        account_id: String,
        store: Arc<TokenStore>,
    ) -> Result<Self> {
        let refresh_token = store.load(&account_id).await?.ok_or_else(|| {
            Error::AuthRequired(format!("no stored credentials for account {account_id}"))
        })?;
        let tokens = auth::refresh(&http, &client_id, &refresh_token).await?;
        store.save(&account_id, &tokens.refresh_token).await?;
        Ok(Self {
            http,
            client_id,
            account_id,
            store,
            tokens: Mutex::new(tokens),
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// A token guaranteed to be valid for at least the refresh skew.
    pub async fn access_token(&self) -> Result<String> {
        let mut tokens = self.tokens.lock().await;
        if tokens.needs_refresh() {
            let refreshed =
                auth::refresh(&self.http, &self.client_id, &tokens.refresh_token).await?;
            // Persist before publishing: if we crash between the two, the next
            // start reads a token that is at worst one rotation stale, rather
            // than one we already burned.
            if refreshed.refresh_token != tokens.refresh_token {
                self.store
                    .save(&self.account_id, &refreshed.refresh_token)
                    .await?;
            }
            *tokens = refreshed;
        }
        Ok(tokens.access_token.clone())
    }

    /// Force the next [`Authenticator::access_token`] call to refresh.
    ///
    /// Used when Graph returns 401 despite a token we believed was still
    /// valid — revocation, a password change, or a conditional-access policy
    /// can all invalidate a token ahead of its stated expiry.
    pub async fn invalidate(&self) {
        let mut tokens = self.tokens.lock().await;
        tokens.expires_at = time::OffsetDateTime::UNIX_EPOCH;
    }

    /// Drop the account's credentials from the keyring (sign out).
    pub async fn forget(&self) -> Result<()> {
        self.store.delete(&self.account_id).await
    }
}

/// Where a Graph request gets its bearer token.
///
/// An enum rather than a trait object: there are exactly two cases, and this
/// keeps the hot path — one `access_token` call per request — free of dynamic
/// dispatch. [`TokenSource::Static`] also lets the sync engine be tested end to
/// end against a fake Graph without a Secret Service running.
#[derive(Clone)]
pub enum TokenSource {
    Interactive(Arc<Authenticator>),
    Static(String),
}

impl TokenSource {
    pub async fn access_token(&self) -> Result<String> {
        match self {
            TokenSource::Interactive(auth) => auth.access_token().await,
            TokenSource::Static(token) => Ok(token.clone()),
        }
    }

    /// Force a refresh on the next use. A no-op for a static token, which has
    /// nothing to refresh from.
    pub async fn invalidate(&self) {
        if let TokenSource::Interactive(auth) = self {
            auth.invalidate().await;
        }
    }
}

impl From<Arc<Authenticator>> for TokenSource {
    fn from(auth: Arc<Authenticator>) -> Self {
        TokenSource::Interactive(auth)
    }
}
