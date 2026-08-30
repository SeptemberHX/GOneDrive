//! Browse and change selective-sync preferences.

use std::path::PathBuf;

use anyhow::Result;

use gonedrive_core::config::Config;
use gonedrive_core::db::{PinState, Store};
use gonedrive_core::sync::selection::{human_bytes, Selection};

use super::{absolute_in_root, load_config, running_daemon};

/// Build a selection view without touching the network.
fn selection() -> Result<(Selection, Config, gonedrive_core::db::Account)> {
    let config = load_config()?;
    let store = Store::open(&gonedrive_core::config::state_db_path())?;
    let accounts = store.accounts()?;
    let account = accounts.into_iter().next().ok_or_else(|| {
        anyhow::anyhow!("no accounts are signed in; run `gonedrived login` first")
    })?;
    let selection = Selection::new(
        store,
        std::sync::Arc::new(config.clone()),
        account.id.clone(),
    );
    Ok((selection, config, account))
}

pub(crate) fn list(path: PathBuf) -> Result<()> {
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
            format!("{} item(s)", entry.items)
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

pub(crate) async fn set_pin(path: PathBuf, pin: PinState, assume_yes: bool) -> Result<()> {
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
        proxy
            .set_pin_state(&absolute.to_string_lossy(), pin.as_str())
            .await?;
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
