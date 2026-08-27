//! The raw fanotify binding.
//!
//! `libc` does not yet expose the pre-content constants that Linux 6.14 added,
//! so they are defined here against `linux/fanotify.h`. Everything unsafe in
//! this crate is confined to this file.

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

/// Notification class that can fill content before it is read.
/// Requires `CAP_SYS_ADMIN`.
pub const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
/// A read is about to happen; the listener may fill the range first.
pub const FAN_PRE_ACCESS: u64 = 0x0010_0000;

pub const FAN_MARK_ADD: u32 = 0x0000_0001;
pub const FAN_MARK_REMOVE: u32 = 0x0000_0002;

pub const FAN_ALLOW: u32 = 0x01;
pub const FAN_DENY: u32 = 0x02;

/// Extra record carrying the byte range the reader asked for.
const FAN_EVENT_INFO_TYPE_RANGE: u8 = 6;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct EventMetadata {
    event_len: u32,
    vers: u8,
    reserved: u8,
    metadata_len: u16,
    mask: u64,
    fd: i32,
    pid: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct InfoHeader {
    info_type: u8,
    pad: u8,
    len: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RangeInfo {
    header: InfoHeader,
    pad: u32,
    offset: u64,
    count: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Response {
    fd: i32,
    response: u32,
}

/// One pre-content event: a reader is blocked until it is answered.
#[derive(Debug)]
pub struct PreContentEvent {
    /// A descriptor for the file, already past the permission check. Writing
    /// through it cannot re-enter the hook.
    pub fd: OwnedFd,
    pub pid: i32,
    pub path: Option<PathBuf>,
    pub offset: u64,
    pub length: u64,
}

pub struct Fanotify {
    fd: OwnedFd,
}

impl Fanotify {
    /// Open a pre-content group. Fails with `EPERM` without `CAP_SYS_ADMIN`,
    /// and with `EINVAL` on a kernel older than 6.14.
    pub fn open() -> io::Result<Self> {
        // SAFETY: a plain syscall with no pointer arguments.
        let fd = unsafe {
            libc::fanotify_init(
                FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC | FAN_NONBLOCK,
                (libc::O_RDWR | libc::O_LARGEFILE) as u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh, valid, owned descriptor.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn as_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Watch a single file.
    ///
    /// Deliberately an inode mark rather than a mount mark: a mount mark routes
    /// every read on the filesystem through this process, which is both a
    /// performance disaster and a much larger blast radius for a bug in a
    /// root-owned daemon.
    pub fn watch(&self, path: &std::path::Path) -> io::Result<()> {
        self.mark(FAN_MARK_ADD, path)
    }

    pub fn unwatch(&self, path: &std::path::Path) -> io::Result<()> {
        self.mark(FAN_MARK_REMOVE, path)
    }

    fn mark(&self, flags: u32, path: &std::path::Path) -> io::Result<()> {
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: `c_path` is a valid NUL-terminated string; `self.fd` is valid.
        let rc = unsafe {
            libc::fanotify_mark(
                self.fd.as_raw_fd(),
                flags,
                FAN_PRE_ACCESS,
                libc::AT_FDCWD,
                c_path.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read whatever events are pending. Returns an empty vector when there are
    /// none, because the group is opened non-blocking and polled.
    pub fn read_events(&self) -> io::Result<Vec<PreContentEvent>> {
        let mut buf = vec![0u8; 8192];
        // SAFETY: writing into a buffer we own, bounded by its length.
        let n = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(Vec::new());
            }
            return Err(err);
        }

        let mut events = Vec::new();
        let mut cursor = 0usize;
        let len = n as usize;

        while cursor + std::mem::size_of::<EventMetadata>() <= len {
            // SAFETY: the kernel guarantees a well-formed metadata record at
            // this offset, and the bound above ensures it is in range.
            let meta: EventMetadata =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(cursor) as *const _) };
            let event_len = meta.event_len as usize;
            if event_len < std::mem::size_of::<EventMetadata>() || cursor + event_len > len {
                break;
            }
            if meta.fd < 0 {
                cursor += event_len;
                continue;
            }
            // SAFETY: the kernel handed us this descriptor; we own it now.
            let fd = unsafe { OwnedFd::from_raw_fd(meta.fd) };

            let (offset, length) = read_range(
                &buf[cursor + meta.metadata_len as usize..cursor + event_len],
            )
            .unwrap_or((0, 0));

            events.push(PreContentEvent {
                path: path_of(&fd),
                fd,
                pid: meta.pid,
                offset,
                length,
            });
            cursor += event_len;
        }
        Ok(events)
    }

    /// Unblock (or fail) a waiting reader.
    ///
    /// Answering is mandatory: a pre-content event left unanswered leaves the
    /// reading process blocked forever.
    pub fn respond(&self, fd: RawFd, allow: bool) -> io::Result<()> {
        let response = Response {
            fd,
            response: if allow { FAN_ALLOW } else { FAN_DENY },
        };
        // SAFETY: writing a correctly-sized response struct to the group fd.
        let rc = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &response as *const Response as *const libc::c_void,
                std::mem::size_of::<Response>(),
            )
        };
        if rc != std::mem::size_of::<Response>() as isize {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Pull the range record out of an event's trailing info records.
fn read_range(mut tail: &[u8]) -> Option<(u64, u64)> {
    while tail.len() >= std::mem::size_of::<InfoHeader>() {
        // SAFETY: bounded by the check above.
        let header: InfoHeader = unsafe { std::ptr::read_unaligned(tail.as_ptr() as *const _) };
        let len = header.len as usize;
        if len == 0 || len > tail.len() {
            return None;
        }
        if header.info_type == FAN_EVENT_INFO_TYPE_RANGE
            && len >= std::mem::size_of::<RangeInfo>()
        {
            // SAFETY: the length check above covers the whole struct.
            let range: RangeInfo = unsafe { std::ptr::read_unaligned(tail.as_ptr() as *const _) };
            return Some((range.offset, range.count));
        }
        tail = &tail[len..];
    }
    None
}

/// Resolve the file behind an event descriptor.
///
/// `FAN_REPORT_FID` cannot be combined with `FAN_CLASS_PRE_CONTENT`, so the
/// path has to come from the descriptor itself.
fn path_of(fd: &OwnedFd) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()
}
