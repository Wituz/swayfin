//! File dialog mode, run by xdg-desktop-portal-termfilechooser through
//! `portal/swayfin-wrapper.sh`: `swayfin --choose MODE PATH OUT`. A bar under the list
//! picks files, a folder, or a name to save as. The picked paths go to OUT, one per
//! line; exiting without writing it cancels.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    font::{self, CELL_H, CELL_W},
    modal::{self, BTN_GAP, BTN_H, FIELD_H, Field, Input},
    ops,
    render::Canvas,
    theme,
};

/// Height of the bar, including its 1px divider (the same as the list's header).
pub const BAR_H: usize = 24;
const PAD: usize = 8;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Open,
    OpenMany,
    Folder,
    Save,
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Field,
    Accept,
    Cancel,
}

/// What the app should do after bar input.
pub enum Action {
    None,
    Redraw,
    Accept,
    Cancel,
}

pub struct Chooser {
    pub mode: Mode,
    out: PathBuf,
    /// The name to save as (save mode only).
    field: Field,
    /// The name field has the keyboard; a click in the list takes it away.
    pub typing: bool,
    /// Why the name was rejected, shown in place of the label until it's edited.
    error: Option<String>,
    hover: Option<Hit>,
    pressed: Option<Hit>,
}

/// The bar's pieces for a window `w` x `h`: (top, label, field x and width, accept x,
/// cancel x).
struct Layout {
    top: usize,
    label: String,
    field: Option<(usize, usize)>,
    accept: usize,
    cancel: usize,
}

/// Parses `--choose MODE PATH OUT`. Returns the chooser, the folder to start in, and a
/// name there to select (an open dialog's suggested file).
pub fn from_args(args: &[OsString]) -> Option<(Chooser, PathBuf, Option<OsString>)> {
    let [flag, mode, path, out] = args else {
        return None;
    };
    if flag != "--choose" {
        return None;
    }
    let mode = match mode.to_str()? {
        "open" => Mode::Open,
        "multiple" => Mode::OpenMany,
        "directory" => Mode::Folder,
        "save" => Mode::Save,
        _ => return None,
    };
    let path = PathBuf::from(path);
    let (start, name) = if path.is_dir() {
        (path, None)
    } else {
        let parent = path.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        (parent, path.file_name().map(OsString::from))
    };
    let mut field = Field::default();
    let mut select = None;
    match (mode, name) {
        (Mode::Save, Some(name)) => {
            let name = name.to_string_lossy();
            field = Field::new(&name, ops::ext_start(&name, false));
        }
        (_, name) => select = name,
    }
    let chooser = Chooser {
        mode,
        out: PathBuf::from(out),
        field,
        typing: mode == Mode::Save,
        error: None,
        hover: None,
        pressed: None,
    };
    Some((chooser, start, select))
}

impl Chooser {
    fn labels(&self) -> (&'static str, &'static str) {
        match self.mode {
            Mode::Open => ("Open file", "Open"),
            Mode::OpenMany => ("Open files", "Open"),
            Mode::Folder => ("Select folder", "Select"),
            Mode::Save => ("Save as", "Save"),
        }
    }

    fn layout(&self, w: usize, h: usize) -> Layout {
        let (title, accept) = self.labels();
        let label = self.error.clone().unwrap_or_else(|| title.to_string());
        let cancel = w.saturating_sub(PAD + modal::btn_w("Cancel"));
        let accept = cancel.saturating_sub(BTN_GAP + modal::btn_w(accept));
        let field = (self.mode == Mode::Save).then(|| {
            let x = PAD + (label.chars().count() + 1) * CELL_W;
            (x, accept.saturating_sub(x + PAD))
        });
        Layout {
            top: h.saturating_sub(BAR_H),
            label,
            field,
            accept,
            cancel,
        }
    }

