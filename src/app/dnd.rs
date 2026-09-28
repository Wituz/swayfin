//! Drag and drop. Our drags offer a text/uri-list; Ctrl held when the drag starts makes
//! it copy-only (Sway passes no keys during drags, so that's the only moment we can see
//! it). Drops of local files (from us, other swayfin windows, or any app) onto a folder
//! row go into that folder, anywhere else into the folder shown, and become a move or
//! copy job. The drop is only finished (acknowledged to the source) once that job is
//! done, so the source window refreshes when the files have actually moved.

use std::{
    ffi::OsString,
    io::{Read, Write as _},
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    thread,
};

use smithay_client_toolkit::{
    data_device_manager::{
        WritePipe,
        data_device::{DataDeviceData, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{DataSourceHandler, DragSource},
    },
    reexports::client::{
        Connection, Proxy, QueueHandle,
        protocol::{
            wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
            wl_data_source::WlDataSource, wl_shm, wl_surface::WlSurface,
        },
    },
    shell::WaylandSurface,
    shm::slot::Buffer,
};

use super::App;
use crate::{
    font::{self, CELL_H, CELL_W},
    mime, ops,
    render::Canvas,
    theme,
};

const URI_LIST: &str = "text/uri-list";
/// The label sits this far below-right of the pointer.
const ICON_OFFSET: usize = 12;
const ICON_PAD_X: usize = 6;
const ICON_PAD_Y: usize = 3;
const ICON_MAX_CHARS: usize = 40;
const OPAQUE: u32 = 0xff00_0000;

pub struct ActiveDrag {
    source: DragSource,
    icon: WlSurface,
    _icon_buffer: Buffer,
    paths: Vec<PathBuf>,
    copy: bool,
}

/// A drop being carried out: finished once its job reports Done with this token.
pub struct PendingDrop {
    pub offer: DragOffer,
    pub token: u64,
}

impl App {
    pub(super) fn start_drag(&mut self, paths: Vec<PathBuf>, label: String) {
        let (Some(manager), Some(_)) = (&self.data_devices, &self.data_device) else {
            self.view.end_drag();
            return;
        };
        let copy = self.ctrl;
        let actions = if copy {
            DndAction::Copy
        } else {
            DndAction::Copy | DndAction::Move
        };
        let source = manager.create_drag_and_drop_source(&self.qh, [URI_LIST], actions);
        let icon = self.compositor.create_surface(&self.qh);
        let icon_buffer = self.draw_drag_icon(&icon, &label);
        let device = self.data_device.as_ref().expect("checked above");
        source.start_drag(
            device,
            self.window.wl_surface(),
            Some(&icon),
            self.press_serial,
        );
        self.drag = Some(ActiveDrag {
            source,
            icon,
            _icon_buffer: icon_buffer,
            paths,
            copy,
        });
    }

    /// The label on a transparent margin, so it floats beside the cursor instead of under it.
    fn draw_drag_icon(&mut self, icon: &WlSurface, label: &str) -> Buffer {
        let chars = label.chars().count().min(ICON_MAX_CHARS);
        let lw = chars * CELL_W + 2 * ICON_PAD_X + 2;
        let lh = CELL_H + 2 * ICON_PAD_Y + 2;
        let (w, h) = (ICON_OFFSET + lw, ICON_OFFSET + lh);
        let (buffer, bytes) = self
            .pool
            .create_buffer(w as i32, h as i32, w as i32 * 4, wl_shm::Format::Argb8888)
            .expect("icon buffer");

        let mut c = Canvas::new(bytes, w, h);
        c.fill(0);
        c.rect(ICON_OFFSET, ICON_OFFSET, lw, lh, OPAQUE | theme::BORDER);
        c.frame(ICON_OFFSET, ICON_OFFSET, lw, lh, OPAQUE | theme::TEXT_DIM);
        let x = ICON_OFFSET + 1 + ICON_PAD_X;
        let y = ICON_OFFSET + 1 + ICON_PAD_Y;
        c.text(
            x,
            y,
            label,
            &font::REGULAR,
            OPAQUE | theme::TEXT,
            x + chars * CELL_W,
        );

        buffer.attach_to(icon).expect("attach icon");
        icon.damage_buffer(0, 0, w as i32, h as i32);
        icon.commit();
        buffer
    }

    fn end_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            drag.icon.destroy();
        }
        self.view.end_drag();
        self.request_redraw();
    }

    /// Accepts the offer only while our own drag is over a folder it can move into.
    fn dnd_update(&mut self, device: &WlDataDevice, x: f64, y: f64, entered: bool) {
        let changed = self.view.dnd_motion(x, y);
        if changed || entered {
            if let Some(offer) = drag_offer(device) {
                let files = offer.with_mime_types(|m| m.iter().any(|t| t == URI_LIST));
                if files && self.view.drop_dir().is_some() {
                    offer.accept_mime_type(offer.serial, Some(URI_LIST.into()));
                    // Move unless the source only allows copying (Ctrl at drag start).
                    offer.set_actions(DndAction::Copy | DndAction::Move, DndAction::Move);
                } else {
                    offer.accept_mime_type(offer.serial, None);
                    offer.set_actions(DndAction::empty(), DndAction::empty());
                }
            }
        }
        if changed {
            self.request_redraw();
        }
    }
}

