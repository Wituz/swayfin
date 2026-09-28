//! Video previews with libmpv's software renderer, drawn into our own window.
//! libmpv is dlopen'd on the first video hover, so startup never pays for it.
//!
//! Everything runs on the UI thread, which also renders: per libmpv's render API rules,
//! it therefore only uses non-blocking client calls (async commands, observed
//! properties, zero-timeout event polling).

use std::{
    ffi::{CStr, CString, c_char, c_double, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr,
};

use libloading::Library;
use smithay_client_toolkit::reexports::calloop::ping::Ping;

#[repr(C)]
struct MpvHandle {
    _private: [u8; 0],
}
#[repr(C)]
struct MpvRenderContext {
    _private: [u8; 0],
}
#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}
#[repr(C)]
struct Event {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}
#[repr(C)]
struct EventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}
#[repr(C)]
struct EventEndFile {
    reason: c_int,
    error: c_int,
}

const PARAM_INVALID: c_int = 0;
const PARAM_API_TYPE: c_int = 1;
const PARAM_SW_SIZE: c_int = 17;
const PARAM_SW_FORMAT: c_int = 18;
const PARAM_SW_STRIDE: c_int = 19;
const PARAM_SW_POINTER: c_int = 20;
const UPDATE_FRAME: u64 = 1;
const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;
const EVENT_NONE: c_int = 0;
const EVENT_END_FILE: c_int = 7;
const EVENT_VIDEO_RECONFIG: c_int = 17;
const EVENT_PROPERTY_CHANGE: c_int = 22;
const END_FILE_REASON_ERROR: c_int = 4;

type WakeFn = unsafe extern "C" fn(*mut c_void);

/// The libmpv functions we use, resolved at runtime.
struct Api {
    create: unsafe extern "C" fn() -> *mut MpvHandle,
    initialize: unsafe extern "C" fn(*mut MpvHandle) -> c_int,
    set_option_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    command_async: unsafe extern "C" fn(*mut MpvHandle, u64, *mut *const c_char) -> c_int,
    set_property_async:
        unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int, *mut c_void) -> c_int,
    observe_property: unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int,
    wait_event: unsafe extern "C" fn(*mut MpvHandle, c_double) -> *mut Event,
    set_wakeup_callback: unsafe extern "C" fn(*mut MpvHandle, Option<WakeFn>, *mut c_void),
    render_create:
        unsafe extern "C" fn(*mut *mut MpvRenderContext, *mut MpvHandle, *mut RenderParam) -> c_int,
    render_set_update_callback:
        unsafe extern "C" fn(*mut MpvRenderContext, Option<WakeFn>, *mut c_void),
    render_update: unsafe extern "C" fn(*mut MpvRenderContext) -> u64,
    render: unsafe extern "C" fn(*mut MpvRenderContext, *mut RenderParam) -> c_int,
}

pub struct Video {
    _lib: Library,
    api: Api,
    mpv: *mut MpvHandle,
    render: *mut MpvRenderContext,
    /// Display size of the current video (aspect-corrected), once mpv knows it.
    pub dims: Option<(usize, usize)>,
    /// mpv's latest `dwidth`/`dheight`. It only reports changes, so a new file of the
    /// same size reports nothing: these are kept across files, and `dims` is taken from
    /// them once the new file's video is configured.
    dw: Option<i64>,
    dh: Option<i64>,
    /// A file was started and its video isn't configured yet.
    pending: bool,
    /// The file couldn't be played.
    pub failed: bool,
    zoom_sent: f64,
}

/// Wakes the event loop from mpv's threads (both callbacks may fire on any thread).
unsafe extern "C" fn wake(ping: *mut c_void) {
    // SAFETY: `ping` is the leaked Box<Ping> from `Video::load`; Ping is thread-safe.
    unsafe { (*(ping as *const Ping)).ping() };
}

