//! Placeholders: files that are real, but whose content is not here yet.
//!
//! This is the Linux answer to what Windows calls Files On-Demand. A
//! placeholder is a genuine file on a genuine ext4 directory — `ls` reports its
//! real size, a file manager shows it, `find` finds it — but every content block
//! is a hole. The filesystem may still charge a small metadata block for the
//! extended attributes that identify the placeholder.
//!
//! Three operations make the illusion work:
//!
//! * [`Placeholder::create`] — the file exists at full size, holding nothing.
//! * [`Placeholder::hydrate`] — content is written back into the holes.
//! * [`Placeholder::dehydrate`] — content extents are discarded, freeing space.
//!
//! State lives in extended attributes rather than only in the database, because
//! xattrs travel with the file. A lost or rebuilt database can be recovered by
//! reading the disk, and a file manager extension can render the right emblem
//! with a `getxattr` instead of an IPC round trip.

use std::ffi::CString;
use std::io::{Seek, SeekFrom};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use time::OffsetDateTime;

use crate::error::{Error, Result};

/// Marks a file as belonging to GOnedrive and records what it holds.
pub const XATTR_STATE: &str = "user.gonedrive.state";
/// The remote item id, so a file found on disk can be matched to the service.
pub const XATTR_ITEM_ID: &str = "user.gonedrive.item_id";
/// The content version the local blocks correspond to.
pub const XATTR_CTAG: &str = "user.gonedrive.ctag";
/// The user's retention choice, so a file manager can distinguish "downloaded"
/// from "always keep here" without asking the daemon.
pub const XATTR_PIN: &str = "user.gonedrive.pin";
/// Whether a placeholder directory's direct children have been published.
/// This mirrors CFAPI's partial-directory population state: metadata may be in
/// SQLite while child inodes are created only when the directory is browsed.
pub const XATTR_POPULATION: &str = "user.gonedrive.population";

/// quickXorHash of the content we last wrote to this file (download, hydration,
/// or the local content at the moment we uploaded it). It is the reference the
/// dehydrate/rebuild/overwrite guards compare the file's *current* content
/// against, so an in-place edit that preserved size and mtime — invisible to
/// the size+mtime heuristic — is still caught before the bytes are discarded.
pub const XATTR_QUICKXOR: &str = "user.gonedrive.quickxor";

/// Transient flag: set while an upload or download is in flight, so the file
/// manager can show a syncing emblem, and removed when it finishes.
pub const XATTR_SYNCING: &str = "user.gonedrive.syncing";
/// Original permission bits while an unprotected placeholder is fail-closed.
pub const XATTR_SAFETY_MODE: &str = "user.gonedrive.safety_mode";
/// The eTag a partial download was started from, kept on the partial file
/// itself. A resume is valid only against that exact version: appending a
/// newer version's tail to an older version's head would publish a file that
/// is neither. Removed before the finished file is published.
pub(crate) const XATTR_PARTIAL_ETAG: &str = "user.gonedrive.partial_etag";

/// The version a partial download belongs to, if it recorded one.
pub(crate) fn partial_etag(file: &std::fs::File) -> Option<String> {
    get_xattr_file(file, XATTR_PARTIAL_ETAG)
        .ok()
        .flatten()
        .and_then(|value| String::from_utf8(value).ok())
}

pub(crate) fn set_partial_etag(file: &std::fs::File, etag: &str) -> Result<()> {
    set_xattr_file(file, XATTR_PARTIAL_ETAG, etag.as_bytes())
}

pub(crate) fn clear_partial_etag(file: &std::fs::File) -> Result<()> {
    remove_xattr_file(file, XATTR_PARTIAL_ETAG)
}

/// What a placeholder currently holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderState {
    /// Metadata only: correct size, no blocks.
    Dehydrated,
    /// Content is present.
    Hydrated,
    /// Not a GOnedrive placeholder at all.
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryPopulation {
    Partial,
    Populated,
    Unmanaged,
}

impl DirectoryPopulation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Populated => "populated",
            Self::Unmanaged => "unmanaged",
        }
    }

    fn parse(raw: &[u8]) -> Self {
        match raw {
            b"partial" => Self::Partial,
            b"populated" => Self::Populated,
            _ => Self::Unmanaged,
        }
    }
}

