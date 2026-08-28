//! Placeholders: files that are real, but whose content is not here yet.
//!
//! This is the Linux answer to what Windows calls Files On-Demand. A
//! placeholder is a genuine file on a genuine ext4 directory — `ls` reports its
//! real size, a file manager shows it, `find` finds it — but every block is a
//! hole, so `du` reports zero and the disk holds nothing.
//!
//! Three operations make the illusion work:
//!
//! * [`Placeholder::create`] — the file exists at full size, holding nothing.
//! * [`Placeholder::hydrate`] — content is written back into the holes.
//! * [`Placeholder::dehydrate`] — content is punched back out, freeing space.
//!
//! State lives in extended attributes rather than only in the database, because
//! xattrs travel with the file. A lost or rebuilt database can be recovered by
//! reading the disk, and a file manager extension can render the right emblem
//! with a `getxattr` instead of an IPC round trip.

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

/// quickXorHash of the content we last wrote to this file (download, hydration,
/// or the local content at the moment we uploaded it). It is the reference the
/// dehydrate/rebuild/overwrite guards compare the file's *current* content
/// against, so an in-place edit that preserved size and mtime — invisible to
/// the size+mtime heuristic — is still caught before the bytes are discarded.
pub const XATTR_QUICKXOR: &str = "user.gonedrive.quickxor";

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
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            apparent_size: meta.len(),
            // `st_blocks` is always in 512-byte units, whatever the filesystem's
            // own block size.
            allocated: meta.blocks() * 512,
        })
    }

    /// Whether the file occupies materially less space than it claims.
    pub fn is_sparse(self) -> bool {
        self.allocated < self.apparent_size
    }
}

pub struct Placeholder;

impl Placeholder {
    /// Create a file that reports `size` bytes but occupies none.
    ///
    /// The file is truncated to full size and then has that whole range punched
    /// out. Truncating alone would already produce a hole on ext4, but not on
    /// every filesystem, and an explicit punch makes the intent unambiguous —
    /// this file is *meant* to be empty, it is not merely un-written.
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
        file.set_len(size)?;
        if size > 0 {
            punch_hole(&file, 0, size)?;
        }
        drop(file);

        Self::write_marks(path, PlaceholderState::Dehydrated, item_id, ctag)?;
        // Stamped last: writing xattrs does not touch mtime, but truncation did.
        if let Some(mtime) = mtime {
            crate::sync::transfer::set_mtime(path, mtime)?;
        }
        Ok(())
    }

    /// Release a hydrated file's blocks, keeping the file and its size.
    ///
    /// This is "free up space". The caller is responsible for having confirmed
    /// the content is safely uploaded first — punching a hole is not reversible
    /// without going back to the service.
    pub fn dehydrate(path: &Path, item_id: &str, ctag: Option<&str>) -> Result<u64> {
        let before = Occupancy::read(path)?;
        let file = std::fs::File::options().write(true).open(path)?;
        if before.apparent_size > 0 {
            punch_hole(&file, 0, before.apparent_size)?;
        }
        file.sync_all()?;
        drop(file);

        Self::write_marks(path, PlaceholderState::Dehydrated, item_id, ctag)?;
        let after = Occupancy::read(path)?;
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

    /// Read what the file itself claims about its state.
    pub fn state(path: &Path) -> PlaceholderState {
        match xattr::get(path, XATTR_STATE) {
            Ok(Some(raw)) => PlaceholderState::parse(&raw),
            _ => PlaceholderState::Unmanaged,
        }
    }

    pub fn item_id(path: &Path) -> Option<String> {
        let raw = xattr::get(path, XATTR_ITEM_ID).ok()??;
        String::from_utf8(raw).ok()
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
        xattr::set(path, XATTR_STATE, state.as_str().as_bytes()).map_err(|e| {
            Error::other(format!("could not mark {} ({e})", path.display()))
        })?;
        xattr::set(path, XATTR_ITEM_ID, item_id.as_bytes())?;
        Ok(())
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

    pub fn pin(path: &Path) -> Option<String> {
        let raw = xattr::get(path, XATTR_PIN).ok()??;
        String::from_utf8(raw).ok()
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

    /// Remove GOnedrive's marks, leaving an ordinary file behind.
    pub fn clear_marks(path: &Path) -> Result<()> {
        for name in [XATTR_STATE, XATTR_ITEM_ID, XATTR_CTAG, XATTR_PIN] {
            let _ = xattr::remove(path, name);
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
}

/// Punch a hole, releasing the blocks while keeping the file's length.
fn punch_hole(file: &std::fs::File, offset: u64, len: u64) -> Result<()> {
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    let rc = unsafe {
        libc::fallocate(
            file.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            len as libc::off_t,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(Error::other(format!(
            "could not release blocks ({err}); \
             the filesystem may not support hole punching"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_placeholder_reports_its_full_size_but_occupies_nothing() {
        let dir = tmp();
        let path = dir.path().join("report.docx");
        Placeholder::create(&path, 8 * 1024 * 1024, "ITEM1", Some("c1"), None).unwrap();

        let occupancy = Occupancy::read(&path).unwrap();
        // This pair is the whole point: `ls` sees 8 MiB, `du` sees nothing.
        assert_eq!(occupancy.apparent_size, 8 * 1024 * 1024);
        assert_eq!(occupancy.allocated, 0);
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
    fn filling_a_placeholder_puts_the_content_back() {
        let dir = tmp();
        let path = dir.path().join("a.bin");
        let data: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();

        Placeholder::create(&path, data.len() as u64, "ITEM1", Some("c1"), None).unwrap();
        assert_eq!(Occupancy::read(&path).unwrap().allocated, 0);

        Placeholder::fill(&path, 0, &data).unwrap();
        Placeholder::mark_hydrated(&path, "ITEM1", Some("c1")).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Hydrated);
        assert!(Occupancy::read(&path).unwrap().allocated > 0);
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

        let freed = Placeholder::dehydrate(&path, "ITEM1", Some("c1")).unwrap();
        assert!(freed >= 2 * 1024 * 1024, "freed only {freed} bytes");

        let after = Occupancy::read(&path).unwrap();
        // The file is still there, still the right size, and now costs nothing.
        assert!(path.exists());
        assert_eq!(after.apparent_size, 2 * 1024 * 1024);
        assert_eq!(after.allocated, 0);
        assert_eq!(Placeholder::state(&path), PlaceholderState::Dehydrated);
    }

    #[test]
    fn a_round_trip_returns_the_original_bytes() {
        let dir = tmp();
        let path = dir.path().join("doc.bin");
        let data: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        Placeholder::dehydrate(&path, "ITEM1", Some("c1")).unwrap();
        assert_eq!(Occupancy::read(&path).unwrap().allocated, 0);

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
