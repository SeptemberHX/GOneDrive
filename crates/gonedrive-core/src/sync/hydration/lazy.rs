//! Turning a fully indexed placeholder tree into a lazily populated one.
//!
//! A drive that was synced in classic full mode, or materialised as a whole
//! placeholder tree, carries every one of its items in SQLite and an inode for
//! each of them on disk. Files-on-demand does not need any of that for folders
//! nobody has opened: the lazy design indexes a directory's children only when
//! a file manager asks for them, and lets the delta cursor carry changes for
//! whatever is indexed. Converting means pruning what the user never touched
//! while keeping every byte they did.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::db::{ItemRecord, PinState};
use crate::error::{Error, Result};
use crate::hydrate::{DirectoryPopulation, Placeholder, PlaceholderState};
use crate::names::fold_case;
use crate::sync::rooted_fs::RootedFs;

use super::super::engine::Engine;

/// What a conversion to lazy population did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LazyConversion {
    /// Folders left fully indexed because they, or something below them, hold
    /// content this device must not lose: downloaded files, pins, local edits.
    pub folders_kept: u64,
    /// Folders that became partial: their children are fetched on first open.
    pub folders_pruned: u64,
    /// Never-downloaded placeholder inodes removed from disk.
    pub placeholders_removed: u64,
    /// Item rows dropped from the local index.
    pub rows_removed: u64,
}

/// One folder the scan decided to make partial.
struct Prune {
    folder: ItemRecord,
    /// The folder's own directory, when it exists on disk.
    path: Option<PathBuf>,
    /// Verified dehydrated placeholders directly inside it.
    /// Each placeholder to unlink, with the item id it must still carry at
    /// that moment. The decision pass runs to completion before the first
    /// unlink, and a large tree takes minutes.
    placeholders: Vec<(PathBuf, String)>,
}

impl Engine {
    /// Convert a fully populated tree into a lazily populated one.
    ///
    /// A folder is kept — and stays fully indexed — when anything under it is
    /// worth keeping: a file whose content is on disk, a pinned item, or an
    /// entry the index does not know about (a local edit not yet uploaded, a
    /// foreign file the user dropped in). Everything else becomes a partial
    /// directory: its never-downloaded placeholders are unlinked, its child
    /// rows are dropped, and the next time a file manager opens it the
    /// directory is populated from Graph exactly like a fresh lazy account.
    ///
    /// Decisions are made in a read-only pass before anything is touched, so
    /// a folder's fate never depends on what was already removed. The
    /// mutating pass then works deepest first and one folder at a time; an
    /// interruption leaves a tree that is still consistent and simply less
    /// converted. `progress` gets `(done, total)` folders and returns false to
    /// stop after the current folder.
    ///
    /// The delta cursor is untouched: it still describes exactly what the
    /// remaining rows know, and changes to unindexed items are deferred by the
    /// same machinery a lazy account relies on every day.
    pub fn convert_to_lazy(
        &self,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<LazyConversion> {
        let account = self.account()?;
        let rooted = self.rooted(&account)?;
        let root_id = account.root_item_id.clone().ok_or_else(|| {
            Error::other("the drive has not been enumerated yet; run a sync first")
        })?;

        // Shallowest first, so a parent precedes its children; reversed below
        // for the deepest-first passes. The root is not in this list and is
        // always kept: it is the one directory a lazy account populates up
        // front.
        let folders = self.store.all_folders_shallow_first(&self.account_id)?;
        let parent_of: HashMap<&str, Option<&str>> = folders
            .iter()
            .map(|folder| (folder.id.as_str(), folder.parent_id.as_deref()))
            .collect();

        // A pinned folder means "keep all of this on the device": nothing
        // under it may be pruned, whether or not it has been downloaded yet.
        let mut pinned_tree: HashSet<&str> = HashSet::new();
        for folder in &folders {
            let inherited = folder
                .parent_id
                .as_deref()
                .is_some_and(|parent| pinned_tree.contains(parent));
            if folder.pin_state == PinState::Pinned || inherited {
                pinned_tree.insert(folder.id.as_str());
            }
        }

        let mut kept: HashSet<String> = HashSet::new();
        kept.insert(root_id.clone());
        let keep = |kept: &mut HashSet<String>, id: &str| {
            let mut current = Some(id);
            while let Some(id) = current {
                if !kept.insert(id.to_string()) {
                    break;
                }
                current = parent_of.get(id).copied().flatten();
            }
        };

        // Pass one: decide. Deepest first, so a folder's child directories
        // have their fate settled before the folder itself is examined.
        let mut plan: Vec<Prune> = Vec::new();
        for folder in folders.iter().rev() {
            if kept.contains(&folder.id) {
                continue;
            }
            if pinned_tree.contains(folder.id.as_str()) {
                keep(&mut kept, &folder.id);
                continue;
            }
            match self.scan_folder(&rooted, &account, folder, &kept)? {
                Some(prune) => plan.push(prune),
                None => keep(&mut kept, &folder.id),
            }
        }

        let mut outcome = LazyConversion {
            folders_kept: folders
                .iter()
                .filter(|folder| kept.contains(&folder.id))
                .count() as u64,
            ..Default::default()
        };

        // Pass two: prune. Still deepest first, so a child directory is empty
        // and unlinked before its parent's row deletion cascades over it.
        let total = plan.len() as u64;
        for (done, prune) in plan.into_iter().enumerate() {
            self.prune_folder(&rooted, &account, prune, &mut outcome)?;
            outcome.folders_pruned += 1;
            if !progress(done as u64 + 1, total) {
                break;
            }
        }
        Ok(outcome)
    }

    /// Decide one folder. `Some` describes how to prune it; `None` means it
    /// holds something worth keeping.
    fn scan_folder(
        &self,
        rooted: &RootedFs,
        account: &crate::db::Account,
        folder: &ItemRecord,
        kept: &HashSet<String>,
    ) -> Result<Option<Prune>> {
        let children = self.store.children(&self.account_id, &folder.id)?;
        if children.iter().any(|child| {
            child.pin_state == PinState::Pinned || (child.is_folder && kept.contains(&child.id))
        }) {
            return Ok(None);
        }

        let Some(path) = self.store.local_path(account, &folder.id)? else {
            return Ok(None);
        };
        let dir = match rooted.open_read(&path) {
            Ok(dir) if dir.metadata()?.is_dir() => dir,
            // Occupied by a file: not ours to reason about, leave it alone.
            Ok(_) => return Ok(None),
            // Never created on disk. Nothing to unlink; rows can still go.
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(Prune {
                    folder: folder.clone(),
                    path: None,
                    placeholders: Vec::new(),
                }));
            }
            Err(error) => return Err(error),
        };
        drop(dir);

