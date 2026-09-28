//! freedesktop.org file associations: MIME types from shared-mime-info globs, default
//! apps from mimeapps.list / mimeinfo.cache, and launching .desktop entries.
//! Everything here runs on worker threads.

use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    sync::OnceLock,
};

pub struct AppInfo {
    pub id: String,
    pub name: String,
    exec: String,
    terminal: bool,
    mime_types: Vec<String>,
    no_display: bool,
    path: PathBuf,
}

fn home() -> PathBuf {
    env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn xdg_home(var: &str, fallback: &str) -> PathBuf {
    env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

fn xdg_dirs(var: &str, fallback: &str) -> Vec<PathBuf> {
    let dirs = env::var(var)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.into());
    dirs.split(':')
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Most important first.
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![xdg_home("XDG_DATA_HOME", ".local/share")];
    dirs.extend(xdg_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
    dirs
}

fn config_home() -> PathBuf {
    xdg_home("XDG_CONFIG_HOME", ".config")
}

/// Every mimeapps.list that applies, most important first (mime-apps spec).
fn mimeapps_files() -> Vec<PathBuf> {
    let desktops: Vec<String> = env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut dirs = vec![config_home()];
    dirs.extend(xdg_dirs("XDG_CONFIG_DIRS", "/etc/xdg"));
    dirs.extend(data_dirs().into_iter().map(|d| d.join("applications")));
    let mut files = Vec::new();
    for dir in dirs {
        for d in &desktops {
            files.push(dir.join(format!("{d}-mimeapps.list")));
        }
        files.push(dir.join("mimeapps.list"));
    }
    files
}

// ---------------------------------------------------------------- MIME types

struct Glob {
    weight: u32,
    mime: String,
    /// Lowercased unless `case_sensitive`.
    pattern: String,
    case_sensitive: bool,
    kind: GlobKind,
}

enum GlobKind {
    Literal,
    /// `*.ext`: the part after the star.
    Suffix(String),
    Pattern,
}

struct MimeDb {
    globs: Vec<Glob>,
    /// Indices into `globs` by exact name / by the text after `*`; the rest need matching.
    literal: HashMap<String, Vec<usize>>,
    suffix: HashMap<String, Vec<usize>>,
    patterns: Vec<usize>,
    aliases: HashMap<String, String>,
    parents: HashMap<String, Vec<String>>,
}

fn db() -> &'static MimeDb {
    static DB: OnceLock<MimeDb> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = MimeDb {
            globs: Vec::new(),
            literal: HashMap::new(),
            suffix: HashMap::new(),
            patterns: Vec::new(),
            aliases: HashMap::new(),
            parents: HashMap::new(),
        };
        for dir in data_dirs().iter().map(|d| d.join("mime")) {
            for line in read_lines(&dir.join("globs2")) {
                // weight:type:glob[:flags]
                let mut f = line.splitn(4, ':');
                let (Some(w), Some(mime), Some(pat)) = (f.next(), f.next(), f.next()) else {
                    continue;
                };
                if pat == "__NOGLOBS__" {
                    continue;
                }
                let case_sensitive = f
                    .next()
                    .is_some_and(|flags| flags.split(',').any(|x| x == "cs"));
                let pattern = if case_sensitive {
                    pat.to_string()
                } else {
                    pat.to_lowercase()
                };
                let wild = |s: &str| s.contains(['*', '?', '[']);
                let kind = match pattern.strip_prefix('*') {
                    _ if !wild(&pattern) => GlobKind::Literal,
                    Some(rest) if !wild(rest) => GlobKind::Suffix(rest.to_string()),
                    _ => GlobKind::Pattern,
                };
                let i = db.globs.len();
                match &kind {
                    GlobKind::Literal => db.literal.entry(pattern.clone()).or_default().push(i),
                    GlobKind::Suffix(sfx) => db.suffix.entry(sfx.clone()).or_default().push(i),
                    GlobKind::Pattern => db.patterns.push(i),
                }
                db.globs.push(Glob {
                    weight: w.parse().unwrap_or(50),
                    mime: mime.to_string(),
                    pattern,
                    case_sensitive,
                    kind,
                });
            }
            for line in read_lines(&dir.join("aliases")) {
                if let Some((alias, canonical)) = line.split_once(' ') {
                    db.aliases.entry(alias.into()).or_insert(canonical.into());
                }
            }
            for line in read_lines(&dir.join("subclasses")) {
                if let Some((child, parent)) = line.split_once(' ') {
                    let parents = db.parents.entry(child.into()).or_default();
                    if !parents.iter().any(|p| p == parent) {
                        parents.push(parent.into());
                    }
                }
            }
        }
        db
    })
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

