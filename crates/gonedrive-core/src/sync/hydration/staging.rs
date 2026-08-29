//! On-demand reads and secure staging for the hydration helper.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::OffsetDateTime;

use crate::db::{ItemRecord, ItemState};
use crate::error::Result;
use crate::hydrate::{Placeholder, PlaceholderState};
use crate::sync::plan::LocalStat;

use super::super::engine::{Direction, Engine, SyncEvent, PROGRESS_MAX_STEP, PROGRESS_MIN_STEP};
use super::super::transfer;

impl Engine {
    // ---- files on demand ---------------------------------------------------

    /// Fill a placeholder's content because something tried to read it.
    ///
    /// Called from the fanotify path with a process blocked on the other end,
    /// so it does the least work that unblocks the reader: fetch, write, mark.
    /// The whole file is fetched rather than the requested range — a reader
    /// asking for the first 4 KiB of a document is almost always about to ask
    /// for the rest, and one request beats hundreds.
    pub async fn hydrate(&self, path: &Path) -> Result<u64> {
        let (account, record, file) = self.open_owned_placeholder(path, true)?;
        let item_id = record.id.clone();

        if Placeholder::state_file(&file) == PlaceholderState::Hydrated {
            return Ok(0);
        }

        tracing::info!(path = %path.display(), size = record.size, "hydrating on access");
        self.store.set_item_state(&item_id, ItemState::Hydrating)?;

        let response = self.graph.download(&item_id, None).await?;
        let mut stream = response.bytes_stream();
        let mut offset = 0u64;
        let name = record.name.clone();

        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            Placeholder::fill_file(&file, offset, &chunk)?;
            offset += chunk.len() as u64;
            self.emit(SyncEvent::Progress {
                name: name.clone(),
                transferred: offset,
                total: Some(record.size),
                direction: Direction::Download,
            });
        }

