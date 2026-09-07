//! Command implementations grouped by user-facing domain.

mod account;
mod selection;
mod storage;
mod syncing;

pub(crate) use account::{login, logout, status};
pub(crate) use selection::{list, set_pin};
pub(crate) use storage::{fetch, free_space, indexer, materialize, on_demand, storage, sync_mode};
pub(crate) use syncing::{sync_once, transfer};

use anyhow::{Context, Result};

use gonedrive_core::config::Config;
use gonedrive_core::db::Store;

use crate::app::App;

pub(super) fn load_config() -> Result<Config> {
    Config::load().context("could not load the configuration")
}

/// The running daemon, if there is one.
///
/// Every command that *changes* anything prefers this route. Running the
/// change in-process while a daemon is also running is a split brain: two
/// processes race the database, the daemon never hears about placeholders the
/// CLI created (so nothing intercepts reads of them), and — worst — both
/// refresh the OAuth token, and Microsoft rotates it on each refresh, so the
/// two processes take turns invalidating each other's credentials until one
/// is signed out.
pub(super) async fn running_daemon() -> Option<gonedrive_ipc::DaemonProxy<'static>> {
    let connection = zbus::Connection::session().await.ok()?;
    let proxy = gonedrive_ipc::DaemonProxy::new(&connection).await.ok()?;
    // Building a proxy proves nothing; a call does.
    proxy.status().await.ok()?;
    Some(proxy)
}

/// Absolute form of a path given relative to the sync root.
pub(super) fn absolute_in_root(path: &std::path::Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let root = store
        .accounts()?
        .into_iter()
        .next()
        .map(|a| a.root_local_path)
        .ok_or_else(|| anyhow::anyhow!("no accounts are signed in"))?;
    Ok(root.join(path))
}

/// Build an engine. Needs the network, because the token is refreshed on start.
pub(super) async fn engine() -> Result<(std::sync::Arc<App>, Config)> {
    let config = load_config()?;
    let app = App::new(config.clone()).await?;
    if app.load_accounts().await? == 0 {
        anyhow::bail!("no accounts are signed in; run `gonedrived login` first");
    }
    Ok((app, config))
}
