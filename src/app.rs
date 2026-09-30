//! Wayland plumbing: one xdg toplevel, drawn into wl_shm buffers on demand.

mod clipboard;
mod dialog;
mod dnd;
mod keyboard;

use std::{
    collections::{HashSet, VecDeque},
    os::fd::AsFd,
    ffi::OsString,
    path::PathBuf,
    rc::Rc,
    thread,
};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData},
    data_device_manager::{DataDeviceManagerState, data_device::DataDevice},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            Interest, LoopHandle, Mode, PostAction, RegistrationToken,
            channel::{self, Channel, Sender},
            generic::Generic,
            ping::{self, Ping},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
        },
        protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
            Shape, WpCursorShapeDeviceV1,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};

use crate::{
    chooser::Chooser,
    fs::{self, Listing},
    modal::{self, Modal, Outcome, Prompt},
    ops::{self, JobMsg},
    player::Player,
    render::Canvas,
    theme,
    thumbs::{self, Thumbnailer},
    video::{FrameBuf, Video},
    view::{Effect, Thumbs, View, ViewState},
    watch::Watcher,
    xkb::{KeymapFd, Mods, Xkb},
};

const APP_ID: &str = "swayfin";
/// File dialogs get their own, so Sway can float them.
const CHOOSER_APP_ID: &str = "swayfin-chooser";
/// Mouse side buttons: evdev BTN_SIDE/BTN_BACK and BTN_EXTRA/BTN_FORWARD (mice use
/// either pair).
const BACK_BUTTONS: [u32; 2] = [0x113, 0x116];
const FORWARD_BUTTONS: [u32; 2] = [0x114, 0x115];
/// Least time between refreshes while the watched folder keeps changing.
const WATCH_THROTTLE: std::time::Duration = std::time::Duration::from_millis(100);
/// Decoded previews kept for hovering back; each can be several MB.
const PREVIEW_CACHE: usize = 3;

#[derive(Clone, PartialEq)]
struct PreviewKey {
    path: PathBuf,
    mtime: i64,
    /// The size it was fitted into (the window, minus the border).
    max: (usize, usize),
}

/// A video's size fitted into the window (inside the 1px border), never enlarged.
fn video_fit(win: (f64, f64), (vw, vh): (usize, usize)) -> (f64, f64) {
    let s = ((win.0 - 2.0) / vw as f64)
        .min((win.1 - 2.0) / vh as f64)
        .min(1.0);
    (vw as f64 * s, vh as f64 * s)
}

struct VideoPreview {
    path: PathBuf,
    /// Scale relative to the fitted size.
    zoom: f64,
    /// The thumbnail's size, for the player's shape until mpv reports the real one.
    guess: (usize, usize),
    /// mpv was told to play it.
    started: bool,
}

/// Where a previewed image sits at `zoom` in a window of `win`: (left, top, width,
/// height) in screen pixels, centered, possibly larger than the window.
fn preview_rect(win: (f64, f64), img: &thumbs::Image, zoom: f64) -> (f64, f64, f64, f64) {
    let (dw, dh) = (img.w as f64 * zoom, img.h as f64 * zoom);
    ((win.0 - dw) / 2.0, (win.1 - dh) / 2.0, dw, dh)
}

struct Preview {
    key: PreviewKey,
    /// The whole image fitted to the window: None until decoded, or if it isn't an image.
    image: Option<Rc<thumbs::Image>>,
    /// A sharp decode of the part that was visible after the latest zoom.
    tile: Option<Rc<thumbs::Image>>,
    /// Scale relative to the fitted size.
    zoom: f64,
    fit_id: u32,
    tile_id: u32,
}

/// Zoom factor per wheel notch.
const ZOOM_STEP: f64 = 1.25;
/// Touchpad scroll distance (px) that counts as one notch.
const ZOOM_NOTCH_PX: f64 = 15.0;

/// A finished directory read, tagged with the request it answers.
pub type Loaded = (u64, Listing);

/// Reads `path` on a new thread and sends the result to the event loop.
pub fn spawn_load(tx: Sender<Loaded>, seq: u64, path: PathBuf, dotfiles: bool) {
    thread::spawn(move || {
        let _ = tx.send((seq, fs::list(path, dotfiles)));
    });
}

