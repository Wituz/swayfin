//! Watches the shown folder with inotify, so changes made by anything (scripts, other
//! apps, our own jobs) show up without navigating.

use std::{
    ffi::CString,
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

/// Everything that changes what the list shows: entries appearing, disappearing,
/// renamed, written (size, mtime) or re-permissioned, and the folder itself going away.
const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_EXCL_UNLINK;

pub struct Watcher {
    fd: OwnedFd,
    wd: Option<i32>,
    /// The downloads redirect's state folder.
    state_wd: Option<i32>,
}

impl AsFd for Watcher {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Watcher {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain syscall; the fd is checked and then owned.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self {
            fd,
            wd: None,
            state_wd: None,
        })
    }

    /// Watches `dir` instead of the previous folder. A folder that can't be watched
    /// (gone, no permission) just isn't.
    pub fn watch(&mut self, dir: &Path) {
        let raw = self.fd.as_raw_fd();
        if let Some(wd) = self.wd.take() {
            // SAFETY: removing our own watch from our own inotify fd.
            unsafe { libc::inotify_rm_watch(raw, wd) };
        }
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: valid NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(raw, c.as_ptr(), MASK) };
        self.wd = (wd >= 0).then_some(wd);
    }

    /// Also watches the downloads redirect's state folder (creating it), for its state
    /// file being replaced or removed.
    pub fn watch_state(&mut self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else {
            return;
        };
        let mask = libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_ONLYDIR;
        // SAFETY: valid NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), mask) };
        self.state_wd = (wd >= 0).then_some(wd);
    }

    /// Reads all pending events. Returns whether any were for the current folder, and
    /// whether any were for the redirect state.
    pub fn drain(&mut self) -> (bool, bool) {
        let mut buf = [0u8; 4096];
        let mut relevant = false;
        let mut state = false;
        loop {
            // SAFETY: reading into our own buffer from our non-blocking fd.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                return (relevant, state);
            }
            let mut off = 0;
            let head = std::mem::size_of::<libc::inotify_event>();
            while off + head <= n as usize {
                // SAFETY: the kernel writes whole inotify_event records (+ name) into buf.
                let ev = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(off).cast::<libc::inotify_event>())
                };
                // Events for a watch we already removed can still be queued.
                relevant |= Some(ev.wd) == self.wd;
                state |= Some(ev.wd) == self.state_wd;
                off += head + ev.len as usize;
            }
        }
    }
}
