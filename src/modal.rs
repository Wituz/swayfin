//! Modals: centered boxes drawn over the list, one at a time. Buttons and the checkbox
//! fire on release over what was pressed; text fields take keyboard input.

use std::{cell::Cell, path::PathBuf};

use crate::{
    font::{self, CELL_H, CELL_W},
    ops::{Choice, Conflict},
    render::Canvas,
    theme,
};

const MARGIN: usize = 16;
const PAD: usize = 12;
const GAP: usize = 12;
const SMALL_GAP: usize = 8;
const MAX_COLS: usize = 60;
pub const BTN_H: usize = 20;
const BTN_PAD: usize = CELL_W;
pub const BTN_GAP: usize = 8;
const CHECK: usize = 12;
const FIELD_COLS: usize = 40;
pub const FIELD_H: usize = 20;
const FIELD_PAD: usize = 4;
const LIST_ROW_H: usize = 18;
const LIST_MAX_ROWS: usize = 10;
const LIST_MIN_ROWS: usize = 3;
const DOUBLE_CLICK_MS: u32 = 400;
const BTN_LEFT: u32 = crate::view::BTN_LEFT;

pub enum Kind {
    Conflict {
        conflict: Conflict,
        checked: bool,
    },
    Errors {
        /// "move", "delete": the title reads "Couldn't <verb> N items:".
        verb: &'static str,
        errors: Vec<String>,
    },
    ConfirmDelete {
        paths: Vec<PathBuf>,
        /// The single item's name, if there is only one.
        name: Option<String>,
    },
    /// File dialog: saving onto `path`, which exists.
    ConfirmReplace { path: PathBuf, name: String },
    /// A one-line text prompt: a new folder's name, a new name, or a zoxide query.
    Prompt {
        purpose: Prompt,
        field: Field,
        error: Option<String>,
        /// Submitted and the result hasn't come back yet.
        pending: bool,
    },
    /// Pick an app to open `file` with, from `apps` (id, name) filtered by `field`.
    OpenWith {
        file: PathBuf,
        file_name: String,
        mime: String,
        apps: Vec<(String, String)>,
        field: Field,
        /// Index into the filtered list.
        highlight: usize,
        /// First visible row of the filtered list.
        scroll: usize,
        /// The "Always use" checkbox; None when the pick is always saved as the default.
        remember: Option<bool>,
        last_click: Option<(usize, u32)>,
    },
}

pub enum Prompt {
    NewFolder,
    Rename { path: PathBuf, old: String },
    Duplicate { path: PathBuf, old: String },
    GoTo,
}

pub enum Outcome {
    /// Conflict answered: the choice, and whether it applies to the rest of the job.
    Answer(Choice, bool),
    /// The prompt's text was submitted. The modal stays open until the result is known.
    Submit(String),
    Delete(Vec<PathBuf>),
    Replace(PathBuf),
    /// Open `file` with app `id`, first saving it as the default for the type if `save`.
    OpenWith {
        file: PathBuf,
        id: String,
        save: Option<String>,
    },
    Dismissed,
}

/// Editing keys for the name field, already resolved from the keyboard layout.
pub enum Input {
    Text(String),
    Backspace { word: bool },
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    Enter,
    Escape,
}

#[derive(Default)]
pub struct Field {
    text: Vec<char>,
    /// Caret position as a char index: 0..=text.len().
    caret: usize,
}

impl Field {
    /// Prefilled with `text`, caret at char index `caret`.
    pub fn new(text: &str, caret: usize) -> Self {
        let text: Vec<char> = text.chars().collect();
        let caret = caret.min(text.len());
        Self { text, caret }
    }

    pub fn text(&self) -> String {
        self.text.iter().collect()
    }

    /// Applies an editing key. Returns false for keys that don't edit (Enter, Esc, Up, Down).
    pub fn edit(&mut self, input: Input) -> bool {
        match input {
            Input::Text(s) => self.insert(&s),
            Input::Backspace { word } => self.backspace(word),
            Input::Delete => self.delete(),
            Input::Left => self.caret = self.caret.saturating_sub(1),
            Input::Right => self.caret = (self.caret + 1).min(self.text.len()),
            Input::Home => self.caret = 0,
            Input::End => self.caret = self.text.len(),
            Input::Enter | Input::Escape | Input::Up | Input::Down => return false,
        }
        true
    }

