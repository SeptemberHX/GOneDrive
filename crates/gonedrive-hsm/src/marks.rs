//! Descriptor-bound path authorization and fanotify mark ownership.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::fanotify::descriptor_path;
use crate::file_handle::{object_is_gone, ReopenableFile};
use crate::shown;
use crate::{FileIdentity, MarkEntry, MarkKind, Shared, SyncRoot};

/// Both lookup directions live under one lock so registration, replacement,
/// event validation, and explicit unwatch always observe one coherent set.
/// The reverse index avoids scanning every existing mark for each new path,
/// which made a cold registration of a large drive quadratic.
#[derive(Default)]
pub(super) struct MarkRegistry {
    by_identity: HashMap<FileIdentity, MarkEntry>,
    by_path: HashMap<u32, HashMap<Arc<Path>, Vec<FileIdentity>>>,
}

impl MarkRegistry {
    pub(super) fn get(&self, identity: &FileIdentity) -> Option<&MarkEntry> {
        self.by_identity.get(identity)
    }

    /// Resolve an unmarked child-directory event through its exact populated
    /// parent mark. FAN_EVENT_ON_CHILD reports the child's descriptor, not the
    /// marked parent's identity, so this reverse lookup is the authorization
    /// boundary for non-permission directory notifications.
    pub(super) fn population_parent(&self, uid: u32, child: &Path) -> Option<Arc<Path>> {
        let parent = child.parent()?;
        self.identities_for_path(uid, parent)
            .into_iter()
            .filter_map(|identity| self.by_identity.get(&identity))
            .find(|entry| entry.kind == MarkKind::PopulationDirectory)
            .map(|entry| entry.path.clone())
    }

    fn insert(&mut self, identity: FileIdentity, entry: MarkEntry) {
        if let Some(previous) = self.by_identity.remove(&identity) {
            self.remove_path_identity(previous.uid, &previous.path, identity);
        }
        self.by_path
            .entry(entry.uid)
            .or_default()
            .entry(entry.path.clone())
            .or_default()
            .push(identity);
        self.by_identity.insert(identity, entry);
    }

    fn remove(&mut self, identity: &FileIdentity) -> Option<MarkEntry> {
        let entry = self.by_identity.remove(identity)?;
        self.remove_path_identity(entry.uid, &entry.path, *identity);
        Some(entry)
    }

    /// Refresh the pathname attached to an inode mark that already exists.
    /// fanotify marks follow the inode across a rename; both indexes must make
    /// the same transition so a later unwatch cannot leave the moved mark
    /// behind under its old name.
    fn refresh(
        &mut self,
        identity: FileIdentity,
        uid: u32,
        path: &Path,
        kind: MarkKind,
        sweep: Option<u64>,
    ) -> bool {
        let Some(mut entry) = self.remove(&identity) else {
            return false;
        };
        if entry.kind != kind {
            self.insert(identity, entry);
            return false;
        }
        entry.uid = uid;
        entry.path = Arc::from(path);
        if let Some(sweep) = sweep {
            entry.sweep = sweep;
        }
        self.insert(identity, entry);
        true
    }

    fn remove_path_identity(&mut self, uid: u32, path: &Path, identity: FileIdentity) {
        let Some(paths) = self.by_path.get_mut(&uid) else {
            return;
        };
        let remove_path = paths.get_mut(path).is_some_and(|identities| {
            identities.retain(|candidate| *candidate != identity);
            identities.is_empty()
        });
        if remove_path {
            paths.remove(path);
        }
        if paths.is_empty() {
            self.by_path.remove(&uid);
        }
    }

    /// Whether this uid still has anything armed.
    ///
    /// A uid with no marks has nothing the helper needs to keep serving, so
    /// its declared roots — one open directory descriptor each — can go when
    /// its session does. A reconnecting daemon re-declares before its first
    /// watch, which is the same path a first connection takes.
    pub(super) fn has_marks_for(&self, uid: u32) -> bool {
        self.by_path.contains_key(&uid)
    }

