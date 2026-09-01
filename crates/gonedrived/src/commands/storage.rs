//! Files-on-demand and local storage management commands.

use std::path::{Path, PathBuf};

use anyhow::Result;

use gonedrive_core::db::Store;
use gonedrive_core::sync::selection::human_bytes;

use super::{absolute_in_root, engine, running_daemon};

pub(crate) async fn free_space(path: PathBuf) -> Result<()> {
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

pub(crate) async fn on_demand(path: PathBuf) -> Result<()> {
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

pub(crate) async fn materialize() -> Result<()> {
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
pub(crate) fn storage(path: PathBuf) -> Result<()> {
    use gonedrive_core::hydrate::PlaceholderState;

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

    let mut rows = Vec::new();
    let mut stack = Vec::new();
    if root.is_dir() {
        stack.push(root.clone());
    } else if let Some(row) = storage_row(&root) {
        rows.push(row);
    }
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
            if let Some(row) = storage_row(&p) {
                rows.push(row);
            }
        }
    }

    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let placeholders = rows
        .iter()
        .filter(|(_, _, state)| *state == PlaceholderState::Dehydrated)
        .count() as u64;
    let hydrated = rows.len() as u64 - placeholders;
    let apparent = rows
        .iter()
        .map(|(_, occupancy, _)| occupancy.apparent_size)
        .sum::<u64>();
    let allocated = rows
        .iter()
        .map(|(_, occupancy, _)| occupancy.allocated)
        .sum::<u64>();
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
            storage_display_path(p, &root).display()
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

fn storage_row(
    path: &Path,
) -> Option<(
    PathBuf,
    gonedrive_core::hydrate::Occupancy,
    gonedrive_core::hydrate::PlaceholderState,
)> {
    let occupancy = gonedrive_core::hydrate::Occupancy::read(path).ok()?;
    Some((
        path.to_path_buf(),
        occupancy,
        gonedrive_core::hydrate::Placeholder::state(path),
    ))
}

fn storage_display_path<'a>(path: &'a Path, root: &'a Path) -> &'a Path {
    match path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative,
        _ => path,
    }
}

/// Hydrate a placeholder explicitly.
///
/// This is the same call the privileged helper makes when the kernel reports
/// that something is about to read the file, so it exercises the whole
/// hydration path without needing the helper to be running.
pub(crate) async fn fetch(path: PathBuf) -> Result<()> {
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
pub(crate) fn indexer(include: bool) -> Result<()> {
    let mut config = gonedrive_core::config::Config::load()?;
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
    let marker = root.join(crate::indexer::MARKER_NAME);

    if include {
        if crate::indexer::remove_marker(&root)? {
            println!("Removed {}.", marker.display());
        } else {
            println!("{} was not excluded.", root.display());
        }
        config.exclude_desktop_indexer = false;
        config.save()?;
        println!("The file indexer will walk {} again.", root.display());
        println!();
        println!("Reads from indexers are still refused by the helper, so this");
        println!("will not download your drive — but it will cost a lot of");
        println!("pointless work on both sides.");
    } else {
        crate::indexer::ensure_marker(&root)?;
        config.exclude_desktop_indexer = true;
        config.save()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_file_is_included_in_the_storage_report() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("one.bin");
        std::fs::write(&file, b"content").unwrap();

        let row = storage_row(&file).expect("a file is a valid storage target");
        assert_eq!(row.0, file);
        assert_eq!(row.1.apparent_size, 7);
        assert_eq!(storage_display_path(&row.0, &row.0), row.0);
    }
}
