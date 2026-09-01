//! GOnedrive's background sync daemon.

mod app;
mod commands;
mod failure_summary;
mod hsm_client;
mod indexer;
mod login;
mod open_intent;
mod placeholder_reports;
mod runtime;
mod service;
mod watcher;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{io::IsTerminal, path::PathBuf};

use gonedrive_core::db::PinState;

use crate::commands::{
    fetch, free_space, indexer, list, login, logout, materialize, on_demand, set_pin, status,
    storage, sync_once, transfer,
};
use crate::runtime::run;

#[cfg(debug_assertions)]
const DEFAULT_LOG_FILTER: &str = "gonedrived=info,gonedrive_core=info";
#[cfg(not(debug_assertions))]
const DEFAULT_LOG_FILTER: &str = "warn";

#[derive(Parser)]
#[command(name = "gonedrived", version, about = "GOnedrive sync daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Sync continuously (the default).
    Run,
    /// Sign in to a Microsoft account.
    Login,
    /// Run a single sync pass and exit.
    Sync,
    /// Transfer what is already queued, without re-reading the file list.
    Transfer,
    /// Show what the daemon knows without starting it.
    Status,
    /// Browse the drive and see what is selected for download.
    Ls {
        /// Folder relative to the sync root. Defaults to the root itself.
        path: Option<PathBuf>,
    },
    /// Keep a folder or file on this device (downloads it, and anything new
    /// that appears inside it).
    Pin {
        path: PathBuf,
        /// Queue the downloads without asking, even if space looks tight.
        #[arg(long)]
        yes: bool,
    },
    /// Stop downloading a folder or file. Existing local copies are kept.
    Unpin { path: PathBuf },
    /// Turn downloaded files back into placeholders, freeing their space.
    /// The files stay visible at their real size and come back when opened.
    FreeSpace { path: PathBuf },
    /// Create placeholders for files that have never been downloaded, so they
    /// appear locally and fetch themselves on first read.
    OnDemand { path: PathBuf },
    /// Fill a placeholder now, the same way reading it would.
    Fetch { path: PathBuf },
    /// Stop the desktop file indexer from walking the sync folder.
    ///
    /// Indexers read every file they find, and a placeholder is
    /// indistinguishable from an ordinary one — so an indexing pass downloads
    /// the whole drive. The privileged helper already refuses those reads, but
    /// telling the indexer to skip the folder outright is cheaper and does not
    /// depend on recognising its process name.
    Indexer {
        /// Undo the exclusion and let the indexer back in.
        #[arg(long)]
        include: bool,
    },
    /// Present the whole drive as a browsable placeholder tree, so the file
    /// manager shows every folder and file (true files-on-demand).
    Materialize,
    /// Show what is on disk, what is a placeholder, and what is neither.
    Storage {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Forget an account's credentials. Local files are kept.
    Logout {
        /// Account id, as shown by `status`. Omit when only one is signed in.
        account_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run().await,
        Command::Login => login().await,
        Command::Sync => sync_once().await,
        Command::Transfer => transfer().await,
        Command::Status => status(),
        Command::Ls { path } => list(path.unwrap_or_default()),
        Command::Pin { path, yes } => set_pin(path, PinState::Pinned, yes).await,
        Command::Unpin { path } => set_pin(path, PinState::Unpinned, true).await,
        Command::FreeSpace { path } => free_space(path).await,
        Command::OnDemand { path } => on_demand(path).await,
        Command::Materialize => materialize().await,
        Command::Indexer { include } => indexer(include),
        Command::Fetch { path } => fetch(path).await,
        Command::Storage { path } => storage(path),
        Command::Logout { account_id } => logout(account_id).await,
    }
}
