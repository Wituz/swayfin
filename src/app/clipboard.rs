//! Ctrl+C / Ctrl+X / Ctrl+V with files, on the system clipboard. We offer the formats
//! file managers exchange: text/uri-list, GNOME's x-special/gnome-copied-files ("copy" or
//! "cut" + URIs) and KDE's application/x-kde-cutselection ("1" = cut), plus the paths as
//! plain text for terminals and editors. Wayland keeps the data with us: the clipboard
//! empties if this window closes.

use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
};

use smithay_client_toolkit::{
    data_device_manager::{WritePipe, data_source::CopyPasteSource},
    reexports::client::protocol::wl_data_source::WlDataSource,
};

use super::{App, dnd};
use crate::{mime, ops};

const URI_LIST: &str = "text/uri-list";
const GNOME: &str = "x-special/gnome-copied-files";
const KDE_CUT: &str = "application/x-kde-cutselection";
const TEXT: &str = "text/plain;charset=utf-8";
const READ_MAX: u64 = 16 * 1024 * 1024;

pub struct Clip {
    source: CopyPasteSource,
    paths: Vec<PathBuf>,
    cut: bool,
}

impl App {
    /// Ctrl+C / Ctrl+X: puts the selection on the clipboard.
    pub(super) fn copy_selection(&mut self, cut: bool) {
        let paths = self.view.selected_paths();
        let (Some(manager), Some(device)) = (&self.data_devices, &self.data_device) else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let source = manager.create_copy_paste_source(&self.qh, [URI_LIST, GNOME, KDE_CUT, TEXT]);
        source.set_selection(device, self.key_serial);
        self.cut = if cut {
            paths.iter().cloned().collect()
        } else {
            HashSet::new()
        };
        self.clip = Some(Clip { source, paths, cut });
        self.request_redraw();
    }

    /// Ctrl+V: copies (or, for a cut, moves) the clipboard's files into the current folder.
    pub(super) fn paste_files(&mut self) {
        if let Some(clip) = &self.clip {
            // Our own clipboard: no need to read it back.
            let (paths, cut) = (clip.paths.clone(), clip.cut);
            self.paste_paths(paths, cut);
            return;
        }
        let Some(offer) = self
            .data_device
            .as_ref()
            .and_then(|d| d.data().selection_offer())
        else {
            return;
        };
        let has = |t: &str| offer.with_mime_types(|m| m.iter().any(|x| x == t));
        // GNOME's format carries the cut flag with the list; otherwise the uri-list plus
        // KDE's flag.
        let (list, flag) = if has(GNOME) {
            (offer.receive(GNOME.into()).ok(), None)
        } else if has(URI_LIST) {
            let flag = has(KDE_CUT)
                .then(|| offer.receive(KDE_CUT.into()).ok())
                .flatten();
            (offer.receive(URI_LIST.into()).ok(), flag)
        } else {
            return;
        };
        let Some(list) = list else { return };
        let gnome = flag.is_none() && has(GNOME);
        // The source only starts writing once our requests reach the compositor.
        let _ = self.conn.flush();
        let jobs = self.jobs.clone();
        thread::spawn(move || {
            let read = |pipe: smithay_client_toolkit::data_device_manager::ReadPipe| {
                let mut buf = Vec::new();
                let _ = pipe.take(READ_MAX).read_to_end(&mut buf);
                buf
            };
            let mut data = read(list);
            let cut = if gnome {
                // "cut\nfile:///..." or "copy\nfile:///..."
                let first = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
                let cut = data[..first].trim_ascii() == b"cut";
                data.drain(..(first + 1).min(data.len()));
                cut
            } else {
                flag.map(read).is_some_and(|f| f.trim_ascii() == b"1")
            };
            let paths = dnd::parse_uri_list(&data);
            let _ = jobs.send(ops::JobMsg::Paste { paths, cut });
        });
    }

    /// Copies (moves, for a cut) `paths` into the current folder. A cut's clipboard is
    /// cleared once the move is done, which tells its owner the files have moved.
    pub(super) fn paste_paths(&mut self, paths: Vec<PathBuf>, cut: bool) {
        self.drop_seq += 1;
        let token = self.drop_seq;
        if cut {
            self.cut_pastes.push(token);
        }
        let dest = self.view.listing.path.clone();
        ops::spawn_transfer(paths, dest, !cut, token, self.jobs.clone());
    }

    /// A pasted cut has been carried out elsewhere or here: the clipboard is spent.
    pub(super) fn clear_clipboard(&mut self) {
        self.clip = None;
        self.cut.clear();
        if let Some(device) = &self.data_device {
            device.unset_selection(self.key_serial);
        }
        self.request_redraw();
    }

    pub(super) fn is_clipboard_source(&self, source: &WlDataSource) -> bool {
        self.clip
            .as_ref()
            .is_some_and(|c| c.source.inner() == source)
    }

    /// Serves a paste of our clipboard in `mime`.
    pub(super) fn clipboard_send(&self, mime: &str, mut fd: WritePipe) {
        let Some(clip) = &self.clip else { return };
        let uris: Vec<String> = clip.paths.iter().map(|p| mime::file_uri(p)).collect();
        let data = match mime {
            URI_LIST => uris.iter().map(|u| format!("{u}\r\n")).collect(),
            GNOME => {
                let verb = if clip.cut { "cut" } else { "copy" };
                format!("{verb}\n{}", uris.join("\n"))
            }
            KDE_CUT => (if clip.cut { "1" } else { "0" }).to_string(),
            TEXT => text_paths(&clip.paths),
            _ => return,
        };
        // Off-thread: a slow reader must not stall the UI on a full pipe.
        thread::spawn(move || {
            let _ = fd.write_all(data.as_bytes());
        });
    }

    /// Another client took over the clipboard (or we cleared it).
    pub(super) fn clipboard_cancelled(&mut self, source: &WlDataSource) -> bool {
        if !self
            .clip
            .as_ref()
            .is_some_and(|c| c.source.inner() == source)
        {
            return false;
        }
        let was_cut = self.clip.take().is_some_and(|c| c.cut);
        self.cut.clear();
        // A cut is cleared by whoever pasted it, once the files are moved: show that.
        if was_cut {
            self.refresh();
        }
        self.request_redraw();
        true
    }
}

fn text_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p: &PathBuf| Path::to_string_lossy(p).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}