impl PlaceholderState {
    fn as_str(self) -> &'static str {
        match self {
            PlaceholderState::Dehydrated => "dehydrated",
            PlaceholderState::Hydrated => "hydrated",
            PlaceholderState::Unmanaged => "unmanaged",
        }
    }

    fn parse(raw: &[u8]) -> Self {
        match raw {
            b"dehydrated" => PlaceholderState::Dehydrated,
            b"hydrated" => PlaceholderState::Hydrated,
            _ => PlaceholderState::Unmanaged,
        }
    }
}

/// How much of a file is actually on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occupancy {
    /// What `ls` reports.
    pub apparent_size: u64,
    /// What `du` reports: allocated blocks, in bytes.
    pub allocated: u64,
}

impl Occupancy {
    pub fn read(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self::from_metadata(&meta))
    }

    pub(crate) fn read_file(file: &std::fs::File) -> Result<Self> {
        Ok(Self::from_metadata(&file.metadata()?))
    }

    fn from_metadata(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            apparent_size: meta.len(),
            // `st_blocks` is always in 512-byte units, whatever the filesystem's
            // own block size.
            allocated: meta.blocks() * 512,
        }
    }

    /// Whether `du` reports materially less space than `ls`.
    ///
    /// This is an approximate storage metric, not a content-layout test: xattr
    /// metadata is included in `allocated`, so a small, completely sparse
    /// placeholder can legitimately return false here.
    pub fn is_sparse(self) -> bool {
        self.allocated < self.apparent_size
    }
}

/// Which logical content ranges of a regular file have physical data extents.
///
/// Unlike [`Occupancy`], this does not confuse filesystem metadata or external
/// xattr blocks with user content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentLayout {
    /// A zero-length file has no range to classify.
    Empty,
    /// Every byte in the apparent range is a hole.
    Sparse,
    /// The file contains both data extents and holes.
    Partial,
    /// Every byte in the apparent range is backed by a data extent.
    Full,
}

impl ContentLayout {
    /// Whether reading the file can return no locally stored content bytes.
    pub fn has_no_data(self) -> bool {
        matches!(self, Self::Empty | Self::Sparse)
    }

    /// Whether every logical byte is locally materialized.
    pub fn is_fully_materialized(self) -> bool {
        matches!(self, Self::Empty | Self::Full)
    }
}

pub struct Placeholder;

impl Placeholder {
    /// Inspect content extents without treating xattr metadata as file data.
    pub fn content_layout(path: &Path) -> Result<ContentLayout> {
        let file = std::fs::File::open(path)?;
        content_layout_file(&file)
    }

    pub(crate) fn content_layout_file(file: &std::fs::File) -> Result<ContentLayout> {
        content_layout_file(file)
    }