    /// Moves the caret to the char boundary nearest `px`, in a field drawn at `x` showing
    /// `cols` columns.
    pub fn click(&mut self, px: usize, x: usize, cols: usize) {
        let tx = x + 1 + FIELD_PAD;
        let col = (px.saturating_sub(tx) + CELL_W / 2) / CELL_W;
        self.caret = (self.offset(cols) + col).min(self.text.len());
    }

    /// Columns of text that fit a field `w` pixels wide.
    pub fn cols_for(w: usize) -> usize {
        w.saturating_sub(2 + 2 * FIELD_PAD) / CELL_W
    }

    /// The field's box, `w` x FIELD_H at (x, y), showing `cols` columns; the caret only
    /// when it has the keyboard.
    pub fn draw(&self, c: &mut Canvas, x: usize, y: usize, w: usize, cols: usize, caret: bool) {
        c.rect(x, y, w, FIELD_H, theme::BG);
        c.frame(x, y, w, FIELD_H, theme::BORDER);
        let (tx, ty) = (x + 1 + FIELD_PAD, y + (FIELD_H - CELL_H) / 2);
        let offset = self.offset(cols);
        let end = (offset + cols).min(self.text.len());
        let visible: String = self.text[offset..end].iter().collect();
        c.text(
            tx,
            ty,
            &visible,
            &font::REGULAR,
            theme::TEXT,
            tx + cols * CELL_W,
        );
        if caret {
            // Tamzen leaves a cell's first column blank (all but "_{£"), so the bar goes
            // there.
            let cx = tx + (self.caret - offset) * CELL_W;
            c.rect(cx, ty, 1, CELL_H, theme::TEXT);
        }
    }

    fn insert(&mut self, s: &str) {
        let chars: Vec<char> = s.chars().filter(|c| !c.is_control()).collect();
        let n = chars.len();
        self.text.splice(self.caret..self.caret, chars);
        self.caret += n;
    }

    /// With `word`, deletes back over any spaces, then over one run of letters/digits
    /// (or one run of other symbols).
    fn backspace(&mut self, word: bool) {
        let end = self.caret;
        let mut start = end.saturating_sub(1);
        if word {
            start = end;
            while start > 0 && self.text[start - 1].is_whitespace() {
                start -= 1;
            }
            let alnum = start > 0 && self.text[start - 1].is_alphanumeric();
            while start > 0
                && !self.text[start - 1].is_whitespace()
                && self.text[start - 1].is_alphanumeric() == alnum
            {
                start -= 1;
            }
        }
        self.text.drain(start..end);
        self.caret = start;
    }

    fn delete(&mut self) {
        if self.caret < self.text.len() {
            self.text.remove(self.caret);
        }
    }

    /// First visible char when `cols` fit: keeps the caret in view.
    fn offset(&self, cols: usize) -> usize {
        self.caret.saturating_sub(cols)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Button(usize),
    Check,
    Field,
    /// A row of the app list: index into the filtered list.
    Row(usize),
}

#[derive(Clone, Copy)]
enum Action {
    Choice(Choice),
    Submit,
    Delete,
    Replace,
    Open,
    Dismiss,
}

pub struct Modal {
    kind: Kind,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    /// App list rows that fit, from the last layout; keys and the wheel scroll by it.
    list_rows: Cell<usize>,
}

struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn contains(&self, x: usize, y: usize) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }
}