        let by_name: HashMap<String, &ItemRecord> = children
            .iter()
            .map(|child| (fold_case(&child.name), child))
            .collect();
        let mut placeholders = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Ok(None);
            };
            let file_type = entry.file_type()?;
            match by_name.get(&fold_case(name)) {
                // A child directory the index knows. If it were kept this
                // folder would already be kept (checked above), so it is a
                // pruned, soon-to-be-empty directory.
                Some(child) if child.is_folder && file_type.is_dir() => {}
                Some(child) if !child.is_folder && file_type.is_file() => {
                    let child_path = path.join(name);
                    let file = rooted.open_read(&child_path)?;
                    let online_only = Placeholder::state_file(&file)
                        == PlaceholderState::Dehydrated
                        && Placeholder::item_id_file(&file).as_deref() == Some(child.id.as_str());
                    if !online_only {
                        // Real bytes, or a file that is not the one we
                        // published: content this device owns.
                        return Ok(None);
                    }
                    placeholders.push((child_path, child.id.clone()));
                }
                // Anything the index cannot account for — a file the user
                // created, a symlink, a foreign directory — is kept, and so
                // is the folder around it.
                _ => return Ok(None),
            }
        }
        Ok(Some(Prune {
            folder: folder.clone(),
            path: Some(path),
            placeholders,
        }))
    }

    /// Make one folder partial: marker first, then inodes, then rows.
    ///
    /// The marker goes first so that a crash between steps leaves a directory
    /// the next open repopulates rather than one the index believes complete
    /// while its placeholders are gone.
    fn prune_folder(
        &self,
        rooted: &RootedFs,
        account: &crate::db::Account,
        prune: Prune,
        outcome: &mut LazyConversion,
    ) -> Result<()> {
        let Prune {
            folder,
            path,
            placeholders,
        } = prune;

        self.store
            .set_folder_population(&self.account_id, &folder.id, false)?;
        if let Some(path) = &path {
            let dir = rooted.open_read(path)?;
            if Placeholder::state_file(&dir) == PlaceholderState::Unmanaged {
                Placeholder::mark_dir_file(&dir, PlaceholderState::Dehydrated, &folder.id)?;
            }
            Placeholder::set_directory_population_file(&dir, DirectoryPopulation::Partial)?;
        }

        // Re-check every inode at the moment of the unlink. The scan that
        // chose these ran earlier — minutes earlier on a large tree — and the
        // helper can fill a placeholder, and the user edit it, in between.
        // Unlinking then would destroy bytes that were never uploaded.
        let mut kept_ids: HashSet<String> = HashSet::new();
        for (placeholder, item_id) in placeholders {
            match rooted.open_read(&placeholder) {
                Ok(file) => {
                    let online_only = Placeholder::state_file(&file)
                        == PlaceholderState::Dehydrated
                        && Placeholder::item_id_file(&file).as_deref() == Some(item_id.as_str());
                    if !online_only {
                        tracing::info!(
                            path = %placeholder.display(),
                            "keeping a placeholder that gained content since the scan"
                        );
                        kept_ids.insert(item_id);
                        continue;
                    }
                }
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            match rooted.remove_file(&placeholder) {
                Ok(()) => outcome.placeholders_removed += 1,
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }

        for child in self.store.children(&self.account_id, &folder.id)? {
            // Its bytes are still on disk. Dropping the row would orphan the
            // file: no id, no state, and nothing to sync it back.
            if kept_ids.contains(&child.id) {
                continue;
            }
            if child.is_folder {
                if let Some(child_path) = self.store.local_path(account, &child.id)? {
                    match rooted.remove_dir(&child_path) {
                        Ok(()) => {}
                        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(Error::Io(error))
                            if error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
                        {
                            // Something appeared since the scan. Keep the
                            // row so the local scan can account for the
                            // directory; it stays partial like its parent.
                            self.store
                                .set_folder_population(&self.account_id, &child.id, false)?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            self.store
                .delete_deferred_remote_subtree(&self.account_id, &child.id)?;
            self.store
                .delete_remote_exclusion_subtree(&self.account_id, &child.id)?;
            outcome.rows_removed += self.store.delete_subtree(&child.id)? as u64;
        }
        Ok(())
    }
}