        Placeholder::mark_hydrated_file(&file, &item_id, record.ctag.as_deref())?;
        let pin = self
            .store
            .effective_pin(&item_id, self.config.download_new.default_pin())?;
        let _ = Placeholder::set_pin_file(&file, pin.as_str());
        // The write moved the mtime; put the service's value back so the file
        // does not look locally modified the moment it is read.
        if let Some(mtime) = record.remote_mtime {
            let times =
                std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
            file.set_times(times)?;
        }
        let local = LocalStat::from_metadata(&file.metadata()?);
        self.store.set_local_stat(
            &item_id,
            OffsetDateTime::from_unix_timestamp(local.mtime).ok(),
            Some(local.size),
            Some(local.ino),
        )?;
        self.store.set_item_state(&item_id, ItemState::Synced)?;
        let _ = self.refresh_folder_ancestors(&account, &item_id);
        Ok(offset)
    }

    /// Download a placeholder's content to a staging file beside it.
    ///
    /// Used on the files-on-demand path, where the daemon must **not** write to
    /// the placeholder itself: filling a hole faults the page in, which is a
    /// read, which re-enters the kernel hook that asked for the hydration in
    /// the first place. The privileged helper does the actual write through the
    /// descriptor the kernel handed it, which is exempt.
    ///
    /// The staging file is a sibling, so the helper's copy stays on one
    /// filesystem, and it carries the suffix the local scanner already ignores.
    pub async fn stage_content(&self, path: &Path) -> Result<PathBuf> {
        self.stage_content_cancellable(path, Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .await
    }

    /// As [`Self::stage_content`], but abandons the transfer when `cancel` is
    /// set.
    pub async fn stage_content_cancellable(
        &self,
        path: &Path,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<PathBuf> {
        let (account, record, _file) = self.open_owned_placeholder(path, false)?;
        let item_id = record.id.clone();
        let rooted = self.rooted(&account)?;

        let staged = transfer::hydration_staging_sibling(path);

        tracing::info!(path = %path.display(), size = record.size, "staging content");
        self.store.set_item_state(&item_id, ItemState::Hydrating)?;

        self.emit(SyncEvent::HydrationStarted {
            path: path.to_path_buf(),
            name: record.name.clone(),
            size: record.size,
        });

        let events = self.events.clone();
        let reported = path.to_path_buf();
        let size = record.size;
        // The transfer reports every chunk, which for a 13 MB file is around a
        // thousand callbacks. Forwarding each one floods the bus for no
        // benefit: nobody can read a progress bar that updates a thousand
        // times. Emit on visible movement instead.
        let step = (size / 100).clamp(PROGRESS_MIN_STEP, PROGRESS_MAX_STEP);
        let mut last_sent = 0u64;

        let limit = self.config.download_limit_bytes_per_sec;
        let result = transfer::download_to_file_limited_rooted(
            &rooted,
            &self.graph,
            transfer::DownloadSpec {
                item_id: &item_id,
                dest: &staged,
                expected_destination: None,
                etag: None,
                mtime: None,
                rate_limit: limit,
            },
            &mut move |done, total| {
                let total = total.unwrap_or(size);
                if done == total || done.saturating_sub(last_sent) >= step {
                    last_sent = done;
                    let _ = events.send(SyncEvent::HydrationProgress {
                        path: reported.clone(),
                        transferred: done,
                        total,
                    });
                }
                !cancel.load(std::sync::atomic::Ordering::Relaxed)
            },
        )
        .await;

        if let Err(e) = result {
            let _ = rooted.remove_file(&staged);
            let _ = rooted.remove_file(&transfer::staging_sibling(&staged));
            self.emit(SyncEvent::HydrationFinished {
                path: path.to_path_buf(),
                ok: false,
            });
            return Err(e);
        }
        Ok(staged)
    }

    /// Stage only a byte range of a placeholder, quietly.
    ///
    /// This is what makes browsing cheap: a file manager sniffing content
    /// types reads the first 16 KiB of each file, and answering that with a
    /// full download turns "open a folder" into "download the folder". The
    /// range is fetched, nothing else changes — no state transition, no
    /// progress events, no activity entry — and the kernel watch stays armed
    /// so a reader that continues past the window comes back for the rest.
    pub async fn stage_range(&self, path: &Path, offset: u64, length: u64) -> Result<PathBuf> {
        let (account, record, file) = self.open_owned_placeholder(path, false)?;
        let item_id = record.id;
        let rooted = self.rooted(&account)?;

        let staged = transfer::hydration_staging_sibling(path);

        // A range that is already real data on disk — a window some earlier
        // sniff filled — is served from the file itself. Browsing the same
        // folder twice must not fetch the same 16 KiB from the cloud twice.
        if let Some(bytes) = read_if_materialized_file(&file, offset, length)? {
            let mut staged_file = tokio::fs::File::from_std(rooted.create_file(&staged, true)?);
            use tokio::io::AsyncWriteExt;
            staged_file.write_all(&bytes).await?;
            staged_file.sync_all().await?;
            tracing::info!(
                path = %path.display(),
                offset,
                bytes = bytes.len(),
                "staged range from local data"
            );
            return Ok(staged);
        }

        let response = self
            .graph
            .download(&item_id, Some(offset..offset + length))
            .await?;
        let bytes = response.bytes().await?;
        let mut staged_file = tokio::fs::File::from_std(rooted.create_file(&staged, true)?);
        use tokio::io::AsyncWriteExt;
        staged_file.write_all(&bytes).await?;
        staged_file.sync_all().await?;
        tracing::info!(
            path = %path.display(),
            offset,
            bytes = bytes.len(),
            "staged range"
        );
        Ok(staged)
    }

    /// Stage a byte range only if it is already real data on disk, never
    /// fetching. Returns `None` if any of the window is still a hole.
    ///
    /// This is what keeps folder browsing offline: a sniff of a window that was
    /// filled by an earlier real read is served locally; a sniff of an
    /// untouched placeholder returns `None`, and the caller lets the read see
    /// zeros rather than reaching for the network.
    pub async fn stage_range_if_local(
        &self,
        path: &Path,
        offset: u64,
        length: u64,
    ) -> Result<Option<PathBuf>> {
        let (account, _record, file) = self.open_owned_placeholder(path, false)?;
        let Some(bytes) = read_if_materialized_file(&file, offset, length)? else {
            return Ok(None);
        };
        let staged = transfer::hydration_staging_sibling(path);
        let rooted = self.rooted(&account)?;
        let mut staged_file = tokio::fs::File::from_std(rooted.create_file(&staged, true)?);
        use tokio::io::AsyncWriteExt;
        staged_file.write_all(&bytes).await?;
        staged_file.sync_all().await?;
        Ok(Some(staged))
    }

    /// Restore a placeholder's modification time to the record's after a
    /// partial range fill.
    ///
    /// A range fill pwrites content into the sparse file, and that write moves
    /// the mtime to the wall clock — so merely browsing a folder, or a
    /// thumbnailer sniffing a file's head, would leave every touched file
    /// stamped "now" instead of its real remote time. The file is still a
    /// placeholder (`full` fills finalise through `finish_hydration`, which
    /// already restores the time), so only the timestamp is corrected here.
    pub fn restore_mtime(&self, path: &Path) -> Result<()> {
        let (_account, record, file) = self.open_owned_placeholder(path, true)?;
        if let Some(mtime) = record.remote_mtime {
            // Only when it actually drifted: a browsing sniff that filled
            // nothing reports the same completion, and opening every sniffed
            // file for writing would make folder browsing crawl. A stat is
            // cheap and does not fault the placeholder.
            use std::os::unix::fs::MetadataExt;
            let drifted = (file.metadata()?.mtime() - mtime.unix_timestamp()).abs() > 2;
            if drifted {
                let times =
                    std::fs::FileTimes::new().set_modified(crate::sync::plan::to_filetime(mtime));
                file.set_times(times)?;
            }
        }
        Ok(())
    }

    /// Return the stable version of a file whose bytes are safe to discard.
    ///
    /// The content hash is the authority. A file whose current
    /// quickXorHash matches the hash we recorded when we last wrote it is
    /// unchanged and safe to discard, whatever its size and mtime say; a
    /// mismatch means it has been edited since and holds bytes the server does
    /// not have. Only an explicitly dehydrated placeholder can treat sparse
    /// extents as cloud-owned cache; ordinary sparse files are user content.
    /// When no hash was ever recorded, fall back to the size+mtime heuristic.
    pub(in crate::sync) fn disposable_local_version(
        &self,
        record: &ItemRecord,
        file: &std::fs::File,
    ) -> Option<transfer::FileVersion> {
        use std::io::{Seek, SeekFrom};

        let before = transfer::FileVersion::capture_file(file).ok()?;
        let occ = match crate::hydrate::Occupancy::read_file(file) {
            Ok(occupancy) => occupancy,
            // If we cannot inspect the file, we cannot prove its bytes are
            // disposable. Every destructive caller must fail closed.
            Err(_) => return None,
        };
        let disposable = if occ.allocated < occ.apparent_size
            && Placeholder::state_file(file) == PlaceholderState::Dehydrated
        {
            // An actual placeholder or partial range cache owns no local
            // content. A hydrated VM image or database can be sparse too and
            // must continue through the hash check below.
            true
        } else if let Some(recorded) = Placeholder::content_hash_file(file) {
            match file.try_clone().and_then(|mut reader| {
                reader.seek(SeekFrom::Start(0))?;
                crate::hash::quick_xor_reader(&mut reader)
            }) {
                Ok(current) => current == recorded,
                // A full file we cannot read to verify is not safe to discard.
                Err(_) => false,
            }
        } else {
            // No recorded hash (a file written before content hashing, or one
            // that arrived some other way): fall back to size+mtime.
            let local = LocalStat::from_metadata(&file.metadata().ok()?);
            !crate::sync::plan::local_is_modified(record, &local)
        };
        if !disposable {
            return None;
        }

        // Hashing can take seconds for a large file. Bind the decision to one
        // unchanged inode so a concurrent rewrite cannot inherit the verdict.
        let after = transfer::FileVersion::capture_file(file).ok()?;
        (after == before).then_some(after)
    }
}

/// The requested window, if every byte of it is already data rather than hole.
///
/// `SEEK_DATA`/`SEEK_HOLE` ask the filesystem directly which parts of a sparse
/// file are materialized, so this needs no bookkeeping of past fills.
fn read_if_materialized_file(
    file: &std::fs::File,
    offset: u64,
    length: u64,
) -> Result<Option<Vec<u8>>> {
    use std::os::unix::fs::FileExt;
    use std::os::unix::io::AsRawFd;

    let size = file.metadata()?.len();
    let end = (offset + length).min(size);
    if offset >= end {
        return Ok(None);
    }

    // SAFETY: valid fd; lseek with SEEK_HOLE finds the next hole at or after
    // `offset`. If that hole starts at or beyond `end`, the window is data.
    let next_hole =
        unsafe { libc::lseek(file.as_raw_fd(), offset as libc::off_t, libc::SEEK_HOLE) };
    if next_hole < 0 || (next_hole as u64) < end {
        return Ok(None);
    }

    let mut buf = vec![0u8; (end - offset) as usize];
    file.read_exact_at(&mut buf, offset)?;
    Ok(Some(buf))
}
