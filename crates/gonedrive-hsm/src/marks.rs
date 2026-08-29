//! Descriptor-bound path authorization and fanotify mark ownership.

use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::{FileIdentity, MarkEntry, Shared};

#[derive(Debug)]
pub(super) struct AuthorisedFile {
    pub(super) file: std::fs::File,
    identity: FileIdentity,
}

fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("{} contains a parent-directory component", path.display());
    }
    Ok(())
}

/// Open and authorise the exact inode in one operation. No symlink component is
/// followed, and every later operation uses the returned descriptor.
pub(super) fn open_authorised(path: &Path, uid: u32, access: i32) -> Result<AuthorisedFile> {
    validate_path(path)?;
    let path_bytes = path.as_os_str().as_encoded_bytes();
    let c_path = std::ffi::CString::new(path_bytes)
        .map_err(|_| anyhow::anyhow!("{} contains a NUL byte", path.display()))?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    let mut flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // openat2 accepts only a narrow flag set with O_PATH; O_NONBLOCK is both
    // unnecessary there and rejected with EINVAL on current kernels. Ordinary
    // opens keep it so a FIFO or device can never stall this privileged path
    // before fstat rejects it.
    if access & libc::O_PATH == 0 {
        flags |= libc::O_NONBLOCK;
    }
    how.flags = flags as u64;
    how.resolve = libc::RESOLVE_NO_MAGICLINKS | libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: `c_path` is NUL terminated and `how` has the ABI size supplied.
    // O_NONBLOCK prevents special-file opens from hanging; those files are
    // rejected immediately after fstat in any case.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        ) as i32
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot open {}", path.display()));
    }
    // SAFETY: `fd` is a fresh descriptor returned by open.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let meta = file
        .metadata()
        .with_context(|| format!("cannot stat {}", path.display()))?;
    if !meta.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.uid() != uid {
        bail!(
            "{} belongs to uid {}, not to the connecting uid {uid}",
            path.display(),
            meta.uid()
        );
    }
    Ok(AuthorisedFile {
        file,
        identity: FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        },
    })
}

pub(super) fn install_mark(shared: &Arc<Shared>, path: &Path, uid: u32) -> Result<FileIdentity> {
    let target = open_authorised(path, uid, libc::O_PATH)?;
    {
        let marks = shared.marks.lock().unwrap();
        if marks
            .get(&target.identity)
            .is_some_and(|entry| entry.uid == uid && entry.path == path)
        {
            return Ok(target.identity);
        }
    }

    shared
        .fan
        .watch_fd(target.file.as_raw_fd())
        .with_context(|| format!("could not watch {}", path.display()))?;

    let stale = {
        let mut marks = shared.marks.lock().unwrap();
        let stale_ids: Vec<_> = marks
            .iter()
            .filter(|(identity, entry)| {
                **identity != target.identity && entry.uid == uid && entry.path == path
            })
            .map(|(identity, _)| *identity)
            .collect();
        let stale = stale_ids
            .into_iter()
            .filter_map(|identity| marks.remove(&identity))
            .collect::<Vec<_>>();
        marks.insert(
            target.identity,
            MarkEntry {
                uid,
                path: path.to_path_buf(),
                file: target.file,
            },
        );
        stale
    };
    for entry in stale {
        if let Err(error) = shared.fan.unwatch_fd(entry.file.as_raw_fd()) {
            tracing::debug!(path = %entry.path.display(), %error, "could not remove stale mark");
        }
    }
    tracing::debug!(path = %path.display(), uid, "watching");
    Ok(target.identity)
}

pub(super) fn remove_marks_for_path(shared: &Arc<Shared>, path: &Path, uid: u32) -> Result<()> {
    validate_path(path)?;
    let removed = {
        let mut marks = shared.marks.lock().unwrap();
        let identities: Vec<_> = marks
            .iter()
            .filter(|(_, entry)| entry.uid == uid && entry.path == path)
            .map(|(identity, _)| *identity)
            .collect();
        identities
            .into_iter()
            .filter_map(|identity| marks.remove(&identity))
            .collect::<Vec<_>>()
    };
    for entry in removed {
        shared
            .fan
            .unwatch_fd(entry.file.as_raw_fd())
            .with_context(|| format!("could not unwatch {}", path.display()))?;
    }
    Ok(())
}