pub struct App {
    registry: RegistryState,
    outputs: OutputState,
    seats: SeatState,
    shm: Shm,
    pool: SlotPool,
    buffer: Option<Buffer>,
    window: Window,
    compositor: CompositorState,
    data_devices: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    drag: Option<dnd::ActiveDrag>,
    /// What we put on the clipboard, and the paths shown dimmed as cut.
    clip: Option<clipboard::Clip>,
    cut: HashSet<PathBuf>,
    /// Tokens of pasted cuts still moving; the clipboard is cleared when they're done.
    cut_pastes: Vec<u64>,
    /// Serial of the last key press; setting the clipboard needs it.
    key_serial: u32,
    /// Drops whose jobs are still running.
    pending_drops: Vec<dnd::PendingDrop>,
    drop_seq: u64,
    /// Serial of the last button press; a drag must be started with it.
    press_serial: u32,
    /// Last pointer position over the window, in surface coordinates.
    pointer_pos: Option<(f64, f64)>,
    /// Shown one at a time, front first.
    modals: VecDeque<Modal>,
    /// Running as a file dialog for the portal.
    chooser: Option<Chooser>,
    jobs: Sender<JobMsg>,
    thumbnailer: Thumbnailer,
    player: Player,
    /// libmpv, loaded on the first video hover (None if that failed or hasn't happened).
    video: Option<Video>,
    video_tried: bool,
    /// Wakes the event loop from mpv's threads.
    video_ping: Ping,
    video_preview: Option<VideoPreview>,
    frame: FrameBuf,
    /// The image preview for the thumbnail under the pointer.
    preview: Option<Preview>,
    /// Recent decode results, oldest first; None = not an image.
    preview_cache: VecDeque<(PreviewKey, Option<Rc<thumbs::Image>>)>,
    /// Ids for preview requests, so late answers can be recognized.
    preview_seq: u32,
    thumbs: Thumbs,
    /// Bumped per listing, so a refresh re-checks thumbnails.
    listing_gen: u64,
    /// (listing_gen, first visible row, row count) thumbnails were last requested for.
    thumb_key: Option<(u64, usize, usize)>,
    cursor_shapes: Option<CursorShapeManager>,
    pointer: Option<(wl_pointer::WlPointer, Option<WpCursorShapeDeviceV1>)>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    ctrl: bool,
    shift: bool,
    alt: bool,
    /// Raw modifier state, fed to `xkb` once it exists.
    mods: Mods,
    /// The compositor's keymap, compiled into `xkb` on first text entry.
    keymap: Option<KeymapFd>,
    xkb: Option<Xkb>,
    pastes: Sender<String>,
    conn: Connection,
    /// Serial of the last pointer enter; cursor shape changes need it.
    enter_serial: u32,
    shape: Shape,
    /// Held key and its repeat timer.
    repeat: Option<(u32, RegistrationToken)>,
    /// Compositor's key repeat (keys/s, delay in ms). Rate 0 disables repeat.
    repeat_info: (u32, u32),
    handle: LoopHandle<'static, Self>,
    view: View,
    /// The folder of the latest navigation (loaded or still loading).
    nav_path: PathBuf,
    /// Folders to go back/forward to, most recent last.
    back: Vec<ViewState>,
    forward: Vec<ViewState>,
    watcher: Option<Watcher>,
    watched: Option<PathBuf>,
    refresh_scheduled: bool,
    /// Show names starting with a dot. Off at every launch.
    dotfiles: bool,
    qh: QueueHandle<Self>,
    loader: Sender<Loaded>,
    load_seq: u64,
    width: u32,
    height: u32,
    configured: bool,
    frame_pending: bool,
    dirty: bool,
    pub exit: bool,
}

