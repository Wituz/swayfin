//! What's on screen: layout, hit testing, pointer input and drawing. No Wayland here.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::{
    font::{self, CELL_H, CELL_W},
    fs::Listing,
    modal::{self, Modal},
    render::Canvas,
    theme,
    thumbs::{self, Thumb},
};

/// Known thumbnails by path: the file's mtime they were made for, and None if there is
/// no thumbnail for it.
pub type Thumbs = HashMap<PathBuf, (i64, Option<Thumb>)>;

const PAD: usize = 8;
const HEADER_H: usize = 24;
const ROW_H: usize = 20;
const UP_W: usize = HEADER_H;
const SIZE_W: usize = 10 * CELL_W;
const DATE_W: usize = 16 * CELL_W;
const COL_GAP: usize = 2 * CELL_W;
/// Names start after the thumbnail column, which every row reserves.
const NAME_X: usize = PAD + thumbs::SIZE + 6;
const SCROLL_ROWS: f64 = 3.0;
const DOUBLE_CLICK_MS: u32 = 400;
const DRAG_THRESHOLD: usize = 4;
pub const BTN_LEFT: u32 = 0x110;

/// A folder as it was left: for going back/forward to it.
#[derive(Clone)]
pub struct ViewState {
    pub path: PathBuf,
    scroll: usize,
    selected: Vec<OsString>,
    cursor: Option<OsString>,
}

/// Where a drop would land.
#[derive(Clone, Copy, PartialEq)]
enum DropTarget {
    /// Into this folder row.
    Row(usize),
    /// Into the folder the window shows.
    Here,
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Up,
    Row(usize),
    /// List area below the last row.
    Blank,
}

pub enum Effect {
    None,
    Redraw,
    Navigate(PathBuf),
    /// Open a file with its default app.
    Open(PathBuf),
    /// Play, pause or resume an audio file.
    TogglePlay(PathBuf),
    /// Start a drag of these paths, with this label on the drag icon.
    StartDrag(Vec<PathBuf>, String),
}

pub struct View {
    pub listing: Listing,
    w: usize,
    h: usize,
    scroll: usize,
    pointer: Option<(usize, usize)>,
    hover: Option<Target>,
    pressed: Option<Target>,
    last_click: Option<(usize, u32)>,
    selected: Vec<bool>,
    /// Selection outside the anchor..=cursor range, restored when that range shrinks.
    base: Vec<bool>,
    /// Fixed end of a range selection: the last plain or Ctrl-clicked row.
    anchor: Option<usize>,
    /// Moving end of a range selection.
    cursor: Option<usize>,
    /// Where a left press on a row happened; a drag starts once the pointer moves away.
    press_at: Option<(usize, usize)>,
    /// A plain press on an already selected row collapses the selection to it on release,
    /// unless it turns into a drag of the whole selection.
    collapse_to: Option<usize>,
    /// Rows being dragged by our own drag, and the folder row it would drop into.
    drag: Option<Vec<usize>>,
    drop_target: Option<DropTarget>,
    /// A Ctrl+press on this row toggles it on release, unless it turns into a drag.
    toggle_on_release: Option<usize>,
    /// Select this entry when the next listing arrives (after a rename, or going up).
    select_next: Option<OsString>,
    /// Scroll and selection to put back when the next listing of its folder arrives.
    restore: Option<ViewState>,
}

impl View {
    pub fn new(listing: Listing) -> Self {
        Self {
            listing,
            w: 0,
            h: 0,
            scroll: 0,
            pointer: None,
            hover: None,
            pressed: None,
            last_click: None,
            selected: Vec::new(),
            base: Vec::new(),
            anchor: None,
            cursor: None,
            press_at: None,
            collapse_to: None,
            drag: None,
            drop_target: None,
            toggle_on_release: None,
            select_next: None,
            restore: None,
        }
    }