/// Content stacked top to bottom inside the modal.
enum Block {
    Line(String, u32),
    Gap(usize),
    Field(usize),
    Check(String),
    /// App list: visible rows, and width in columns.
    List(usize, usize),
    Buttons(Vec<&'static str>),
}

enum Item {
    Text(usize, usize, String, u32),
    Field(Rect, usize),
    Check(Rect, String),
    List(Rect, usize),
    Button(Rect, &'static str),
}

struct Layout {
    frame: Rect,
    items: Vec<Item>,
}

pub fn btn_w(label: &str) -> usize {
    label.len() * CELL_W + 2 * BTN_PAD
}

/// A `btn_w(label)` x BTN_H button at (x, y); `hot` while hovered.
pub fn draw_button(c: &mut Canvas, x: usize, y: usize, label: &str, hot: bool) {
    c.rect(x, y, btn_w(label), BTN_H, theme::BORDER);
    let color = if hot { theme::TEXT } else { theme::TEXT_DIM };
    let ty = y + (BTN_H - CELL_H) / 2;
    c.text(x + BTN_PAD, ty, label, &font::REGULAR, color, x + btn_w(label));
}

impl Block {
    fn size(&self) -> (usize, usize) {
        match self {
            Block::Line(s, _) => (s.chars().count() * CELL_W, CELL_H),
            Block::Gap(h) => (0, *h),
            Block::Field(cols) => (cols * CELL_W + 2 * FIELD_PAD + 2, FIELD_H),
            Block::Check(l) => (CHECK + CELL_W + l.chars().count() * CELL_W, CELL_H),
            Block::List(rows, cols) => (cols * CELL_W + 2 * FIELD_PAD + 2, rows * LIST_ROW_H + 2),
            Block::Buttons(labels) => {
                let w = labels.iter().map(|l| btn_w(l)).sum::<usize>()
                    + BTN_GAP * labels.len().saturating_sub(1);
                (w, BTN_H)
            }
        }
    }
}

impl Modal {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            hover: None,
            pressed: None,
            list_rows: Cell::new(LIST_MAX_ROWS),
        }
    }

    /// `save_always`: the pick becomes the default (no app was set); otherwise an
    /// "Always use" checkbox decides.
    pub fn open_with(
        file: PathBuf,
        mime: String,
        apps: Vec<(String, String)>,
        save_always: bool,
    ) -> Self {
        let file_name = file
            .file_name()
            .unwrap_or(file.as_os_str())
            .to_string_lossy()
            .into_owned();
        Self::new(Kind::OpenWith {
            file,
            file_name,
            mime,
            apps,
            field: Field::default(),
            highlight: 0,
            scroll: 0,
            remember: (!save_always).then_some(false),
            last_click: None,
        })
    }

    fn field(&self) -> Option<&Field> {
        match &self.kind {
            Kind::Prompt { field, .. } | Kind::OpenWith { field, .. } => Some(field),
            _ => None,
        }
    }

    fn field_mut(&mut self) -> Option<&mut Field> {
        match &mut self.kind {
            Kind::Prompt { field, .. } | Kind::OpenWith { field, .. } => Some(field),
            _ => None,
        }
    }

    /// Indices of the apps whose name or id contains the filter text (case-insensitive).
    fn filtered(&self) -> Vec<usize> {
        let Kind::OpenWith { apps, field, .. } = &self.kind else {
            return Vec::new();
        };
        let needle: String = field.text.iter().collect::<String>().to_lowercase();
        (0..apps.len())
            .filter(|&i| {
                let (id, name) = &apps[i];
                name.to_lowercase().contains(&needle) || id.to_lowercase().contains(&needle)
            })
            .collect()
    }

    fn checked(&self) -> bool {
        matches!(
            self.kind,
            Kind::Conflict { checked: true, .. }
                | Kind::OpenWith {
                    remember: Some(true),
                    ..
                }
        )
    }

    pub fn new_folder() -> Self {
        Self::prompt(Prompt::NewFolder)
    }

    pub fn go_to() -> Self {
        Self::prompt(Prompt::GoTo)
    }

    fn prompt(purpose: Prompt) -> Self {
        Self::new(Kind::Prompt {
            purpose,
            field: Field::default(),
            error: None,
            pending: false,
        })
    }

    /// Prefilled with the current name, caret before the extension.
    pub fn rename(path: PathBuf, old: String, is_dir: bool) -> Self {
        let caret = crate::ops::ext_start(&old, is_dir);
        Self::new(Kind::Prompt {
            field: Field::new(&old, caret),
            purpose: Prompt::Rename { path, old },
            error: None,
            pending: false,
        })
    }

    /// Prefilled with a free `name_N.ext`, caret before the extension.
    pub fn duplicate(path: PathBuf, old: String, name: String, is_dir: bool) -> Self {
        let caret = crate::ops::ext_start(&name, is_dir);
        Self::new(Kind::Prompt {
            field: Field::new(&name, caret),
            purpose: Prompt::Duplicate { path, old },
            error: None,
            pending: false,
        })
    }

