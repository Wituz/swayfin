//! Moving files, on a worker thread per job. Name conflicts are sent to the UI as
//! questions and the worker blocks until the answer comes back.

use std::{
    env,
    ffi::{CString, OsStr, OsString},
    fs::{self, File, FileTimes, Metadata},
    io,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::symlink,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

use smithay_client_toolkit::reexports::calloop::channel::Sender;

use crate::mime;

#[derive(Clone, Copy, PartialEq)]
pub enum Choice {
    /// Replace a file, or merge a folder into the existing one.
    Replace,
    Leave,
    Suffix,
    Cancel,
}

pub struct Conflict {
    pub name: String,
    pub dest: String,
    /// Both are folders, so Replace merges.
    pub merge: bool,
    /// Both are folders or both are not; Replace is only offered then.
    pub can_replace: bool,
    /// Known conflicts after this one in the same folder.
    pub remaining: usize,
    /// The answer, and whether it applies to all later conflicts in this job.
    pub reply: mpsc::Sender<(Choice, bool)>,
}

pub enum JobMsg {
    Conflict(Conflict),
    /// The job ended. `verb` names the operation ("move", "delete"); one line per item
    /// that failed. `token` identifies transfers started by a drop (0 otherwise).
    Done {
        verb: &'static str,
        errors: Vec<String>,
        token: u64,
    },
    /// No app is set for `mime` (or the user asked to pick one): offer `apps` (id, name).
    /// With `save_always`, the pick becomes the default; otherwise the user decides.
    ChooseApp {
        file: PathBuf,
        mime: String,
        apps: Vec<(String, String)>,
        save_always: bool,
    },
    /// Files read from another app's clipboard, to paste (move if `cut`).
    Paste {
        paths: Vec<PathBuf>,
        cut: bool,
    },
    /// A create or rename finished: the new path, or the reason it failed.
    Named(Result<PathBuf, String>),
}

/// Moves (or with `copy`, copies) `items` into the folder `dest`. `token` comes back in
/// the Done message.
pub fn spawn_transfer(
    items: Vec<PathBuf>,
    dest: PathBuf,
    copy: bool,
    token: u64,
    ui: Sender<JobMsg>,
) {
    thread::spawn(move || {
        let root = items
            .first()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut job = Job {
            ui: &ui,
            root,
            copy,
            sticky: None,
            tmp_seq: 0,
            errors: Vec::new(),
        };
        let _ = job.move_into(&items, &dest);
        let _ = ui.send(JobMsg::Done {
            verb: if copy { "copy" } else { "move" },
            errors: job.errors,
            token,
        });
    });
}

/// Why `name` can't be a folder name, if it can't.
pub fn invalid_name(name: &str) -> Option<String> {
    if name.is_empty() {
        Some("Enter a name.".into())
    } else if name == "." || name == ".." {
        Some(format!("\"{name}\" can't be used as a name."))
    } else if name.contains('/') {
        Some("Names can't contain \"/\".".into())
    } else {
        None
    }
}

/// Creates the folder `name` in `dir` off the UI thread.
pub fn spawn_create(dir: PathBuf, name: String, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let path = dir.join(&name);
        let result = match fs::create_dir(&path) {
            Ok(()) => Ok(path),
            Err(e) => Err(name_error(&e, &name, &dir)),
        };
        let _ = ui.send(JobMsg::Named(result));
    });
}

/// Asks zoxide for the best folder matching the words of `query`, excluding `here`.
/// Answers with `JobMsg::Named`.
pub fn spawn_zoxide_query(query: String, here: PathBuf, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let output = Command::new("zoxide")
            .arg("query")
            .arg("--exclude")
            .arg(&here)
            .arg("--")
            .args(query.split_whitespace())
            .stdin(Stdio::null())
            .output();
        let result = match output {
            Ok(out) if out.status.success() => {
                let mut bytes = out.stdout;
                while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    bytes.pop();
                }
                Ok(PathBuf::from(OsString::from_vec(bytes)))
            }
            // zoxide's own message, e.g. "zoxide: no match found" -> "No match found."
            Ok(out) => {
                let msg = String::from_utf8_lossy(&out.stderr);
                let msg = msg.trim().trim_start_matches("zoxide:").trim();
                let mut chars = msg.chars();
                Err(match chars.next() {
                    Some(c) => format!(
                        "{}{}.",
                        c.to_uppercase(),
                        chars.as_str().trim_end_matches('.')
                    ),
                    None => format!("zoxide failed ({}).", out.status),
                })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err("zoxide isn't installed.".into()),
            Err(e) => Err(format!("Couldn't run zoxide: {}", reason(&e))),
        };
        let _ = ui.send(JobMsg::Named(result));
    });
}