    fn identities_for_path(&self, uid: u32, path: &Path) -> Vec<FileIdentity> {
        self.by_path
            .get(&uid)
            .and_then(|paths| paths.get(path))
            .cloned()
            .unwrap_or_default()
    }

    fn stale_identities(&self, uid: u32, sweep: u64) -> Vec<FileIdentity> {
        self.by_identity
            .iter()
            .filter(|(_, entry)| entry.uid == uid && entry.sweep != sweep)
            .map(|(identity, _)| *identity)
            .collect()
    }
}

#[derive(Debug)]
pub(super) struct AuthorisedFile {
    pub(super) file: std::fs::File,
    identity: FileIdentity,
}

fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", shown(path));
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        bail!("{} contains a parent-directory component", shown(path));
    }
    Ok(())
}

/// Resolution rules for everything opened beneath a root: no symlink anywhere
/// in the relative path, no magic link, no escape above the root. Crossing
/// into a sub-mount is allowed — a separate disk mounted inside the sync
/// folder is a legitimate layout — but every opened file is then checked
/// against [`REFUSED_FILESYSTEMS`], so a FUSE tree bind-mounted beneath a
/// local root still gets nothing.
const BENEATH_ROOT: u64 =
    libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;

/// Filesystem magic numbers on which the helper refuses to operate.
///
/// The lever these close is a local user mounting something whose server they
/// control and then pointing the helper at it: every read then blocks for as
/// long as that server likes, holding a root-owned worker against everyone
/// else. FUSE is the obvious one — any user can mount it — and the network
/// filesystems are the same shape with a remote peer.
///
/// overlayfs is deliberately **not** here, though an earlier revision listed
/// it. Mounting an overlay needs privileges or a user namespace, and anyone
/// with those can mount FUSE instead, which this list already refuses — so it
/// bought almost nothing. What it cost was real: a container's `/tmp` and
/// root filesystem are overlays, so the helper refused to serve a sync root in
/// any container, and the test suite could not run in one either.
const REFUSED_FILESYSTEMS: &[(u64, &str)] = &[
    (0x6573_5546, "fuse"),
    // An autofs mount point blocks while it mounts on demand, and only root
    // configures one, so refusing it costs an ordinary user nothing.
    (0x0187, "autofs"),
    (0x6969, "nfs"),
    (0x517B, "smb"),
    (0xFF53_4D42, "cifs"),
    (0xFE53_4D42, "smb2"),
    (0x0102_1997, "9p"),
    (0x5346_414F, "afs"),
    (0x00C3_6400, "ceph"),
    (0x7375_7245, "coda"),
    (0x0116_1970, "gfs2"),
    (0x7461_636F, "ocfs2"),
];

fn openat2(dirfd: i32, path: &Path, flags: i32, resolve: u64) -> std::io::Result<File> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
    })?;
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = flags as u64;
    how.resolve = resolve;
    // SAFETY: `c_path` is NUL terminated and `how` has the ABI size supplied.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dirfd,
            c_path.as_ptr(),
            &how,
            std::mem::size_of::<libc::open_how>(),
        ) as i32
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor returned by open.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Authorise a directory as a sync root: absolute, a real directory owned by
/// the peer, on a local filesystem.
///
/// Symlinks in the root's own path are followed on purpose: `/home` is a
/// symlink on some distributions and users routinely point `~/OneDrive` at
/// another disk. What matters for confinement is the descriptor this returns
/// — every later path resolves relative to it and may not follow links — not
/// how the root itself was named.
pub(super) fn open_root(path: &Path, uid: u32) -> Result<SyncRoot> {
    validate_path(path)?;
    let dir = openat2(
        libc::AT_FDCWD,
        path,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        libc::RESOLVE_NO_MAGICLINKS,
    )
    .with_context(|| format!("cannot open {}", shown(path)))?;
    let meta = dir
        .metadata()
        .with_context(|| format!("cannot stat {}", shown(path)))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", shown(path));
    }
    if meta.uid() != uid {
        bail!(
            "{} belongs to uid {}, not to the connecting uid {uid}",
            shown(path),
            meta.uid()
        );
    }
    if let Some(name) = refused_filesystem(&dir)? {
        bail!(
            "{} is on a {name} filesystem, which the helper does not serve",
            shown(path)
        );
    }
    Ok(SyncRoot {
        path: Arc::from(path),
        dir,
    })
}