    /// Keep an unprotected sparse placeholder visible but unreadable. The HSM
    /// restores the exact saved mode on the same inode after its mark succeeds.
    pub(crate) fn lock_unprotected_file(file: &std::fs::File) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let mode = file.metadata()?.mode() & 0o7777;
        if mode == 0 {
            return Ok(());
        }
        set_xattr_file(file, XATTR_SAFETY_MODE, &mode.to_le_bytes())?;
        // SAFETY: `file` is owned by the caller and `fchmod` has no pointers.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            let _ = remove_xattr_file(file, XATTR_SAFETY_MODE);
            return Err(error.into());
        }
        Ok(())
    }

    /// Create a file that reports `size` bytes but has no content blocks.
    ///
    /// The file is truncated to zero and then extended to its advertised size,
    /// making the whole logical range a hole without allocating its contents.
    pub fn create(
        path: &Path,
        size: u64,
        item_id: &str,
        ctag: Option<&str>,
        mtime: Option<OffsetDateTime>,
    ) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        reset_to_sparse_size(&file, size)?;
        drop(file);

        Self::write_marks(path, PlaceholderState::Dehydrated, item_id, ctag)?;
        // Stamped last: writing xattrs does not touch mtime, but truncation did.
        if let Some(mtime) = mtime {
            crate::sync::transfer::set_mtime(path, mtime)?;
        }
        Ok(())
    }

    pub(crate) fn create_file(
        file: &std::fs::File,
        size: u64,
        item_id: &str,
        ctag: Option<&str>,
        mtime: Option<OffsetDateTime>,
    ) -> Result<()> {
        reset_to_sparse_size(file, size)?;
        Self::write_marks_file(file, PlaceholderState::Dehydrated, item_id, ctag)?;
        if let Some(mtime) = mtime {
            let times =
                std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
            file.set_times(times)?;
        }
        Ok(())
    }

    /// Release a hydrated file's blocks, keeping the file and its size.
    ///
    /// This is "free up space". The caller is responsible for having confirmed
    /// the content is safely uploaded first — discarding extents is not reversible
    /// without going back to the service.
    pub fn dehydrate(path: &Path, item_id: &str, ctag: Option<&str>) -> Result<u64> {
        let file = std::fs::File::options().write(true).open(path)?;
        Self::dehydrate_file(&file, item_id, ctag)
    }

    pub(crate) fn dehydrate_file(
        file: &std::fs::File,
        item_id: &str,
        ctag: Option<&str>,
    ) -> Result<u64> {
        let before = Occupancy::read_file(file)?;
        let version_before = writable_version(file)?;
        // Publish the fail-closed state before discarding bytes. If the process
        // dies between the two truncates, a later scan must see a placeholder
        // and fetch the remote copy, never upload the temporary zero length.
        Self::write_marks_file(file, PlaceholderState::Dehydrated, item_id, ctag)?;
        file.sync_all()?;

        // The caller proved these bytes are safely on the server, but that was
        // before the mark and its fsync — and an fsync is long enough for an
        // application holding this file open to save into it. Re-read the
        // inode: releasing space must never discard a write nobody has seen.
        let discarded = if writable_version(file)? == version_before {
            reset_to_sparse_size(file, before.apparent_size)
        } else {
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "the file was written while space was being released",
            )))
        };
        if let Err(error) = discarded {
            // Put the marks back to describe what the bytes actually are.
            // Leaving the file claiming "dehydrated" while it is still full
            // makes the scanner read later edits as a placeholder's and never
            // upload them.
            Self::mark_hydrated_file(file, item_id, ctag)?;
            let _ = file.sync_all();
            return Err(error);
        }
        file.sync_all()?;
        let after = Occupancy::read_file(file)?;
        Ok(before.allocated.saturating_sub(after.allocated))
    }

    /// Write content back into a placeholder at `offset`.
    ///
    /// Writes through a positional write rather than truncating and rewriting,
    /// so a partial hydration — filling only the range a reader asked for — is
    /// expressible, and so the file's identity (inode, hard links, open file
    /// descriptors) survives.
    pub fn fill(path: &Path, offset: u64, data: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::options().write(true).open(path)?;
        file.write_all_at(data, offset)?;
        Ok(())
    }

    pub(crate) fn fill_file(file: &std::fs::File, offset: u64, data: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        file.write_all_at(data, offset)?;
        Ok(())
    }

    /// Mark a fully-filled placeholder as holding its content.
    pub fn mark_hydrated(path: &Path, item_id: &str, ctag: Option<&str>) -> Result<()> {
        Self::write_marks(path, PlaceholderState::Hydrated, item_id, ctag)?;
        // Record the content hash of the bytes now on disk, so a later
        // dehydrate/rebuild can tell an untouched copy from an in-place edit
        // that the size+mtime heuristic would miss. Computed from the actual
        // file rather than trusting a stored server hash, so the xattr always
        // reflects exactly what is on disk.
        if let Ok(hash) = crate::hash::quick_xor_file(path) {
            let _ = xattr::set(path, XATTR_QUICKXOR, hash.as_bytes());
        }
        Ok(())
    }

    pub(crate) fn mark_hydrated_file(
        file: &std::fs::File,
        item_id: &str,
        ctag: Option<&str>,
    ) -> Result<()> {
        Self::write_marks_file(file, PlaceholderState::Hydrated, item_id, ctag)?;
        if let Ok(mut reader) = file.try_clone() {
            if reader.seek(SeekFrom::Start(0)).is_ok() {
                if let Ok(hash) = crate::hash::quick_xor_reader(&mut reader) {
                    let _ = set_xattr_file(file, XATTR_QUICKXOR, hash.as_bytes());
                }
            }
        }
        Ok(())
    }

    /// Read what the file itself claims about its state.
    pub fn state(path: &Path) -> PlaceholderState {
        match xattr::get(path, XATTR_STATE) {
            Ok(Some(raw)) => PlaceholderState::parse(&raw),
            _ => PlaceholderState::Unmanaged,
        }
    }

    pub(crate) fn state_file(file: &std::fs::File) -> PlaceholderState {
        match get_xattr_file(file, XATTR_STATE) {
            Ok(Some(raw)) => PlaceholderState::parse(&raw),
            _ => PlaceholderState::Unmanaged,
        }
    }

    pub fn item_id(path: &Path) -> Option<String> {
        let raw = xattr::get(path, XATTR_ITEM_ID).ok()??;
        String::from_utf8(raw).ok()
    }

    pub(crate) fn item_id_file(file: &std::fs::File) -> Option<String> {
        String::from_utf8(get_xattr_file(file, XATTR_ITEM_ID).ok()??).ok()
    }

    pub fn ctag(path: &Path) -> Option<String> {
        let raw = xattr::get(path, XATTR_CTAG).ok()??;
        String::from_utf8(raw).ok()
    }

    /// Mark a directory with its aggregate state, so a file manager can badge
    /// folders the way it badges files.
    ///
    /// A directory cannot be a sparse placeholder, so this only writes the
    /// xattrs — no hole punching. `dehydrated` means "nothing inside is on this
    /// device" (an online-only folder), `hydrated` means "some content is
    /// here". Mirrors what Windows shows on a folder.
    pub fn mark_dir(path: &Path, state: PlaceholderState, item_id: &str) -> Result<()> {
        xattr::set(path, XATTR_STATE, state.as_str().as_bytes())
            .map_err(|e| Error::other(format!("could not mark {} ({e})", path.display())))?;
        xattr::set(path, XATTR_ITEM_ID, item_id.as_bytes())?;
        Ok(())
    }

    pub(crate) fn mark_dir_file(
        file: &std::fs::File,
        state: PlaceholderState,
        item_id: &str,
    ) -> Result<()> {
        set_xattr_file(file, XATTR_STATE, state.as_str().as_bytes())?;
        set_xattr_file(file, XATTR_ITEM_ID, item_id.as_bytes())?;
        Ok(())
    }

    pub fn directory_population(path: &Path) -> DirectoryPopulation {
        match xattr::get(path, XATTR_POPULATION) {
            Ok(Some(raw)) => DirectoryPopulation::parse(&raw),
            _ => DirectoryPopulation::Unmanaged,
        }
    }

    pub(crate) fn directory_population_file(file: &std::fs::File) -> DirectoryPopulation {
        match get_xattr_file(file, XATTR_POPULATION) {
            Ok(Some(raw)) => DirectoryPopulation::parse(&raw),
            _ => DirectoryPopulation::Unmanaged,
        }
    }

    pub(crate) fn set_directory_population_file(
        file: &std::fs::File,
        population: DirectoryPopulation,
    ) -> Result<()> {
        set_xattr_file(file, XATTR_POPULATION, population.as_str().as_bytes())
    }

    /// Record the retention choice on the file itself.
    ///
    /// Duplicating this from the database is deliberate: a file manager
    /// rendering a folder of a thousand files needs an answer per file, and a
    /// `getxattr` is a syscall while asking the daemon is a round trip.
    pub fn set_pin(path: &Path, pin: &str) -> Result<()> {
        xattr::set(path, XATTR_PIN, pin.as_bytes())?;
        Ok(())
    }

    pub(crate) fn set_pin_file(file: &std::fs::File, pin: &str) -> Result<()> {
        set_xattr_file(file, XATTR_PIN, pin.as_bytes())
    }

    pub fn pin(path: &Path) -> Option<String> {
        let raw = xattr::get(path, XATTR_PIN).ok()??;
        String::from_utf8(raw).ok()
    }

    pub(crate) fn pin_file(file: &std::fs::File) -> Option<String> {
        String::from_utf8(get_xattr_file(file, XATTR_PIN).ok()??).ok()
    }

    /// Record the content hash of the bytes we just wrote, so a later guard can
    /// tell whether the file has been edited since.
    pub fn set_content_hash(path: &Path, hash: &str) -> Result<()> {
        xattr::set(path, XATTR_QUICKXOR, hash.as_bytes())?;
        Ok(())
    }

    /// The content hash recorded the last time we wrote this file, if any.
    pub fn content_hash(path: &Path) -> Option<String> {
        let raw = xattr::get(path, XATTR_QUICKXOR).ok()??;
        String::from_utf8(raw).ok()
    }

    pub(crate) fn content_hash_file(file: &std::fs::File) -> Option<String> {
        String::from_utf8(get_xattr_file(file, XATTR_QUICKXOR).ok()??).ok()
    }

    /// Flag a file as actively transferring, so the file manager shows the
    /// syncing emblem while an upload or download is in flight, and clear it
    /// when the transfer finishes. A transient marker layered over the durable
    /// state, so nothing about the placeholder's real state changes.
    pub fn set_syncing(path: &Path, syncing: bool) {
        if syncing {
            let _ = xattr::set(path, XATTR_SYNCING, b"1");
        } else {
            let _ = xattr::remove(path, XATTR_SYNCING);
        }
    }

    pub(crate) fn set_syncing_file(file: &std::fs::File, syncing: bool) -> Result<()> {
        if syncing {
            set_xattr_file(file, XATTR_SYNCING, b"1")
        } else {
            remove_xattr_file(file, XATTR_SYNCING)
        }
    }

    /// Remove GOnedrive's marks, leaving an ordinary file behind.
    pub fn clear_marks(path: &Path) -> Result<()> {
        for name in managed_xattrs() {
            let _ = xattr::remove(path, name);
        }
        Ok(())
    }

    /// Descriptor-bound counterpart used while reconciling a copied
    /// placeholder. Clearing through the already-validated inode prevents a
    /// concurrent path replacement from losing its own marks instead.
    pub(crate) fn clear_marks_file(file: &std::fs::File) -> Result<()> {
        if let Some(raw) = get_xattr_file(file, XATTR_SAFETY_MODE)? {
            if let Ok(bytes) = <[u8; 4]>::try_from(raw.as_slice()) {
                let mode = u32::from_le_bytes(bytes);
                // SAFETY: `file` is live and `fchmod` has no pointer inputs.
                if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
        }
        for name in managed_xattrs() {
            let _ = remove_xattr_file(file, name);
        }
        Ok(())
    }

    fn write_marks(
        path: &Path,
        state: PlaceholderState,
        item_id: &str,
        ctag: Option<&str>,
    ) -> Result<()> {
        xattr::set(path, XATTR_STATE, state.as_str().as_bytes()).map_err(|e| {
            Error::other(format!(
                "could not mark {} ({e}); does the filesystem support user extended attributes?",
                path.display()
            ))
        })?;
        xattr::set(path, XATTR_ITEM_ID, item_id.as_bytes())?;
        if let Some(ctag) = ctag {
            xattr::set(path, XATTR_CTAG, ctag.as_bytes())?;
        }
        Ok(())
    }

    fn write_marks_file(
        file: &std::fs::File,
        state: PlaceholderState,
        item_id: &str,
        ctag: Option<&str>,
    ) -> Result<()> {
        set_xattr_file(file, XATTR_STATE, state.as_str().as_bytes())?;
        set_xattr_file(file, XATTR_ITEM_ID, item_id.as_bytes())?;
        if let Some(ctag) = ctag {
            set_xattr_file(file, XATTR_CTAG, ctag.as_bytes())?;
        }
        Ok(())
    }
}

