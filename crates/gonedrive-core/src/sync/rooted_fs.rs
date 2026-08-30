//! Race-resistant filesystem operations confined to one sync root.
//!
//! A lexical `starts_with(root)` check does not stop an intermediate symlink
//! from redirecting a later path-based operation. This module resolves every
//! directory from an already-open root descriptor and refuses symlinks at all
//! levels. Mutation methods operate on resolved parent descriptors, so a
//! concurrent rename of an ancestor cannot move an operation outside the root.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

const RESOLVE_FLAGS: u64 =
    libc::RESOLVE_BENEATH | libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_SYMLINKS;

pub(crate) struct RootedFs {
    root: PathBuf,
    root_fd: File,
}

impl RootedFs {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        let root_fd = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .map_err(|error| path_error(root, error))?;
        Ok(Self {
            root: root.to_path_buf(),
            root_fd,
        })
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, path: &Path) -> bool {
        self.relative(path).is_ok()
    }

    pub(crate) fn open_read(&self, path: &Path) -> Result<File> {
        // The sync root is a valid read-only directory target when folder
        // emblems are recomputed up to the top. Reuse the already verified,
        // O_NOFOLLOW root descriptor; mutation helpers still reject an empty
        // relative path through `parent()` below.
        if path == self.root {
            return self
                .root_fd
                .try_clone()
                .map_err(|error| path_error(path, error));
        }
        self.open_file(path, libc::O_RDONLY, 0)
    }

    pub(crate) fn open_write(&self, path: &Path) -> Result<File> {
        self.open_file(path, libc::O_RDWR, 0)
    }

    pub(crate) fn open_path(&self, path: &Path) -> Result<File> {
        self.open_file(path, libc::O_PATH, 0)
    }

    pub(crate) fn create_file(&self, path: &Path, truncate: bool) -> Result<File> {
        let (parent, name) = self.parent(path, true)?;
        let mut flags = libc::O_RDWR | libc::O_CREAT;
        if truncate {
            flags |= libc::O_TRUNC;
        }
        openat2(parent.as_raw_fd(), &name, flags, 0o600).map_err(|error| path_error(path, error))
    }

    pub(crate) fn create_new_file(&self, path: &Path) -> Result<File> {
        let (parent, name) = self.parent(path, true)?;
        openat2(
            parent.as_raw_fd(),
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(|error| path_error(path, error))
    }

    pub(crate) fn open_append(&self, path: &Path) -> Result<File> {
        self.open_file(path, libc::O_WRONLY | libc::O_APPEND, 0)
    }

    pub(crate) fn metadata(&self, path: &Path) -> Result<Option<Metadata>> {
        match self.open_path(path) {
            Ok(file) => Ok(Some(file.metadata()?)),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn set_times(&self, path: &Path, times: std::fs::FileTimes) -> Result<()> {
        let file = self.open_write(path).or_else(|_| self.open_read(path))?;
        file.set_times(times)?;
        Ok(())
    }

    pub(crate) fn ensure_dir(&self, path: &Path) -> Result<()> {
        // A root-level file has the sync root itself as its parent. It already
        // exists by construction, so this is a successful no-op rather than
        // an attempt to address a nameless child.
        if path == self.root {
            return Ok(());
        }
        let relative = self.relative(path)?;
        let mut current = self.root_fd.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(outside_error(path, &self.root));
            };
            current = open_or_create_dir(current.as_raw_fd(), name)
                .map_err(|error| path_error(path, error))?;
        }
        Ok(())
    }

    pub(crate) fn remove_file(&self, path: &Path) -> Result<()> {
        self.unlink(path, 0)
    }

    pub(crate) fn remove_dir(&self, path: &Path) -> Result<()> {
        self.unlink(path, libc::AT_REMOVEDIR)
    }

    fn unlink(&self, path: &Path, flags: i32) -> Result<()> {
        let (parent, name) = self.parent(path, false)?;
        // SAFETY: `parent` and `name` remain valid for the syscall.
        let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if rc == 0 {
            Ok(())
        } else {
            Err(path_error(path, std::io::Error::last_os_error()))
        }
    }

    pub(crate) fn rename(&self, old: &Path, new: &Path) -> Result<()> {
        let (old_parent, old_name) = self.parent(old, false)?;
        let (new_parent, new_name) = self.parent(new, true)?;
        // SAFETY: both directory descriptors and names are valid for the call.
        let rc = unsafe {
            libc::renameat(
                old_parent.as_raw_fd(),
                old_name.as_ptr(),
                new_parent.as_raw_fd(),
                new_name.as_ptr(),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(path_error(new, std::io::Error::last_os_error()))
        }
    }

    /// Rename `old` to `new` only when `new` does not exist at the instant of
    /// the syscall. Unlike an existence check followed by `renameat`, this
    /// cannot overwrite a file created by another process in between.
    pub(crate) fn rename_noreplace(&self, old: &Path, new: &Path) -> Result<()> {
        let (old_parent, old_name) = self.parent(old, false)?;
        let (new_parent, new_name) = self.parent(new, true)?;
        // SAFETY: both directory descriptors and names remain valid for the
        // syscall, and RENAME_NOREPLACE has no extra pointer arguments.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                old_parent.as_raw_fd(),
                old_name.as_ptr(),
                new_parent.as_raw_fd(),
                new_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            ) as i32
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(path_error(new, std::io::Error::last_os_error()))
        }
    }

    fn open_file(&self, path: &Path, flags: i32, mode: u32) -> Result<File> {
        let (parent, name) = self.parent(path, false)?;
        openat2(parent.as_raw_fd(), &name, flags, mode).map_err(|error| path_error(path, error))
    }

    fn parent(&self, path: &Path, create: bool) -> Result<(File, CString)> {
        let relative = self.relative(path)?;
        let name = relative
            .file_name()
            .ok_or_else(|| outside_error(path, &self.root))?;
        let mut current = self.root_fd.try_clone()?;
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let Component::Normal(component) = component else {
                    return Err(outside_error(path, &self.root));
                };
                current = if create {
                    open_or_create_dir(current.as_raw_fd(), component)
                } else {
                    open_dir(current.as_raw_fd(), component)
                }
                .map_err(|error| path_error(path, error))?;
            }
        }
        Ok((current, cstring(name)?))
    }

    fn relative<'a>(&self, path: &'a Path) -> Result<&'a Path> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| outside_error(path, &self.root))?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(outside_error(path, &self.root));
        }
        Ok(relative)
    }
}