/// Whether this filesystem magic is one the helper refuses.
#[cfg(test)]
pub(super) fn refused_magic(magic: u64) -> bool {
    REFUSED_FILESYSTEMS.iter().any(|(known, _)| *known == magic)
}

fn refused_filesystem(dir: &File) -> Result<Option<&'static str>> {
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid out-pointer for the duration of the call.
    if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error()).context("fstatfs failed");
    }
    #[allow(clippy::unnecessary_cast)]
    let magic = stat.f_type as u64;
    Ok(REFUSED_FILESYSTEMS
        .iter()
        .find(|(known, _)| *known == magic)
        .map(|(_, name)| *name))
}

/// Replace the peer's declared roots. Roots outlive the session the way marks
/// do: a reconnecting daemon re-declares them before its first watch, and a
/// hydration reply for a mark installed under the old set must still resolve.
pub(super) fn register_roots(shared: &Arc<Shared>, paths: &[String], uid: u32) -> Result<usize> {
    let mut roots = Vec::with_capacity(paths.len());
    for path in paths {
        roots.push(open_root(Path::new(path), uid)?);
    }
    let count = roots.len();
    shared.roots.lock().unwrap().insert(uid, Arc::new(roots));
    Ok(count)
}

/// Open and authorise the exact inode in one operation.
///
/// The path must lie beneath one of the daemon's declared roots and is
/// resolved relative to that root's descriptor under [`BENEATH_ROOT`], so
/// neither a symlink nor a lexical trick can point the helper outside the
/// tree. Every later operation uses the returned descriptor.
pub(super) fn open_authorised(
    roots: &[SyncRoot],
    path: &Path,
    uid: u32,
    access: i32,
) -> Result<AuthorisedFile> {
    open_authorised_kind(roots, path, uid, access, MarkKind::Placeholder)
}