    fn hit(&self, (x, y): (usize, usize), w: usize, h: usize) -> Option<Hit> {
        let l = self.layout(w, h);
        let y = y.checked_sub(l.top + 1)?;
        let (_, accept) = self.labels();
        let in_button = |bx: usize, label| {
            (bx..bx + modal::btn_w(label)).contains(&x) && (2..2 + BTN_H).contains(&y)
        };
        if in_button(l.accept, accept) {
            Some(Hit::Accept)
        } else if in_button(l.cancel, "Cancel") {
            Some(Hit::Cancel)
        } else {
            let (fx, fw) = l.field?;
            ((fx..fx + fw).contains(&x) && (2..2 + FIELD_H).contains(&y)).then_some(Hit::Field)
        }
    }

    pub fn hovering_field(&self) -> bool {
        self.hover == Some(Hit::Field)
    }

    /// Pointer at `pos` (None: not over the bar). Returns true if a redraw is needed.
    pub fn motion(&mut self, pos: Option<(usize, usize)>, w: usize, h: usize) -> bool {
        let hover = pos.and_then(|p| self.hit(p, w, h));
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// A left press on the bar: in the field, it takes the keyboard and moves the caret.
    /// Returns true if a redraw is needed.
    pub fn press(&mut self, (x, _): (usize, usize), w: usize, h: usize) -> bool {
        self.pressed = self.hover;
        if self.hover != Some(Hit::Field) {
            return false;
        }
        if let Some((fx, fw)) = self.layout(w, h).field {
            self.field.click(x, fx, Field::cols_for(fw));
        }
        self.typing = true;
        true
    }

    /// Buttons fire on release over what was pressed.
    pub fn release(&mut self) -> Action {
        match (self.pressed.take(), self.hover) {
            (Some(Hit::Accept), Some(Hit::Accept)) => Action::Accept,
            (Some(Hit::Cancel), Some(Hit::Cancel)) => Action::Cancel,
            _ => Action::None,
        }
    }

    /// A key for the name field.
    pub fn key(&mut self, input: Input) -> Action {
        match input {
            Input::Enter => Action::Accept,
            Input::Escape => Action::Cancel,
            input => {
                if !self.field.edit(input) {
                    return Action::None;
                }
                // The error was about the old name.
                self.error = None;
                Action::Redraw
            }
        }
    }

    pub fn name(&self) -> String {
        self.field.text()
    }

    /// A file clicked in the list becomes the name to save as.
    pub fn set_name(&mut self, name: &str) {
        self.field = Field::new(name, ops::ext_start(name, false));
        self.error = None;
    }

    pub fn fail(&mut self, reason: String) {
        self.error = Some(reason);
    }

    /// Hands the picked paths to the portal.
    pub fn write(&self, paths: &[PathBuf]) -> io::Result<()> {
        let mut out = Vec::new();
        for p in paths {
            out.extend_from_slice(p.as_os_str().as_encoded_bytes());
            out.push(b'\n');
        }
        fs::write(&self.out, out)
    }

    pub fn draw(&self, c: &mut Canvas, w: usize, h: usize) {
        let l = self.layout(w, h);
        c.rect(0, l.top, w, BAR_H, theme::BG);
        c.rect(0, l.top, w, 1, theme::BORDER);
        let y = l.top + 1;
        let ty = y + (BAR_H - 1 - CELL_H) / 2;
        let label_end = l.field.map_or(l.accept, |(x, _)| x);
        c.text(PAD, ty, &l.label, &font::REGULAR, theme::TEXT_DIM, label_end);
        if let Some((fx, fw)) = l.field.filter(|&(_, fw)| fw > 0) {
            self.field
                .draw(c, fx, y + 2, fw, Field::cols_for(fw), self.typing);
        }
        let (_, accept) = self.labels();
        modal::draw_button(c, l.accept, y + 2, accept, self.hover == Some(Hit::Accept));
        modal::draw_button(c, l.cancel, y + 2, "Cancel", self.hover == Some(Hit::Cancel));
    }
}
