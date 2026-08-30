//! The raw fanotify binding.
//!
//! `libc` does not yet expose the pre-content constants that Linux 6.14 added,
//! so they are defined here against `linux/fanotify.h`. Everything unsafe in
//! this crate is confined to this file.

use std::ffi::CString;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

/// Notification class that can fill content before it is read.
/// Requires `CAP_SYS_ADMIN`.
pub const FAN_CLASS_PRE_CONTENT: u32 = 0x0000_0008;
/// Report a per-event descriptor open error in `EventMetadata::fd` instead of
/// failing the read of the whole fanotify queue. Linux automatically denies a
/// permission event whose descriptor could not be opened.
const FAN_REPORT_FD_ERROR: u32 = 0x0000_2000;
/// A read is about to happen; the listener may fill the range first.
pub const FAN_PRE_ACCESS: u64 = 0x0010_0000;
/// A file is being opened. Fires before the opener does anything — crucially,
/// before a sparse-aware copier (`cp`, `tar`, `rsync`) issues SEEK_DATA and
/// skips the placeholder's holes without ever reading. The only hook that can
/// force such a tool to see real content is on the open itself.
pub const FAN_OPEN_PERM: u64 = 0x0001_0000;

pub const FAN_MARK_ADD: u32 = 0x0000_0001;
pub const FAN_MARK_REMOVE: u32 = 0x0000_0002;

pub const FAN_ALLOW: u32 = 0x01;
pub const FAN_DENY: u32 = 0x02;

/// Extra record carrying the byte range the reader asked for.
const FAN_EVENT_INFO_TYPE_RANGE: u8 = 6;

const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;
const FAN_INIT_FLAGS: u32 =
    FAN_CLASS_PRE_CONTENT | FAN_CLOEXEC | FAN_NONBLOCK | FAN_REPORT_FD_ERROR;

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
    /// True for an open-permission event, false for a pre-content read.
    pub is_open: bool,
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
            libc::fanotify_init(FAN_INIT_FLAGS, (libc::O_RDWR | libc::O_LARGEFILE) as u32)
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
    pub fn watch_fd(&self, fd: RawFd) -> io::Result<()> {
        self.mark_fd(FAN_MARK_ADD, fd)
    }

    pub fn unwatch_fd(&self, fd: RawFd) -> io::Result<()> {
        self.mark_fd(FAN_MARK_REMOVE, fd)
    }

    /// Mark the exact object already opened by the caller.
    ///
    /// The user-supplied pathname must not be reopened after authorisation: a
    /// rename in between could make the mark land on a different inode. Resolve
    /// the helper's own live descriptor through procfs instead. That magic link
    /// is bound to this descriptor, not to the original pathname, and the
    /// owning `File` remains alive for the whole syscall.
    ///
    /// In particular, do not pass an `O_PATH` descriptor with a NULL pathname.
    /// Although newer upstream kernels accept that form, deployed HSM-enabled
    /// kernels can reject it with `EBADF`.
    fn mark_fd(&self, flags: u32, fd: RawFd) -> io::Result<()> {
        let target = descriptor_path(fd);
        // SAFETY: the fanotify descriptor and C string remain valid throughout
        // the call. `target` names the exact descriptor borrowed by the caller.
        let rc = unsafe {
            libc::fanotify_mark(
                self.fd.as_raw_fd(),
                flags,
                FAN_PRE_ACCESS | FAN_OPEN_PERM,
                libc::AT_FDCWD,
                target.as_ptr(),
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
                if meta.fd >= 0 {
                    // The kernel transfers ownership even when the surrounding
                    // record is unusable; do not leak it on the defensive path.
                    unsafe { libc::close(meta.fd) };
                }
                break;
            }
            if meta.fd < 0 {
                // With FAN_REPORT_FD_ERROR the kernel has already denied this
                // permission event. Skipping only this record keeps the group
                // alive for every other placeholder.
                tracing::debug!(
                    pid = meta.pid,
                    error = -meta.fd,
                    "fanotify could not open an event target"
                );
                cursor += event_len;
                continue;
            }
            // SAFETY: the kernel handed us this descriptor; we own it now.
            let fd = unsafe { OwnedFd::from_raw_fd(meta.fd) };

            let metadata_len = meta.metadata_len as usize;
            if metadata_len < std::mem::size_of::<EventMetadata>() || metadata_len > event_len {
                cursor += event_len;
                continue;
            }

            let (offset, length) =
                read_range(&buf[cursor + metadata_len..cursor + event_len]).unwrap_or((0, 0));

            events.push(PreContentEvent {
                path: path_of(&fd),
                fd,
                pid: meta.pid,
                offset,
                length,
                is_open: meta.mask & FAN_OPEN_PERM != 0,
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

pub(super) fn descriptor_path(fd: RawFd) -> CString {
    CString::new(format!("/proc/self/fd/{fd}"))
        .expect("a decimal file descriptor cannot contain a NUL byte")
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
        if header.info_type == FAN_EVENT_INFO_TYPE_RANGE && len >= std::mem::size_of::<RangeInfo>()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw bytes of a `#[repr(C)]` value, as the kernel would lay them out.
    fn as_bytes<T: Copy>(v: &T) -> Vec<u8> {
        // SAFETY: reading `size_of::<T>()` bytes of a value we hold by ref.
        unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
            .to_vec()
    }

    fn range_record(offset: u64, count: u64) -> RangeInfo {
        RangeInfo {
            header: InfoHeader {
                info_type: FAN_EVENT_INFO_TYPE_RANGE,
                pad: 0,
                len: std::mem::size_of::<RangeInfo>() as u16,
            },
            pad: 0,
            offset,
            count,
        }
    }

    #[test]
    fn read_range_decodes_a_range_record() {
        let buf = as_bytes(&range_record(4096, 65536));
        assert_eq!(read_range(&buf), Some((4096, 65536)));
    }

    #[test]
    fn read_range_skips_records_of_other_types_to_find_the_range() {
        // A leading non-range info record, then the range: the parser must walk
        // past the first by its len and still find the second.
        let other = InfoHeader {
            info_type: 1,
            pad: 0,
            len: 8,
        };
        let mut buf = as_bytes(&other);
        buf.extend_from_slice(&[0u8; 4]); // pad the 'other' record out to len 8
        buf.extend_from_slice(&as_bytes(&range_record(512, 16)));
        assert_eq!(read_range(&buf), Some((512, 16)));
    }

    #[test]
    fn read_range_returns_none_for_empty_or_rangeless_buffers() {
        assert_eq!(read_range(&[]), None);
        // A record whose declared len is zero must not loop forever.
        let zero = InfoHeader {
            info_type: 1,
            pad: 0,
            len: 0,
        };
        assert_eq!(read_range(&as_bytes(&zero)), None);
    }

    #[test]
    fn mark_targets_are_bound_to_the_helpers_live_descriptor() {
        assert_eq!(
            descriptor_path(42).to_bytes(),
            b"/proc/self/fd/42",
            "never regress to O_PATH plus a NULL fanotify pathname"
        );
    }

    #[test]
    fn descriptor_open_failures_are_scoped_to_one_event() {
        assert_ne!(FAN_INIT_FLAGS & FAN_REPORT_FD_ERROR, 0);
    }
}
