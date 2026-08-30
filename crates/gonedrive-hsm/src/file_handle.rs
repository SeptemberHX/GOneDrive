//! Reopenable inode handles without one permanently open descriptor per file.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::sync::{Arc, Mutex};

#[repr(C)]
struct FileHandleBuffer {
    header: libc::file_handle,
    bytes: [u8; libc::MAX_HANDLE_SZ as usize],
}

impl FileHandleBuffer {
    fn empty() -> Self {
        Self {
            header: libc::file_handle {
                handle_bytes: libc::MAX_HANDLE_SZ as u32,
                handle_type: 0,
                f_handle: [],
            },
            bytes: [0; libc::MAX_HANDLE_SZ as usize],
        }
    }

    fn from_handle(handle: &OpaqueFileHandle) -> Self {
        let mut buffer = Self::empty();
        buffer.header.handle_bytes = handle.bytes.len() as u32;
        buffer.header.handle_type = handle.kind;
        buffer.bytes[..handle.bytes.len()].copy_from_slice(&handle.bytes);
        buffer
    }
}

#[derive(Debug, PartialEq, Eq)]
struct OpaqueFileHandle {
    kind: i32,
    bytes: Box<[u8]>,
}

impl OpaqueFileHandle {
    fn capture(file: &File) -> std::io::Result<(i32, Self)> {
        let mut buffer = FileHandleBuffer::empty();
        let mut mount_id = 0;
        let empty = b"\0";
        // SAFETY: `buffer` includes MAX_HANDLE_SZ writable bytes immediately
        // after the C header, and AT_EMPTY_PATH addresses the already-open,
        // authorised descriptor rather than reopening an attacker-controlled
        // pathname.
        let rc = unsafe {
            libc::name_to_handle_at(
                file.as_raw_fd(),
                empty.as_ptr().cast(),
                &mut buffer.header,
                &mut mount_id,
                libc::AT_EMPTY_PATH,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let len = buffer.header.handle_bytes as usize;
        if len == 0 || len > buffer.bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("kernel returned an invalid {len}-byte file handle"),
            ));
        }
        Ok((
            mount_id,
            Self {
                kind: buffer.header.handle_type,
                bytes: buffer.bytes[..len].into(),
            },
        ))
    }
}

#[derive(Default)]
pub(super) struct MountHandles {
    anchors: Mutex<HashMap<i32, Arc<File>>>,
}

impl MountHandles {
    fn anchor(&self, mount_id: i32) -> std::io::Result<Arc<File>> {
        let mut anchors = self.anchors.lock().unwrap();
        if let Some(anchor) = anchors.get(&mount_id) {
            return Ok(anchor.clone());
        }
        // Keep a descriptor for the mount point itself. A descriptor for the
        // first placeholder appears to work until that inode is replaced or
        // deleted; on deployed kernels it can then make open_by_handle_at fail
        // with EBADF for every otherwise-valid handle on the mount.
        let anchor = Arc::new(open_mount_anchor(mount_id)?);
        anchors.insert(mount_id, anchor.clone());
        Ok(anchor)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.anchors.lock().unwrap().len()
    }
}

pub(super) struct PersistentFileHandle {
    mount: Arc<File>,
    handle: OpaqueFileHandle,
}

impl PersistentFileHandle {
    fn capture(file: &File, mounts: &MountHandles) -> std::io::Result<Self> {
        let (mount_id, handle) = OpaqueFileHandle::capture(file)?;
        Ok(Self {
            mount: mounts.anchor(mount_id)?,
            handle,
        })
    }

    fn open(&self) -> std::io::Result<File> {
        let mut buffer = FileHandleBuffer::from_handle(&self.handle);
        // SAFETY: the opaque bytes came from name_to_handle_at on this mount,
        // the backing buffer has the required C layout, and a successful fd is
        // immediately transferred into File ownership.
        let fd = unsafe {
            libc::open_by_handle_at(
                self.mount.as_raw_fd(),
                &mut buffer.header,
                libc::O_PATH | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor returned by open_by_handle_at.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_mount_anchor(mount_id: i32) -> std::io::Result<File> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")?;
    for line in mountinfo.lines() {
        let mut fields = line.split_whitespace();
        let Some(id) = fields.next() else {
            continue;
        };
        if id.parse::<i32>().ok() != Some(mount_id) {
            continue;
        }
        // id, parent, major:minor, root, mount point
        let mount_point = fields.nth(3).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("mountinfo entry {mount_id} has no mount point"),
            )
        })?;
        return File::open(decode_mountinfo_path(mount_point)?);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("mount id {mount_id} is absent from /proc/self/mountinfo"),
    ))
}