/// Size and modification time of an open file, as the identity of its current
/// contents. Cheap enough to re-read either side of an fsync.
fn writable_version(file: &std::fs::File) -> Result<(u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;

    let meta = file.metadata()?;
    Ok((meta.len(), meta.mtime(), meta.mtime_nsec()))
}

fn managed_xattrs() -> [&'static str; 8] {
    [
        XATTR_STATE,
        XATTR_ITEM_ID,
        XATTR_CTAG,
        XATTR_PIN,
        XATTR_POPULATION,
        XATTR_QUICKXOR,
        XATTR_SYNCING,
        XATTR_SAFETY_MODE,
    ]
}

fn get_xattr_file(file: &std::fs::File, name: &str) -> Result<Option<Vec<u8>>> {
    let name = CString::new(name).expect("xattr constants contain no NUL");
    // SAFETY: descriptor and name are valid; null buffer with size zero asks
    // the kernel for the required allocation.
    let size = unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENODATA) {
            return Ok(None);
        }
        return Err(error.into());
    }
    let mut value = vec![0u8; size as usize];
    // SAFETY: `value` owns exactly the capacity advertised by the first call.
    let read = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if read < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    value.truncate(read as usize);
    Ok(Some(value))
}

fn set_xattr_file(file: &std::fs::File, name: &str, value: &[u8]) -> Result<()> {
    let name = CString::new(name).expect("xattr constants contain no NUL");
    // SAFETY: all pointers are valid for the duration of the call.
    let rc = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn remove_xattr_file(file: &std::fs::File, name: &str) -> Result<()> {
    let name = CString::new(name).expect("xattr constants contain no NUL");
    // SAFETY: the descriptor and NUL-terminated attribute name are valid.
    let rc = unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENODATA) {
        Ok(())
    } else {
        Err(error.into())
    }
}