impl Video {
    /// Loads libmpv and sets up a software-rendering player. None if libmpv is missing.
    pub fn load(ping: Ping) -> Option<Self> {
        // SAFETY: loading a well-known system library and resolving its documented API.
        unsafe {
            let lib = Library::new("libmpv.so.2").ok()?;
            macro_rules! sym {
                ($name:literal) => {
                    *lib.get(concat!($name, "\0").as_bytes()).ok()?
                };
            }
            let api = Api {
                create: sym!("mpv_create"),
                initialize: sym!("mpv_initialize"),
                set_option_string: sym!("mpv_set_option_string"),
                command_async: sym!("mpv_command_async"),
                set_property_async: sym!("mpv_set_property_async"),
                observe_property: sym!("mpv_observe_property"),
                wait_event: sym!("mpv_wait_event"),
                set_wakeup_callback: sym!("mpv_set_wakeup_callback"),
                render_create: sym!("mpv_render_context_create"),
                render_set_update_callback: sym!("mpv_render_context_set_update_callback"),
                render_update: sym!("mpv_render_context_update"),
                render: sym!("mpv_render_context_render"),
            };

            let mpv = (api.create)();
            if mpv.is_null() {
                return None;
            }
            for (k, v) in [
                ("vo", "libmpv"),
                // Decode on the GPU where possible, copied back for the software renderer.
                // vaapi specifically: "auto" probes other backends first, which costs ~0.5 s
                // before the first frame.
                ("hwdec", "vaapi-copy"),
                ("loop-file", "inf"),
                ("terminal", "no"),
                ("input-default-bindings", "no"),
                ("sub-auto", "no"),
                ("sid", "no"),
            ] {
                let (k, v) = (CString::new(k).ok()?, CString::new(v).ok()?);
                (api.set_option_string)(mpv, k.as_ptr(), v.as_ptr());
            }
            if (api.initialize)(mpv) < 0 {
                return None;
            }

            let mut sw = *b"sw\0";
            let mut params = [
                RenderParam {
                    kind: PARAM_API_TYPE,
                    data: sw.as_mut_ptr().cast(),
                },
                RenderParam {
                    kind: PARAM_INVALID,
                    data: ptr::null_mut(),
                },
            ];
            let mut render = ptr::null_mut();
            if (api.render_create)(&mut render, mpv, params.as_mut_ptr()) < 0 {
                return None;
            }

            // Lives as long as the process; mpv may call back at any time.
            let ping: *mut c_void = Box::into_raw(Box::new(ping)).cast();
            (api.set_wakeup_callback)(mpv, Some(wake), ping);
            (api.render_set_update_callback)(render, Some(wake), ping);
            for (id, name) in [(1u64, c"dwidth"), (2, c"dheight")] {
                (api.observe_property)(mpv, id, name.as_ptr(), FORMAT_INT64);
            }

            Some(Self {
                _lib: lib,
                api,
                mpv,
                render,
                dims: None,
                dw: None,
                dh: None,
                pending: false,
                failed: false,
                zoom_sent: 0.0,
            })
        }
    }

    fn command(&self, args: &[&CStr]) {
        let mut argv: Vec<*const c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(ptr::null());
        // SAFETY: NUL-terminated argv of valid C strings; mpv copies them.
        unsafe {
            (self.api.command_async)(self.mpv, 0, argv.as_mut_ptr());
        }
    }

    /// Starts `path` from the beginning (replacing whatever was playing).
    pub fn play(&mut self, path: &Path) {
        let Ok(file) = CString::new(path.as_os_str().as_bytes()) else {
            self.failed = true;
            return;
        };
        self.forget_dims();
        self.failed = false;
        self.command(&[c"loadfile", &file, c"replace"]);
    }

    pub fn stop(&mut self) {
        self.forget_dims();
        self.command(&[c"stop"]);
    }

    /// The current size belongs to the previous file; wait for the next one's.
    pub fn forget_dims(&mut self) {
        self.dims = None;
        self.pending = true;
    }

    fn update_dims(&mut self) -> bool {
        let dims = match (self.dw, self.dh) {
            (Some(w), Some(h)) if w > 0 && h > 0 && !self.pending => Some((w as usize, h as usize)),
            _ => None,
        };
        std::mem::replace(&mut self.dims, dims) != dims
    }

    /// mpv's own zoom (log2 of the scale beyond fitting the render target), for when the
    /// zoomed video is bigger than the window and only its center is rendered.
    pub fn set_zoom(&mut self, log2: f64) {
        if log2 == self.zoom_sent {
            return;
        }
        self.zoom_sent = log2;
        let mut v = log2;
        // SAFETY: valid handle; mpv copies the double.
        unsafe {
            (self.api.set_property_async)(
                self.mpv,
                0,
                c"video-zoom".as_ptr(),
                FORMAT_DOUBLE,
                (&raw mut v).cast(),
            );
        }
    }

