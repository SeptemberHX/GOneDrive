//! Desktop-indexer policy for a files-on-demand sync root.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};

pub(crate) const MARKER_NAME: &str = ".trackerignore";

const MARKER_CONTENT: &[u8] =
    b"# GOnedrive: desktop indexers must not traverse cloud placeholders.\n\
# Change this with: gonedrived indexer --include\n";

/// Apply the configured default before the watcher or first remote population
/// starts. An explicit opt-in does not remove a marker the user may have
/// created for another reason; the `indexer --include` command owns removal.
pub(crate) fn ensure_policy(root: &Path, exclude: bool) -> Result<()> {
    if exclude {
        ensure_marker(root)?;
    }
    Ok(())
}

pub(crate) fn ensure_marker(root: &Path) -> Result<()> {
    let marker = root.join(MARKER_NAME);
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&marker)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&marker)
                .with_context(|| format!("could not inspect {}", marker.display()))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "refusing non-regular desktop-indexer marker {}",
                    marker.display()
                );
            }
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not create {}", marker.display()));
        }
    };
    file.write_all(MARKER_CONTENT)
        .with_context(|| format!("could not write {}", marker.display()))?;
    file.sync_data()
        .with_context(|| format!("could not persist {}", marker.display()))?;
    Ok(())
}

pub(crate) fn remove_marker(root: &Path) -> Result<bool> {
    let marker = root.join(MARKER_NAME);
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("could not remove {}", marker.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn exclusion_is_created_once_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();

        ensure_policy(dir.path(), true).unwrap();
        ensure_policy(dir.path(), true).unwrap();

        let marker = dir.path().join(MARKER_NAME);
        let metadata = std::fs::symlink_metadata(&marker).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&marker).unwrap(), MARKER_CONTENT);
    }

    #[test]
    fn explicit_inclusion_does_not_create_a_marker() {
        let dir = tempfile::tempdir().unwrap();

        ensure_policy(dir.path(), false).unwrap();

        assert!(!dir.path().join(MARKER_NAME).exists());
    }

    #[test]
    fn a_symlink_cannot_redirect_marker_creation() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, b"untouched").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join(MARKER_NAME)).unwrap();

        assert!(ensure_marker(dir.path()).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
    }
}
