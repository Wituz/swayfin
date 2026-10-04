//! Directory listing. Runs off the UI thread; all display strings are formatted here.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs, io,
    os::unix::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::sort::{self, Sort};

pub struct Entry {
    pub raw: OsString,
    pub name: String,
    pub is_dir: bool,
    pub size: String,
    pub modified: String,
    /// Modification time in seconds, for matching thumbnails.
    pub mtime: i64,
    /// Audio by its name: gets the play/pause icon instead of a thumbnail.
    pub audio: bool,
    /// Video by its name: hovering its thumbnail plays it.
    pub video: bool,
    /// Name starts with a dot.
    pub dot: bool,
}

pub struct Listing {
    pub path: PathBuf,
    pub entries: Vec<Entry>,
    pub error: Option<String>,
    /// Whether dotfiles were included.
    pub dotfiles: bool,
    pub sort: Sort,
}

impl Listing {
    pub fn sort_by(&mut self, sort: Sort) {
        let order = sort::order(&self.entries, sort);
        sort::permute(&mut self.entries, &order);
        self.sort = sort;
    }
}

/// Reads `path`, sorted the way it was last sorted.
pub fn list(path: PathBuf, dotfiles: bool) -> Listing {
    let (entries, error) = match read(&path, dotfiles) {
        Ok(entries) => (entries, None),
        Err(e) => (Vec::new(), Some(e.kind().to_string())),
    };
    let sort = sort::get(&path);
    let mut listing = Listing {
        path,
        entries,
        error,
        dotfiles,
        sort,
    };
    listing.sort_by(sort);
    listing
}

/// Unsorted. Dotfiles only if `dotfiles`; names listed in the folder's `.hidden` file (one per line) never, except `.hidden` itself.
fn read(path: &Path, dotfiles: bool) -> io::Result<Vec<Entry>> {
    let hidden: HashSet<OsString> = fs::read(path.join(".hidden"))
        .map(|bytes| {
            bytes
                .split(|&b| b == b'\n')
                .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
                .filter(|l| !l.is_empty() && *l != b".hidden")
                .map(|l| OsString::from_vec(l.to_vec()))
                .collect()
        })
        .unwrap_or_default();

    let mut entries = Vec::new();
    for de in fs::read_dir(path)?.flatten() {
        let raw = de.file_name();
        let dot = raw.as_encoded_bytes().starts_with(b".");
        if (dot && !dotfiles) || hidden.contains(&raw) {
            continue;
        }
        // fstatat on the dir fd; only symlinks pay for a second, following stat.
        let Ok(mut md) = de.metadata() else { continue };
        if md.is_symlink() {
            md = fs::metadata(de.path()).unwrap_or(md);
        }
        let is_dir = md.is_dir();
        let name = raw.to_string_lossy().into_owned();
        entries.push(Entry {
            audio: !is_dir && crate::mime::is_audio(&name),
            video: !is_dir && crate::mime::is_video(&name),
            name,
            raw,
            is_dir,
            size: if is_dir {
                String::new()
            } else {
                format_size(md.len())
            },
            modified: md.modified().map(format_time).unwrap_or_default(),
            mtime: md.mtime(),
            dot,
        });
    }
    Ok(entries)
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let (mut v, mut u) = (bytes as f64 / 1024.0, 0);
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// Local time as `YYYY-MM-DD HH:MM`.
fn format_time(t: SystemTime) -> String {
    let Ok(d) = t.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let secs = d.as_secs() as libc::time_t;
    // SAFETY: localtime_r only writes into the tm we own.
    let tm = unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        libc::localtime_r(&secs, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}
