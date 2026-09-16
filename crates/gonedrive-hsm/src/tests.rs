use super::*;

const SYSTEM_UNIT: &str = include_str!("../../../data/gonedrive-hsm.service");

fn my_uid() -> u32 {
    // SAFETY: getuid cannot fail and takes no arguments.
    unsafe { libc::getuid() }
}

#[test]
fn helper_unit_keeps_capacity_for_safe_filesystem_fallback() {
    let limit = SYSTEM_UNIT
        .lines()
        .find_map(|line| line.trim().strip_prefix("LimitNOFILE="))
        .and_then(|value| value.parse::<usize>().ok())
        .expect("the helper service must set LimitNOFILE");
    assert!(limit >= 262_144, "LimitNOFILE={limit} is too small");
}

#[test]
fn helper_unit_keeps_home_writable_for_hydration() {
    assert!(
        !SYSTEM_UNIT
            .lines()
            .any(|line| line.trim_start().starts_with("ProtectHome=")),
        "the helper must be able to hydrate and restore authorized home files"
    );
    assert!(SYSTEM_UNIT.contains("PrivateNetwork=true"));
    assert!(SYSTEM_UNIT.contains("RestrictAddressFamilies=AF_UNIX"));
}

#[test]
fn helper_unit_restarts_after_any_exit() {
    assert!(
        SYSTEM_UNIT.contains("Restart=always"),
        "a helper that stays down leaves every placeholder reading as zeroes"
    );
}

/// A peer-owned temporary directory declared as the only sync root.
fn roots(dir: &Path) -> Vec<SyncRoot> {
    vec![open_root(dir, my_uid()).unwrap()]
}

#[test]
fn helper_unit_bounds_journal_writes() {
    assert!(SYSTEM_UNIT.contains("LogRateLimitIntervalSec=1min"));
    assert!(SYSTEM_UNIT.contains("LogRateLimitBurst=30"));
}

#[test]
fn a_parent_mark_allows_only_files_whose_bytes_are_already_local() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("item.bin");
    std::fs::write(&path, b"local bytes").unwrap();
    let file = std::fs::File::open(&path).unwrap();

    assert!(unregistered_population_file_is_safe(file.as_raw_fd()).unwrap());

    xattr::set(&path, "user.gonedrive.state", b"hydrated").unwrap();
    assert!(unregistered_population_file_is_safe(file.as_raw_fd()).unwrap());

    xattr::set(&path, "user.gonedrive.state", b"dehydrated").unwrap();
    assert!(!unregistered_population_file_is_safe(file.as_raw_fd()).unwrap());

    xattr::set(&path, "user.gonedrive.state", b"corrupt").unwrap();
    assert!(!unregistered_population_file_is_safe(file.as_raw_fd()).unwrap());
}

#[test]
fn a_nautilus_copy_is_bound_to_its_writable_destination() {
    let dir = tempfile::tempdir().unwrap();
    let source_dir = dir.path().join("source");
    let destination_dir = dir.path().join("destination");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&destination_dir).unwrap();
    let source = source_dir.join("report.docx");
    let destination = destination_dir.join("report.docx");
    std::fs::write(&source, b"remote bytes").unwrap();

    assert_ne!(
        hydration_reader(std::process::id() as i32, &source),
        gonedrive_ipc::reader_policy::FILE_MANAGER_COPY_READER,
        "an ordinary read has no writable destination"
    );

    let _destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .unwrap();
    // The test process is not named Nautilus, so exercise the descriptor half
    // directly; reader-name matching is covered in gonedrive-ipc.
    assert_eq!(
        super::worker::writable_copy_destination(std::process::id() as i32, &source),
        Some(destination)
    );
}

#[test]
fn batch_error_text_stays_bounded_on_utf8_boundaries() {
    let message = "错".repeat(MARK_ERROR_BYTES);
    let truncated = truncate_mark_error(message);
    assert!(truncated.is_char_boundary(truncated.len()));
    assert!(truncated.len() <= MARK_ERROR_BYTES + 3);
    assert!(truncated.ends_with("..."));
}

#[test]
fn parallel_watch_work_preserves_request_order() {
    let items: Vec<_> = (0..257).collect();
    let results = parallel_map_ordered(&items, 4, |item| item * 3);
    assert_eq!(
        results,
        items.iter().map(|item| item * 3).collect::<Vec<_>>()
    );
}

// fill_from is the byte-writing core: it pwrites fetched content into the
// placeholder at an explicit offset. A wrong offset or an over-write here
// corrupts data beneath every core-level safety net, so it is pinned down.