    pub fn prompt_for(&self) -> Option<&Prompt> {
        match &self.kind {
            Kind::Prompt { purpose, .. } => Some(purpose),
            _ => None,
        }
    }

    /// The conflict this modal asks about, if it is one.
    pub fn conflict(&self) -> Option<&Conflict> {
        match &self.kind {
            Kind::Conflict { conflict, .. } => Some(conflict),
            _ => None,
        }
    }

    pub fn takes_text(&self) -> bool {
        self.field().is_some()
    }

    /// A prompt that was submitted and is waiting for its result.
    pub fn is_pending(&self) -> bool {
        matches!(self.kind, Kind::Prompt { pending: true, .. })
    }

    pub fn hovering_field(&self) -> bool {
        self.hover == Some(Hit::Field)
    }

    /// The input was rejected or the operation failed: show why and let the user retry.
    pub fn prompt_failed(&mut self, reason: String) {
        if let Kind::Prompt { error, pending, .. } = &mut self.kind {
            *error = Some(reason);
            *pending = false;
        }
    }

    fn buttons(&self) -> Vec<(&'static str, Action)> {
        match &self.kind {
            Kind::Conflict { conflict, .. } => {
                let mut b = Vec::with_capacity(4);
                if conflict.can_replace {
                    let label = if conflict.merge { "Merge" } else { "Replace" };
                    b.push((label, Action::Choice(Choice::Replace)));
                }
                b.push(("Leave it", Action::Choice(Choice::Leave)));
                b.push(("Number suffix", Action::Choice(Choice::Suffix)));
                b.push(("Cancel", Action::Choice(Choice::Cancel)));
                b
            }
            Kind::Errors { .. } => vec![("OK", Action::Dismiss)],
            Kind::ConfirmDelete { .. } => {
                vec![("Delete", Action::Delete), ("Cancel", Action::Dismiss)]
            }
            Kind::ConfirmReplace { .. } => {
                vec![("Replace", Action::Replace), ("Cancel", Action::Dismiss)]
            }
            Kind::Prompt { purpose, .. } => {
                let label = match purpose {
                    Prompt::NewFolder => "Create",
                    Prompt::Rename { .. } => "Rename",
                    Prompt::Duplicate { .. } => "Duplicate",
                    Prompt::GoTo => "Go",
                };
                vec![(label, Action::Submit), ("Cancel", Action::Dismiss)]
            }
            Kind::OpenWith { .. } => vec![("Open", Action::Open), ("Cancel", Action::Dismiss)],
        }
    }