fn content_layout_file(file: &std::fs::File) -> Result<ContentLayout> {
    let size = file.metadata()?.len();
    if size == 0 {
        return Ok(ContentLayout::Empty);
    }

    // lseek changes a file description's cursor, and dup/try_clone would share
    // that cursor with the caller. Reopening the descriptor through procfs binds
    // the probe to the same inode while giving it an independent cursor.
    let probe_path = format!("/proc/self/fd/{}", file.as_raw_fd());
    let probe = std::fs::File::open(&probe_path).map_err(|error| {
        Error::other(format!(
            "could not open a private extent probe for {probe_path} ({error})"
        ))
    })?;
    let Some(first_data) = seek_extent(&probe, 0, libc::SEEK_DATA)? else {
        return Ok(ContentLayout::Sparse);
    };
    if first_data >= size {
        return Ok(ContentLayout::Sparse);
    }
    if first_data > 0 {
        return Ok(ContentLayout::Partial);
    }

    let Some(first_hole) = seek_extent(&probe, 0, libc::SEEK_HOLE)? else {
        return Err(Error::other(
            "filesystem returned no hole or logical EOF for a non-empty file",
        ));
    };
    if first_hole >= size {
        Ok(ContentLayout::Full)
    } else {
        Ok(ContentLayout::Partial)
    }
}