fn open_authorised_kind(
    roots: &[SyncRoot],
    path: &Path,
    uid: u32,
    access: i32,
    kind: MarkKind,
) -> Result<AuthorisedFile> {
    validate_path(path)?;
    // `strip_prefix` matches whole components, so `/home/u/OneDrive-evil` is
    // not beneath `/home/u/OneDrive`.
    let Some((root, relative)) = roots.iter().find_map(|root| {
        path.strip_prefix(&*root.path)
            .ok()
            .map(|relative| (root, relative))
    }) else {
        bail!("{} is outside every registered sync root", shown(path));
    };
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    let mut flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // openat2 accepts only a narrow flag set with O_PATH; these are both
    // unnecessary there and rejected with EINVAL on current kernels. Ordinary
    // opens keep O_NONBLOCK so a FIFO or device can never stall this
    // privileged path before fstat rejects it, and O_NOCTTY so a terminal
    // device the peer owns cannot become this process's controlling terminal
    // — the type check comes after the open, and a hang-up on an acquired
    // terminal would take the helper down with it.
    if access & libc::O_PATH == 0 {
        flags |= libc::O_NONBLOCK | libc::O_NOCTTY;
    }
    let file = openat2(root.dir.as_raw_fd(), relative, flags, BENEATH_ROOT)
        .with_context(|| format!("cannot open {}", shown(path)))?;
    let meta = file
        .metadata()
        .with_context(|| format!("cannot stat {}", shown(path)))?;
    let expected_type = match kind {
        MarkKind::Placeholder => meta.file_type().is_file(),
        MarkKind::PopulationDirectory => meta.file_type().is_dir(),
    };
    if !expected_type {
        let expected = match kind {
            MarkKind::Placeholder => "a regular file",
            MarkKind::PopulationDirectory => "a directory",
        };
        bail!("{} is not {expected}", shown(path));
    }
    if meta.uid() != uid {
        bail!(
            "{} belongs to uid {}, not to the connecting uid {uid}",
            shown(path),
            meta.uid()
        );
    }
    // The root was checked at registration, but a sub-mount beneath it may be
    // anything; a FUSE file here could park a root-owned worker forever.
    if let Some(name) = refused_filesystem(&file)? {
        bail!(
            "{} is on a {name} filesystem, which the helper does not serve",
            shown(path)
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
    install_typed_mark(shared, path, uid, MarkKind::Placeholder)
}

pub(super) fn install_directory_mark(
    shared: &Arc<Shared>,
    path: &Path,
    uid: u32,
) -> Result<FileIdentity> {
    install_typed_mark(shared, path, uid, MarkKind::PopulationDirectory)
}

fn install_typed_mark(
    shared: &Arc<Shared>,
    path: &Path,
    uid: u32,
    kind: MarkKind,
) -> Result<FileIdentity> {
    let roots = shared.roots_for(uid);
    let target = open_authorised_kind(&roots, path, uid, libc::O_PATH, kind)?;
    let sweep = shared.sweeps.lock().unwrap().get(&uid).copied();
    let existing_stale = {
        let mut marks = shared.marks.lock().unwrap();
        if marks.refresh(target.identity, uid, path, kind, sweep) {
            // Inode marks survive a rename. Refresh the path without another
            // syscall so watcher reconciliation naturally follows the inode.
            let stale = marks
                .identities_for_path(uid, path)
                .into_iter()
                .filter(|identity| *identity != target.identity)
                .filter_map(|identity| marks.remove(&identity).map(|entry| (identity, entry)))
                .collect();
            Some(stale)
        } else {
            None
        }
    };
    if let Some(stale) = existing_stale {
        retire_stale_marks(shared, stale);
        if kind == MarkKind::Placeholder {
            restore_locked_placeholder(&target.file)
                .with_context(|| format!("could not unlock protected {}", shown(path)))?;
        }
        return Ok(target.identity);
    }

    // Open a safety-locked placeholder before installing its permission mark.
    // Reopening it afterwards would itself block on the new mark before the
    // registry entry exists. The descriptor remains bound to this exact inode.
    let locked = if kind == MarkKind::Placeholder {
        LockedPlaceholder::prepare(&target.file)
            .with_context(|| format!("could not inspect safety lock on {}", shown(path)))?
    } else {
        None
    };
    match kind {
        MarkKind::Placeholder => shared.fan.watch_fd(target.file.as_raw_fd()),
        MarkKind::PopulationDirectory => shared.fan.watch_directory_fd(target.file.as_raw_fd()),
    }
    .with_context(|| format!("could not watch {}", shown(path)))?;
    if let Some(locked) = locked {
        if let Err(error) = locked.restore() {
            let _ = match kind {
                MarkKind::Placeholder => shared.fan.unwatch_fd(target.file.as_raw_fd()),
                MarkKind::PopulationDirectory => {
                    shared.fan.unwatch_directory_fd(target.file.as_raw_fd())
                }
            };
            return Err(error)
                .with_context(|| format!("could not unlock protected {}", shown(path)));
        }
    }
    let (file, fallback_error) = ReopenableFile::capture(target.file, &shared.mounts);
    if let Some(error) = fallback_error {
        let fallbacks = shared.descriptor_fallbacks.fetch_add(1, Ordering::Relaxed) + 1;
        if fallbacks == 1 {
            tracing::warn!(
                path = %shown(path),
                %error,
                "filesystem cannot reopen persistent file handles; retaining one descriptor per placeholder on this filesystem"
            );
        }
    }

    let stale = {
        let mut marks = shared.marks.lock().unwrap();
        // Another connection may have installed the same inode while the
        // persistent handle was captured. Reuse its entry and drop ours.
        if !marks.refresh(target.identity, uid, path, kind, sweep) {
            marks.insert(
                target.identity,
                MarkEntry {
                    uid,
                    path: Arc::from(path),
                    file,
                    kind,
                    sweep: sweep.unwrap_or(0),
                },
            );
        }
        marks
            .identities_for_path(uid, path)
            .into_iter()
            .filter(|identity| *identity != target.identity)
            .filter_map(|identity| marks.remove(&identity).map(|entry| (identity, entry)))
            .collect::<Vec<_>>()
    };
    // A concurrent registration may have won after fanotify_mark. Adding the
    // same inode mark is idempotent; dropping our duplicate handle is enough.
    retire_stale_marks(shared, stale);
    tracing::debug!(path = %shown(path), uid, ?kind, "watching");
    Ok(target.identity)
}

const XATTR_SAFETY_MODE: &[u8] = b"user.gonedrive.safety_mode\0";

struct LockedPlaceholder {
    path_file: File,
    mode: libc::mode_t,
}

impl LockedPlaceholder {
    fn prepare(path_file: &File) -> std::io::Result<Option<Self>> {
        if path_file.metadata()?.mode() & 0o7777 != 0 {
            return Ok(None);
        }
        let target = descriptor_path(path_file.as_raw_fd());
        let mut raw = [0u8; std::mem::size_of::<u32>()];
        // Read through the helper-owned O_PATH descriptor's procfs magic link.
        // Reopening it O_RDONLY can itself block on an existing fanotify mark,
        // deadlocking registration if the event loop is the component being
        // recovered. The live descriptor still binds every operation below to
        // this exact authorised inode.
        let size = unsafe {
            libc::getxattr(
                target.as_ptr(),
                XATTR_SAFETY_MODE.as_ptr().cast(),
                raw.as_mut_ptr().cast(),
                raw.len(),
            )
        };
        if size < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENODATA) {
                return Ok(None);
            }
            return Err(error);
        }
        if size as usize != raw.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid saved placeholder mode",
            ));
        }
        let mode = u32::from_le_bytes(raw) as libc::mode_t;
        if mode & !0o7777 != 0 || mode == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "saved placeholder mode is outside the permission bits",
            ));
        }
        // Never hand back setuid, setgid or the sticky bit, whatever the xattr
        // says. A root process restoring them is not an escalation — the file
        // belongs to the user who asked, and they could chmod it themselves —
        // but a privileged helper that writes those bits on a path it did not
        // choose has no reason to exist, and this keeps the audit answer short.
        let mode = mode & 0o0777;
        if mode == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "saved placeholder mode carried nothing but special bits",
            ));
        }
        Ok(Some(Self {
            path_file: path_file.try_clone()?,
            mode,
        }))
    }

    fn restore(self) -> std::io::Result<()> {
        let target = descriptor_path(self.path_file.as_raw_fd());
        // chmod follows the helper's own immutable descriptor link, not the
        // user-supplied path. The saved mode was range-checked above.
        if unsafe { libc::chmod(target.as_ptr(), self.mode) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The file is already accessible and protected by fanotify. A stale
        // private xattr is harmless, so do not turn successful recovery into a
        // failed registration solely because its cleanup was refused.
        let rc = unsafe { libc::removexattr(target.as_ptr(), XATTR_SAFETY_MODE.as_ptr().cast()) };
        if rc != 0 {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "could not remove a restored placeholder safety marker"
            );
        }
        Ok(())
    }
}

