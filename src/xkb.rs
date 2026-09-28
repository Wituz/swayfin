//! Keyboard layout for text entry. libxkbcommon is dlopen'd and the compositor's keymap
//! compiled only when a text field first opens, so startup never pays for either.

use std::{
    env,
    ffi::CString,
    os::fd::{AsRawFd, OwnedFd},
    ptr,
};

use xkbcommon_dl::{
    XkbCommon, XkbCommonCompose, xkb_compose_compile_flags, xkb_compose_state,
    xkb_compose_state_flags, xkb_compose_status, xkb_compose_table, xkb_context, xkb_context_flags,
    xkb_keymap, xkb_keymap_compile_flags, xkb_keymap_format, xkb_state, xkbcommon_compose_option,
    xkbcommon_option,
};

/// The keymap as the compositor sent it, kept unparsed until needed.
pub struct KeymapFd {
    pub fd: OwnedFd,
    pub size: usize,
}

#[derive(Clone, Copy, Default)]
pub struct Mods {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

pub struct Typed {
    pub sym: u32,
    /// What the key types, if anything (nothing while a dead key waits for the next key).
    pub text: Option<String>,
}

pub struct Xkb {
    lib: &'static XkbCommon,
    context: *mut xkb_context,
    keymap: *mut xkb_keymap,
    state: *mut xkb_state,
    compose: Option<Compose>,
}

struct Compose {
    lib: &'static XkbCommonCompose,
    table: *mut xkb_compose_table,
    state: *mut xkb_compose_state,
}

/// Wayland keycodes are evdev codes; xkb's are offset by 8.
const EVDEV_OFFSET: u32 = 8;

impl Xkb {
    /// None if libxkbcommon isn't installed or the keymap doesn't compile.
    pub fn load(keymap: &KeymapFd, mods: Mods) -> Option<Self> {
        let lib = xkbcommon_option()?;
        // SAFETY: plain libxkbcommon calls; every pointer is null-checked before use and
        // the mapping is only read within `size` bytes (the keymap is NUL-terminated).
        unsafe {
            let map = libc::mmap(
                ptr::null_mut(),
                keymap.size,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                keymap.fd.as_raw_fd(),
                0,
            );
            if map == libc::MAP_FAILED {
                return None;
            }
            let context = (lib.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            let km = if context.is_null() {
                ptr::null_mut()
            } else {
                (lib.xkb_keymap_new_from_string)(
                    context,
                    map.cast(),
                    xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                    xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
                )
            };
            libc::munmap(map, keymap.size);
            if km.is_null() {
                if !context.is_null() {
                    (lib.xkb_context_unref)(context);
                }
                return None;
            }
            let state = (lib.xkb_state_new)(km);
            if state.is_null() {
                (lib.xkb_keymap_unref)(km);
                (lib.xkb_context_unref)(context);
                return None;
            }
            let mut xkb = Self {
                lib,
                context,
                keymap: km,
                state,
                compose: Compose::load(context),
            };
            xkb.set_mods(mods);
            Some(xkb)
        }
    }

    pub fn set_mods(&mut self, m: Mods) {
        // SAFETY: state is valid for self's lifetime.
        unsafe {
            (self.lib.xkb_state_update_mask)(
                self.state,
                m.depressed,
                m.latched,
                m.locked,
                0,
                0,
                m.group,
            );
        }
    }

    /// Whether holding `key` (evdev code) should repeat.
    pub fn repeats(&self, key: u32) -> bool {
        // SAFETY: keymap is valid for self's lifetime.
        unsafe { (self.lib.xkb_keymap_key_repeats)(self.keymap, key + EVDEV_OFFSET) != 0 }
    }

    /// Resolves a pressed key (evdev code), running it through dead keys / compose.
    pub fn key(&mut self, key: u32) -> Typed {
        let code = key + EVDEV_OFFSET;
        // SAFETY: state is valid for self's lifetime.
        let sym = unsafe { (self.lib.xkb_state_key_get_one_sym)(self.state, code) };
        if let Some(c) = &self.compose {
            match c.feed(sym) {
                ComposeResult::Composing => return Typed { sym, text: None },
                ComposeResult::Composed(text) => {
                    return Typed {
                        sym,
                        text: Some(text),
                    };
                }
                ComposeResult::Cancelled => return Typed { sym, text: None },
                ComposeResult::Nothing => {}
            }
        }
        // SAFETY: first call with a null buffer returns the length; then fill a sized buffer.
        let text = unsafe {
            let len = (self.lib.xkb_state_key_get_utf8)(self.state, code, ptr::null_mut(), 0);
            (len > 0).then(|| {
                let mut buf = vec![0u8; len as usize + 1];
                (self.lib.xkb_state_key_get_utf8)(
                    self.state,
                    code,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                );
                buf.truncate(len as usize);
                String::from_utf8_lossy(&buf).into_owned()
            })
        };
        Typed { sym, text }
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        // SAFETY: each pointer was created by us and is released exactly once.
        unsafe {
            if let Some(c) = &self.compose {
                (c.lib.xkb_compose_state_unref)(c.state);
                (c.lib.xkb_compose_table_unref)(c.table);
            }
            (self.lib.xkb_state_unref)(self.state);
            (self.lib.xkb_keymap_unref)(self.keymap);
            (self.lib.xkb_context_unref)(self.context);
        }
    }
}

enum ComposeResult {
    Nothing,
    Composing,
    Composed(String),
    Cancelled,
}

impl Compose {
    /// The compose table for the user's locale (dead keys live here). None if there is none.
    unsafe fn load(context: *mut xkb_context) -> Option<Self> {
        let lib = xkbcommon_compose_option()?;
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|v| env::var(v).ok())
            .find(|v| !v.is_empty())
            .unwrap_or_else(|| "C".into());
        let locale = CString::new(locale).ok()?;
        // SAFETY: context is valid; results are null-checked.
        unsafe {
            let table = (lib.xkb_compose_table_new_from_locale)(
                context,
                locale.as_ptr(),
                xkb_compose_compile_flags::XKB_COMPOSE_COMPILE_NO_FLAGS,
            );
            if table.is_null() {
                return None;
            }
            let state = (lib.xkb_compose_state_new)(
                table,
                xkb_compose_state_flags::XKB_COMPOSE_STATE_NO_FLAGS,
            );
            if state.is_null() {
                (lib.xkb_compose_table_unref)(table);
                return None;
            }
            Some(Self { lib, table, state })
        }
    }

    fn feed(&self, sym: u32) -> ComposeResult {
        // SAFETY: state is valid for self's lifetime.
        unsafe {
            (self.lib.xkb_compose_state_feed)(self.state, sym);
            match (self.lib.xkb_compose_state_get_status)(self.state) {
                xkb_compose_status::XKB_COMPOSE_NOTHING => ComposeResult::Nothing,
                xkb_compose_status::XKB_COMPOSE_COMPOSING => ComposeResult::Composing,
                xkb_compose_status::XKB_COMPOSE_CANCELLED => {
                    (self.lib.xkb_compose_state_reset)(self.state);
                    ComposeResult::Cancelled
                }
                xkb_compose_status::XKB_COMPOSE_COMPOSED => {
                    let mut buf = [0u8; 64];
                    let len = (self.lib.xkb_compose_state_get_utf8)(
                        self.state,
                        buf.as_mut_ptr().cast(),
                        buf.len(),
                    );
                    (self.lib.xkb_compose_state_reset)(self.state);
                    let len = (len.max(0) as usize).min(buf.len() - 1);
                    ComposeResult::Composed(String::from_utf8_lossy(&buf[..len]).into_owned())
                }
            }
        }
    }
}
