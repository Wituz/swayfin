//! Downloads redirect (Shift+D): new files in the downloads folder are moved into a
//! chosen folder. One redirect is shared by all windows through a state file in
//! $XDG_RUNTIME_DIR, and a `swayfin --downloads` process does the moving, so it keeps
//! going after the windows close. That process exits when the redirect is turned off.

use std::{
    collections::HashMap,
    env,
    ffi::{CString, OsStr, OsString},
    fs::{self, File},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::ops;

pub const DAEMON_ARG: &str = "--downloads";
/// Holds the target folder's path; absent when the redirect is off.
const STATE: &str = "downloads";
/// Held (flock) by the running daemon, so there is only one.
const LOCK: &str = "downloads.lock";
/// A file is moved once this long passes without further writes or renames to it.
const SETTLE: Duration = Duration::from_millis(500);
/// Browsers' names for downloads in progress. Firefox also keeps an empty placeholder
/// under the final name until its `.part` is renamed over it.
const PARTIAL: [&str; 5] = [".part", ".crdownload", ".download", ".partial", ".tmp"];

/// Where the state and lock files live.
pub fn state_dir() -> PathBuf {
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("swayfin"),
        // SAFETY: getuid can't fail.
        _ => env::temp_dir().join(format!("swayfin-{}", unsafe { libc::getuid() })),
    }
}

/// The folder downloads are redirected to, if the redirect is on.
pub fn target() -> Option<PathBuf> {
    let bytes = fs::read(state_dir().join(STATE)).ok()?;
    (!bytes.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(&bytes)))
}

/// Shift+D in `here`: redirects downloads there, or turns the redirect off if it already
/// goes there (or `here` is the downloads folder). Windows see the change through their
/// watch on the state folder.
pub fn toggle(here: PathBuf) {
    thread::spawn(move || {
        let dir = state_dir();
        let state = dir.join(STATE);
        if target().as_ref() == Some(&here) || here == downloads_dir() {
            let _ = fs::remove_file(state);
            return;
        }
        let _ = fs::create_dir_all(&dir);
        let tmp = dir.join(format!(".{STATE}-{}", std::process::id()));
        if fs::write(&tmp, here.as_os_str().as_bytes()).is_err() || fs::rename(&tmp, &state).is_err()
        {
            let _ = fs::remove_file(tmp);
            return;
        }
        run_daemon();
    });
}

/// A window started while the redirect is on: makes sure the daemon runs (it doesn't
/// survive a crash, and the state file does).
pub fn resume() {
    thread::spawn(run_daemon);
}

/// Starts the daemon from our own binary (even if it was rebuilt since), detached into
/// its own process group, and reaps it. It exits at once if one is already running.
fn run_daemon() {
    let spawned = Command::new("/proc/self/exe")
        .arg(DAEMON_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Ok(mut child) = spawned {
        let _ = child.wait();
    }
}

/// XDG_DOWNLOAD_DIR from user-dirs.dirs, else ~/Downloads.
fn downloads_dir() -> PathBuf {
    let home = env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let config = env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(|| home.join(".config"), PathBuf::from);
    let from_file = fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|text| {
            let line = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?;
            let value = line.trim().trim_matches('"');
            Some(match value.strip_prefix("$HOME") {
                Some(rest) => home.join(rest.trim_start_matches('/')),
                None => PathBuf::from(value),
            })
        });
    from_file.unwrap_or_else(|| home.join("Downloads"))
}

fn flock(file: &File, op: i32) -> bool {
    // SAFETY: flock on our own open file.
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

fn add_watch(fd: &OwnedFd, dir: &Path, mask: u32) -> Option<i32> {
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: valid NUL-terminated path, our own inotify fd.
    let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) };
    (wd >= 0).then_some(wd)
}

/// `swayfin --downloads`: moves files that finish arriving in the downloads folder into
/// the target, until the redirect is turned off.
pub fn daemon() {
    let dir = state_dir();
    let _ = fs::create_dir_all(&dir);
    let Ok(lock) = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK))
    else {
        return;
    };
    if !flock(&lock, libc::LOCK_EX | libc::LOCK_NB) {
        return;
    }
    // SAFETY: plain syscall; the fd is checked and then owned.
    let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if raw < 0 {
        return;
    }
    // SAFETY: a fresh fd we own.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let downloads = downloads_dir();
    let (Some(state_wd), Some(downloads_wd)) = (
        add_watch(&fd, &dir, libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_ONLYDIR),
        add_watch(
            &fd,
            &downloads,
            libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_ONLYDIR,
        ),
    ) else {
        return;
    };

    let mut target = target();
    // Names that arrived, and when they're considered complete.
    let mut pending: HashMap<OsString, Instant> = HashMap::new();
    let mut buf = [0u8; 4096];
    loop {
        if target.is_none() {
            // Turned off. A window may have turned it back on and started a new daemon
            // meanwhile: let that one take over, or carry on if none has.
            flock(&lock, libc::LOCK_UN);
            target = self::target();
            if target.is_none() || !flock(&lock, libc::LOCK_EX | libc::LOCK_NB) {
                return;
            }
        }

        let now = Instant::now();
        let timeout = pending.values().min().map_or(-1, |due| {
            due.saturating_duration_since(now).as_millis() as i32 + 1
        });
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if ready > 0 {
            // SAFETY: reading into our own buffer from our inotify fd, which is readable.
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            let head = std::mem::size_of::<libc::inotify_event>();
            let mut off = 0;
            while n > 0 && off + head <= n as usize {
                // SAFETY: the kernel writes whole inotify_event records (+ name) into buf.
                let ev = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(off).cast::<libc::inotify_event>())
                };
                let name = &buf[off + head..off + head + ev.len as usize];
                let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                off += head + ev.len as usize;
                if ev.wd == state_wd {
                    target = self::target();
                } else if ev.wd == downloads_wd && !name.is_empty() {
                    pending.insert(
                        OsStr::from_bytes(name).to_os_string(),
                        Instant::now() + SETTLE,
                    );
                }
            }
        }

        let now = Instant::now();
        let due: Vec<OsString> = pending
            .iter()
            .filter(|&(_, &at)| at <= now)
            .map(|(name, _)| name.clone())
            .collect();
        for name in due {
            pending.remove(&name);
            if let Some(to) = &target {
                redirect(&downloads, &name, to);
            }
        }
    }
}

/// Moves `name` from the downloads folder into `to`, unless it's still downloading
/// (a browser's partial file, or the placeholder for one) or hidden.
fn redirect(downloads: &Path, name: &OsStr, to: &Path) {
    let bytes = name.as_bytes();
    if to == downloads
        || bytes.starts_with(b".")
        || PARTIAL.iter().any(|s| bytes.ends_with(s.as_bytes()))
    {
        return;
    }
    let in_progress = PARTIAL.iter().any(|s| {
        let mut partial = name.to_os_string();
        partial.push(s);
        fs::symlink_metadata(downloads.join(partial)).is_ok()
    });
    let src = downloads.join(name);
    if in_progress || fs::symlink_metadata(&src).is_err() {
        return;
    }
    let _ = ops::move_free(&src, to);
}