    /// Installs a freshly read listing. A new listing of the same folder (a refresh)
    /// keeps everything by name: scroll, selection, the last-touched row, and any press,
    /// double-click or drag in progress, since refreshes can come at any time (the folder
    /// is watched). A folder being returned to via history gets its saved state back.
    pub fn set_listing(&mut self, listing: Listing) {
        let same = listing.path == self.listing.path;
        let restore = self.restore.take().filter(|r| r.path == listing.path);
        let select_next = self.select_next.take();
        let old = std::mem::replace(&mut self.listing, listing);
        let index: HashMap<OsString, usize> = self
            .listing
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.raw.clone(), i))
            .collect();
        let find = |name: &OsString| index.get(name).copied();
        let n = self.listing.entries.len();

        if same {
            let remap = |i: usize| old.entries.get(i).and_then(|e| find(&e.raw));
            let names = |flags: &[bool]| -> Vec<bool> {
                let mut out = vec![false; n];
                for (e, _) in old.entries.iter().zip(flags).filter(|(_, f)| **f) {
                    if let Some(j) = find(&e.raw) {
                        out[j] = true;
                    }
                }
                out
            };
            self.selected = names(&self.selected);
            self.base = names(&self.base);
            let row = |t: Option<Target>| match t {
                Some(Target::Row(i)) => remap(i).map(Target::Row),
                other => other,
            };
            self.pressed = row(self.pressed);
            self.anchor = self.anchor.and_then(remap);
            self.cursor = self.cursor.and_then(remap);
            self.collapse_to = self.collapse_to.and_then(remap);
            self.toggle_on_release = self.toggle_on_release.and_then(remap);
            self.last_click = self.last_click.and_then(|(i, t)| remap(i).map(|j| (j, t)));
            self.drag = self
                .drag
                .take()
                .map(|rows| rows.into_iter().filter_map(remap).collect());
            self.drop_target = match self.drop_target {
                Some(DropTarget::Row(i)) => remap(i).map(DropTarget::Row),
                other => other,
            };
        } else {
            self.selected = vec![false; n];
            self.scroll = 0;
            self.anchor = None;
            self.cursor = None;
            if let Some(r) = restore {
                for name in &r.selected {
                    if let Some(i) = find(name) {
                        self.selected[i] = true;
                    }
                }
                self.cursor = r.cursor.as_ref().and_then(find);
                self.anchor = self.cursor;
                self.scroll = r.scroll;
            }
            self.base = self.selected.clone();
            self.pressed = None;
            self.last_click = None;
            self.press_at = None;
            self.collapse_to = None;
            self.toggle_on_release = None;
            self.drag = None;
            self.drop_target = None;
        }
        // A renamed item, or the folder we came up from: selected, and the last-touched
        // row so arrows continue from it.
        if let Some(i) = select_next.as_ref().and_then(find) {
            self.selected[i] = true;
            self.base[i] = true;
            self.anchor = Some(i);
            self.cursor = Some(i);
            self.scroll_into_view(i);
        }
        self.scroll = self.scroll.min(self.max_scroll());
        self.update_hover();
    }

    /// Where the view is, for coming back to it via history.
    pub fn snapshot(&self) -> ViewState {
        let name = |i: usize| self.listing.entries[i].raw.clone();
        ViewState {
            path: self.listing.path.clone(),
            scroll: self.scroll,
            selected: (0..self.selected.len())
                .filter(|&i| self.selected[i])
                .map(name)
                .collect(),
            cursor: self.cursor.map(name),
        }
    }

    /// Applies `state` when the listing of its folder arrives.
    pub fn restore_on_load(&mut self, state: ViewState) {
        self.restore = Some(state);
    }

    pub fn select_on_load(&mut self, name: OsString) {
        self.select_next = Some(name);
    }

    /// The rename modal for the selection, if exactly one item is selected.
    pub fn rename_target(&self) -> Option<Modal> {
        let mut rows = (0..self.selected.len()).filter(|&i| self.selected[i]);
        let (Some(i), None) = (rows.next(), rows.next()) else {
            return None;
        };
        let e = &self.listing.entries[i];
        Some(Modal::rename(
            self.listing.path.join(&e.raw),
            e.name.clone(),
            e.is_dir,
        ))
    }

    pub fn resize(&mut self, w: usize, h: usize) {
        self.w = w;
        self.h = h;
        self.scroll = self.scroll.min(self.max_scroll());
        self.update_hover();
    }

    fn max_scroll(&self) -> usize {
        (self.listing.entries.len() * ROW_H).saturating_sub(self.h.saturating_sub(HEADER_H))
    }

    fn up_target(&self) -> Option<PathBuf> {
        self.listing.path.parent().map(PathBuf::from)
    }

    fn hit(&self, x: usize, y: usize) -> Option<Target> {
        if y < HEADER_H {
            return (x < UP_W).then_some(Target::Up);
        }
        let i = (y - HEADER_H + self.scroll) / ROW_H;
        Some(if i < self.listing.entries.len() {
            Target::Row(i)
        } else {
            Target::Blank
        })
    }

    /// Returns true if the hovered target changed.
    fn update_hover(&mut self) -> bool {
        let hover = self.pointer.and_then(|(x, y)| self.hit(x, y));
        std::mem::replace(&mut self.hover, hover) != hover
    }

    pub fn motion(&mut self, x: f64, y: f64) -> Effect {
        let (x, y) = (x.max(0.0) as usize, y.max(0.0) as usize);
        self.pointer = Some((x, y));
        if let (Some((px, py)), Some(Target::Row(i))) = (self.press_at, self.pressed) {
            if px.abs_diff(x).max(py.abs_diff(y)) >= DRAG_THRESHOLD {
                return self.start_drag(i);
            }
        }
        redraw_if(self.update_hover())
    }

    /// Dragging a selected row drags the whole selection; otherwise just that row. A
    /// Ctrl+press drag takes the selection plus the pressed row, without toggling it.
    fn start_drag(&mut self, row: usize) -> Effect {
        self.press_at = None;
        self.pressed = None;
        self.collapse_to = None;
        self.last_click = None;
        let ctrl_press = self.toggle_on_release.take().is_some();
        let rows: Vec<usize> = if self.selected[row] || ctrl_press {
            (0..self.selected.len())
                .filter(|&i| self.selected[i] || i == row)
                .collect()
        } else {
            vec![row]
        };
        let paths = rows
            .iter()
            .map(|&i| self.listing.path.join(&self.listing.entries[i].raw))
            .collect();
        let label = match rows.len() {
            1 => self.listing.entries[row].name.clone(),
            n => format!("{n} items"),
        };
        self.drag = Some(rows);
        Effect::StartDrag(paths, label)
    }

    pub fn leave(&mut self) -> Effect {
        self.pointer = None;
        self.pressed = None;
        self.press_at = None;
        self.collapse_to = None;
        self.toggle_on_release = None;
        redraw_if(self.update_hover())
    }

    /// A drag moved over the window at (x, y): a folder row takes the drop, anywhere else
    /// the current folder does. For our own drag, its rows and the folder they're already
    /// in aren't targets. Returns true if the drop target changed.
    pub fn dnd_motion(&mut self, x: f64, y: f64) -> bool {
        let (x, y) = (x.max(0.0) as usize, y.max(0.0) as usize);
        let own = self.drag.as_deref();
        let target = match self.hit(x, y) {
            Some(Target::Row(i))
                if self.listing.entries[i].is_dir && !own.is_some_and(|r| r.contains(&i)) =>
            {
                Some(DropTarget::Row(i))
            }
            _ if own.is_some() => None,
            _ if self.listing.error.is_some() => None,
            _ => Some(DropTarget::Here),
        };
        std::mem::replace(&mut self.drop_target, target) != target
    }

    /// Returns true if a drop target was shown.
    pub fn dnd_leave(&mut self) -> bool {
        self.drop_target.take().is_some()
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
        self.drop_target = None;
    }

    /// The folder a drop would currently land in.
    pub fn drop_dir(&self) -> Option<PathBuf> {
        match self.drop_target? {
            DropTarget::Row(i) => Some(self.listing.path.join(&self.listing.entries[i].raw)),
            DropTarget::Here => Some(self.listing.path.clone()),
        }
    }

    /// Plain click selects one row, Ctrl toggles, Shift selects from the anchor
    /// (Ctrl+Shift adds that range). A plain second click on the same folder opens it.
    pub fn press(&mut self, button: u32, time: u32, ctrl: bool, shift: bool) -> Effect {
        if button != BTN_LEFT {
            return Effect::None;
        }
        // An audio file's play/pause icon is a button of its own: selection stays.
        if let Some(path) = self.hovered_audio_icon() {
            self.pressed = None;
            self.press_at = None;
            self.last_click = None;
            return Effect::TogglePlay(path);
        }
        self.pressed = self.hover;
        self.press_at = None;
        self.collapse_to = None;
        self.toggle_on_release = None;
        let i = match self.hover {
            Some(Target::Row(i)) => i,
            Some(Target::Blank) if !ctrl && !shift => {
                self.last_click = None;
                return redraw_if(self.clear_selection());
            }
            _ => return Effect::None,
        };
        self.press_at = self.pointer;

        if shift {
            if ctrl {
                self.base.clone_from(&self.selected);
            } else {
                self.clear_selection();
            }
            self.anchor = Some(self.anchor.unwrap_or(i));
            self.cursor = Some(i);
            self.apply_range();
            self.last_click = None;
            return Effect::Redraw;
        }
        if ctrl {
            // Toggled on release, so that Ctrl+drag (copy) leaves the selection alone.
            self.toggle_on_release = Some(i);
            self.last_click = None;
            return Effect::None;
        }

        let double = matches!(self.last_click,
            Some((j, t)) if j == i && time.wrapping_sub(t) <= DOUBLE_CLICK_MS);
        if double {
            self.last_click = None;
            let entry = &self.listing.entries[i];
            let path = self.listing.path.join(&entry.raw);
            return if entry.is_dir {
                Effect::Navigate(path)
            } else {
                Effect::Open(path)
            };
        }
        self.last_click = Some((i, time));
        if self.selected[i] {
            self.collapse_to = Some(i);
            return Effect::None;
        }
        self.select_only(i);
        Effect::Redraw
    }

    fn select_only(&mut self, i: usize) {
        self.clear_selection();
        self.selected[i] = true;
        self.anchor = Some(i);
        self.cursor = Some(i);
    }

    /// "e": the one selected folder, or the current folder when nothing is selected.
    pub fn edit_target(&self) -> Option<PathBuf> {
        let mut rows = (0..self.selected.len()).filter(|&i| self.selected[i]);
        match (rows.next(), rows.next()) {
            (None, _) => Some(self.listing.path.clone()),
            (Some(i), None) if self.listing.entries[i].is_dir => {
                Some(self.listing.path.join(&self.listing.entries[i].raw))
            }
            _ => None,
        }
    }

    /// Paths of the selected rows, in list order.
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        (0..self.selected.len())
            .filter(|&i| self.selected[i])
            .map(|i| self.listing.path.join(&self.listing.entries[i].raw))
            .collect()
    }

    /// The single selected row, if exactly one is selected.
    fn single_selected(&self) -> Option<usize> {
        let mut rows = (0..self.selected.len()).filter(|&i| self.selected[i]);
        match (rows.next(), rows.next()) {
            (Some(i), None) => Some(i),
            _ => None,
        }
    }

    /// Enter: opens the one selected folder or file.
    pub fn open_selected_any(&self) -> Effect {
        let Some(i) = self.single_selected() else {
            return Effect::None;
        };
        let e = &self.listing.entries[i];
        let path = self.listing.path.join(&e.raw);
        if e.is_dir {
            Effect::Navigate(path)
        } else {
            Effect::Open(path)
        }
    }

    /// Alt+Enter: the one selected file.
    pub fn open_with_target(&self) -> Option<PathBuf> {
        let i = self.single_selected()?;
        let e = &self.listing.entries[i];
        (!e.is_dir).then(|| self.listing.path.join(&e.raw))
    }

    /// Right arrow: opens the selection if it is exactly one folder.
    pub fn open_selected(&self) -> Effect {
        let mut rows = (0..self.selected.len()).filter(|&i| self.selected[i]);
        match (rows.next(), rows.next()) {
            (Some(i), None) if self.listing.entries[i].is_dir => {
                Effect::Navigate(self.listing.path.join(&self.listing.entries[i].raw))
            }
            _ => Effect::None,
        }
    }

    /// Left arrow: goes up like the up button, but selects the folder we came from.
    pub fn go_up(&mut self) -> Effect {
        let Some(parent) = self.up_target() else {
            return Effect::None;
        };
        self.select_next = self.listing.path.file_name().map(|n| n.to_os_string());
        Effect::Navigate(parent)
    }

    /// Up/Down: selects only the row before/after the last-touched one, stopping at the
    /// ends. With nothing selected, selects the first row.
    pub fn step(&mut self, down: bool) -> Effect {
        let n = self.listing.entries.len();
        if n == 0 {
            return Effect::None;
        }
        let selected = |i: &usize| self.selected[*i];
        let row = if !self.selected.contains(&true) {
            0
        } else {
            // Without a last-touched row (e.g. after Ctrl+A), step off the selection's edge.
            let from = self.cursor.unwrap_or_else(|| {
                if down {
                    (0..n).rev().find(selected).unwrap_or(0)
                } else {
                    (0..n).find(selected).unwrap_or(0)
                }
            });
            if down {
                (from + 1).min(n - 1)
            } else {
                from.saturating_sub(1)
            }
        };
        self.select_only(row);
        self.scroll_into_view(row);
        self.update_hover();
        Effect::Redraw
    }

    /// Shift+Up/Down: moves the range's cursor one row and reselects anchor..=cursor,
    /// keeping whatever was selected outside the range. Starts at the first row.
    pub fn extend(&mut self, down: bool) -> Effect {
        let n = self.listing.entries.len();
        if n == 0 {
            return Effect::None;
        }
        let cursor = match self.cursor {
            Some(c) if down => (c + 1).min(n - 1),
            Some(c) => c.saturating_sub(1),
            None => {
                self.base.clone_from(&self.selected);
                0
            }
        };
        self.cursor = Some(cursor);
        self.anchor = Some(self.anchor.unwrap_or(cursor));
        self.apply_range();
        self.scroll_into_view(cursor);
        self.update_hover();
        Effect::Redraw
    }

    fn apply_range(&mut self) {
        self.selected.clone_from(&self.base);
        if let (Some(a), Some(c)) = (self.anchor, self.cursor) {
            self.selected[a.min(c)..=a.max(c)].fill(true);
        }
    }

    fn scroll_into_view(&mut self, row: usize) {
        let top = row * ROW_H;
        let visible = self.h.saturating_sub(HEADER_H);
        if top < self.scroll {
            self.scroll = top;
        } else if top + ROW_H > self.scroll + visible {
            self.scroll = (top + ROW_H).saturating_sub(visible);
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// The confirmation modal for deleting the selection, if anything is selected.
    pub fn confirm_delete(&self) -> Option<Modal> {
        let rows: Vec<usize> = (0..self.selected.len())
            .filter(|&i| self.selected[i])
            .collect();
        let paths: Vec<PathBuf> = rows
            .iter()
            .map(|&i| self.listing.path.join(&self.listing.entries[i].raw))
            .collect();
        let name = match rows[..] {
            [] => return None,
            [i] => Some(self.listing.entries[i].name.clone()),
            _ => None,
        };
        Some(Modal::new(modal::Kind::ConfirmDelete { paths, name }))
    }

    pub fn select_all(&mut self) -> Effect {
        self.base.fill(true);
        if self.selected.iter().all(|&s| s) {
            return Effect::None;
        }
        self.selected.fill(true);
        Effect::Redraw
    }

    /// Returns true if anything was selected.
    fn clear_selection(&mut self) -> bool {
        let any = self.selected.contains(&true);
        self.selected.fill(false);
        self.base.fill(false);
        any
    }

    /// Buttons fire on release, and only if released over what was pressed.
    pub fn release(&mut self, button: u32) -> Effect {
        if button != BTN_LEFT {
            return Effect::None;
        }
        self.press_at = None;
        if let Some(i) = self.toggle_on_release.take() {
            self.pressed = None;
            self.selected[i] = !self.selected[i];
            self.base.clone_from(&self.selected);
            self.anchor = Some(i);
            self.cursor = Some(i);
            return Effect::Redraw;
        }
        if let Some(i) = self.collapse_to.take() {
            self.pressed = None;
            self.select_only(i);
            return Effect::Redraw;
        }
        match (self.pressed.take(), self.hover) {
            (Some(Target::Up), Some(Target::Up)) => {
                self.up_target().map_or(Effect::None, Effect::Navigate)
            }
            _ => Effect::None,
        }
    }

    /// `value120` is wheel notches (120 = one notch); `pixels` is used when there are none (touchpads).
    pub fn scroll(&mut self, value120: i32, pixels: f64) -> Effect {
        let delta = if value120 != 0 {
            value120 as f64 / 120.0 * SCROLL_ROWS * ROW_H as f64
        } else {
            pixels
        };
        let next = (self.scroll as f64 + delta).clamp(0.0, self.max_scroll() as f64) as usize;
        if next == self.scroll {
            return Effect::None;
        }
        self.scroll = next;
        self.update_hover();
        Effect::Redraw
    }

    /// `playing`: the audio file mpv has, and whether it's paused. `cut`: paths cut to
    /// the clipboard, drawn dimmed.
    pub fn draw(
        &self,
        c: &mut Canvas,
        thumbs: &Thumbs,
        playing: Option<(&Path, bool)>,
        cut: &HashSet<PathBuf>,
    ) {
        c.fill(theme::BG);
        self.draw_rows(c, thumbs, playing, cut);
        self.draw_header(c);
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let first = self.scroll / ROW_H;
        let count = self.h.saturating_sub(HEADER_H).div_ceil(ROW_H) + 1;
        first..(first + count).min(self.listing.entries.len())
    }

    /// Files that should have thumbnails, in the order to make them: the visible rows,
    /// then a screen below, then a screen above. Folders always use the folder icon.
    /// (path, mtime)
    pub fn thumb_rows(&self) -> impl Iterator<Item = (PathBuf, i64)> + '_ {
        let vis = self.visible_range();
        let page = vis.len();
        let n = self.listing.entries.len();
        let below = vis.end..(vis.end + page).min(n);
        let above = vis.start.saturating_sub(page)..vis.start;
        vis.chain(below)
            .chain(above.rev())
            .map(|i| &self.listing.entries[i])
            .filter(|e| !e.is_dir && !e.audio)
            .map(|e| (self.listing.path.join(&e.raw), e.mtime))
    }

    /// The row whose thumbnail slot the pointer is over.
    fn hovered_slot(&self) -> Option<usize> {
        let (x, _) = self.pointer?;
        let Some(Target::Row(i)) = self.hover else {
            return None;
        };
        (PAD..PAD + thumbs::SIZE).contains(&x).then_some(i)
    }

    /// The file whose thumbnail the pointer is over (not folders or audio):
    /// (path, mtime, is video).
    pub fn hovered_thumb(&self) -> Option<(PathBuf, i64, bool)> {
        let e = &self.listing.entries[self.hovered_slot()?];
        (!e.is_dir && !e.audio).then(|| (self.listing.path.join(&e.raw), e.mtime, e.video))
    }

    fn hovered_audio_icon(&self) -> Option<PathBuf> {
        let e = &self.listing.entries[self.hovered_slot()?];
        e.audio.then(|| self.listing.path.join(&e.raw))
    }

    /// "p": the selection, if it is exactly one audio file.
    pub fn play_target(&self) -> Option<PathBuf> {
        let e = &self.listing.entries[self.single_selected()?];
        e.audio.then(|| self.listing.path.join(&e.raw))
    }

    /// Identifies what `thumb_rows` covers, to notice when it changes.
    pub fn thumb_window(&self) -> (PathBuf, usize, usize) {
        let vis = self.visible_range();
        (self.listing.path.clone(), vis.start, vis.len())
    }

    fn draw_rows(
        &self,
        c: &mut Canvas,
        thumbs: &Thumbs,
        playing: Option<(&Path, bool)>,
        cut: &HashSet<PathBuf>,
    ) {
        let text_dy = (ROW_H - CELL_H) / 2;
        let date_x = self.w.saturating_sub(PAD + DATE_W);
        let size_end = date_x.saturating_sub(COL_GAP);
        let name_end = size_end.saturating_sub(SIZE_W + COL_GAP);

        if let Some(err) = &self.listing.error {
            c.text(
                PAD,
                HEADER_H + text_dy,
                err,
                &font::REGULAR,
                theme::TEXT_DIM,
                self.w,
            );
            return;
        }

        let first = self.scroll / ROW_H;
        let visible = (self.h.saturating_sub(HEADER_H)).div_ceil(ROW_H) + 1;
        for (i, e) in self
            .listing
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
        {
            // Rows under the header are overdrawn by it, so y never goes negative.
            let y = HEADER_H + i * ROW_H - self.scroll;
            let selected = self.selected[i];
            let (fg, dim) = if selected {
                c.rect(0, y, self.w, ROW_H, theme::TEXT);
                (theme::BG, theme::BG)
            } else {
                if self.hover == Some(Target::Row(i)) {
                    c.rect(0, y, self.w, ROW_H, theme::BORDER);
                }
                (theme::TEXT, theme::TEXT_DIM)
            };
            if self.drop_target == Some(DropTarget::Row(i)) {
                let color = if selected { theme::BG } else { theme::TEXT };
                c.frame(0, y, self.w, ROW_H, color);
            }
            let ty = y + text_dy;
            let face = if e.is_dir {
                &font::BOLD
            } else {
                &font::REGULAR
            };
            let path = self.listing.path.join(&e.raw);
            let name_color = if (e.dot || cut.contains(&path)) && !selected {
                theme::TEXT_DIM
            } else {
                fg
            };
            c.text(NAME_X, ty, &e.name, face, name_color, name_end);
            let slot_y = y + (ROW_H - thumbs::SIZE) / 2;
            if e.is_dir {
                draw_folder_icon(c, PAD, slot_y);
            } else if e.audio {
                let hot = self.hovered_slot() == Some(i);
                if playing.is_some_and(|(p, paused)| p == path && !paused) {
                    let color = if hot { theme::PAUSE_HOT } else { theme::PAUSE };
                    draw_pause_icon(c, PAD, slot_y, color);
                } else {
                    let color = if hot { theme::PLAY_HOT } else { theme::PLAY };
                    draw_play_icon(c, PAD, slot_y, color);
                }
            } else if let Some((mtime, Some(t))) = thumbs.get(&path) {
                if *mtime == e.mtime {
                    // Centered in the SIZE x SIZE slot.
                    let tx = PAD + (thumbs::SIZE - t.w) / 2;
                    c.blit(tx, slot_y + (thumbs::SIZE - t.h) / 2, t.w, t.h, &t.px);
                }
            }
            let size_x = size_end.saturating_sub(e.size.len() * CELL_W);
            c.text(size_x, ty, &e.size, &font::REGULAR, dim, size_end);
            c.text(date_x, ty, &e.modified, &font::REGULAR, dim, self.w);
        }
    }

    fn draw_header(&self, c: &mut Canvas) {
        // Dropping into the current folder: outline the whole list.
        if self.drop_target == Some(DropTarget::Here) {
            c.frame(
                0,
                HEADER_H,
                self.w,
                self.h.saturating_sub(HEADER_H),
                theme::TEXT,
            );
        }
        c.rect(0, 0, self.w, HEADER_H, theme::BG);
        c.rect(0, HEADER_H - 1, self.w, 1, theme::BORDER);

        let can_go_up = self.up_target().is_some();
        if can_go_up && self.hover == Some(Target::Up) {
            c.rect(0, 0, UP_W, HEADER_H - 1, theme::BORDER);
        }
        let arrow = if can_go_up {
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        draw_up_arrow(c, (UP_W - 7) / 2, (HEADER_H - 1 - 9) / 2, arrow);

        let ty = (HEADER_H - 1 - CELL_H) / 2;
        let mut right = self.w.saturating_sub(PAD);
        if self.listing.dotfiles {
            const LABEL: &str = "dotfiles";
            right = right.saturating_sub(LABEL.len() * CELL_W);
            c.text(right, ty, LABEL, &font::REGULAR, theme::TEXT_DIM, self.w);
            right = right.saturating_sub(COL_GAP);
        }

        // Show the tail of the path when it doesn't fit.
        let x = UP_W + PAD;
        let fit = right.saturating_sub(x) / CELL_W;
        let path = self.listing.path.to_string_lossy();
        let skip = path.chars().count().saturating_sub(fit);
        let tail: String = path.chars().skip(skip).collect();
        c.text(x, ty, &tail, &font::REGULAR, theme::TEXT, right);
    }
}

fn redraw_if(changed: bool) -> Effect {
    if changed {
        Effect::Redraw
    } else {
        Effect::None
    }
}

/// Play triangle, 10x12, centered in the 16px slot. Row widths follow an equilateral
/// triangle's outline, rounded to whole pixels.
fn draw_play_icon(c: &mut Canvas, x: usize, y: usize, color: u32) {
    for r in 0..12 {
        let w = (10.0 * (1.0 - (r as f64 - 5.5).abs() / 6.0)).round() as usize;
        c.rect(x + 4, y + 2 + r, w, 1, color);
    }
}

/// Pause: two 4x12 bars, centered in the 16px slot.
fn draw_pause_icon(c: &mut Canvas, x: usize, y: usize, color: u32) {
    c.rect(x + 3, y + 2, 4, 12, color);
    c.rect(x + 9, y + 2, 4, 12, color);
}

/// 16x12 pixel folder, a tab over a body, vertically centered in the 16px slot.
fn draw_folder_icon(c: &mut Canvas, x: usize, y: usize) {
    c.rect(x, y + 2, 6, 2, theme::TEXT_DIM);
    c.rect(x, y + 4, thumbs::SIZE, 10, theme::TEXT_DIM);
}

/// 7x9 pixel arrow: a solid triangle head on a 1px stem.
fn draw_up_arrow(c: &mut Canvas, x: usize, y: usize, color: u32) {
    for r in 0..4 {
        c.rect(x + 3 - r, y + r, 2 * r + 1, 1, color);
    }
    c.rect(x + 3, y + 4, 1, 5, color);
}
