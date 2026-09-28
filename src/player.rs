//! Audio playback with mpv: one file at a time, paused and resumed over mpv's IPC socket.
//! mpv gets SIGTERM if swayfin dies, so it never outlives us.

use std::{
    env,
    io::Write,
    os::unix::{net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use smithay_client_toolkit::reexports::calloop::channel::Sender;

struct Playing {
    path: PathBuf,
    pid: u32,
    socket: PathBuf,
    paused: bool,
}

pub struct Player {
    playing: Option<Playing>,
    /// Receives the pid of each mpv that exits.
    ended: Sender<u32>,
}

impl Player {
    pub fn new(ended: Sender<u32>) -> Self {
        Self {
            playing: None,
            ended,
        }
    }

    /// The file playing (or paused), and whether it's paused.
    pub fn state(&self) -> Option<(&Path, bool)> {
        self.playing.as_ref().map(|p| (p.path.as_path(), p.paused))
    }

    /// Plays `path`, or pauses/resumes it if it's the current file. Anything else that
    /// was playing stops.
    pub fn toggle(&mut self, path: PathBuf) {
        if let Some(p) = self.playing.as_mut().filter(|p| p.path == path) {
            p.paused = !p.paused;
            send_command(p.socket.clone(), r#"{"command":["cycle","pause"]}"#);
            return;
        }
        self.stop();
        self.start(path);
    }

    fn start(&mut self, path: PathBuf) {
        let runtime = env::var_os("XDG_RUNTIME_DIR").map_or_else(env::temp_dir, PathBuf::from);
        let socket = runtime.join(format!("swayfin-mpv-{}.sock", std::process::id()));
        let mut cmd = Command::new("mpv");
        cmd.args(["--no-video", "--no-terminal", "--idle=no"])
            .arg(format!("--input-ipc-server={}", socket.display()))
            .arg("--")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: prctl is async-signal-safe; it only sets this child's parent-death signal.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let Ok(mut child) = cmd.spawn() else {
            return;
        };
        let pid = child.id();
        let ended = self.ended.clone();
        thread::spawn(move || {
            let _ = child.wait();
            let _ = ended.send(pid);
        });
        self.playing = Some(Playing {
            path,
            pid,
            socket,
            paused: false,
        });
    }

    pub fn stop(&mut self) {
        if let Some(p) = self.playing.take() {
            // SAFETY: plain kill(2) on the child we started. Until its exit is reported
            // (which clears `playing`) the pid is at most just reaped, not reused.
            unsafe {
                libc::kill(p.pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }

    /// An mpv exited (the file ended, or it was stopped).
    pub fn ended(&mut self, pid: u32) -> bool {
        if self.playing.as_ref().is_some_and(|p| p.pid == pid) {
            self.playing = None;
            return true;
        }
        false
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Sends one JSON command to mpv. Off-thread, retrying briefly: right after starting,
/// mpv may not have created its socket yet.
fn send_command(socket: PathBuf, command: &'static str) {
    thread::spawn(move || {
        for _ in 0..40 {
            if let Ok(mut s) = UnixStream::connect(&socket) {
                let _ = s.write_all(format!("{command}\n").as_bytes());
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
    });
}