    fn blocks(&self, cols: usize, h: usize) -> Vec<Block> {
        let mut blocks = Vec::new();
        let lines = |blocks: &mut Vec<Block>, text: &str, color| {
            for l in wrap(text, cols, cols) {
                blocks.push(Block::Line(l, color));
            }
        };
        match &self.kind {
            Kind::Conflict { conflict, .. } => {
                let msg = format!(
                    "\"{}\" already exists in \"{}\".",
                    conflict.name, conflict.dest
                );
                lines(&mut blocks, &msg, theme::TEXT);
                if conflict.remaining > 0 {
                    let n = conflict.remaining;
                    let files = if n == 1 { "file" } else { "files" };
                    blocks.push(Block::Gap(GAP));
                    blocks.push(Block::Check(format!(
                        "Do the same for the remainder of the {n} {files}"
                    )));
                }
            }
            Kind::ConfirmDelete { paths, name } => {
                let msg = match name {
                    Some(name) => format!("Permanently delete \"{name}\"?"),
                    None => format!("Permanently delete {} items?", paths.len()),
                };
                lines(&mut blocks, &msg, theme::TEXT);
            }
            Kind::ConfirmReplace { name, .. } => {
                let msg = format!("\"{name}\" already exists. Replace it?");
                lines(&mut blocks, &msg, theme::TEXT);
            }
            Kind::Errors { verb, errors } => {
                let n = errors.len();
                let s = if n == 1 { "" } else { "s" };
                let title = format!("Couldn't {verb} {n} item{s}:");
                lines(&mut blocks, &title, theme::TEXT);
                blocks.push(Block::Line(String::new(), theme::TEXT));
                // Keep the modal inside the window; summarize what doesn't fit.
                let chrome = 2 * PAD + GAP + BTN_H + 2 * MARGIN;
                let max_lines = (h.saturating_sub(chrome) / CELL_H).max(blocks.len() + 1);
                for (i, e) in errors.iter().enumerate() {
                    // Continuation lines are indented so each error reads as one entry.
                    let wrapped = wrap(e, cols, cols.saturating_sub(2));
                    let left = errors.len() - i;
                    if blocks.len() + wrapped.len() + usize::from(left > 1) > max_lines {
                        blocks.push(Block::Line(format!("...and {left} more"), theme::TEXT_DIM));
                        break;
                    }
                    for (j, l) in wrapped.into_iter().enumerate() {
                        let l = if j == 0 { l } else { format!("  {l}") };
                        blocks.push(Block::Line(l, theme::TEXT_DIM));
                    }
                }
            }
            Kind::Prompt { purpose, error, .. } => {
                match purpose {
                    Prompt::NewFolder => blocks.push(Block::Line("New folder".into(), theme::TEXT)),
                    Prompt::Rename { old, .. } => {
                        lines(&mut blocks, &format!("Rename \"{old}\""), theme::TEXT)
                    }
                    Prompt::Duplicate { old, .. } => {
                        lines(&mut blocks, &format!("Duplicate \"{old}\""), theme::TEXT)
                    }
                    Prompt::GoTo => {
                        blocks.push(Block::Line("Quick go-to folder".into(), theme::TEXT))
                    }
                }
                blocks.push(Block::Gap(SMALL_GAP));
                blocks.push(Block::Field(FIELD_COLS.min(cols)));
                if let Some(e) = error {
                    blocks.push(Block::Gap(SMALL_GAP));
                    lines(&mut blocks, e, theme::TEXT_DIM);
                }
            }
            Kind::OpenWith {
                file_name,
                mime,
                remember,
                ..
            } => {
                lines(
                    &mut blocks,
                    &format!("Open \"{file_name}\" with..."),
                    theme::TEXT,
                );
                let sub = match remember {
                    None => format!("{mime} - saved as default"),
                    Some(_) => mime.clone(),
                };
                lines(&mut blocks, &sub, theme::TEXT_DIM);
                blocks.push(Block::Gap(SMALL_GAP));
                let field_cols = FIELD_COLS.min(cols);
                blocks.push(Block::Field(field_cols));
                blocks.push(Block::Gap(SMALL_GAP));
                // As many rows as fit (between the min and max), after everything else.
                let fixed = 3 * CELL_H + FIELD_H + BTN_H + 2 * SMALL_GAP + 2 * GAP + 2;
                let room = h.saturating_sub(2 * (MARGIN + PAD) + fixed) / LIST_ROW_H;
                let rows = room.clamp(LIST_MIN_ROWS, LIST_MAX_ROWS);
                self.list_rows.set(rows);
                blocks.push(Block::List(rows, field_cols));
                if remember.is_some() {
                    blocks.push(Block::Gap(GAP));
                    blocks.push(Block::Check(format!("Always use for {mime}")));
                }
            }
        }
        blocks.push(Block::Gap(GAP));
        blocks.push(Block::Buttons(self.buttons().iter().map(|b| b.0).collect()));
        blocks
    }

    fn layout(&self, w: usize, h: usize) -> Layout {
        let cols = MAX_COLS
            .min(w.saturating_sub(2 * (MARGIN + PAD) + 2) / CELL_W)
            .max(1);
        let blocks = self.blocks(cols, h);
        let inner_w = blocks.iter().map(|b| b.size().0).max().unwrap_or(0);
        let inner_h: usize = blocks.iter().map(|b| b.size().1).sum();
        let (fw, fh) = (inner_w + 2 * PAD, inner_h + 2 * PAD);
        let frame = Rect {
            x: w.saturating_sub(fw) / 2,
            y: h.saturating_sub(fh) / 2,
            w: fw,
            h: fh,
        };

        let x = frame.x + PAD;
        let mut y = frame.y + PAD;
        let mut items = Vec::new();
        for block in blocks {
            let (bw, bh) = block.size();
            match block {
                Block::Line(s, color) => items.push(Item::Text(x, y, s, color)),
                Block::Gap(_) => {}
                Block::Field(cols) => items.push(Item::Field(Rect { x, y, w: bw, h: bh }, cols)),
                Block::Check(l) => items.push(Item::Check(Rect { x, y, w: bw, h: bh }, l)),
                Block::List(rows, _) => items.push(Item::List(Rect { x, y, w: bw, h: bh }, rows)),
                Block::Buttons(labels) => {
                    let mut bx = frame.x + (fw - bw) / 2;
                    for l in labels {
                        let r = Rect {
                            x: bx,
                            y,
                            w: btn_w(l),
                            h: BTN_H,
                        };
                        bx += r.w + BTN_GAP;
                        items.push(Item::Button(r, l));
                    }
                }
            }
            y += bh;
        }
        Layout { frame, items }
    }