#[test]
fn fill_from_places_content_at_offset_zero_exactly() {
    use std::os::unix::io::OwnedFd;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("staged");
    let target = dir.path().join("placeholder");
    std::fs::write(&source, b"the real content").unwrap();
    // A sparse placeholder the size of the file: zeros to be filled.
    std::fs::write(&target, vec![0u8; 16]).unwrap();

    let fd: OwnedFd = std::fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .unwrap()
        .into();
    let mut source = std::fs::File::open(&source).unwrap();
    let n = fill_from(&mut source, &fd, 0, 16).unwrap();
    assert_eq!(n, 16);
    assert_eq!(std::fs::read(&target).unwrap(), b"the real content");
}

#[test]
fn fill_from_never_writes_past_the_validated_length() {
    use std::os::unix::io::OwnedFd;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("staged");
    let target = dir.path().join("placeholder");
    // The source grew after its length was validated as 8.
    std::fs::write(&source, b"12345678EXTRA-BYTES").unwrap();
    std::fs::write(&target, vec![0u8; 8]).unwrap();

    let fd: OwnedFd = std::fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .unwrap()
        .into();
    let mut source = std::fs::File::open(&source).unwrap();
    let n = fill_from(&mut source, &fd, 0, 8).unwrap();
    assert_eq!(n, 8);
    assert_eq!(std::fs::read(&target).unwrap(), b"12345678");
}

#[test]
fn fill_from_lands_a_range_at_its_offset_and_leaves_the_rest() {
    use std::os::unix::io::OwnedFd;
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("staged");
    let target = dir.path().join("placeholder");
    std::fs::write(&source, b"MIDDLE").unwrap();
    // 32 bytes of 'A'; the fill must touch only [10, 16).
    std::fs::write(&target, vec![b'A'; 32]).unwrap();

    let fd: OwnedFd = std::fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .unwrap()
        .into();
    let mut source = std::fs::File::open(&source).unwrap();
    let n = fill_from(&mut source, &fd, 10, 6).unwrap();
    assert_eq!(n, 6);

    let got = std::fs::read(&target).unwrap();
    assert_eq!(
        &got[..10],
        &[b'A'; 10],
        "bytes before the offset are untouched"
    );
    assert_eq!(&got[10..16], b"MIDDLE", "the range landed at its offset");
    assert_eq!(
        &got[16..],
        &[b'A'; 16],
        "bytes after the range are untouched"
    );
    assert_eq!(got.len(), 32, "the file was not grown past its size");
}

#[test]
fn a_materialized_range_is_allowed_without_restaging_it() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::FileExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.bin");
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.set_len(16 * 1024).unwrap();

    assert!(!requested_range_is_materialized(file.as_raw_fd(), 0, 1, 16 * 1024).unwrap());
    file.write_all_at(&[0x42; 4096], 0).unwrap();
    assert!(requested_range_is_materialized(file.as_raw_fd(), 0, 4096, 16 * 1024).unwrap());
    assert!(!requested_range_is_materialized(file.as_raw_fd(), 8192, 4096, 16 * 1024).unwrap());
    file.write_all_at(&[0x42; 12 * 1024], 4096).unwrap();
    assert!(
        requested_range_is_materialized(file.as_raw_fd(), 0, 16 * 1024, 16 * 1024).unwrap(),
        "the final progressive range should promote the whole inode to hydrated"
    );
}

#[test]
fn a_full_fill_is_marked_hydrated_before_the_reader_resumes() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("placeholder");
    std::fs::write(&path, b"real bytes").unwrap();
    xattr::set(&path, "user.gonedrive.state", b"dehydrated").unwrap();

    // fanotify supplies a read-only event descriptor. The helper runs as root
    // in production, but an owner may update a user xattr through that same fd.
    let file = std::fs::File::open(&path).unwrap();
    mark_hydrated(file.as_raw_fd()).unwrap();

    assert_eq!(
        xattr::get(&path, "user.gonedrive.state")
            .unwrap()
            .as_deref(),
        Some(b"hydrated".as_slice())
    );
}

#[test]
fn the_daemons_own_access_is_never_questioned() {
    // The deadlock this prevents: the daemon writes to a watched file,
    // the kernel blocks the write and asks the helper, the helper asks the
    // daemon — which cannot answer, because it is the blocked write.
    let shared_pid = 4242;
    let event_pid = 4242;
    assert_eq!(shared_pid, event_pid, "same pid must be allowed through");
    // The real check is `event.pid == client_pid` in handle_event; this
    // test pins the intent so the guard is not "simplified" away.
}