/// The terminal to run programs in ($TERMINAL, or kitty) followed by whatever that
/// terminal needs before the command; `-e` is the common convention.
pub fn terminal_argv() -> Vec<OsString> {
    let terminal = env::var("TERMINAL")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "kitty".into());
    let program = Path::new(&terminal)
        .file_name()
        .map_or(terminal.clone(), |n| n.to_string_lossy().into_owned());
    let prefix: &[&str] = match program.as_str() {
        "kitty" | "foot" => &[],
        "wezterm" => &["start", "--"],
        "gnome-terminal" | "kgx" => &["--"],
        _ => &["-e"],
    };
    let mut argv = vec![OsString::from(terminal)];
    argv.extend(prefix.iter().map(OsString::from));
    argv
}

/// Starts `argv` in `dir`, detached into its own process group so it outlives swayfin,
/// and reaps it when it exits. Blocks the calling (worker) thread until then.
fn launch(argv: &[OsString], dir: &Path) -> Result<(), String> {
    let program = argv[0].to_string_lossy().into_owned();
    let spawned = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    match spawned {
        Ok(mut child) => {
            let _ = child.wait();
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(format!("{program} isn't installed")),
        Err(e) => Err(format!("couldn't start {program}: {}", reason(&e))),
    }
}

fn report_open(ui: &Sender<JobMsg>, path: &Path, why: String) {
    let _ = ui.send(JobMsg::Done {
        verb: "open",
        errors: vec![format!("{}: {why}", display_name(path))],
        token: 0,
    });
}

/// Opens `nvim .` in `dir` in a new terminal.
pub fn spawn_editor(dir: PathBuf, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let mut argv = terminal_argv();
        argv.extend(["nvim", "."].map(OsString::from));
        if let Err(why) = launch(&argv, &dir) {
            report_open(&ui, &dir, why);
        }
    });
}

/// Opens `file` with its type's default app, or asks the user to choose one (which then
/// becomes the default) when there is none.
pub fn spawn_open(file: PathBuf, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let mime = mime::detect(&file);
        match mime::default_app(&mime) {
            Some(app) => open_with_app(&app, &file, &ui),
            None => {
                let apps = mime::candidates(&mime);
                let _ = ui.send(JobMsg::ChooseApp {
                    file,
                    mime,
                    apps,
                    save_always: true,
                });
            }
        }
    });
}

/// Alt+Enter: asks which app to open `file` with.
pub fn spawn_open_with(file: PathBuf, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let mime = mime::detect(&file);
        let apps = mime::candidates(&mime);
        let _ = ui.send(JobMsg::ChooseApp {
            file,
            mime,
            apps,
            save_always: false,
        });
    });
}

/// Opens `file` with the app `id`, first making it the default for `save` if given.
pub fn spawn_launch(file: PathBuf, id: String, save: Option<String>, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        if let Some(mime) = save {
            if let Err(e) = mime::set_default(&mime, &id) {
                let why = format!("couldn't save the default app: {}", reason(&e));
                report_open(&ui, &file, why);
            }
        }
        match mime::load_app(&id) {
            Some(app) => open_with_app(&app, &file, &ui),
            None => report_open(&ui, &file, format!("{id} is no longer installed")),
        }
    });
}

fn open_with_app(app: &mime::AppInfo, file: &Path, ui: &Sender<JobMsg>) {
    let dir = file.parent().unwrap_or(Path::new("/"));
    if let Err(why) = launch(&app.command(file), dir) {
        report_open(ui, file, format!("{} ({why})", app.name));
    }
}