    fn hit(&self, x: usize, y: usize, w: usize, h: usize) -> Option<Hit> {
        let mut button = 0;
        for item in self.layout(w, h).items {
            match item {
                Item::Button(r, _) => {
                    if r.contains(x, y) {
                        return Some(Hit::Button(button));
                    }
                    button += 1;
                }
                Item::Check(r, _) if r.contains(x, y) => return Some(Hit::Check),
                Item::Field(r, _) if r.contains(x, y) => return Some(Hit::Field),
                Item::List(r, rows) if r.contains(x, y) => {
                    let Kind::OpenWith { scroll, .. } = &self.kind else {
                        return None;
                    };
                    let row = (y - r.y).saturating_sub(1) / LIST_ROW_H;
                    let idx = scroll + row.min(rows - 1);
                    return (idx < self.filtered().len()).then_some(Hit::Row(idx));
                }
                _ => {}
            }
        }
        None
    }

    /// Returns true if a redraw is needed.
    pub fn motion(&mut self, pos: Option<(usize, usize)>, w: usize, h: usize) -> bool {
        let hover = pos.and_then(|(x, y)| self.hit(x, y, w, h));
        std::mem::replace(&mut self.hover, hover) != hover
    }

    /// A press in a text field moves the caret there; a press on an app row highlights
    /// it, and a second press on it within the double-click time opens it.
    /// Returns (redraw, outcome).
    pub fn press(
        &mut self,
        button: u32,
        time: u32,
        pos: Option<(usize, usize)>,
        w: usize,
        h: usize,
    ) -> (bool, Option<Outcome>) {
        if button != BTN_LEFT {
            return (false, None);
        }
        self.pressed = self.hover;
        match self.hover {
            Some(Hit::Field) => {
                let Some((px, _)) = pos else {
                    return (false, None);
                };
                let found = self.layout(w, h).items.into_iter().find_map(|i| match i {
                    Item::Field(r, cols) => Some((r, cols)),
                    _ => None,
                });
                let (Some((r, cols)), Some(field)) = (found, self.field_mut()) else {
                    return (false, None);
                };
                field.click(px, r.x, cols);
                (true, None)
            }
            Some(Hit::Row(i)) => {
                let Kind::OpenWith {
                    highlight,
                    last_click,
                    ..
                } = &mut self.kind
                else {
                    return (false, None);
                };
                *highlight = i;
                let double = matches!(*last_click,
                    Some((j, t)) if j == i && time.wrapping_sub(t) <= DOUBLE_CLICK_MS);
                if double {
                    *last_click = None;
                    return (true, self.act(Action::Open));
                }
                *last_click = Some((i, time));
                (true, None)
            }
            _ => (false, None),
        }
    }

    /// Wheel over the app list. Returns true if a redraw is needed.
    pub fn scroll(&mut self, value120: i32, pixels: f64) -> bool {
        let len = self.filtered().len();
        let rows = self.list_rows.get();
        let Kind::OpenWith { scroll, .. } = &mut self.kind else {
            return false;
        };
        let delta = if value120 != 0 {
            value120 as f64 / 120.0 * 3.0
        } else {
            pixels / LIST_ROW_H as f64
        };
        let max = len.saturating_sub(rows) as f64;
        let next = (*scroll as f64 + delta).round().clamp(0.0, max) as usize;
        std::mem::replace(scroll, next) != next
    }