fn open_or_create_dir(parent: RawFd, name: &OsStr) -> std::io::Result<File> {
    let name = cstring_io(name)?;
    // SAFETY: the descriptor is borrowed and the C string is valid.
    let rc = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    openat2(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
}

fn open_dir(parent: RawFd, name: &OsStr) -> std::io::Result<File> {
    openat2(
        parent,
        &cstring_io(name)?,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )
}

fn openat2(parent: RawFd, name: &CString, flags: i32, mode: u32) -> std::io::Result<File> {
    // `open_how` is non-exhaustive in libc, so initialise it before assigning
    // the three kernel-defined fields.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64;
    how.mode = mode as u64;
    how.resolve = RESOLVE_FLAGS;
    // SAFETY: `name` is NUL-terminated and `how` has the exact ABI size passed.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent,
            name.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        ) as i32
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: openat2 returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn cstring(name: &OsStr) -> Result<CString> {
    cstring_io(name).map_err(Error::Io)
}

fn cstring_io(name: &OsStr) -> std::io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains a NUL byte",
        )
    })
}

fn outside_error(path: &Path, root: &Path) -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "refusing filesystem access outside sync root: {} (root {})",
            path.display(),
            root.display()
        ),
    ))
}

fn path_error(path: &Path, error: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(
        error.kind(),
        format!("{}: {error}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_parent_components_and_paths_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RootedFs::open(dir.path()).unwrap();
        assert!(!fs.contains(&dir.path().join("a/../../outside")));
        assert!(!fs.contains(Path::new("/etc/passwd")));
    }

    #[test]
    fn creates_and_renames_through_resolved_directory_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RootedFs::open(dir.path()).unwrap();
        let old = dir.path().join("a/b/old");
        let new = dir.path().join("a/c/new");
        std::io::Write::write_all(&mut fs.create_file(&old, true).unwrap(), b"ok").unwrap();
        fs.rename(&old, &new).unwrap();
        assert_eq!(std::fs::read(new).unwrap(), b"ok");
    }

    #[test]
    fn no_replace_rename_preserves_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RootedFs::open(dir.path()).unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::write(&old, b"source").unwrap();
        std::fs::write(&new, b"destination").unwrap();

        let error = fs.rename_noreplace(&old, &new).unwrap_err();
        assert!(matches!(
            error,
            Error::Io(ref error) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read(old).unwrap(), b"source");
        assert_eq!(std::fs::read(new).unwrap(), b"destination");
    }

    #[test]
    fn the_sync_root_itself_is_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RootedFs::open(dir.path()).unwrap();

        fs.ensure_dir(dir.path()).unwrap();
        std::io::Write::write_all(
            &mut fs.create_file(&dir.path().join("root-file"), true).unwrap(),
            b"ok",
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join("root-file")).unwrap(), b"ok");
    }

    #[test]
    fn an_intermediate_symlink_cannot_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let fs = RootedFs::open(root.path()).unwrap();
        let escaped = root.path().join("link/file");
        assert!(fs.create_file(&escaped, true).is_err());
        assert!(!outside.path().join("file").exists());
    }
}
