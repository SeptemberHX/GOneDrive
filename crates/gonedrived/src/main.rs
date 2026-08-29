//! GOnedrive's background sync daemon.

mod app;
mod hsm_client;
mod login;
mod service;
mod watcher;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use gonedrive_core::config::Config;
use gonedrive_core::db::{PinState, Store};
use gonedrive_core::sync::selection::{available_bytes, human_bytes, Selection};

use crate::app::{App, DaemonEvent};
use crate::login::{open_in_browser, Login};
use crate::watcher::LocalWatcher;

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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gonedrived=info,gonedrive_core=info".into()),
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

fn load_config() -> Result<Config> {
    Config::load().context("could not load the configuration")
}

async fn run() -> Result<()> {
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
    let _watcher = if accounts > 0 {
        watcher::check_watch_limit(&app.config.sync_root);
        match LocalWatcher::start(
            &app.config.sync_root,
            Duration::from_millis(app.config.local_debounce_ms),
            app.wake.clone(),
        ) {
            Ok(watcher) => Some(watcher),
            Err(e) => {
                // Not fatal: the periodic scan still converges, just less
                // promptly, and saying so beats appearing to work perfectly.
                tracing::warn!(error = %e, "watching for local changes failed; \
                                            falling back to periodic scans only");
                None
            }
        }
    } else {
        None
    };

    app.clone().run(shutdown_signal()).await;
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

async fn login() -> Result<()> {
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

async fn sync_once() -> Result<()> {
    if let Some(proxy) = running_daemon().await {
        println!("Asking the running daemon to sync…");
        return sync_via_daemon(&proxy).await;
    }
    let config = load_config()?;
    let app = App::new(config).await?;
    if app.load_accounts().await? == 0 {
        anyhow::bail!("no accounts are signed in; run `gonedrived login` first");
    }

    // A first pass over a large drive spends minutes reading the file list
    // before it transfers anything. Printing nothing for that whole time is
    // indistinguishable from having hung.
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

async fn transfer() -> Result<()> {
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

fn status() -> Result<()> {
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
            println!("  Quota:        {} of {}", human_bytes(used), human_bytes(total));
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
    println!("\nConfig: {}", gonedrive_core::config::config_file().display());
    Ok(())
}

async fn logout(account_id: Option<String>) -> Result<()> {
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

/// The running daemon, if there is one.
///
/// Every command that *changes* anything prefers this route. Running the
/// change in-process while a daemon is also running is a split brain: two
/// processes race the database, the daemon never hears about placeholders the
/// CLI created (so nothing intercepts reads of them), and — worst — both
/// refresh the OAuth token, and Microsoft rotates it on each refresh, so the
/// two processes take turns invalidating each other's credentials until one
/// is signed out.
async fn running_daemon() -> Option<gonedrive_ipc::DaemonProxy<'static>> {
    let connection = zbus::Connection::session().await.ok()?;
    let proxy = gonedrive_ipc::DaemonProxy::new(&connection).await.ok()?;
    // Building a proxy proves nothing; a call does.
    proxy.status().await.ok()?;
    Some(proxy)
}

/// Absolute form of a path given relative to the sync root.
fn absolute_in_root(path: &std::path::Path) -> Result<std::path::PathBuf> {
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

/// Ask the daemon for a pass and wait until it settles.
async fn sync_via_daemon(proxy: &gonedrive_ipc::DaemonProxy<'static>) -> Result<()> {
    proxy.sync_now().await?;
    // Give the pass a moment to actually start before judging "idle".
    tokio::time::sleep(Duration::from_millis(800)).await;

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
            "signed_out" | "error" => {
                anyhow::bail!("the daemon reports: {}", status.last_error)
            }
            _ => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// Build a selection view without touching the network.
fn selection() -> Result<(Selection, Config, gonedrive_core::db::Account)> {
    let config = load_config()?;
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let accounts = store.accounts()?;
    let account = accounts
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no accounts are signed in; run `gonedrived login` first"))?;
    let selection = Selection::new(
        store,
        std::sync::Arc::new(config.clone()),
        account.id.clone(),
    );
    Ok((selection, config, account))
}

fn list(path: PathBuf) -> Result<()> {
    let (selection, _, _) = selection()?;
    let entries = selection.list(&path)?;

    if entries.is_empty() {
        // Gated on real content rather than on `delta_link`, which is only
        // written once a full enumeration finishes: an interrupted first pass
        // still leaves plenty to browse.
        println!("Nothing here yet. Run `gonedrived sync` to read the file list.");
        println!("(That downloads no content — only names and sizes.)");
        return Ok(());
    }

    let width = entries
        .iter()
        .map(|e| e.item.name.chars().count() + usize::from(e.item.is_folder))
        .max()
        .unwrap_or(0)
        .min(48);

    for entry in &entries {
        // Distinguishing an explicit choice from an inherited one matters:
        // unpinning a subfolder of a pinned folder is a different act from
        // unpinning something that was never selected.
        let mark = match (entry.pin.wants_content(), entry.explicit) {
            (true, true) => "[*]",
            (true, false) => "[✓]",
            (false, true) => "[x]",
            (false, false) => "[ ]",
        };
        let name = if entry.item.is_folder {
            format!("{}/", entry.item.name)
        } else {
            entry.item.name.clone()
        };
        let count = if entry.item.is_folder {
            format!("{} file(s)", entry.files)
        } else {
            String::new()
        };
        println!(
            "  {mark} {name:<width$}  {:>10}  {count}",
            human_bytes(entry.bytes)
        );
    }

    let selected: u64 = entries
        .iter()
        .filter(|e| e.pin.wants_content())
        .map(|e| e.bytes)
        .sum();
    let total: u64 = entries.iter().map(|e| e.bytes).sum();
    println!();
    println!(
        "  {} of {} selected for download here.",
        human_bytes(selected),
        human_bytes(total)
    );
    println!("  [*] pinned here   [✓] via a parent   [x] excluded   [ ] not selected");
    println!("  Choose with: gonedrived pin <path>");
    Ok(())
}

async fn set_pin(path: PathBuf, pin: PinState, assume_yes: bool) -> Result<()> {
    let (selection, config, account) = selection()?;

    // Check the cost before committing to it. A drive is routinely larger than
    // the disk under it, and finding that out after six hours of downloading is
    // not an acceptable way to learn it.
    if pin.wants_content() && !assume_yes {
        let preview = selection.preview(&path)?;
        if preview.exceeds_free_space(config.min_free_bytes) {
            println!(
                "{} would download {} into {}, which has {} free.",
                path.display(),
                human_bytes(preview.bytes),
                account.root_local_path.display(),
                preview
                    .free_bytes
                    .map(human_bytes)
                    .unwrap_or_else(|| "an unknown amount".into()),
            );
            println!();
            println!("Nothing was queued. Pin a smaller folder, free some space, or");
            println!("re-run with --yes to queue it anyway (downloads pause automatically");
            println!("before the disk fills).");
            return Ok(());
        }
    }

    if let Some(proxy) = running_daemon().await {
        let absolute = absolute_in_root(&path)?;
        proxy.set_pin_state(&absolute.to_string_lossy(), pin.as_str()).await?;
        if pin.wants_content() {
            println!("Pinned {}; the daemon is fetching it.", path.display());
        } else {
            println!("{} will no longer be downloaded.", path.display());
            println!("Existing local files were left in place.");
        }
        return Ok(());
    }

    let outcome = selection.set_pin(&path, pin)?;

    if !pin.wants_content() {
        println!("{} will no longer be downloaded.", path.display());
        println!("Existing local files were left in place.");
        return Ok(());
    }

    if outcome.files == 0 {
        println!("{} is already up to date locally.", path.display());
        return Ok(());
    }

    println!(
        "Queued {} file(s), {} to download.",
        outcome.files,
        human_bytes(outcome.bytes)
    );
    if let Some(free) = outcome.free_bytes {
        println!("Free space: {}", human_bytes(free));
    }
    if outcome.exceeds_free_space(config.min_free_bytes) {
        println!();
        println!("WARNING: this selection is larger than the free space on");
        println!("{}.", account.root_local_path.display());
        println!("Downloads will pause automatically before the disk fills, but you");
        println!("should unpin something or free space first:");
        println!("  gonedrived unpin {}", path.display());
    }
    println!();
    println!("Run `gonedrived run` (or `gonedrived sync`) to transfer them.");
    Ok(())
}

/// Build an engine. Needs the network, because the token is refreshed on start.
async fn engine() -> Result<(std::sync::Arc<App>, Config)> {
    let config = load_config()?;
    let app = App::new(config.clone()).await?;
    if app.load_accounts().await? == 0 {
        anyhow::bail!("no accounts are signed in; run `gonedrived login` first");
    }
    Ok((app, config))
}

async fn free_space(path: PathBuf) -> Result<()> {
    if let Some(proxy) = running_daemon().await {
        let absolute = absolute_in_root(&path)?;
        let freed = proxy.free_up_space(&absolute.to_string_lossy()).await?;
        println!("Freed {}.", human_bytes(freed));
        println!("The files stay visible at their real size and download again when opened.");
        return Ok(());
    }
    let (app, _) = engine().await?;
    let outcome = app.free_space(&path).await?;

    if outcome.files == 0 && outcome.skipped.is_empty() {
        println!("Nothing to free under {}.", path.display());
        return Ok(());
    }
    println!(
        "Freed {} across {} file(s).",
        human_bytes(outcome.freed),
        outcome.files
    );
    println!("They are still listed at their real size and download again when opened.");

    for (path, reason) in &outcome.skipped {
        println!("  kept {}: {reason}", path.display());
    }
    if !outcome.paths.is_empty() {
        println!();
        println!("Reads are intercepted by the helper. If it is not running yet:");
        println!("  sudo gonedrive-hsm");
    }
    Ok(())
}

async fn on_demand(path: PathBuf) -> Result<()> {
    if let Some(proxy) = running_daemon().await {
        let absolute = absolute_in_root(&path)?;
        let files = proxy.make_on_demand(&absolute.to_string_lossy()).await?;
        println!("Created {files} placeholder(s) under {}.", path.display());
        return Ok(());
    }
    let (app, _) = engine().await?;
    let outcome = app.make_on_demand(&path).await?;
    println!(
        "Created {} placeholder(s) under {}.",
        outcome.files,
        path.display()
    );
    for (path, reason) in outcome.skipped.iter().take(5) {
        println!("  skipped {}: {reason}", path.display());
    }
    if outcome.files > 0 {
        println!();
        println!("They occupy no space and fetch themselves on first read.");
        println!("The helper must be running: sudo gonedrive-hsm");
    }
    Ok(())
}

async fn materialize() -> Result<()> {
    if let Some(proxy) = running_daemon().await {
        println!("Materialising the whole drive as placeholders…");
        let n = proxy.materialize_tree().await?;
        println!("{n} placeholder(s) ready. Your entire OneDrive is now");
        println!("browsable in the file manager; files download when opened.");
        return Ok(());
    }
    anyhow::bail!("the daemon is not running; start it with `gonedrived run`")
}

/// Report what each file under a path actually costs on disk.
fn storage(path: PathBuf) -> Result<()> {
    use gonedrive_core::hydrate::{Occupancy, Placeholder, PlaceholderState};

    // A path argument is relative to the sync root, the same way `ls` and
    // `pin` take it — an absolute path is accepted too, for scripting.
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let sync_root = store
        .accounts()?
        .into_iter()
        .next()
        .map(|a| a.root_local_path)
        .ok_or_else(|| anyhow::anyhow!("no accounts are signed in"))?;
    let root = if path.as_os_str() == "." {
        sync_root
    } else if path.is_absolute() {
        path
    } else {
        sync_root.join(path)
    };
    if !root.exists() {
        anyhow::bail!("{} is not on disk", root.display());
    }

    let mut placeholders = 0u64;
    let mut hydrated = 0u64;
    let mut apparent = 0u64;
    let mut allocated = 0u64;

    let mut stack = vec![root.clone()];
    let mut rows = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(occupancy) = Occupancy::read(&p) else {
                continue;
            };
            apparent += occupancy.apparent_size;
            allocated += occupancy.allocated;
            let state = Placeholder::state(&p);
            match state {
                PlaceholderState::Dehydrated => placeholders += 1,
                _ => hydrated += 1,
            }
            rows.push((p, occupancy, state));
        }
    }

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (p, occupancy, state) in &rows {
        let mark = match state {
            PlaceholderState::Dehydrated => "☁",
            PlaceholderState::Hydrated => "✓",
            PlaceholderState::Unmanaged => " ",
        };
        println!(
            "  {mark} {:>10} on disk  {:>10} apparent  {}",
            human_bytes(occupancy.allocated),
            human_bytes(occupancy.apparent_size),
            p.strip_prefix(&root).unwrap_or(p).display()
        );
    }

    println!();
    println!("  ☁ placeholder (no content)   ✓ downloaded");
    println!(
        "  {} file(s): {} placeholders, {} with content",
        rows.len(),
        placeholders,
        hydrated
    );
    println!(
        "  {} apparent, {} actually on disk",
        human_bytes(apparent),
        human_bytes(allocated)
    );
    Ok(())
}

/// Hydrate a placeholder explicitly.
///
/// This is the same call the privileged helper makes when the kernel reports
/// that something is about to read the file, so it exercises the whole
/// hydration path without needing the helper to be running.
async fn fetch(path: PathBuf) -> Result<()> {
    use gonedrive_core::hydrate::Occupancy;

    if let Some(proxy) = running_daemon().await {
        let absolute = absolute_in_root(&path)?;
        let bytes = proxy.fetch_now(&absolute.to_string_lossy()).await?;
        let after = Occupancy::read(&absolute)?;
        println!("filled {}", human_bytes(bytes));
        println!(
            "now: {} on disk, {} apparent",
            human_bytes(after.allocated),
            human_bytes(after.apparent_size)
        );
        return Ok(());
    }

    let (app, _) = engine().await?;
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let sync_root = store
        .accounts()?
        .into_iter()
        .next()
        .map(|a| a.root_local_path)
        .ok_or_else(|| anyhow::anyhow!("no accounts are signed in"))?;
    let full = if path.is_absolute() {
        path
    } else {
        sync_root.join(path)
    };

    let before = Occupancy::read(&full)?;
    println!(
        "before: {} on disk, {} apparent",
        human_bytes(before.allocated),
        human_bytes(before.apparent_size)
    );

    let bytes = app.hydrate(&full).await?;

    let after = Occupancy::read(&full)?;
    println!(
        "after:  {} on disk, {} apparent",
        human_bytes(after.allocated),
        human_bytes(after.apparent_size)
    );
    println!("filled {}", human_bytes(bytes));
    Ok(())
}

/// Add or remove the marker desktop indexers look for.
fn indexer(include: bool) -> Result<()> {
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let root = store
        .accounts()?
        .into_iter()
        .next()
        .map(|a| a.root_local_path)
        .ok_or_else(|| anyhow::anyhow!("no accounts are signed in"))?;

    // GNOME's localsearch skips any directory containing one of these; the
    // list is `ignored-directories-with-content` in its GSettings schema. The
    // file is deliberately excluded from sync, because it describes a choice
    // made on *this* machine.
    let marker = root.join(".trackerignore");

    if include {
        match std::fs::remove_file(&marker) {
            Ok(()) => println!("Removed {}.", marker.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("{} was not excluded.", root.display());
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        println!("The file indexer will walk {} again.", root.display());
        println!();
        println!("Reads from indexers are still refused by the helper, so this");
        println!("will not download your drive — but it will cost a lot of");
        println!("pointless work on both sides.");
    } else {
        std::fs::write(
            &marker,
            "# GOnedrive: tells desktop file indexers to skip this folder.\n\
             # Placeholders look like ordinary files, so an indexing pass would\n\
             # download the entire drive.\n\
             # Undo with: gonedrived indexer --include\n",
        )?;
        println!("Excluded {} from the file indexer.", root.display());
        println!();
        println!("Trade-off: desktop search will no longer find these files.");
        println!("Windows does not force this choice because Windows Search");
        println!("understands placeholders; no Linux indexer does yet.");
        println!();
        println!("Undo with: gonedrived indexer --include");
    }

    // The setting is read per directory as it is walked, so nothing needs
    // restarting — but a pass already under way will finish first.
    Ok(())
}