fn canonical(mime: &str) -> String {
    db().aliases
        .get(mime)
        .cloned()
        .unwrap_or_else(|| mime.to_string())
}

/// The file's MIME type: by name via the glob tables, else a text/binary sniff.
pub fn detect(path: &Path) -> String {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    match by_name(&name) {
        Some(mime) => canonical(mime),
        None => sniff(path),
    }
}

/// The MIME type the name alone implies (no file access): exact names and `*suffix`
/// globs are hash lookups, only the few complex patterns are matched one by one.
pub fn by_name(name: &str) -> Option<&'static str> {
    let db = db();
    let lower = name.to_lowercase();
    let mut candidates: Vec<usize> = Vec::new();
    for subject in [name, lower.as_str()] {
        candidates.extend(db.literal.get(subject).into_iter().flatten());
        for (i, _) in subject.char_indices() {
            candidates.extend(db.suffix.get(&subject[i..]).into_iter().flatten());
        }
    }
    candidates.extend(db.patterns.iter().copied().filter(|&i| {
        let g = &db.globs[i];
        let subject = if g.case_sensitive { name } else { &lower };
        glob_match(g.pattern.as_bytes(), subject.as_bytes())
    }));
    // A candidate must also match in its own case mode (the lookups above tried both).
    let matches = |g: &Glob| {
        let subject = if g.case_sensitive { name } else { &lower };
        match &g.kind {
            GlobKind::Literal => subject == g.pattern,
            GlobKind::Suffix(sfx) => subject.ends_with(sfx.as_str()),
            GlobKind::Pattern => true,
        }
    };
    // Higher weight wins, then literal names, then the longest pattern.
    candidates
        .into_iter()
        .map(|i| &db.globs[i])
        .filter(|g| matches(g))
        .max_by_key(|g| {
            (
                g.weight,
                matches!(g.kind, GlobKind::Literal),
                g.pattern.len(),
            )
        })
        .map(|g| g.mime.as_str())
}

/// Video, judged by name (for hover playback).
pub fn is_video(name: &str) -> bool {
    by_name(name).is_some_and(|m| canonical(m).starts_with("video/"))
}

/// Audio, judged by name (for the play icon).
pub fn is_audio(name: &str) -> bool {
    by_name(name).is_some_and(|m| canonical(m).starts_with("audio/"))
}

/// No name match: empty, text (valid UTF-8 without NULs) or binary.
fn sniff(path: &Path) -> String {
    let mut buf = [0u8; 512];
    let n = File::open(path)
        .and_then(|mut f| f.read(&mut buf))
        .unwrap_or(0);
    let head = &buf[..n];
    let text = !head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            // A multi-byte char cut off by the 512-byte window is still text.
            Err(e) => e.error_len().is_none(),
        };
    match (n, text) {
        (0, _) => "application/x-zerosize",
        (_, true) => "text/plain",
        _ => "application/octet-stream",
    }
    .into()
}

/// fnmatch-style: `*`, `?` and `[...]` (with `!`/`^` negation and ranges).
fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    match pat.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|i| glob_match(&pat[1..], &s[i..])),
        Some(b'?') => !s.is_empty() && glob_match(&pat[1..], &s[1..]),
        Some(b'[') => {
            let Some(end) = pat.iter().skip(2).position(|&b| b == b']').map(|i| i + 2) else {
                return !s.is_empty() && s[0] == b'[' && glob_match(&pat[1..], &s[1..]);
            };
            let Some(&c) = s.first() else { return false };
            let mut set = &pat[1..end];
            let negate = matches!(set.first(), Some(b'!' | b'^'));
            if negate {
                set = &set[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    hit |= (set[i]..=set[i + 2]).contains(&c);
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            hit != negate && glob_match(&pat[end + 1..], &s[1..])
        }
        Some(&b) => s.first() == Some(&b) && glob_match(&pat[1..], &s[1..]),
    }
}