/// Ranks `path` in zoxide's database, in the background. Failures are ignored: ranking
/// is a convenience and must never get in the way of navigating.
pub fn zoxide_add(path: PathBuf) {
    thread::spawn(move || {
        let _ = Command::new("zoxide")
            .arg("add")
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Renames `path` to `name` in the same folder, never replacing an existing item.
pub fn spawn_rename(path: PathBuf, name: String, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let dst = path.with_file_name(&name);
        let dir = path.parent().unwrap_or(Path::new("/"));
        let result = match rename(&path, &dst, false) {
            Ok(()) => Ok(dst),
            Err(e) => Err(name_error(&e, &name, dir)),
        };
        let _ = ui.send(JobMsg::Named(result));
    });
}

fn name_error(e: &io::Error, name: &str, dir: &Path) -> String {
    if e.kind() == io::ErrorKind::AlreadyExists {
        format!("\"{name}\" already exists.")
    } else {
        explain(e, &[dir])
    }
}

/// Char index where a name's extension starts: the first dot, ignoring a leading one
/// (the same rule as the number suffix). Folders have no extension.
pub fn ext_start(name: &str, is_dir: bool) -> usize {
    let len = name.chars().count();
    if is_dir {
        return len;
    }
    name.chars()
        .skip(1)
        .position(|c| c == '.')
        .map_or(len, |i| i + 1)
}

/// Permanently deletes `paths` (folders recursively). A failure inside a folder doesn't
/// stop the rest of it from being deleted; each item reports its first failure.
pub fn spawn_delete(paths: Vec<PathBuf>, ui: Sender<JobMsg>) {
    thread::spawn(move || {
        let mut errors = Vec::new();
        for path in &paths {
            let mut first = None;
            delete_tree(path, &mut first);
            if let Some((at, e)) = first {
                let name = display_name(path);
                let mut line = format!("{name}: {}", explain(&e, &[at.parent().unwrap_or(&at)]));
                if at != *path {
                    let sub = at
                        .strip_prefix(path.parent().unwrap_or(path))
                        .unwrap_or(&at);
                    line.push_str(&format!(" (at {})", sub.to_string_lossy()));
                }
                errors.push(line);
            }
        }
        let _ = ui.send(JobMsg::Done {
            verb: "delete",
            errors,
            token: 0,
        });
    });
}

/// Deletes as much of `path` as possible, keeping the first error.
fn delete_tree(path: &Path, first: &mut Option<TreeError>) {
    let mut fail = |e: io::Error| {
        first.get_or_insert((path.to_path_buf(), e));
    };
    let md = match fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) => return fail(e),
    };
    if !md.is_dir() {
        if let Err(e) = fs::remove_file(path) {
            fail(e);
        }
        return;
    }
    match fs::read_dir(path) {
        Ok(rd) => {
            for entry in rd {
                match entry {
                    Ok(entry) => delete_tree(&entry.path(), first),
                    Err(e) => {
                        first.get_or_insert((path.to_path_buf(), e));
                    }
                }
            }
        }
        Err(e) => {
            first.get_or_insert((path.to_path_buf(), e));
            return;
        }
    }
    if let Err(e) = fs::remove_dir(path) {
        // A child that couldn't be deleted is already the more useful error.
        first.get_or_insert((path.to_path_buf(), e));
    }
}

struct Cancelled;

struct Job<'a> {
    ui: &'a Sender<JobMsg>,
    /// Paths in error messages are shown relative to this (the folder the drag came from).
    root: PathBuf,
    /// Copy instead of move: the originals stay.
    copy: bool,
    /// A choice the user asked to repeat for the rest of the job.
    sticky: Option<Choice>,
    tmp_seq: u32,
    errors: Vec<String>,
}