fn seek_extent(file: &std::fs::File, offset: u64, whence: libc::c_int) -> Result<Option<u64>> {
    let offset = libc::off_t::try_from(offset)
        .map_err(|_| Error::other("extent offset exceeds the filesystem offset type"))?;
    // SAFETY: `file` owns a valid regular-file descriptor and lseek has no
    // pointer arguments. SEEK_DATA/SEEK_HOLE inspect extent metadata only.
    let position = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if position >= 0 {
        return Ok(Some(position as u64));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENXIO) {
        Ok(None)
    } else {
        Err(Error::other(format!(
            "could not inspect file extents ({error})"
        )))
    }
}

/// Discard every content extent while preserving this inode and logical size.
///
/// Hole punching cannot remove an unaligned fragment smaller than the
/// filesystem block size; it may leave a tiny file as an allocated block full
/// of zeroes. A truncate-to-zero cycle drops every extent first, and extending
/// the empty file recreates the entire logical range as a hole.
fn reset_to_sparse_size(file: &std::fs::File, size: u64) -> Result<()> {
    file.set_len(0)?;
    file.set_len(size)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn assert_no_content_blocks(path: &Path) {
        let occupancy = Occupancy::read(path).unwrap();
        let layout = Placeholder::content_layout(path).unwrap();
        assert!(
            layout.has_no_data(),
            "{path:?} has {layout:?} content extents (occupancy: {occupancy:?})"
        );
    }

    #[test]
    fn a_placeholder_reports_its_full_size_but_occupies_nothing() {
        let dir = tmp();
        let path = dir.path().join("report.docx");
        Placeholder::create(&path, 8 * 1024 * 1024, "ITEM1", Some("c1"), None).unwrap();

        let occupancy = Occupancy::read(&path).unwrap();
        // `ls` sees 8 MiB while the content range has no data extents. Some
        // filesystems still charge `du` for an external xattr metadata block.
        assert_eq!(occupancy.apparent_size, 8 * 1024 * 1024);
        assert_no_content_blocks(&path);
        assert!(occupancy.is_sparse());

        assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
        assert_eq!(Placeholder::item_id(&path).as_deref(), Some("ITEM1"));
        assert_eq!(Placeholder::ctag(&path).as_deref(), Some("c1"));
    }

    #[test]
    fn a_placeholder_reads_back_as_zeroes_rather_than_failing() {
        // Reading an unhydrated placeholder without the kernel hook in place
        // yields zeroes. That is exactly why the hook is not optional for
        // real use, and why these files are marked in xattrs.
        let dir = tmp();
        let path = dir.path().join("a.bin");
        Placeholder::create(&path, 4096, "ITEM1", None, None).unwrap();
        let content = std::fs::read(&path).unwrap();
        assert_eq!(content.len(), 4096);
        assert!(content.iter().all(|&b| b == 0));
    }

    #[test]
    fn a_failed_mark_safety_locks_without_removing_the_placeholder() {
        use std::os::unix::fs::MetadataExt;

        let dir = tmp();
        let path = dir.path().join("visible.bin");
        Placeholder::create(&path, 4096, "ITEM1", None, None).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let original = file.metadata().unwrap().mode() & 0o7777;

        Placeholder::lock_unprotected_file(&file).unwrap();

        assert!(path.exists(), "a mark failure must not hide the file name");
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o7777, 0);

        // An unprivileged owner cannot read user xattrs while the inode is
        // mode 000. Restore the original mode before inspecting the marker;
        // the privileged HSM can read it while installing the replacement
        // watch.
        assert_eq!(unsafe { libc::fchmod(file.as_raw_fd(), original) }, 0);
        assert_eq!(
            get_xattr_file(&file, XATTR_SAFETY_MODE).unwrap(),
            Some(original.to_le_bytes().to_vec())
        );
    }

    #[test]
    fn filling_a_placeholder_puts_the_content_back() {
        let dir = tmp();
        let path = dir.path().join("a.bin");
        let data: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();

        Placeholder::create(&path, data.len() as u64, "ITEM1", Some("c1"), None).unwrap();
        assert_no_content_blocks(&path);

        Placeholder::fill(&path, 0, &data).unwrap();
        Placeholder::mark_hydrated(&path, "ITEM1", Some("c1")).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);
        assert_eq!(
            Placeholder::content_layout(&path).unwrap(),
            ContentLayout::Full
        );
    }

    #[test]
    fn partial_fills_land_at_the_right_offset() {
        // Range hydration: filling only what a reader asked for.
        let dir = tmp();
        let path = dir.path().join("a.bin");
        Placeholder::create(&path, 1024 * 1024, "ITEM1", None, None).unwrap();

        Placeholder::fill(&path, 512 * 1024, b"MIDDLE").unwrap();
        let content = std::fs::read(&path).unwrap();
        assert_eq!(&content[512 * 1024..512 * 1024 + 6], b"MIDDLE");
        // Everything outside the filled range is still a hole.
        assert!(content[..512 * 1024].iter().all(|&b| b == 0));
        assert_eq!(content.len(), 1024 * 1024);
    }

    #[test]
    fn a_partial_fill_moves_the_mtime_and_restoring_puts_it_back() {
        // The bug the user hit: browsing a folder or a thumbnailer sniffing a
        // file's head fills a range, and that write stamps the file "now". The
        // remote time must be restorable while the file stays a placeholder.
        use crate::sync::transfer;
        use std::os::unix::fs::MetadataExt;
        use time::OffsetDateTime;

        let dir = tmp();
        let path = dir.path().join("doc.bin");
        let remote = OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap();

        Placeholder::create(&path, 1024 * 1024, "ITEM1", Some("c1"), Some(remote)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mtime(), 1_600_000_000);

        // A reader asks for the head — a range fill lands and bumps the time.
        Placeholder::fill(&path, 0, b"HELLO").unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().mtime(), 1_600_000_000);

        // Restoring returns the remote time; the file is still a placeholder.
        transfer::set_mtime(&path, remote).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mtime(), 1_600_000_000);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    }

    #[test]
    fn dehydrating_frees_the_blocks_and_keeps_the_file() {
        let dir = tmp();
        let path = dir.path().join("big.bin");
        let data = vec![7u8; 2 * 1024 * 1024];
        std::fs::write(&path, &data).unwrap();

        let before = Occupancy::read(&path).unwrap();
        assert!(before.allocated >= 2 * 1024 * 1024);
        assert_eq!(
            Placeholder::content_layout(&path).unwrap(),
            ContentLayout::Full
        );

        let freed = Placeholder::dehydrate(&path, "ITEM1", Some("c1")).unwrap();

        let after = Occupancy::read(&path).unwrap();
        assert_eq!(
            freed,
            before.allocated.saturating_sub(after.allocated),
            "reported freed space must match the actual allocation change"
        );
        assert!(freed > 0, "dehydration did not release content blocks");
        // The file is still there, still the right size, and now costs only
        // whatever metadata/xattr overhead this filesystem requires.
        assert!(path.exists());
        assert_eq!(after.apparent_size, 2 * 1024 * 1024);
        assert_no_content_blocks(&path);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    }

    #[test]
    fn dehydrating_a_sub_block_file_removes_its_last_content_extent() {
        let dir = tmp();
        let path = dir.path().join("tiny.md");
        std::fs::write(&path, b"twenty bytes of text!").unwrap();

        Placeholder::dehydrate(&path, "ITEM1", Some("c1")).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 21);
        assert_no_content_blocks(&path);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    }

    #[test]
    fn a_round_trip_returns_the_original_bytes() {
        let dir = tmp();
        let path = dir.path().join("doc.bin");
        let data: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        Placeholder::dehydrate(&path, "ITEM1", Some("c1")).unwrap();
        assert_no_content_blocks(&path);

        Placeholder::fill(&path, 0, &data).unwrap();
        Placeholder::mark_hydrated(&path, "ITEM1", Some("c1")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data);
    }

    #[test]
    fn an_ordinary_file_is_not_mistaken_for_a_placeholder() {
        let dir = tmp();
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(Placeholder::state(&path), PlaceholderState::Unmanaged);
        assert!(Placeholder::item_id(&path).is_none());
    }

    #[test]
    fn marks_can_be_removed_leaving_an_ordinary_file() {
        let dir = tmp();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello").unwrap();
        Placeholder::mark_hydrated(&path, "ITEM1", Some("c1")).unwrap();
        assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);

        Placeholder::clear_marks(&path).unwrap();
        assert_eq!(Placeholder::state(&path), PlaceholderState::Unmanaged);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn a_zero_byte_file_is_handled_without_a_hole_punch() {
        let dir = tmp();
        let path = dir.path().join("empty");
        Placeholder::create(&path, 0, "ITEM1", None, None).unwrap();
        assert_eq!(Occupancy::read(&path).unwrap().apparent_size, 0);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    }

    #[test]
    fn placeholders_keep_their_remote_timestamp() {
        let dir = tmp();
        let path = dir.path().join("a.bin");
        let when = OffsetDateTime::from_unix_timestamp(1_600_000_000).unwrap();
        Placeholder::create(&path, 1024, "ITEM1", None, Some(when)).unwrap();

        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().mtime(),
            when.unix_timestamp()
        );
    }
}

#[cfg(test)]
mod dehydrate_guard_tests {
    use super::*;

    #[test]
    fn a_failed_truncation_leaves_the_marks_describing_the_bytes() {
        // The fail-closed order writes "dehydrated" before discarding content,
        // so a *failed* discard has to put the marks back. Left claiming to be
        // a placeholder while still full, the scanner reads later edits as a
        // placeholder's and never uploads them.
        //
        // A read-only descriptor gives a deterministic failure: setting the
        // xattrs needs write permission on the file, which we have, while
        // ftruncate on an O_RDONLY descriptor cannot succeed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.bin");
        std::fs::write(&path, vec![7u8; 4096]).unwrap();

        let readonly = std::fs::File::open(&path).unwrap();
        let error = Placeholder::dehydrate_file(&readonly, "F1", Some("c1")).unwrap_err();
        drop(readonly);

        assert!(matches!(error, Error::Io(_)), "unexpected error: {error:?}");
        assert_eq!(
            Placeholder::state(&path),
            PlaceholderState::Hydrated,
            "the marks must describe the bytes that are still there"
        );
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            4096,
            "nothing was discarded"
        );
    }
}