impl App {
    /// `loads` must already carry request #1 (the start folder). `select` is a name in
    /// it to select once listed.
    pub fn new(
        conn: &Connection,
        handle: LoopHandle<'static, Self>,
        loader: Sender<Loaded>,
        loads: Channel<Loaded>,
        start: PathBuf,
        chooser: Option<Chooser>,
        select: Option<OsString>,
    ) -> Self {
        let (globals, queue) = registry_queue_init(conn).expect("registry");
        let qh = queue.handle();
        WaylandSource::new(conn.clone(), queue)
            .insert(handle.clone())
            .expect("wayland source");
        handle
            .insert_source(loads, |event, _, app| {
                if let channel::Event::Msg((seq, listing)) = event {
                    app.loaded(seq, listing);
                }
            })
            .expect("loader channel");
        let (pastes, pasted) = channel::channel();
        handle
            .insert_source(pasted, |event, _, app| {
                if let channel::Event::Msg(text) = event {
                    app.pasted(text);
                }
            })
            .expect("paste channel");
        let (video_ping, video_wake) = ping::make_ping().expect("video ping");
        handle
            .insert_source(video_wake, |_, _, app| app.video_event())
            .expect("video wakeups");
        let (ended_tx, ended_rx) = channel::channel();
        handle
            .insert_source(ended_rx, |event, _, app| {
                if let channel::Event::Msg(pid) = event {
                    if app.player.ended(pid) {
                        app.request_redraw();
                    }
                }
            })
            .expect("player channel");
        let watcher = Watcher::new().ok();
        if let Some(fd) = watcher
            .as_ref()
            .and_then(|w| w.as_fd().try_clone_to_owned().ok())
        {
            handle
                .insert_source(
                    Generic::new(fd, Interest::READ, Mode::Level),
                    |_, _, app| {
                        app.folder_changed();
                        Ok(PostAction::Continue)
                    },
                )
                .expect("folder watch");
        }
        let (preview_tx, preview_rx) = channel::channel();
        handle
            .insert_source(preview_rx, |event, _, app| {
                if let channel::Event::Msg(done) = event {
                    app.preview_done(done);
                }
            })
            .expect("preview channel");
        let (thumb_tx, thumb_rx) = channel::channel();
        handle
            .insert_source(thumb_rx, |event, _, app| {
                if let channel::Event::Msg(done) = event {
                    app.thumb_done(done);
                }
            })
            .expect("thumbnail channel");
        let (jobs, job_msgs) = channel::channel();
        handle
            .insert_source(job_msgs, |event, _, app| {
                if let channel::Event::Msg(msg) = event {
                    app.job_msg(msg);
                }
            })
            .expect("job channel");

        let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
        let xdg = XdgShell::bind(&globals, &qh).expect("xdg_wm_base");
        let shm = Shm::bind(&globals, &qh).expect("wl_shm");

        let window = xdg.create_window(
            compositor.create_surface(&qh),
            WindowDecorations::RequestServer,
            &qh,
        );
        window.set_title(APP_ID);
        window.set_app_id(if chooser.is_some() {
            CHOOSER_APP_ID
        } else {
            APP_ID
        });
        // Initial bufferless commit; the compositor answers with a configure.
        window.commit();

        // Sized for a typical tile; grows on first configure if needed.
        let pool = SlotPool::new(1280 * 720 * 4, &shm).expect("shm pool");

        let mut view = View::new(Listing {
            path: start.clone(),
            entries: Vec::new(),
            error: None,
            dotfiles: false,
        });
        if let Some(name) = select {
            view.select_on_load(name);
        }

        Self {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            seats: SeatState::new(&globals, &qh),
            shm,
            pool,
            buffer: None,
            window,
            data_devices: DataDeviceManagerState::bind(&globals, &qh).ok(),
            compositor,
            data_device: None,
            drag: None,
            clip: None,
            cut: HashSet::new(),
            cut_pastes: Vec::new(),
            key_serial: 0,
            pending_drops: Vec::new(),
            drop_seq: 0,
            press_serial: 0,
            pointer_pos: None,
            modals: VecDeque::new(),
            chooser,
            jobs,
            thumbnailer: Thumbnailer::new(thumbs::Ui {
                thumbs: thumb_tx,
                previews: preview_tx,
            }),
            player: Player::new(ended_tx),
            video: None,
            video_tried: false,
            video_ping,
            video_preview: None,
            frame: FrameBuf::default(),
            preview: None,
            preview_cache: VecDeque::new(),
            preview_seq: 0,
            thumbs: Thumbs::new(),
            listing_gen: 0,
            thumb_key: None,
            cursor_shapes: CursorShapeManager::bind(&globals, &qh).ok(),
            pointer: None,
            keyboard: None,
            ctrl: false,
            shift: false,
            alt: false,
            mods: Mods::default(),
            keymap: None,
            xkb: None,
            pastes,
            conn: conn.clone(),
            enter_serial: 0,
            shape: Shape::Default,
            repeat: None,
            repeat_info: (25, 600),
            handle,
            nav_path: start,
            back: Vec::new(),
            forward: Vec::new(),
            watcher,
            watched: None,
            refresh_scheduled: false,
            view,
            dotfiles: false,
            qh,
            loader,
            load_seq: 1,
            width: 0,
            height: 0,
            configured: false,
            frame_pending: false,
            dirty: false,
            exit: false,
        }
    }

    /// Opens a folder the user chose to go to, and ranks it in zoxide like a shell cd.
    fn go(&mut self, path: PathBuf) {
        if path != self.view.listing.path {
            self.back.push(self.view.snapshot());
            self.forward.clear();
        }
        ops::zoxide_add(path.clone());
        self.navigate(path);
    }

    /// Mouse back/forward buttons: returns to a folder from history, as it was left.
    fn history(&mut self, back: bool) {
        let (from, to) = if back {
            (&mut self.back, &mut self.forward)
        } else {
            (&mut self.forward, &mut self.back)
        };
        let Some(state) = from.pop() else {
            return;
        };
        to.push(self.view.snapshot());
        let path = state.path.clone();
        self.view.restore_on_load(state);
        self.navigate(path);
    }

    fn navigate(&mut self, path: PathBuf) {
        self.load_seq += 1;
        self.nav_path = path.clone();
        spawn_load(self.loader.clone(), self.load_seq, path, self.dotfiles);
    }

    pub(super) fn toggle_dotfiles(&mut self) {
        self.dotfiles = !self.dotfiles;
        self.refresh();
    }

    /// Re-reads the folder being shown, or the one being navigated to if that load is
    /// still underway (so a refresh never undoes a navigation).
    fn refresh(&mut self) {
        self.navigate(self.nav_path.clone());
    }