impl Job<'_> {
    fn move_into(&mut self, items: &[PathBuf], dest: &Path) -> Result<(), Cancelled> {
        let mut remaining = items
            .iter()
            .filter(|p| lstat(&dest.join(name_of(p))).is_some())
            .count();

        for src in items {
            let name = name_of(src);
            let target = dest.join(name);
            if !self.copy && src.parent() == Some(dest) {
                continue; // already there
            }
            if dest.starts_with(src) {
                let verb = if self.copy { "copy" } else { "move" };
                self.fail(src, format!("can't {verb} a folder into itself"));
                continue;
            }
            let Some(existing) = lstat(&target) else {
                self.transfer_one(src, &target, false);
                continue;
            };
            remaining = remaining.saturating_sub(1);

            let src_dir = lstat(src).is_some_and(|m| m.is_dir());
            let merge = src_dir && existing.is_dir();
            let can_replace = src_dir == existing.is_dir();
            let choice = match self.sticky {
                Some(Choice::Replace) if !can_replace => None,
                sticky => sticky,
            };
            let choice = match choice {
                Some(c) => c,
                None => {
                    let (reply, answer) = mpsc::channel();
                    let conflict = Conflict {
                        name: name.to_string_lossy().into_owned(),
                        dest: display_name(dest),
                        merge,
                        can_replace,
                        remaining,
                        reply,
                    };
                    self.ask(conflict, answer)?
                }
            };

            match choice {
                Choice::Cancel => return Err(Cancelled),
                // Copying onto itself (a copy dropped into its own folder) changes nothing.
                Choice::Leave => {}
                Choice::Replace if target == *src => {}
                Choice::Suffix => self.transfer_one(src, &suffixed(dest, name, src_dir), false),
                Choice::Replace if merge => self.merge(src, &target)?,
                Choice::Replace => self.transfer_one(src, &target, true),
            }
        }
        Ok(())
    }

    fn ask(
        &mut self,
        conflict: Conflict,
        answer: mpsc::Receiver<(Choice, bool)>,
    ) -> Result<Choice, Cancelled> {
        self.ui
            .send(JobMsg::Conflict(conflict))
            .map_err(|_| Cancelled)?;
        let (choice, for_rest) = answer.recv().map_err(|_| Cancelled)?;
        if for_rest {
            self.sticky = Some(choice);
        }
        match choice {
            Choice::Cancel => Err(Cancelled),
            c => Ok(c),
        }
    }

    /// Moves (copies) the contents of `src` into the existing folder `dest`. A move then
    /// removes `src`, unless something was left behind in it.
    fn merge(&mut self, src: &Path, dest: &Path) -> Result<(), Cancelled> {
        let children: Vec<PathBuf> = match fs::read_dir(src) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(e) => {
                self.fail(src, reason(&e));
                return Ok(());
            }
        };
        self.move_into(&children, dest)?;
        if self.copy {
            return Ok(());
        }
        match fs::remove_dir(src) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => self.fail(
                src,
                format!("{}, merged but couldn't remove the original", reason(&e)),
            ),
        }
        Ok(())
    }

    fn transfer_one(&mut self, src: &Path, dst: &Path, replace: bool) {
        if self.copy {
            self.copy_one(src, dst, replace);
        } else {
            self.move_one(src, dst, replace);
        }
    }

    /// Copies to a hidden temp name next to `dst`, then renames it into place, so `dst`
    /// never exists half-copied.
    fn copy_one(&mut self, src: &Path, dst: &Path, replace: bool) {
        self.tmp_seq += 1;
        let tmp = dst.with_file_name(format!(".swayfin-{}-{}", std::process::id(), self.tmp_seq));
        if let Err((path, e)) = copy_tree(src, &tmp) {
            let _ = remove_tree(&tmp);
            let at = if path == src {
                String::new()
            } else {
                format!(" (at {})", self.rel(&path))
            };
            let dir = dst.parent().unwrap_or(dst);
            self.fail(src, format!("{}{at}", explain(&e, &[dir])));
            return;
        }
        if let Err(e) = rename(&tmp, dst, replace) {
            let _ = remove_tree(&tmp);
            let why = if e.kind() == io::ErrorKind::AlreadyExists {
                format!("\"{}\" appeared meanwhile", display_name(dst))
            } else {
                explain_rename(&e, src, dst)
            };
            self.fail(src, why);
        }
    }

    fn move_one(&mut self, src: &Path, dst: &Path, replace: bool) {
        match rename(src, dst, replace) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => self.move_across(src, dst),
            Err(e) => self.fail(src, explain_rename(&e, src, dst)),
        }
    }

    /// Different file systems: copy to a hidden temp name next to `dst`, rename it into
    /// place, then delete the original. The original is only touched after a full copy.
    fn move_across(&mut self, src: &Path, dst: &Path) {
        self.tmp_seq += 1;
        let tmp = dst.with_file_name(format!(".swayfin-{}-{}", std::process::id(), self.tmp_seq));
        if let Err((path, e)) = copy_tree(src, &tmp) {
            let _ = remove_tree(&tmp);
            let at = if path == src {
                String::new()
            } else {
                format!(" (at {})", self.rel(&path))
            };
            self.fail(src, format!("{}{at}, while copying", reason(&e)));
            return;
        }
        if let Err(e) = fs::rename(&tmp, dst) {
            let _ = remove_tree(&tmp);
            self.fail(src, explain_rename(&e, src, dst));
            return;
        }
        if let Err((path, e)) = remove_tree(src) {
            self.fail(
                src,
                format!(
                    "{}, copied but couldn't remove the original (at {})",
                    reason(&e),
                    self.rel(&path)
                ),
            );
        }
    }

    fn fail(&mut self, path: &Path, reason: String) {
        let line = format!("{}: {reason}", self.rel(path));
        self.errors.push(line);
    }

    fn rel(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn name_of(path: &Path) -> &OsStr {
    path.file_name().unwrap_or(path.as_os_str())
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn lstat(path: &Path) -> Option<Metadata> {
    fs::symlink_metadata(path).ok()
}

/// `name_N.ext` for files (the suffix goes before the first dot, ignoring a leading one),
/// `name_N` for folders; the first N that's free.
fn suffixed(dest: &Path, name: &OsStr, is_dir: bool) -> PathBuf {
    let bytes = name.as_bytes();
    let split = if is_dir {
        bytes.len()
    } else {
        bytes
            .iter()
            .skip(1)
            .position(|&b| b == b'.')
            .map_or(bytes.len(), |i| i + 1)
    };
    let (stem, ext) = bytes.split_at(split);
    (1..)
        .map(|n| {
            let mut s = stem.to_vec();
            s.extend_from_slice(format!("_{n}").as_bytes());
            s.extend_from_slice(ext);
            dest.join(OsStr::from_bytes(&s))
        })
        .find(|p| lstat(p).is_none())
        .unwrap()
}

/// rename(2); unless `replace`, fails with EEXIST instead of clobbering something that
/// appeared at `dst` since the conflict check.
fn rename(src: &Path, dst: &Path, replace: bool) -> io::Result<()> {
    if replace {
        return fs::rename(src, dst);
    }
    let (s, d) = (cstr(src)?, cstr(dst)?);
    // SAFETY: both are valid NUL-terminated paths.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            s.as_ptr(),
            libc::AT_FDCWD,
            d.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    // Some file systems (NFS, many FUSE) don't support RENAME_NOREPLACE: check, then rename.
    if e.raw_os_error() == Some(libc::EINVAL) {
        if lstat(dst).is_some() {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        return fs::rename(src, dst);
    }
    Err(e)
}

fn cstr(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

/// The OS message without Rust's " (os error N)" suffix.
fn reason(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn explain_rename(e: &io::Error, src: &Path, dst: &Path) -> String {
    let dirs: Vec<&Path> = [src.parent(), dst.parent()].into_iter().flatten().collect();
    explain(e, &dirs)
}

/// For permission errors, names the folder among `dirs` that can't be written to.
fn explain(e: &io::Error, dirs: &[&Path]) -> String {
    let base = reason(e);
    if !matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM)) {
        return base;
    }
    match dirs.iter().find(|d| !writable(d)) {
        Some(dir) => format!("{base}, can't write to {}", dir.to_string_lossy()),
        None => base,
    }
}

fn writable(dir: &Path) -> bool {
    // SAFETY: valid NUL-terminated path.
    cstr(dir).is_ok_and(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
}

type TreeError = (PathBuf, io::Error);

/// Copies a file, symlink or folder tree, keeping permissions and modification times.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), TreeError> {
    let at = |e| (src.to_path_buf(), e);
    let md = fs::symlink_metadata(src).map_err(at)?;
    let ft = md.file_type();
    if ft.is_symlink() {
        symlink(fs::read_link(src).map_err(at)?, dst).map_err(at)
    } else if ft.is_dir() {
        fs::create_dir(dst).map_err(at)?;
        for entry in fs::read_dir(src).map_err(at)? {
            let entry = entry.map_err(at)?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, md.permissions()).map_err(at)?;
        set_times(dst, &md);
        Ok(())
    } else if ft.is_file() {
        fs::copy(src, dst).map_err(at)?;
        set_times(dst, &md);
        Ok(())
    } else {
        Err(at(io::Error::other(
            "special files (sockets, pipes, devices) can't be copied",
        )))
    }
}

/// Best effort; a move shouldn't fail over timestamps.
fn set_times(path: &Path, md: &Metadata) {
    let (Ok(modified), Ok(accessed)) = (md.modified(), md.accessed()) else {
        return;
    };
    if let Ok(f) = File::open(path) {
        let _ = f.set_times(
            FileTimes::new()
                .set_modified(modified)
                .set_accessed(accessed),
        );
    }
}

fn remove_tree(path: &Path) -> Result<(), TreeError> {
    let at = |e| (path.to_path_buf(), e);
    if fs::symlink_metadata(path).map_err(at)?.is_dir() {
        for entry in fs::read_dir(path).map_err(at)? {
            remove_tree(&entry.map_err(at)?.path())?;
        }
        fs::remove_dir(path).map_err(at)
    } else {
        fs::remove_file(path).map_err(at)
    }
}