fn drag_offer(device: &WlDataDevice) -> Option<DragOffer> {
    device.data::<DataDeviceData>()?.drag_offer()
}

/// Largest uri-list we read from a drop.
const URI_LIST_MAX: u64 = 16 * 1024 * 1024;

/// Local paths from a text/uri-list (RFC 2483): one URI per line, `#` lines are comments.
/// Only file URIs on this machine (no host, or "localhost") are kept.
pub(super) fn parse_uri_list(list: &[u8]) -> Vec<PathBuf> {
    list.split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .filter(|l| !l.is_empty() && !l.starts_with(b"#"))
        .filter_map(|l| {
            let rest = l.strip_prefix(b"file://")?;
            let path = match rest.strip_prefix(b"localhost") {
                Some(p) => p,
                None => rest,
            };
            path.starts_with(b"/")
                .then(|| PathBuf::from(OsString::from_vec(percent_decode(path))))
        })
        .collect()
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' && i + 2 < s.len() {
            if let (Some(h), Some(l)) = (hex(s[i + 1]), hex(s[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

fn uri_list(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| mime::file_uri(p) + "\r\n").collect()
}

impl DataDeviceHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        if surface == self.window.wl_surface() {
            self.dnd_update(device, x, y, true);
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        self.dnd_update(device, x, y, false);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        if self.view.dnd_leave() {
            self.request_redraw();
        }
    }

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        let offer = drag_offer(device);
        let dir = self.view.drop_dir();
        if self.view.dnd_leave() {
            self.request_redraw();
        }
        let (Some(offer), Some(dir)) = (offer, dir) else {
            if let Some(offer) = drag_offer(device) {
                offer.destroy();
            }
            return;
        };
        self.drop_seq += 1;
        let token = self.drop_seq;
        if let Some(drag) = &self.drag {
            // Our own drag, dropped in this window: we already have the paths.
            ops::spawn_transfer(drag.paths.clone(), dir, drag.copy, token, self.jobs.clone());
        } else {
            let copy = offer.selected_action == DndAction::Copy;
            let Ok(pipe) = offer.receive(URI_LIST.into()) else {
                offer.destroy();
                return;
            };
            // The source only starts writing once our request reaches the compositor.
            let _ = self.conn.flush();
            let jobs = self.jobs.clone();
            thread::spawn(move || {
                let mut list = Vec::new();
                let _ = pipe.take(URI_LIST_MAX).read_to_end(&mut list);
                let paths = parse_uri_list(&list);
                ops::spawn_transfer(paths, dir, copy, token, jobs);
            });
        }
        self.pending_drops.push(PendingDrop { offer, token });
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataSourceHandler for App {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        mut fd: WritePipe,
    ) {
        if self.is_clipboard_source(source) {
            self.clipboard_send(&mime, fd);
            return;
        }
        let Some(drag) = self.drag.as_ref().filter(|d| d.source.inner() == source) else {
            return;
        };
        if mime != URI_LIST {
            return;
        }
        // Off-thread: a slow reader must not stall the UI on a full pipe.
        let list = uri_list(&drag.paths);
        thread::spawn(move || {
            let _ = fd.write_all(list.as_bytes());
        });
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        if !self.clipboard_cancelled(source) {
            self.end_drag();
        }
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        self.end_drag();
        // Dropped elsewhere and carried out: our files may have moved away.
        self.refresh();
    }

    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl DataOfferHandler for App {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}
