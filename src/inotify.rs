//! An inotify watch on one directory, for waiting on files to change there
//! (`gitfold watch`, `gitfold inbox --wait`).

use std::ffi::CString;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// Events that mean the watch itself is gone or incomplete: the directory was
/// deleted or moved, or events were dropped.
pub const LOST: u32 =
    libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF | libc::IN_Q_OVERFLOW;

/// One inotify watch on a directory (also told when the directory itself is
/// deleted or moved).
pub struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    pub fn watch(dir: &Path, mask: u32) -> Result<Inotify> {
        // SAFETY: plain syscalls; the descriptor is owned from here on.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("inotify");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = CString::new(dir.as_os_str().as_bytes())?;
        let mask = mask | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("watching {}", dir.display()));
        }
        Ok(Inotify { fd })
    }

    /// The events (mask, name) that come within `limit` (forever if `None`),
    /// as one read returns them; empty if none came in time.
    pub fn read(&self, limit: Option<Duration>) -> Result<Vec<(u32, Vec<u8>)>> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = limit.map_or(-1, |d| d.as_millis().min(i32::MAX as u128) as i32);
        loop {
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                return Err(e).context("waiting for inotify");
            }
            if n == 0 {
                return Ok(Vec::new());
            }
            break;
        }
        let mut buf = vec![0u8; 64 * 1024];
        // SAFETY: reading into a buffer of the given length.
        let len = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if len < 0 {
            return Err(std::io::Error::last_os_error()).context("reading inotify");
        }
        let len = len as usize;
        let mut events = Vec::new();
        let mut at = 0;
        let header = std::mem::size_of::<libc::inotify_event>();
        while at + header <= len {
            // SAFETY: the kernel writes whole events; read unaligned.
            let event: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            let name = &buf[at + header..at + header + event.len as usize];
            let name = name.split(|b| *b == 0).next().unwrap_or_default().to_vec();
            events.push((event.mask, name));
            at += header + event.len as usize;
        }
        Ok(events)
    }
}