    /// The watched folder changed on disk: refresh, at most every WATCH_THROTTLE while
    /// changes keep coming (a script writing a file fires constantly).
    fn folder_changed(&mut self) {
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        if !watcher.drain() || self.refresh_scheduled {
            return;
        }
        self.refresh_scheduled = true;
        let timer = Timer::from_duration(WATCH_THROTTLE);
        let _ = self.handle.insert_source(timer, |_, _, app| {
            app.refresh_scheduled = false;
            app.refresh();
            TimeoutAction::Drop
        });
    }

    fn job_msg(&mut self, msg: JobMsg) {
        match msg {
            JobMsg::Conflict(conflict) => self.push_modal(Modal::new(modal::Kind::Conflict {
                conflict,
                checked: false,
            })),
            JobMsg::Paste { paths, cut } => self.paste_paths(paths, cut),
            JobMsg::Done {
                verb,
                errors,
                token,
            } => {
                if let Some(i) = self.cut_pastes.iter().position(|&t| t == token) {
                    self.cut_pastes.swap_remove(i);
                    self.clear_clipboard();
                }
                // A drop's job is done: now tell the source (it refreshes then).
                if let Some(i) = self.pending_drops.iter().position(|d| d.token == token) {
                    let done = self.pending_drops.swap_remove(i);
                    done.offer.finish();
                    done.offer.destroy();
                }
                self.refresh();
                if !errors.is_empty() {
                    self.push_modal(Modal::new(modal::Kind::Errors { verb, errors }));
                }
            }
            JobMsg::ChooseApp {
                file,
                mime,
                apps,
                save_always,
            } => self.open_text_modal(Modal::open_with(file, mime, apps, save_always)),
            JobMsg::Named(result) => {
                // Only the prompt that asked gets the answer (it may have been cancelled).
                let Some(front) = self.modals.front_mut().filter(|m| m.is_pending()) else {
                    return;
                };
                match result {
                    Ok(path) => {
                        let go_to = matches!(front.prompt_for(), Some(Prompt::GoTo));
                        if let Some(Prompt::Rename { .. }) = front.prompt_for() {
                            if let Some(name) = path.file_name() {
                                self.view.select_on_load(name.to_os_string());
                            }
                        }
                        self.finish_modal(Outcome::Dismissed);
                        if go_to {
                            self.go(path);
                        } else {
                            self.refresh();
                        }
                    }
                    Err(reason) => {
                        front.prompt_failed(reason);
                        self.request_redraw();
                    }
                }
            }
        }
    }

    /// A prompt was submitted: validate, then create, rename or query zoxide off-thread.
    fn submit_prompt(&mut self, name: String) {
        let Some(front) = self.modals.front_mut() else {
            return;
        };
        if let Some(Prompt::GoTo) = front.prompt_for() {
            let here = self.view.listing.path.clone();
            ops::spawn_zoxide_query(name, here, self.jobs.clone());
            return;
        }
        if let Some(reason) = ops::invalid_name(&name) {
            front.prompt_failed(reason);
            return;
        }
        match front.prompt_for() {
            Some(Prompt::NewFolder) => {
                ops::spawn_create(self.view.listing.path.clone(), name, self.jobs.clone())
            }
            Some(Prompt::Rename { path, old }) => {
                if name == *old {
                    self.finish_modal(Outcome::Dismissed);
                } else {
                    ops::spawn_rename(path.clone(), name, self.jobs.clone());
                }
            }
            Some(Prompt::GoTo) | None => {}
        }
    }

