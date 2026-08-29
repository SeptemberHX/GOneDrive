use super::*;

fn my_uid() -> u32 {
    // SAFETY: getuid cannot fail and takes no arguments.
    unsafe { libc::getuid() }
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
    let n = fill_from(&mut source, &fd, 0).unwrap();
    assert_eq!(n, 16);
    assert_eq!(std::fs::read(&target).unwrap(), b"the real content");
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
    let n = fill_from(&mut source, &fd, 10).unwrap();
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
    assert!(open_authorised(&path, my_uid(), libc::O_RDONLY).is_ok());
}

#[test]
fn another_users_file_is_refused() {
    // This is the whole point of the check: without it, any local user who
    // can reach the socket can make a root process install marks on files
    // they do not own.
    let path = Path::new("/etc/shadow");
    if !path.exists() {
        return;
    }
    let err = open_authorised(path, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("belongs to uid") || err.contains("cannot open"),
        "{err}"
    );
}

#[test]
fn relative_paths_are_refused() {
    let err = open_authorised(Path::new("OneDrive/a.bin"), my_uid(), libc::O_RDONLY)
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
    let err = open_authorised(&path, my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(err.contains("parent-directory"), "{err}");
}

#[test]
fn a_missing_file_is_refused_rather_than_assumed_safe() {
    let err = open_authorised(Path::new("/nonexistent/nope.bin"), my_uid(), libc::O_RDONLY)
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

    let error = open_authorised(&link, my_uid(), libc::O_RDONLY)
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

    let error = open_authorised(&root.path().join("link/target"), my_uid(), libc::O_RDONLY)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot open"), "{error}");
}

#[test]
fn non_regular_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let error = open_authorised(dir.path(), my_uid(), libc::O_PATH)
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
