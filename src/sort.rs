//! Sort order of a folder's listing, remembered per folder in
//! `$XDG_STATE_HOME/swayfin/sort`. Folders always come before files.

use std::{
    collections::HashMap,
    env,
    ffi::OsStr,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
};

use crate::fs::Entry;

#[derive(Clone, Copy, PartialEq)]
pub enum Key {
    Name,
    Date,
    Type,
}

pub const KEYS: [Key; 3] = [Key::Name, Key::Date, Key::Type];

/// The active key, and each key's own direction (true = descending), indexed by `Key`.
#[derive(Clone, Copy, PartialEq)]
pub struct Sort {
    pub key: Key,
    desc: [bool; 3],
}

/// Newest first; Name and Type start ascending.
impl Default for Sort {
    fn default() -> Self {
        Self {
            key: Key::Date,
            desc: [false, true, false],
        }
    }
}

impl Sort {
    pub fn desc(&self) -> bool {
        self.desc[self.key as usize]
    }

    /// A key's button was clicked: sort by it in its own direction, or flip it if it's
    /// already the active one.
    pub fn click(&mut self, key: Key) {
        if self.key == key {
            self.desc[key as usize] ^= true;
        } else {
            self.key = key;
        }
    }
}

/// Indices into `entries` in display order: folders first, then by the key; ties go by
/// name, ascending.
pub fn order(entries: &[Entry], sort: Sort) -> Vec<usize> {
    let names: Vec<String> = entries.iter().map(|e| e.name.to_lowercase()).collect();
    let exts: Vec<String> = match sort.key {
        Key::Type => entries
            .iter()
            .map(|e| {
                let start = crate::ops::ext_start(&e.name, e.is_dir);
                e.name.chars().skip(start).collect::<String>().to_lowercase()
            })
            .collect(),
        _ => Vec::new(),
    };
    let by_name = |a: usize, b: usize| {
        names[a]
            .cmp(&names[b])
            .then_with(|| entries[a].raw.cmp(&entries[b].raw))
    };
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| {
        let key = match sort.key {
            Key::Name => by_name(a, b),
            Key::Date => entries[a].mtime.cmp(&entries[b].mtime),
            Key::Type => exts[a].cmp(&exts[b]),
        };
        let key = if sort.desc() { key.reverse() } else { key };
        entries[b]
            .is_dir
            .cmp(&entries[a].is_dir)
            .then(key)
            .then_with(|| by_name(a, b))
    });
    order
}

/// Reorders `entries` so that position i holds what was at `order[i]`.
pub fn permute<T>(entries: &mut Vec<T>, order: &[usize]) {
    let mut old: Vec<Option<T>> = std::mem::take(entries).into_iter().map(Some).collect();
    *entries = order.iter().map(|&i| old[i].take().unwrap()).collect();
}

/// Loaded on first use, which is the first directory read (off the UI thread).
fn store() -> &'static Mutex<HashMap<PathBuf, Sort>> {
    static STORE: OnceLock<Mutex<HashMap<PathBuf, Sort>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(load()))
}

/// The sort saved for `dir`, or the default.
pub fn get(dir: &Path) -> Sort {
    store().lock().unwrap().get(dir).copied().unwrap_or_default()
}

/// Remembers `sort` for `dir` and saves the file on a thread.
pub fn set(dir: PathBuf, sort: Sort) {
    {
        let mut map = store().lock().unwrap();
        if sort == Sort::default() {
            map.remove(&dir);
        } else {
            map.insert(dir, sort);
        }
    }
    thread::spawn(save);
}

fn file() -> Option<PathBuf> {
    let state = env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("swayfin/sort"))
}

/// One line per folder: the key (`n`, `d`, `t`), each key's direction (`+` ascending,
/// `-` descending), a space and the path.
fn load() -> HashMap<PathBuf, Sort> {
    let Some(bytes) = file().and_then(|f| fs::read(f).ok()) else {
        return HashMap::new();
    };
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            let (head, path) = (line.get(..5)?, line.get(5..)?);
            let key = match head[0] {
                b'n' => Key::Name,
                b'd' => Key::Date,
                b't' => Key::Type,
                _ => return None,
            };
            let dir = |b: u8| match b {
                b'+' => Some(false),
                b'-' => Some(true),
                _ => None,
            };
            let desc = [dir(head[1])?, dir(head[2])?, dir(head[3])?];
            (head[4] == b' ' && !path.is_empty())
                .then(|| (PathBuf::from(OsStr::from_bytes(path)), Sort { key, desc }))
        })
        .collect()
}

/// Writes the whole store; one writer at a time, so the last save always wins.
fn save() {
    static WRITE: Mutex<()> = Mutex::new(());
    let _writing = WRITE.lock().unwrap();
    let mut out = Vec::new();
    for (path, sort) in store().lock().unwrap().iter() {
        let path = path.as_os_str().as_bytes();
        if path.contains(&b'\n') {
            continue;
        }
        out.push(match sort.key {
            Key::Name => b'n',
            Key::Date => b'd',
            Key::Type => b't',
        });
        out.extend(sort.desc.map(|d| if d { b'-' } else { b'+' }));
        out.push(b' ');
        out.extend_from_slice(path);
        out.push(b'\n');
    }
    let Some(file) = file() else { return };
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let tmp = file.with_extension("tmp");
    if fs::write(&tmp, out).is_ok() {
        let _ = fs::rename(&tmp, &file);
    }
}