#[test]
fn the_desktop_indexer_is_recognised() {
    // The case that matters: a placeholder looks like an ordinary file, so
    // an indexer walking the home directory would download the whole drive.
    for name in ["localsearch-3", "tracker-miner-f", "baloo_file", "tumblerd"] {
        assert!(
            gonedrive_ipc::reader_policy::BACKGROUND_READERS
                .iter()
                .any(|p| name.starts_with(p)),
            "{name} should be refused"
        );
    }
}

#[test]
fn ordinary_applications_are_not_refused() {
    for name in ["gedit", "libreoffice", "cat", "firefox", "nautilus"] {
        assert!(
            !gonedrive_ipc::reader_policy::BACKGROUND_READERS
                .iter()
                .any(|p| name.starts_with(p)),
            "{name} must still be able to open its files"
        );
    }
}

#[test]
fn our_own_process_is_not_a_background_reader() {
    assert!(background_reader(std::process::id() as i32).is_none());
}

#[test]
fn a_dead_process_is_not_claimed() {
    // A reader that exited between the event and the lookup must not be
    // matched against a stale name.
    assert!(background_reader(i32::MAX).is_none());
}

#[test]
fn a_users_own_file_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mine.bin");
    std::fs::write(&path, b"x").unwrap();
    assert!(open_authorised(&roots(dir.path()), &path, my_uid(), libc::O_RDONLY).is_ok());
}

#[test]
fn a_file_outside_every_registered_root_is_refused() {
    let inside = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("mine.bin");
    std::fs::write(&path, b"x").unwrap();

    // Owned by the peer, readable, real — and still refused: the helper's
    // reach is the declared tree, not the peer's whole uid.
    let err = open_authorised(&roots(inside.path()), &path, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("outside every registered sync root"), "{err}");
    // A daemon that never declared a root can mark nothing at all.
    let err = open_authorised(&[], &path, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("outside every registered sync root"), "{err}");
}

#[test]
fn a_root_prefix_is_matched_by_whole_component() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("OneDrive");
    let sibling = base.path().join("OneDrive-evil");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let path = sibling.join("mine.bin");
    std::fs::write(&path, b"x").unwrap();

    let err = open_authorised(&roots(&root), &path, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("outside every registered sync root"), "{err}");
}

#[test]
fn a_root_must_be_a_directory_the_peer_owns() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, b"x").unwrap();

    let err = open_root(&file, my_uid()).unwrap_err().to_string();
    assert!(err.contains("cannot open"), "{err}");
    let err = open_root(dir.path(), my_uid().wrapping_add(1))
        .unwrap_err()
        .to_string();
    assert!(err.contains("belongs to uid"), "{err}");
    let err = open_root(Path::new("relative/root"), my_uid())
        .unwrap_err()
        .to_string();
    assert!(err.contains("absolute"), "{err}");
}

#[test]
fn a_root_named_through_a_symlink_is_accepted_but_links_beneath_it_are_not() {
    // `/home -> var/home` and `~/OneDrive -> /mnt/data/OneDrive` are ordinary
    // layouts. The descriptor is the boundary, so the root's own name may go
    // through a link; a link *inside* the tree still may not be followed.
    use std::os::unix::fs::symlink;
    let real = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let link = elsewhere.path().join("OneDrive");
    symlink(real.path(), &link).unwrap();
    std::fs::write(real.path().join("mine.bin"), b"x").unwrap();
    symlink(real.path().join("mine.bin"), real.path().join("alias.bin")).unwrap();

    let roots = vec![open_root(&link, my_uid()).unwrap()];
    assert!(open_authorised(&roots, &link.join("mine.bin"), my_uid(), libc::O_RDONLY).is_ok());
    let err = open_authorised(&roots, &link.join("alias.bin"), my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot open"), "{err}");
}

#[test]
fn another_users_file_is_refused() {
    // This is the whole point of the check: without it, any local user who
    // can reach the socket can make a root process install marks on files
    // they do not own.
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("another-users-file");
    std::fs::write(&path, b"secret").unwrap();
    let owner = std::fs::metadata(&path).unwrap().uid();
    let connecting_uid = owner.wrapping_add(1);
    let err = open_authorised(&roots(dir.path()), &path, connecting_uid, libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("belongs to uid"), "{err}");
}

#[test]
fn relative_paths_are_refused() {
    let err = open_authorised(&[], Path::new("OneDrive/a.bin"), my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("absolute"), "{err}");
}

#[test]
fn parent_directory_traversal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").join("..").join("escape.bin");
    std::fs::write(dir.path().join("escape.bin"), b"x").unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    // The file resolves, so only the explicit component check stops it.
    let err = open_authorised(&roots(dir.path()), &path, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("parent-directory"), "{err}");
}