    /// Handles what woke us. Returns true if something visible changed (new frame,
    /// size known, or the file failed).
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            // SAFETY: zero timeout never blocks; the event stays valid until the next call.
            let ev = unsafe { &*(self.api.wait_event)(self.mpv, 0.0) };
            match ev.event_id {
                EVENT_NONE => break,
                EVENT_PROPERTY_CHANGE => {
                    // SAFETY: PROPERTY_CHANGE carries an mpv_event_property.
                    let prop = unsafe { &*(ev.data as *const EventProperty) };
                    let value = (prop.format == FORMAT_INT64 && !prop.data.is_null())
                        // SAFETY: INT64 data points to an i64.
                        .then(|| unsafe { *(prop.data as *const i64) });
                    match ev.reply_userdata {
                        1 => self.dw = value,
                        2 => self.dh = value,
                        _ => {}
                    }
                    changed |= self.update_dims();
                }
                EVENT_VIDEO_RECONFIG => {
                    self.pending = false;
                    changed |= self.update_dims();
                }
                EVENT_END_FILE => {
                    // SAFETY: END_FILE carries an mpv_event_end_file.
                    let end = unsafe { &*(ev.data as *const EventEndFile) };
                    if end.reason == END_FILE_REASON_ERROR {
                        self.failed = true;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        // SAFETY: valid render context, called from the rendering thread.
        let flags = unsafe { (self.api.render_update)(self.render) };
        changed | (flags & UPDATE_FRAME != 0)
    }

    /// Renders the pending frame into a throwaway buffer. mpv's core waits (with a long
    /// timeout) for each frame to be rendered, so frames we don't show must still be
    /// consumed, e.g. while switching files.
    pub fn discard_frame(&mut self) {
        let mut scratch = FrameBuf::default();
        let stride = FrameBuf::stride_for(1);
        self.render(scratch.frame(1, 1), 1, 1, stride);
    }

    /// Renders the current frame into `buf` (w x h, `stride` bytes per row, "bgr0" =
    /// our XRGB8888). `buf` must be 64-byte aligned with room for h rows.
    pub fn render(&mut self, buf: &mut [u8], w: usize, h: usize, stride: usize) {
        debug_assert!(buf.len() >= stride * h && buf.as_ptr() as usize % 64 == 0);
        let mut size = [w as c_int, h as c_int];
        let mut stride = stride;
        let mut format = *b"bgr0\0";
        let mut params = [
            RenderParam {
                kind: PARAM_SW_SIZE,
                data: size.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: PARAM_SW_FORMAT,
                data: format.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: PARAM_SW_STRIDE,
                data: (&raw mut stride).cast(),
            },
            RenderParam {
                kind: PARAM_SW_POINTER,
                data: buf.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: params describe `buf`, which is large enough and aligned (checked above).
        unsafe {
            (self.api.render)(self.render, params.as_mut_ptr());
        }
    }
}

/// A 64-byte aligned pixel buffer for mpv to render into.
#[derive(Default)]
pub struct FrameBuf {
    raw: Vec<u8>,
    pub w: usize,
    pub h: usize,
    pub stride: usize,
}

impl FrameBuf {
    /// Bytes per row for a frame `w` wide: 64-byte multiples, as mpv prefers.
    pub fn stride_for(w: usize) -> usize {
        (w * 4).div_ceil(64) * 64
    }

    /// Resizes for a w x h frame; returns the aligned bytes.
    pub fn frame(&mut self, w: usize, h: usize) -> &mut [u8] {
        self.w = w;
        self.h = h;
        self.stride = Self::stride_for(w);
        let need = self.stride * h + 64;
        if self.raw.len() < need {
            self.raw = vec![0; need];
        }
        let off = self.raw.as_ptr().align_offset(64);
        &mut self.raw[off..off + self.stride * h]
    }

    /// The rendered frame as XRGB rows (`stride / 4` pixels apart).
    pub fn pixels(&self) -> &[u32] {
        let off = self.raw.as_ptr().align_offset(64);
        let bytes = &self.raw[off..off + self.stride * self.h];
        // SAFETY: 64-byte aligned, so u32-aligned; any bit pattern is a valid u32.
        let (pre, px, _) = unsafe { bytes.align_to::<u32>() };
        debug_assert!(pre.is_empty());
        px
    }
}
