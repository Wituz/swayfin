//! File dialog mode (`crate::chooser`): picking, saving, and the bar's pointer input.

use std::path::PathBuf;

use smithay_client_toolkit::seat::pointer::PointerEventKind;

use super::App;
use crate::{
    chooser::{Action, BAR_H, Mode},
    modal::{self, Modal},
    ops,
    view::BTN_LEFT,
};

impl App {
    /// Height of the list: the window minus the dialog's bar.
    pub(super) fn list_h(&self) -> usize {
        let bar = if self.chooser.is_some() { BAR_H } else { 0 };
        (self.height as usize).saturating_sub(bar)
    }

    /// Whether the pointer is over the dialog's bar.
    pub(super) fn over_bar(&self) -> bool {
        self.chooser.is_some()
            && self
                .pointer_pos
                .is_some_and(|(_, y)| y >= self.list_h() as f64)
    }

    /// Pointer input for the bar. Returns true if the list shouldn't see the event.
    pub(super) fn dialog_pointer(&mut self, kind: &PointerEventKind) -> bool {
        let over_bar = self.over_bar();
        let pos = self.pointer_px().filter(|_| over_bar);
        let (w, h) = (self.width as usize, self.height as usize);
        let Some(ch) = self.chooser.as_mut() else {
            return false;
        };
        let mut redraw = ch.motion(pos, w, h);
        match *kind {
            PointerEventKind::Press {
                button: BTN_LEFT, ..
            } => match pos {
                Some(pos) => redraw |= ch.press(pos, w, h),
                // A click in the list takes the keyboard from the name field.
                None => redraw |= std::mem::replace(&mut ch.typing, false),
            },
            PointerEventKind::Release {
                button: BTN_LEFT, ..
            } => {
                let action = ch.release();
                self.dialog_action(action);
            }
            _ => {}
        }
        if redraw {
            self.request_redraw();
        }
        over_bar
    }

    pub(super) fn dialog_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Redraw => self.request_redraw(),
            Action::Accept => self.dialog_accept(),
            Action::Cancel => self.exit = true,
        }
    }

    /// Enter or the Open/Select/Save button. A single selected folder is entered
    /// (picked, when choosing a folder); otherwise the selected files, the current folder,
    /// or the typed name are picked.
    pub(super) fn dialog_accept(&mut self) {
        let Some(ch) = &self.chooser else {
            return;
        };
        let selected = self.view.selected_entries();
        let one_dir = match &selected[..] {
            [(path, true)] => Some(path.clone()),
            _ => None,
        };
        match ch.mode {
            Mode::Open | Mode::OpenMany => {
                if let Some(dir) = one_dir {
                    return self.go(dir);
                }
                let files: Vec<PathBuf> = selected
                    .into_iter()
                    .filter(|(_, is_dir)| !is_dir)
                    .map(|(path, _)| path)
                    .collect();
                if !files.is_empty() && (ch.mode == Mode::OpenMany || files.len() == 1) {
                    self.dialog_finish(files);
                }
            }
            Mode::Folder => {
                let dir = one_dir.unwrap_or_else(|| self.view.listing.path.clone());
                self.dialog_finish(vec![dir]);
            }
            Mode::Save => {
                if let (false, Some(dir)) = (ch.typing, one_dir) {
                    return self.go(dir);
                }
                let name = ch.name();
                match ops::invalid_name(&name) {
                    Some(reason) => self.dialog_fail(reason),
                    None => self.dialog_save(self.view.listing.path.join(name)),
                }
            }
        }
    }

    /// A file was double-clicked (or Enter'd): pick it, or save onto it.
    pub(super) fn dialog_file(&mut self, path: PathBuf) {
        let Some(ch) = self.chooser.as_mut() else {
            return;
        };
        match ch.mode {
            Mode::Open | Mode::OpenMany => self.dialog_finish(vec![path]),
            Mode::Folder => {}
            Mode::Save => {
                if let Some(name) = path.file_name() {
                    ch.set_name(&name.to_string_lossy());
                }
                self.dialog_save(path);
            }
        }
    }

    /// In a save dialog, a single file selected in the list becomes the name.
    pub(super) fn sync_save_name(&mut self) {
        let name = self.view.selected_file_name();
        if let (Some(ch), Some(name)) = (self.chooser.as_mut(), name) {
            if ch.mode == Mode::Save {
                ch.set_name(&name);
                self.request_redraw();
            }
        }
    }

    /// Saves as `path`, asking first if it exists.
    fn dialog_save(&mut self, path: PathBuf) {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match std::fs::metadata(&path) {
            Ok(m) if m.is_dir() => self.dialog_fail(format!("\"{name}\" is a folder.")),
            Ok(_) => self.push_modal(Modal::new(modal::Kind::ConfirmReplace { path, name })),
            Err(_) => self.dialog_finish(vec![path]),
        }
    }

    fn dialog_fail(&mut self, reason: String) {
        if let Some(ch) = self.chooser.as_mut() {
            ch.fail(reason);
            self.request_redraw();
        }
    }

    /// Hands `paths` to the portal and closes.
    pub(super) fn dialog_finish(&mut self, paths: Vec<PathBuf>) {
        let Some(ch) = &self.chooser else {
            return;
        };
        match ch.write(&paths) {
            Ok(()) => self.exit = true,
            Err(e) => self.push_modal(Modal::new(modal::Kind::Errors {
                verb: "pass on",
                errors: paths.iter().map(|p| format!("{}: {e}", p.display())).collect(),
            })),
        }
    }
}