fn decode_mountinfo_path(encoded: &str) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            let Some(octal) = bytes.get(cursor + 1..cursor + 4) else {
                return Err(invalid_mountinfo_escape(encoded));
            };
            if !octal.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                return Err(invalid_mountinfo_escape(encoded));
            }
            decoded.push((octal[0] - b'0') * 64 + (octal[1] - b'0') * 8 + (octal[2] - b'0'));
            cursor += 4;
        } else {
            decoded.push(bytes[cursor]);
            cursor += 1;
        }
    }
    Ok(std::ffi::OsString::from_vec(decoded).into())
}

fn invalid_mountinfo_escape(encoded: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("invalid mountinfo path escape in {encoded:?}"),
    )
}

/// A precise way to reopen a marked inode. Filesystems with exportable file
/// handles use only one mount anchor; unsupported filesystems retain the old,
/// safe descriptor-per-file behavior rather than weakening unwatch semantics.
pub(super) enum ReopenableFile {
    Handle(PersistentFileHandle),
    Descriptor(File),
}

impl ReopenableFile {
    pub(super) fn capture(file: File, mounts: &MountHandles) -> (Self, Option<std::io::Error>) {
        match PersistentFileHandle::capture(&file, mounts) {
            Ok(handle) => (Self::Handle(handle), None),
            Err(error) => (Self::Descriptor(file), Some(error)),
        }
    }

    pub(super) fn with_fd<T>(
        &self,
        operation: impl FnOnce(RawFd) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        match self {
            Self::Handle(handle) => {
                let file = handle.open()?;
                operation(file.as_raw_fd())
            }
            Self::Descriptor(file) => operation(file.as_raw_fd()),
        }
    }

    #[cfg(test)]
    pub(super) fn is_descriptor_fallback(&self) -> bool {
        matches!(self, Self::Descriptor(_))
    }
}

pub(super) fn object_is_gone(error: &std::io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| code == libc::ESTALE || code == libc::ENOENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_handle_buffer_has_space_after_its_header() {
        assert_eq!(
            std::mem::size_of::<FileHandleBuffer>(),
            std::mem::size_of::<libc::file_handle>() + libc::MAX_HANDLE_SZ as usize
        );
    }

    #[test]
    fn supported_filesystems_share_one_mount_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let first = File::open(dir.path()).unwrap();
        let second_path = dir.path().join("second");
        std::fs::write(&second_path, b"x").unwrap();
        let second = File::open(&second_path).unwrap();
        let mounts = MountHandles::default();

        let (first, first_error) = ReopenableFile::capture(first, &mounts);
        let (second, second_error) = ReopenableFile::capture(second, &mounts);
        if first_error.is_none() && second_error.is_none() {
            assert!(!first.is_descriptor_fallback());
            assert!(!second.is_descriptor_fallback());
            assert_eq!(mounts.len(), 1);
        }
    }

    #[test]
    fn mountinfo_paths_are_decoded_without_assuming_utf8() {
        use std::os::unix::ffi::OsStrExt;

        let decoded = decode_mountinfo_path(r"/mnt/space\040tab\011slash\134byte\377").unwrap();
        assert_eq!(
            decoded.as_os_str().as_bytes(),
            b"/mnt/space tab\tslash\\byte\xff"
        );
        assert!(decode_mountinfo_path(r"/mnt/bad\x20").is_err());
    }

    #[test]
    fn the_current_mount_id_resolves_to_a_directory_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let file = File::open(dir.path()).unwrap();
        let (mount_id, _) = OpaqueFileHandle::capture(&file).unwrap();
        assert!(open_mount_anchor(mount_id)
            .unwrap()
            .metadata()
            .unwrap()
            .is_dir());
    }

    #[test]
    fn persistent_identity_survives_a_rename_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let before_path = dir.path().join("before");
        let after_path = dir.path().join("after");
        std::fs::write(&before_path, b"x").unwrap();
        let before = File::open(&before_path).unwrap();
        let before = OpaqueFileHandle::capture(&before).ok();

        std::fs::rename(&before_path, &after_path).unwrap();
        let after = File::open(&after_path).unwrap();
        let after = OpaqueFileHandle::capture(&after).ok();

        if let (Some(before), Some(after)) = (before, after) {
            assert_eq!(before, after);
        }
    }

    #[test]
    fn stale_and_missing_handles_are_treated_as_already_gone() {
        assert!(object_is_gone(&std::io::Error::from_raw_os_error(
            libc::ESTALE
        )));
        assert!(object_is_gone(&std::io::Error::from_raw_os_error(
            libc::ENOENT
        )));
        assert!(!object_is_gone(&std::io::Error::from_raw_os_error(
            libc::EPERM
        )));
    }
}