    /// Returns (redraw, outcome).
    pub fn release(&mut self, button: u32) -> (bool, Option<Outcome>) {
        if button != BTN_LEFT {
            return (false, None);
        }
        let hit = self.pressed.take();
        if hit != self.hover {
            return (false, None);
        }
        match hit {
            Some(Hit::Check) => {
                match &mut self.kind {
                    Kind::Conflict { checked, .. } => *checked = !*checked,
                    Kind::OpenWith {
                        remember: Some(r), ..
                    } => *r = !*r,
                    _ => {}
                }
                (true, None)
            }
            Some(Hit::Button(i)) => {
                let action = self.buttons()[i].1;
                (true, self.act(action))
            }
            Some(Hit::Field | Hit::Row(_)) | None => (false, None),
        }
    }

    fn act(&mut self, action: Action) -> Option<Outcome> {
        match (action, &mut self.kind) {
            (Action::Choice(choice), Kind::Conflict { checked, conflict }) => {
                Some(Outcome::Answer(choice, *checked && conflict.remaining > 0))
            }
            (Action::Delete, Kind::ConfirmDelete { paths, .. }) => {
                Some(Outcome::Delete(std::mem::take(paths)))
            }
            (Action::Replace, Kind::ConfirmReplace { path, .. }) => {
                Some(Outcome::Replace(path.clone()))
            }
            (Action::Submit, Kind::Prompt { field, pending, .. }) => {
                if *pending {
                    return None;
                }
                *pending = true;
                Some(Outcome::Submit(field.text.iter().collect()))
            }
            (Action::Open, Kind::OpenWith { .. }) => {
                let filtered = self.filtered();
                let Kind::OpenWith {
                    file,
                    mime,
                    apps,
                    highlight,
                    remember,
                    ..
                } = &self.kind
                else {
                    return None;
                };
                let &i = filtered.get(*highlight)?;
                Some(Outcome::OpenWith {
                    file: file.clone(),
                    id: apps[i].0.clone(),
                    save: remember.unwrap_or(true).then(|| mime.clone()),
                })
            }
            _ => Some(Outcome::Dismissed),
        }
    }

    /// Keyboard input: editing keys for the name field; Enter/Esc also confirm or cancel
    /// a delete. Returns (redraw, outcome).
    pub fn key(&mut self, input: Input) -> (bool, Option<Outcome>) {
        let confirm = match self.kind {
            Kind::ConfirmDelete { .. } => Some(Action::Delete),
            Kind::ConfirmReplace { .. } => Some(Action::Replace),
            _ => None,
        };
        if let Some(action) = confirm {
            return match input {
                Input::Enter => (true, self.act(action)),
                Input::Escape => (true, Some(Outcome::Dismissed)),
                _ => (false, None),
            };
        }
        if let Kind::OpenWith { .. } = self.kind {
            return self.list_key(input);
        }
        let Kind::Prompt { field, error, .. } = &mut self.kind else {
            return (false, None);
        };
        match input {
            Input::Enter => return (true, self.act(Action::Submit)),
            Input::Up | Input::Down => return (false, None),
            Input::Escape => return (true, Some(Outcome::Dismissed)),
            input => {
                field.edit(input);
            }
        }
        // The error was about the old name.
        *error = None;
        (true, None)
    }

    /// Keys in the "Open with" modal: arrows move the highlight, typing filters.
    fn list_key(&mut self, input: Input) -> (bool, Option<Outcome>) {
        let len = self.filtered().len();
        let rows = self.list_rows.get();
        let Kind::OpenWith {
            field,
            highlight,
            scroll,
            ..
        } = &mut self.kind
        else {
            return (false, None);
        };
        match input {
            Input::Enter => return (true, self.act(Action::Open)),
            Input::Escape => return (true, Some(Outcome::Dismissed)),
            Input::Up | Input::Down => {
                if len == 0 {
                    return (false, None);
                }
                *highlight = match input {
                    Input::Up => highlight.saturating_sub(1),
                    _ => (*highlight + 1).min(len - 1),
                };
                if *highlight < *scroll {
                    *scroll = *highlight;
                } else if *highlight >= *scroll + rows {
                    *scroll = *highlight + 1 - rows;
                }
                return (true, None);
            }
            input => {
                field.edit(input);
            }
        }
        // The filter changed: start from the best match again.
        *highlight = 0;
        *scroll = 0;
        (true, None)
    }