fn restore_locked_placeholder(path_file: &File) -> std::io::Result<()> {
    if let Some(locked) = LockedPlaceholder::prepare(path_file)? {
        locked.restore()?;
    }
    Ok(())
}

fn retire_stale_marks(shared: &Arc<Shared>, stale: Vec<(FileIdentity, MarkEntry)>) {
    for (identity, entry) in stale {
        if let Err(error) = unwatch_entry(shared, &entry) {
            tracing::warn!(path = %shown(&entry.path), %error, "could not remove stale mark");
            shared.marks.lock().unwrap().insert(identity, entry);
        }
    }
}

pub(super) fn remove_marks_for_path(shared: &Arc<Shared>, path: &Path, uid: u32) -> Result<()> {
    validate_path(path)?;
    let identities = shared.marks.lock().unwrap().identities_for_path(uid, path);
    let mut failures = Vec::new();
    for identity in identities {
        match remove_mark_by_identity(shared, identity) {
            Some(Ok(())) => {}
            Some(Err(error)) => failures.push(error),
            None => {}
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!(
            "could not unwatch {}: {}",
            shown(path),
            failures
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

/// Release a departed uid's sync roots, if nothing of its is still armed.
///
/// Roots deliberately outlive a session so a restarting daemon can resume, but
/// nothing ever removed them: every uid that had ever connected held a
/// directory descriptor for the rest of the helper's life. Bounded by the
/// number of local users, and still worth not leaking.
pub(super) fn release_roots_if_idle(shared: &Arc<Shared>, uid: u32) {
    let armed = shared
        .marks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .has_marks_for(uid);
    if armed {
        return;
    }
    let dropped = shared
        .roots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&uid)
        .is_some();
    if dropped {
        tracing::debug!(uid, "released the sync roots of a uid with nothing armed");
    }
}

pub(super) fn begin_mark_sweep(shared: &Arc<Shared>, uid: u32) {
    let sweep = shared.next_sweep.fetch_add(1, Ordering::Relaxed);
    shared.sweeps.lock().unwrap().insert(uid, sweep);
}

pub(super) fn cancel_mark_sweep(shared: &Arc<Shared>, uid: u32) {
    shared.sweeps.lock().unwrap().remove(&uid);
}

pub(super) fn commit_mark_sweep(shared: &Arc<Shared>, uid: u32) -> Result<usize> {
    let sweep = shared
        .sweeps
        .lock()
        .unwrap()
        .remove(&uid)
        .ok_or_else(|| anyhow::anyhow!("no mark sweep is active for uid {uid}"))?;
    let stale = shared.marks.lock().unwrap().stale_identities(uid, sweep);
    let mut removed = 0usize;
    let mut failures = Vec::new();
    for identity in stale {
        match remove_mark_by_identity(shared, identity) {
            Some(Ok(())) => removed += 1,
            Some(Err(error)) => failures.push(error),
            None => {}
        }
    }
    if failures.is_empty() {
        Ok(removed)
    } else {
        bail!(
            "could not retire {} stale mark(s): {}",
            failures.len(),
            failures
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        )
    }
}

pub(super) fn remove_mark_by_event_fd(
    shared: &Arc<Shared>,
    identity: FileIdentity,
    fd: std::os::fd::RawFd,
) -> std::io::Result<()> {
    match shared.fan.unwatch_fd(fd) {
        Ok(()) => {
            shared.marks.lock().unwrap().remove(&identity);
            Ok(())
        }
        Err(error) if object_is_gone(&error) => {
            shared.marks.lock().unwrap().remove(&identity);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn unwatch_entry(shared: &Arc<Shared>, entry: &MarkEntry) -> std::io::Result<()> {
    let result = entry.file.with_fd(|fd| match entry.kind {
        MarkKind::Placeholder => shared.fan.unwatch_fd(fd),
        MarkKind::PopulationDirectory => shared.fan.unwatch_directory_fd(fd),
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) if object_is_gone(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_mark_by_identity(
    shared: &Arc<Shared>,
    identity: FileIdentity,
) -> Option<std::io::Result<()>> {
    let mut marks = shared.marks.lock().unwrap();
    let result = marks
        .get(&identity)
        .map(|entry| unwatch_entry(shared, entry));
    if result.as_ref().is_some_and(|result| result.is_ok()) {
        marks.remove(&identity);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uid: u32, path: &str, sweep: u64) -> MarkEntry {
        entry_with_kind(uid, path, sweep, MarkKind::Placeholder)
    }

    fn entry_with_kind(uid: u32, path: &str, sweep: u64, kind: MarkKind) -> MarkEntry {
        MarkEntry {
            uid,
            path: Arc::from(Path::new(path)),
            file: ReopenableFile::Descriptor(tempfile::tempfile().unwrap()),
            kind,
            sweep,
        }
    }

    #[test]
    fn a_committed_sweep_retires_only_unseen_marks_for_its_uid() {
        let current = FileIdentity { dev: 1, ino: 1 };
        let stale = FileIdentity { dev: 1, ino: 2 };
        let other_user = FileIdentity { dev: 1, ino: 3 };
        let mut marks = MarkRegistry::default();
        marks.insert(current, entry(1000, "/tmp/current", 7));
        marks.insert(stale, entry(1000, "/tmp/stale", 6));
        marks.insert(other_user, entry(1001, "/tmp/other", 0));

        assert_eq!(marks.stale_identities(1000, 7), vec![stale]);
    }

    #[test]
    fn reverse_path_index_tracks_replacements_and_removals() {
        let first = FileIdentity { dev: 1, ino: 1 };
        let replacement = FileIdentity { dev: 1, ino: 2 };
        let path = Path::new("/tmp/placeholder");
        let mut marks = MarkRegistry::default();
        marks.insert(first, entry(1000, "/tmp/placeholder", 1));
        marks.insert(replacement, entry(1000, "/tmp/placeholder", 1));

        let mut identities = marks.identities_for_path(1000, path);
        identities.sort_by_key(|identity| identity.ino);
        assert_eq!(identities, vec![first, replacement]);

        marks.remove(&first);
        assert_eq!(marks.identities_for_path(1000, path), vec![replacement]);
        marks.remove(&replacement);
        assert!(marks.identities_for_path(1000, path).is_empty());
    }

    #[test]
    fn refreshing_a_renamed_inode_moves_the_reverse_index() {
        let identity = FileIdentity { dev: 1, ino: 7 };
        let old = Path::new("/tmp/Todo/2025");
        let new = Path::new("/tmp/Done/2025");
        let mut marks = MarkRegistry::default();
        marks.insert(identity, entry(1000, old.to_str().unwrap(), 3));

        assert!(marks.refresh(identity, 1000, new, MarkKind::Placeholder, Some(4)));
        assert!(marks.identities_for_path(1000, old).is_empty());
        assert_eq!(marks.identities_for_path(1000, new), vec![identity]);
        let refreshed = marks.get(&identity).unwrap();
        assert_eq!(refreshed.path.as_ref(), new);
        assert_eq!(refreshed.sweep, 4);
    }

    #[test]
    fn a_population_mark_authorises_only_direct_children_for_its_uid() {
        let parent = FileIdentity { dev: 1, ino: 9 };
        let mut marks = MarkRegistry::default();
        marks.insert(
            parent,
            entry_with_kind(1000, "/sync/Populated", 1, MarkKind::PopulationDirectory),
        );

        assert_eq!(
            marks.population_parent(1000, Path::new("/sync/Populated/Partial")),
            Some(Arc::from(Path::new("/sync/Populated")))
        );
        assert!(marks
            .population_parent(1000, Path::new("/sync/Populated/Partial/Grandchild"))
            .is_none());
        assert!(marks
            .population_parent(1001, Path::new("/sync/Populated/Partial"))
            .is_none());
    }

    #[test]
    fn a_safety_locked_inode_is_restored_without_a_read_open() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("placeholder");
        std::fs::write(&path, []).unwrap();
        xattr::set(&path, "user.gonedrive.safety_mode", &0o640u32.to_le_bytes()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();

        let uid = unsafe { libc::geteuid() };
        let roots = vec![open_root(temp.path(), uid).unwrap()];
        let path_file = open_authorised(&roots, &path, uid, libc::O_PATH)
            .unwrap()
            .file;
        // `prepare` reads a mode-000 inode's xattr and intentionally requires
        // the production helper's CAP_DAC_OVERRIDE. Exercise the descriptor-
        // bound restore path here without making the test suite run as root.
        LockedPlaceholder {
            path_file,
            mode: 0o640,
        }
        .restore()
        .unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert_eq!(
            xattr::get(&path, "user.gonedrive.safety_mode").unwrap(),
            None
        );
    }
}