/// `mime` followed by its ancestors, nearest first. Every text type is a text/plain.
fn with_ancestors(mime: &str) -> Vec<String> {
    let mut out = vec![canonical(mime)];
    let mut i = 0;
    while i < out.len() {
        let parents = db().parents.get(&out[i]).cloned().unwrap_or_default();
        for p in parents {
            let p = canonical(&p);
            if !out.contains(&p) {
                out.push(p);
            }
        }
        i += 1;
    }
    if mime.starts_with("text/") && !out.iter().any(|m| m == "text/plain") {
        out.push("text/plain".into());
    }
    out
}

// ---------------------------------------------------------------- associations

/// `(section, key, value)` for every entry of an ini-style file.
fn ini(path: &Path) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in read_lines(path) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            out.push((section.clone(), k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

fn ids(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// The app that opens `mime` (or, failing that, one of its ancestors): the user's
/// default, else an added association, else any app declaring the type.
pub fn default_app(mime: &str) -> Option<AppInfo> {
    let files: Vec<_> = mimeapps_files().iter().map(|f| ini(f)).collect();
    let caches: Vec<_> = data_dirs()
        .iter()
        .map(|d| ini(&d.join("applications/mimeinfo.cache")))
        .collect();
    let entries_for = |entries: &[(String, String, String)], section: &str, mime: &str| {
        entries
            .iter()
            .filter(|(s, k, _)| s == section && canonical(k) == mime)
            .flat_map(|(_, _, v)| ids(v).map(String::from).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };

    for mime in with_ancestors(mime) {
        for f in &files {
            for id in entries_for(f, "Default Applications", &mime) {
                if let Some(app) = load_app(&id) {
                    return Some(app);
                }
            }
        }
        let removed: HashSet<String> = files
            .iter()
            .flat_map(|f| entries_for(f, "Removed Associations", &mime))
            .collect();
        let added = files
            .iter()
            .flat_map(|f| entries_for(f, "Added Associations", &mime));
        let cached = caches
            .iter()
            .flat_map(|c| entries_for(c, "MIME Cache", &mime));
        for id in added.chain(cached) {
            if !removed.contains(&id) {
                if let Some(app) = load_app(&id) {
                    return Some(app);
                }
            }
        }
    }
    None
}

/// Every visible app, the ones declaring `mime` (or an ancestor) first, then by name.
pub fn candidates(mime: &str) -> Vec<(String, String)> {
    let related: HashSet<String> = with_ancestors(mime).into_iter().collect();
    let mut apps: Vec<(bool, String, String)> = all_apps()
        .into_iter()
        .filter(|a| !a.no_display)
        .map(|a| {
            let rel = a.mime_types.iter().any(|m| related.contains(&canonical(m)));
            (!rel, a.name, a.id)
        })
        .collect();
    apps.sort_by_cached_key(|(unrelated, name, _)| (*unrelated, name.to_lowercase()));
    apps.into_iter().map(|(_, name, id)| (id, name)).collect()
}

/// Makes `id` the user's default for `mime` in ~/.config/mimeapps.list, keeping the rest
/// of the file as it is.
pub fn set_default(mime: &str, id: &str) -> io::Result<()> {
    let path = config_home().join("mimeapps.list");
    let old = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let entry = format!("{mime}={id}");
    let mut lines: Vec<String> = old.lines().map(String::from).collect();
    let section = lines
        .iter()
        .position(|l| l.trim() == "[Default Applications]");
    match section {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| start + 1 + i);
            let existing = (start + 1..end).find(|&i| {
                lines[i]
                    .split_once('=')
                    .is_some_and(|(k, _)| k.trim() == mime)
            });
            match existing {
                Some(i) => lines[i] = entry,
                None => lines.insert(start + 1, entry),
            }
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[Default Applications]".into());
            lines.push(entry);
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write beside it and rename over, so a crash never leaves a half-written file.
    let tmp = path.with_file_name(format!(".mimeapps.list.swayfin-{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &path)
}

// ---------------------------------------------------------------- desktop entries

/// Finds and parses `id` (e.g. "org.kde.kate.desktop"). None if it isn't installed,
/// is hidden, isn't an application, or its TryExec program is missing.
pub fn load_app(id: &str) -> Option<AppInfo> {
    // "vendor-app.desktop" may also live at "vendor/app.desktop".
    let mut rels = vec![PathBuf::from(id)];
    for (i, _) in id.match_indices('-') {
        rels.push(PathBuf::from(format!("{}/{}", &id[..i], &id[i + 1..])));
    }
    for dir in data_dirs() {
        for rel in &rels {
            let path = dir.join("applications").join(rel);
            if path.is_file() {
                return parse_app(id.to_string(), path);
            }
        }
    }
    None
}

fn all_apps() -> Vec<AppInfo> {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    for dir in data_dirs() {
        let root = dir.join("applications");
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(&d) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension() != Some(OsStr::new("desktop")) {
                    continue;
                }
                let rel = path.strip_prefix(&root).unwrap_or(&path);
                let id = rel.to_string_lossy().replace('/', "-");
                // The first data dir that has an id owns it (the user's overrides the system's).
                if seen.insert(id.clone()) {
                    apps.extend(parse_app(id, path));
                }
            }
        }
    }
    apps
}

fn parse_app(id: String, path: PathBuf) -> Option<AppInfo> {
    let entries = ini(&path);
    let get = |key: &str| {
        entries
            .iter()
            .find(|(s, k, _)| s == "Desktop Entry" && k == key)
            .map(|(_, _, v)| unescape(v))
    };
    let flag = |key: &str| get(key).is_some_and(|v| v == "true");
    if get("Type").as_deref() != Some("Application") || flag("Hidden") {
        return None;
    }
    if let Some(try_exec) = get("TryExec") {
        if !program_exists(&try_exec) {
            return None;
        }
    }
    Some(AppInfo {
        name: get("Name").unwrap_or_else(|| id.clone()),
        exec: get("Exec")?,
        terminal: flag("Terminal"),
        mime_types: get("MimeType")
            .map(|v| ids(&v).map(String::from).collect())
            .unwrap_or_default(),
        no_display: flag("NoDisplay"),
        id,
        path,
    })
}

/// Desktop-entry string escapes: \s \n \t \r \\.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn program_exists(program: &str) -> bool {
    let executable = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return executable(Path::new(program));
    }
    env::var_os("PATH")
        .is_some_and(|path| env::split_paths(&path).any(|d| executable(&d.join(program))))
}

impl AppInfo {
    /// The command line that opens `file`: Exec with its field codes expanded (the file
    /// is appended if Exec has none), wrapped in a terminal for Terminal=true apps.
    pub fn command(&self, file: &Path) -> Vec<OsString> {
        let mut argv: Vec<OsString> = Vec::new();
        let mut took_file = false;
        for token in split_exec(&self.exec) {
            match token.as_str() {
                "%f" | "%F" => {
                    argv.push(file.as_os_str().to_owned());
                    took_file = true;
                }
                "%u" | "%U" => {
                    argv.push(file_uri(file).into());
                    took_file = true;
                }
                "%i" => {}
                _ => {
                    let mut out = String::new();
                    let mut chars = token.chars();
                    while let Some(c) = chars.next() {
                        if c != '%' {
                            out.push(c);
                            continue;
                        }
                        match chars.next() {
                            Some('%') => out.push('%'),
                            Some('f' | 'F') => {
                                out.push_str(&file.to_string_lossy());
                                took_file = true;
                            }
                            Some('u' | 'U') => {
                                out.push_str(&file_uri(file));
                                took_file = true;
                            }
                            Some('c') => out.push_str(&self.name),
                            Some('k') => out.push_str(&self.path.to_string_lossy()),
                            // Deprecated or unknown codes are dropped.
                            _ => {}
                        }
                    }
                    argv.push(out.into());
                }
            }
        }
        if !took_file {
            argv.push(file.as_os_str().to_owned());
        }
        if self.terminal {
            let mut wrapped = crate::ops::terminal_argv();
            wrapped.extend(argv);
            wrapped
        } else {
            argv
        }
    }
}

/// Splits Exec into arguments: whitespace separates, double quotes group, and inside
/// quotes a backslash escapes `"`, `` ` ``, `$` and `\`.
fn split_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => match chars.next() {
                Some(e @ ('"' | '`' | '$' | '\\')) => cur.push(e),
                Some(o) => {
                    cur.push('\\');
                    cur.push(o);
                }
                None => cur.push('\\'),
            },
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            c => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    args
}

/// RFC 8089 file URI, percent-encoding every byte outside the unreserved set and '/'.
pub fn file_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}