    pub fn draw(&self, c: &mut Canvas, w: usize, h: usize) {
        let l = self.layout(w, h);
        let f = &l.frame;
        c.rect(f.x, f.y, f.w, f.h, theme::SURFACE);
        c.frame(f.x, f.y, f.w, f.h, theme::BORDER);
        let right = f.x + f.w - PAD;

        let mut button = 0;
        for item in &l.items {
            match item {
                Item::Text(x, y, s, color) => {
                    c.text(*x, *y, s, &font::REGULAR, *color, right);
                }
                Item::Field(r, cols) => {
                    if let Some(field) = self.field() {
                        field.draw(c, r.x, r.y, r.w, *cols, true);
                    }
                }
                Item::Check(r, label) => {
                    let hot = self.hover == Some(Hit::Check);
                    let color = if hot { theme::TEXT } else { theme::TEXT_DIM };
                    let by = r.y + (CELL_H - CHECK) / 2;
                    c.frame(r.x, by, CHECK, CHECK, color);
                    if self.checked() {
                        c.rect(r.x + 3, by + 3, CHECK - 6, CHECK - 6, theme::TEXT);
                    }
                    c.text(
                        r.x + CHECK + CELL_W,
                        r.y,
                        label,
                        &font::REGULAR,
                        color,
                        right,
                    );
                }
                Item::List(r, rows) => self.draw_list(c, r, *rows),
                Item::Button(r, label) => {
                    let hot = self.hover == Some(Hit::Button(button));
                    button += 1;
                    draw_button(c, r.x, r.y, label, hot);
                }
            }
        }
    }
}

impl Modal {
    fn draw_list(&self, c: &mut Canvas, r: &Rect, rows: usize) {
        let Kind::OpenWith {
            apps,
            highlight,
            scroll,
            ..
        } = &self.kind
        else {
            return;
        };
        c.rect(r.x, r.y, r.w, r.h, theme::BG);
        c.frame(r.x, r.y, r.w, r.h, theme::BORDER);
        let filtered = self.filtered();
        let tx = r.x + 1 + FIELD_PAD;
        let right = r.x + r.w - 1 - FIELD_PAD;
        let dy = (LIST_ROW_H - CELL_H) / 2;
        if filtered.is_empty() {
            let msg = "No matching apps";
            c.text(
                tx,
                r.y + 1 + dy,
                msg,
                &font::REGULAR,
                theme::TEXT_DIM,
                right,
            );
            return;
        }
        for (row, (idx, &app)) in filtered
            .iter()
            .enumerate()
            .skip(*scroll)
            .take(rows)
            .enumerate()
        {
            let y = r.y + 1 + row * LIST_ROW_H;
            let color = if idx == *highlight {
                c.rect(r.x + 1, y, r.w - 2, LIST_ROW_H, theme::TEXT);
                theme::BG
            } else {
                if self.hover == Some(Hit::Row(idx)) {
                    c.rect(r.x + 1, y, r.w - 2, LIST_ROW_H, theme::BORDER);
                }
                theme::TEXT
            };
            c.text(tx, y + dy, &apps[app].1, &font::REGULAR, color, right);
        }
    }
}

/// Greedy word wrap: the first line gets `first` columns, the rest `rest`.
/// Words longer than a line are broken.
fn wrap(text: &str, first: usize, rest: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line: Vec<char> = Vec::new();
    let cols = |lines: &Vec<String>| if lines.is_empty() { first } else { rest }.max(1);
    for word in text.split(' ') {
        let mut word: Vec<char> = word.chars().collect();
        if !line.is_empty() && line.len() + 1 + word.len() > cols(&lines) {
            lines.push(std::mem::take(&mut line).into_iter().collect());
        }
        if !line.is_empty() {
            line.push(' ');
        }
        while line.len() + word.len() > cols(&lines) {
            let rest = word.split_off(cols(&lines) - line.len());
            line.append(&mut word);
            lines.push(std::mem::take(&mut line).into_iter().collect());
            word = rest;
        }
        line.append(&mut word);
    }
    lines.push(line.into_iter().collect());
    lines
}