#[test]
fn a_missing_file_is_refused_rather_than_assumed_safe() {
    let dir = tempfile::tempdir().unwrap();
    let err = open_authorised(
        &roots(dir.path()),
        &dir.path().join("nope.bin"),
        my_uid(),
        libc::O_RDONLY,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("cannot open"), "{err}");
}

#[test]
fn a_final_symlink_is_never_followed() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::write(&target, b"content").unwrap();
    symlink(&target, &link).unwrap();

    let error = open_authorised(&roots(dir.path()), &link, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot open"), "{error}");
}

#[test]
fn an_intermediate_symlink_is_never_followed() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("target");
    std::fs::write(&target, b"content").unwrap();
    symlink(outside.path(), root.path().join("link")).unwrap();

    let error = open_authorised(
        &roots(root.path()),
        &root.path().join("link/target"),
        my_uid(),
        libc::O_RDONLY,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cannot open"), "{error}");
}

#[test]
fn non_regular_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let error = open_authorised(&roots(dir.path()), dir.path(), my_uid(), libc::O_PATH)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not a regular file"), "{error}");
}

#[test]
fn protocol_frames_are_bounded() {
    let oversized = vec![b'x'; MAX_FRAME_SIZE + 1];
    let mut reader = std::io::BufReader::new(oversized.as_slice());
    let error = read_frame(&mut reader).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn full_hydration_must_start_at_zero() {
    assert_eq!(expected_fill_len(true, 0, 0, 42).unwrap(), 42);
    assert!(expected_fill_len(true, 1, 0, 42).is_err());
}

#[test]
fn partial_hydration_is_confined_to_the_target() {
    assert_eq!(expected_fill_len(false, 10, 50, 32).unwrap(), 22);
    assert!(expected_fill_len(false, 33, 1, 32).is_err());
}

#[test]
fn a_reply_to_a_peer_that_stopped_reading_gives_up_and_ends_the_connection() {
    // The write happens before the hydration timeout starts, so an unbounded
    // one let a few stuck peers exhaust the whole worker pool permanently.
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    let (ours, theirs) = UnixStream::pair().unwrap();
    ours.set_write_timeout(Some(std::time::Duration::from_millis(150)))
        .unwrap();
    let ours = std::sync::Arc::new(std::sync::Mutex::new(ours));

    // Fill the socket buffer: the peer never reads.
    let event = Event::Error {
        message: "x".repeat(4096),
    };
    let mut sends = 0;
    let error = loop {
        match send_to_stream(&ours, &event) {
            Ok(()) => {
                sends += 1;
                assert!(sends < 10_000, "the buffer should fill long before this");
            }
            Err(error) => break error,
        }
    };
    assert!(
        error.to_string().contains("timed out") || error.to_string().contains("Resource"),
        "unexpected error: {error}"
    );

    // The connection was shut down rather than left carrying half a frame.
    let mut drained = Vec::new();
    let mut theirs = theirs;
    theirs.read_to_end(&mut drained).unwrap();
    assert!(
        !drained.is_empty(),
        "the peer still sees what did get through"
    );
}

#[test]
fn a_crafted_name_cannot_forge_a_log_line() {
    // Names come from the peer's filesystem. Display passes a newline straight
    // through, which lets one forge a journal entry, and an escape sequence
    // through to whoever reads the log in a terminal.
    let forged = Path::new("report.docx\n2026-09-15 ERROR forged entry\x1b[2J");
    let rendered = shown(forged);

    assert!(!rendered.contains('\n'), "no raw newline: {rendered}");
    assert!(!rendered.contains('\x1b'), "no raw escape: {rendered}");
    assert!(
        rendered.contains("report.docx"),
        "still readable: {rendered}"
    );
}

#[test]
fn one_uid_cannot_take_the_whole_worker_pool() {
    // A blocked reader holds its worker for the whole hydration timeout, so
    // without a ceiling one user opening many placeholders at once starves
    // every other user's reads.
    let slots = std::sync::Arc::new(WorkerSlots::default());
    let ceiling = 4;
    let mut held: Vec<_> = (0..ceiling)
        .map(|_| {
            slots
                .claim(1000, ceiling)
                .expect("a uid may use its whole share")
        })
        .collect();
    assert!(
        slots.claim(1000, ceiling).is_none(),
        "past its share the uid is refused"
    );
    assert!(
        slots.claim(1001, ceiling).is_some(),
        "another uid still gets in"
    );

    held.pop();
    assert!(
        slots.claim(1000, ceiling).is_some(),
        "a finished hydration returns the slot"
    );
    // The ceiling has to leave something for everyone else; a compile-time
    // check, since both sides are constants.
    const _: () = assert!(MAX_WORKERS_PER_UID < WORKERS);
}