    fn modal_outcome(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Submit(name) => {
                self.submit_prompt(name);
                self.request_redraw();
            }
            Outcome::Delete(paths) => {
                ops::spawn_delete(paths, self.jobs.clone());
                self.finish_modal(Outcome::Dismissed);
            }
            Outcome::Replace(path) => {
                self.finish_modal(Outcome::Dismissed);
                self.dialog_finish(vec![path]);
            }
            Outcome::OpenWith { file, id, save } => {
                ops::spawn_launch(file, id, save, self.jobs.clone());
                self.finish_modal(Outcome::Dismissed);
            }
            other => self.finish_modal(other),
        }
    }

    /// I-beam over the name field, the default arrow everywhere else.
    fn update_shape(&mut self) {
        let field = match self.modals.front() {
            Some(modal) => modal.hovering_field(),
            None => self.chooser.as_ref().is_some_and(Chooser::hovering_field),
        };
        let want = if field {
            Shape::Text
        } else {
            Shape::Default
        };
        if want != self.shape {
            if let Some((_, Some(device))) = &self.pointer {
                device.set_shape(self.enter_serial, want);
            }
            self.shape = want;
        }
    }

    fn push_modal(&mut self, modal: Modal) {
        self.modals.push_back(modal);
        if self.modals.len() == 1 {
            // The list stops reacting to the pointer while a modal is up.
            self.view.leave();
            let pos = self.pointer_px();
            let (w, h) = (self.width as usize, self.height as usize);
            self.modals[0].motion(pos, w, h);
            self.update_shape();
            self.request_redraw();
        }
    }

    fn finish_modal(&mut self, outcome: Outcome) {
        let Some(done) = self.modals.pop_front() else {
            return;
        };
        if let (Outcome::Answer(choice, for_rest), Some(conflict)) = (outcome, done.conflict()) {
            let _ = conflict.reply.send((choice, for_rest));
        }
        let pos = self.pointer_px();
        let (w, h) = (self.width as usize, self.height as usize);
        match self.modals.front_mut() {
            Some(next) => {
                next.motion(pos, w, h);
            }
            None => {
                if let Some((x, y)) = self.pointer_pos {
                    self.view.motion(x, y);
                }
            }
        }
        self.update_shape();
        self.request_redraw();
    }

    fn pointer_px(&self) -> Option<(usize, usize)> {
        self.pointer_pos
            .map(|(x, y)| (x.max(0.0) as usize, y.max(0.0) as usize))
    }

    fn modal_pointer(&mut self, kind: &PointerEventKind) {
        let pos = self.pointer_px();
        let (w, h) = (self.width as usize, self.height as usize);
        let Some(modal) = self.modals.front_mut() else {
            return;
        };
        match *kind {
            PointerEventKind::Enter { .. }
            | PointerEventKind::Motion { .. }
            | PointerEventKind::Leave { .. } => {
                if modal.motion(pos, w, h) {
                    self.request_redraw();
                }
            }
            PointerEventKind::Press { button, time, .. } => {
                match modal.press(button, time, pos, w, h) {
                    (_, Some(outcome)) => self.modal_outcome(outcome),
                    (true, None) => self.request_redraw(),
                    (false, None) => {}
                }
            }
            PointerEventKind::Release { button, .. } => match modal.release(button) {
                (_, Some(outcome)) => self.modal_outcome(outcome),
                (true, None) => self.request_redraw(),
                (false, None) => {}
            },
            PointerEventKind::Axis { vertical, .. } => {
                if modal.scroll(vertical.value120, vertical.absolute) {
                    self.request_redraw();
                }
            }
        }
    }

    fn loaded(&mut self, seq: u64, listing: Listing) {
        // Drop answers to requests that were superseded by a newer navigation.
        if seq == self.load_seq {
            if listing.path != self.view.listing.path {
                self.thumbnailer.cancel();
                // Playback belongs to the folder it was started in.
                self.player.stop();
            }
            if self.watched.as_ref() != Some(&listing.path) {
                if let Some(w) = &mut self.watcher {
                    w.watch(&listing.path);
                }
                self.watched = Some(listing.path.clone());
            }
            self.view.set_listing(listing);
            self.listing_gen += 1;
            self.request_redraw();
        }
    }

    /// Asks for thumbnails of the rows around the visible ones, whenever those rows (or
    /// the listing) change.
    fn update_thumbs(&mut self) {
        let (_, first, count) = self.view.thumb_window();
        let key = (self.listing_gen, first, count);
        if self.thumb_key == Some(key) {
            return;
        }
        self.thumb_key = Some(key);
        let wanted = thumbs::missing(&self.thumbs, self.view.thumb_rows());
        if !wanted.is_empty() {
            self.thumbnailer.request(wanted);
        }
    }

    fn thumb_done(&mut self, done: thumbs::Done) {
        let shown = done.path.parent() == Some(self.view.listing.path.as_path());
        self.thumbs.insert(done.path, (done.mtime, done.thumb));
        // A thumbnail appearing under the pointer makes it previewable.
        if shown | self.update_preview() {
            self.request_redraw();
        }
    }

    /// Shows, switches or hides the preview to match the thumbnail under the pointer.
    /// Returns true if what's drawn changed.
    fn update_preview(&mut self) -> bool {
        let hovered = self.view.hovered_thumb().filter(|_| self.modals.is_empty());
        let was_shown = self.preview.as_ref().is_some_and(|p| p.image.is_some());

        // Videos play in the preview spot, via libmpv.
        let video = hovered.as_ref().filter(|h| h.2).map(|h| h.0.clone());
        let video_changed = self.update_video(video);
        if self.video_preview.is_some() {
            self.preview = None;
            return was_shown | video_changed;
        }

        let wanted = hovered.filter(
            |(path, mtime, _)| matches!(self.thumbs.get(path), Some((m, Some(_))) if m == mtime),
        );
        let Some((path, mtime, _)) = wanted else {
            self.preview = None;
            return was_shown | video_changed;
        };
        // As big as the window allows, inside the 1px border.
        let max = (
            (self.width as usize).saturating_sub(2).max(1),
            (self.height as usize).saturating_sub(2).max(1),
        );
        let key = PreviewKey { path, mtime, max };
        if self.preview.as_ref().is_some_and(|p| p.key == key) {
            return false;
        }
        self.preview_seq += 1;
        let fit_id = self.preview_seq;
        let image = match self.preview_cache.iter().find(|(k, _)| *k == key) {
            Some((_, image)) => image.clone(),
            None => {
                let (w, h) = key.max;
                let req = thumbs::PreviewRequest::Fit { w, h };
                self.thumbnailer.preview(fit_id, key.path.clone(), req);
                None
            }
        };
        let shown = image.is_some();
        self.preview = Some(Preview {
            key,
            image,
            tile: None,
            zoom: 1.0,
            fit_id,
            tile_id: 0,
        });
        shown || was_shown
    }

    /// Starts, switches or stops the video preview. Returns true if it went away (a
    /// shown video only reappears once mpv has a frame).
    fn update_video(&mut self, wanted: Option<PathBuf>) -> bool {
        if self.video_preview.as_ref().map(|v| &v.path) == wanted.as_ref() {
            return false;
        }
        let Some(path) = wanted else {
            if let Some(v) = &mut self.video {
                v.stop();
            }
            return self.video_preview.take().is_some();
        };
        if self.video_tried && self.video.is_none() {
            return false; // no libmpv: videos just have no preview
        }
        // Open the (empty) player right away, shaped like the thumbnail; mpv is loaded
        // and started only after that frame is on screen.
        let guess = match self.thumbs.get(&path) {
            Some((_, Some(t))) => (t.w, t.h),
            _ => (16, 9),
        };
        if let Some(v) = &mut self.video {
            v.forget_dims(); // they were the previous file's
        }
        self.video_preview = Some(VideoPreview {
            path,
            zoom: 1.0,
            guess,
            started: false,
        });
        self.handle.insert_idle(|app| app.start_video());
        true
    }

    fn start_video(&mut self) {
        let Some(vp) = self.video_preview.as_mut().filter(|vp| !vp.started) else {
            return;
        };
        vp.started = true;
        if !self.video_tried {
            self.video_tried = true;
            self.video = Video::load(self.video_ping.clone());
        }
        match &mut self.video {
            Some(v) => v.play(&vp.path),
            None => {
                self.video_preview = None;
                self.request_redraw();
            }
        }
    }

    /// The video's display size, or until mpv knows it, the thumbnail's aspect ratio
    /// scaled up so that fitting fills the window.
    fn video_dims(&self) -> Option<((usize, usize), bool)> {
        let vp = self.video_preview.as_ref()?;
        Some(match self.video.as_ref().and_then(|v| v.dims) {
            Some(dims) => (dims, true),
            None => ((vp.guess.0 * 4096, vp.guess.1 * 4096), false),
        })
    }

    fn video_event(&mut self) {
        let Some(v) = &mut self.video else { return };
        if !v.poll() {
            return;
        }
        if v.failed {
            self.video_preview = None;
        }
        if self.video_preview.is_some() && v.dims.is_some() {
            self.request_redraw();
        } else {
            // Nothing shows this frame (switching files, or no preview): consume it anyway
            // so mpv doesn't stall waiting for it.
            v.discard_frame();
            if self.video_preview.is_some() || v.failed {
                self.request_redraw();
            }
        }
    }

    fn preview_done(&mut self, done: thumbs::PreviewDone) {
        let Some(preview) = self.preview.as_mut() else {
            return; // the pointer moved on
        };
        let image = done.image.map(Rc::new);
        if done.id == preview.fit_id && preview.image.is_none() {
            preview.image = image.clone();
            self.preview_cache.push_back((preview.key.clone(), image));
            if self.preview_cache.len() > PREVIEW_CACHE {
                self.preview_cache.pop_front();
            }
        } else if done.id == preview.tile_id {
            preview.tile = image;
        } else {
            return; // superseded
        }
        self.request_redraw();
    }

    /// Zooms the preview around its center by `notches` (positive = out, like the wheel's
    /// scroll direction), then asks for a sharp decode of what's visible. Returns false
    /// if no preview is shown.
    fn zoom_preview(&mut self, notches: f64) -> bool {
        let (ww, wh) = (self.width as f64, self.height as f64);
        if let Some(((vw, vh), _)) = self.video_dims() {
            // Same range as images: down to a pixel, up to one video pixel over several
            // windows. mpv does the scaling; see `draw`.
            let fit = video_fit((ww, wh), (vw, vh)).0 / vw as f64;
            let max = 4.0 * ww.max(wh) / fit.max(1e-9);
            let min = 1.0 / (fit * vw.max(vh) as f64).max(1.0);
            if let Some(vp) = self.video_preview.as_mut() {
                vp.zoom = (vp.zoom * ZOOM_STEP.powf(-notches)).clamp(min, max.max(1.0));
            }
            return true;
        }
        let Some(p) = self.preview.as_mut() else {
            return false;
        };
        let Some(img) = p.image.clone() else {
            return false;
        };
        let (nw, nh) = (img.native.0 as f64, img.native.1 as f64);
        // Unbounded in practice: from a 1px image up to one native pixel spanning several
        // windows (only to keep the math in range).
        let min = 1.0 / img.w.max(img.h) as f64;
        let max = 4.0 * ww.max(wh) * nw / img.w as f64;
        p.zoom = (p.zoom * ZOOM_STEP.powf(-notches)).clamp(min, max);
        let zoom = p.zoom;
        let path = p.key.path.clone();

        let (left, top, dw, dh) = preview_rect((ww, wh), &img, zoom);
        let (vx0, vx1) = (left.max(0.0), (left + dw).min(ww));
        let (vy0, vy1) = (top.max(0.0), (top + dh).min(wh));
        if vx1 <= vx0 || vy1 <= vy0 || zoom == 1.0 {
            return true;
        }
        // The native pixels on screen, and how many screen pixels they span (never more
        // than native: past 1:1 the pixels are enlarged when drawn).
        let x0 = ((vx0 - left) / dw * nw).floor().max(0.0) as usize;
        let x1 = (((vx1 - left) / dw * nw).ceil() as usize).min(img.native.0);
        let y0 = ((vy0 - top) / dh * nh).floor().max(0.0) as usize;
        let y1 = (((vy1 - top) / dh * nh).ceil() as usize).min(img.native.1);
        let (rw, rh) = (x1.saturating_sub(x0).max(1), y1.saturating_sub(y0).max(1));
        let w = ((rw as f64 / nw * dw).round() as usize).clamp(1, rw);
        let h = ((rh as f64 / nh * dh).round() as usize).clamp(1, rh);

        self.preview_seq += 1;
        let id = self.preview_seq;
        if let Some(p) = self.preview.as_mut() {
            p.tile_id = id;
        }
        let req = thumbs::PreviewRequest::Region {
            rect: (x0, y0, rw, rh),
            size: (w, h),
        };
        self.thumbnailer.preview(id, path, req);
        true
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::None => {}
            Effect::Redraw => self.request_redraw(),
            Effect::Navigate(path) => self.go(path),
            Effect::Open(path) if self.chooser.is_some() => self.dialog_file(path),
            Effect::Open(path) => ops::spawn_open(path, self.jobs.clone()),
            Effect::TogglePlay(path) => {
                self.player.toggle(path);
                self.request_redraw();
            }
            Effect::StartDrag(paths, label) => self.start_drag(paths, label),
        }
    }

    /// Draws now, or on the next frame callback if the compositor hasn't shown the last one yet.
    fn request_redraw(&mut self) {
        if self.frame_pending {
            self.dirty = true;
        } else if self.configured {
            self.draw();
        }
    }

    fn draw(&mut self) {
        self.update_thumbs();
        self.update_preview();
        let video_dims = self.video_dims();
        let (w, h) = (self.width as i32, self.height as i32);
        let stride = w * 4;

        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(w, h, stride, wl_shm::Format::Xrgb8888)
                .expect("buffer")
                .0
        });
        let bytes = match self.pool.canvas(buffer) {
            Some(bytes) => bytes,
            // Compositor still holds the last buffer: allocate a second one.
            None => {
                let (next, bytes) = self
                    .pool
                    .create_buffer(w, h, stride, wl_shm::Format::Xrgb8888)
                    .expect("buffer");
                *buffer = next;
                bytes
            }
        };

        let mut canvas = Canvas::new(bytes, w as usize, h as usize);
        self.view
            .draw(&mut canvas, &self.thumbs, self.player.state(), &self.cut);
        if let Some(ch) = &self.chooser {
            ch.draw(&mut canvas, w as usize, h as usize);
        }
        if let (Some(vp), Some(((vw, vh), known))) = (&self.video_preview, video_dims) {
            // Fitted like an image (never enlarged), then zoomed. mpv renders only what
            // fits the window: when zoomed past it, its own video-zoom crops the center.
            let win = (w as f64, h as f64);
            let (fw, fh) = video_fit(win, (vw, vh));
            let (dw, dh) = (fw * vp.zoom, fh * vp.zoom);
            let tw = dw.min(win.0).round().max(1.0) as usize;
            let th = dh.min(win.1).round().max(1.0) as usize;
            let (x, y) = ((w as usize - tw) / 2, (h as usize - th) / 2);
            canvas.dim();
            match self.video.as_mut().filter(|_| known) {
                Some(video) => {
                    let mpv_fit = (tw as f64 / vw as f64).min(th as f64 / vh as f64);
                    video.set_zoom((dw / vw as f64 / mpv_fit).log2());
                    let stride = FrameBuf::stride_for(tw);
                    video.render(self.frame.frame(tw, th), tw, th, stride);
                    canvas.copy(x, y, tw, th, self.frame.pixels(), self.frame.stride / 4);
                }
                // Still loading: the empty player.
                None => canvas.rect(x, y, tw, th, theme::BG),
            }
            if x >= 1 && y >= 1 {
                canvas.frame(x - 1, y - 1, tw + 2, th + 2, theme::BORDER);
            }
        }
        if let Some(p) = &self.preview {
            if let Some(img) = &p.image {
                // Centered over the dimmed list: the fitted image stretched to the zoom,
                // then the sharp tile of the visible part over it once it has arrived.
                canvas.dim();
                let win = (w as f64, h as f64);
                let (left, top, dw, dh) = preview_rect(win, img, p.zoom);
                canvas.stretch(&img.px, img.w, img.h, (left, top, dw, dh));
                if let Some(t) = &p.tile {
                    let (nw, nh) = (t.native.0 as f64, t.native.1 as f64);
                    let (cx, cy, cw, ch) = t.covers;
                    let rect = (
                        left + cx as f64 / nw * dw,
                        top + cy as f64 / nh * dh,
                        cw as f64 / nw * dw,
                        ch as f64 / nh * dh,
                    );
                    canvas.stretch(&t.px, t.w, t.h, rect);
                }
                // A 1px Sway-style border while the whole image is in view.
                if left >= 1.0 && top >= 1.0 {
                    let (x, y) = (left.round() as usize, top.round() as usize);
                    let (iw, ih) = (dw.round() as usize, dh.round() as usize);
                    canvas.frame(x - 1, y - 1, iw + 2, ih + 2, theme::BORDER);
                }
            }
        }
        if let Some(modal) = self.modals.front() {
            modal.draw(&mut canvas, w as usize, h as usize);
        }

        let surface = self.window.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        buffer.attach_to(surface).expect("attach");
        self.window.commit();
        self.frame_pending = true;
        self.dirty = false;
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let w = configure.new_size.0.map_or(800, |v| v.get());
        let h = configure.new_size.1.map_or(600, |v| v.get());
        if (w, h) != (self.width, self.height) {
            self.buffer = None;
            self.width = w;
            self.height = h;
            self.view.resize(w as usize, self.list_h());
        }
        self.configured = true;
        // Configures must be answered promptly, so bypass frame pacing.
        self.draw();
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.window.wl_surface() {
                continue;
            }
            let (x, y) = event.position;
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_pos = Some((x, y));
                }
                PointerEventKind::Leave { .. } => self.pointer_pos = None,
                PointerEventKind::Press { serial, .. } => self.press_serial = serial,
                _ => {}
            }
            if let PointerEventKind::Enter { serial } = event.kind {
                self.enter_serial = serial;
                if let Some((_, Some(device))) = &self.pointer {
                    device.set_shape(serial, Shape::Default);
                }
                self.shape = Shape::Default;
            }
            if !self.modals.is_empty() {
                self.modal_pointer(&event.kind);
                self.update_shape();
                continue;
            }
            let over_bar = self.dialog_pointer(&event.kind);
            self.update_shape();
            let effect = match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } if over_bar => {
                    self.view.leave()
                }
                PointerEventKind::Press { .. } | PointerEventKind::Axis { .. } if over_bar => {
                    Effect::None
                }
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.view.motion(x, y)
                }
                PointerEventKind::Leave { .. } => self.view.leave(),
                PointerEventKind::Press { button, .. } if BACK_BUTTONS.contains(&button) => {
                    self.history(true);
                    Effect::None
                }
                PointerEventKind::Press { button, .. } if FORWARD_BUTTONS.contains(&button) => {
                    self.history(false);
                    Effect::None
                }
                PointerEventKind::Press { button, time, .. } => {
                    self.view.press(button, time, self.ctrl, self.shift)
                }
                PointerEventKind::Release { button, .. } => self.view.release(button),
                PointerEventKind::Axis { vertical, .. } => {
                    // Over a shown preview the wheel zooms instead of scrolling the list.
                    let notches = if vertical.value120 != 0 {
                        vertical.value120 as f64 / 120.0
                    } else {
                        vertical.absolute / ZOOM_NOTCH_PX
                    };
                    if self.zoom_preview(notches) {
                        Effect::Redraw
                    } else {
                        self.view.scroll(vertical.value120, vertical.absolute)
                    }
                }
            };
            self.apply(effect);
            if matches!(event.kind, PointerEventKind::Press { .. }) && !over_bar {
                self.sync_save_name();
            }
        }
        if self.update_preview() {
            self.request_redraw();
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seats.get_pointer(qh, &seat).expect("pointer");
            let shape = self
                .cursor_shapes
                .as_ref()
                .map(|m| m.get_shape_device(&pointer, qh));
            self.pointer = Some((pointer, shape));
            if let (None, Some(manager)) = (&self.data_device, &self.data_devices) {
                self.data_device = Some(manager.get_data_device(qh, &seat));
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(seat.get_keyboard(qh, keyboard::RawKeyboard));
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some((pointer, shape)) = self.pointer.take() {
                if let Some(shape) = shape {
                    shape.destroy();
                }
                pointer.release();
            }
        }
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl CompositorHandler for App {
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        if self.dirty {
            self.draw();
        }
    }

    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
delegate_dispatch2!(App);
