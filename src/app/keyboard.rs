//! Keyboard: shortcuts use raw evdev codes (physical positions) and the raw modifier mask,
//! so no layout is needed at startup. Text entry resolves keys through `Xkb`, which is
//! only loaded when a text field opens.

use std::{io::Read, thread, time::Duration};

use smithay_client_toolkit::{
    dispatch2::Dispatch2,
    reexports::{
        calloop::timer::{TimeoutAction, Timer},
        client::{Connection, QueueHandle, WEnum, protocol::wl_keyboard},
    },
};
use xkbcommon_dl::keysyms as k;

use super::App;
use crate::{
    modal::{Input, Modal},
    ops,
    view::Effect,
    xkb::{KeymapFd, Mods, Xkb},
};

pub struct RawKeyboard;

/// Bit order of the modifier mask is fixed in every standard keymap: Shift, Lock, Control, ...
const MOD_SHIFT: u32 = 1 << 0;
const MOD_CTRL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;
const KEY_ESC: u32 = 1;
const KEY_ENTER: u32 = 28;
const KEY_E: u32 = 18;
const KEY_P: u32 = 25;
const KEY_A: u32 = 30;
const KEY_X: u32 = 45;
const KEY_C: u32 = 46;
const KEY_V: u32 = 47;
const KEY_K: u32 = 37;
const KEY_Z: u32 = 44;
const KEY_DOT: u32 = 52;
const KEY_F2: u32 = 60;
const KEY_N: u32 = 49;
const KEY_UP: u32 = 103;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_DOWN: u32 = 108;
const KEY_DELETE: u32 = 111;
const KEY_KPENTER: u32 = 96;

/// Clipboard types for pasting, most preferred first.
const PASTE_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
const PASTE_MAX: u64 = 64 * 1024;

impl Dispatch2<wl_keyboard::WlKeyboard, App> for RawKeyboard {
    fn event(
        &self,
        app: &mut App,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                app.keymap = Some(KeymapFd {
                    fd,
                    size: size as usize,
                });
                // Layout changed while text entry is live: recompile now.
                if app.xkb.is_some() {
                    app.xkb = Xkb::load(app.keymap.as_ref().unwrap(), app.mods);
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                let active = mods_depressed | mods_latched;
                app.ctrl = active & MOD_CTRL != 0;
                app.shift = active & MOD_SHIFT != 0;
                app.alt = active & MOD_ALT != 0;
                app.mods = Mods {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                };
                if let Some(xkb) = &mut app.xkb {
                    xkb.set_mods(app.mods);
                }
            }
            wl_keyboard::Event::Leave { .. } => {
                app.ctrl = false;
                app.shift = false;
                app.alt = false;
                app.stop_repeat();
            }
            wl_keyboard::Event::Key {
                key,
                serial,
                state: WEnum::Value(state),
                ..
            } => match state {
                wl_keyboard::KeyState::Pressed => {
                    app.key_serial = serial;
                    app.key_pressed(key)
                }
                wl_keyboard::KeyState::Released => app.key_released(key),
                _ => {}
            },
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                app.repeat_info = (rate.max(0) as u32, delay.max(0) as u32);
            }
            _ => {}
        }
    }
}

impl App {
    fn key_pressed(&mut self, key: u32) {
        self.stop_repeat();
        if !self.handle_key(key) {
            return;
        }
        let (rate, delay) = self.repeat_info;
        if rate == 0 {
            return;
        }
        let interval = Duration::from_micros(1_000_000 / rate as u64);
        let timer = Timer::from_duration(Duration::from_millis(delay as u64));
        let token = self
            .handle
            .insert_source(timer, move |_, _, app| {
                if app.handle_key(key) {
                    TimeoutAction::ToDuration(interval)
                } else {
                    app.repeat = None;
                    TimeoutAction::Drop
                }
            })
            .expect("repeat timer");
        self.repeat = Some((key, token));
    }

    fn key_released(&mut self, key: u32) {
        if self.repeat.is_some_and(|(k, _)| k == key) {
            self.stop_repeat();
        }
    }

    pub(super) fn stop_repeat(&mut self) {
        if let Some((_, token)) = self.repeat.take() {
            self.handle.remove(token);
        }
    }

    /// Acts on a pressed key. Returns true if holding it should repeat the action.
    fn handle_key(&mut self, key: u32) -> bool {
        if self.modals.front().is_some_and(Modal::takes_text) {
            return self.text_key(key);
        }
        if !self.modals.is_empty() {
            // Other modals only know Enter/Esc (the delete confirmation uses them).
            let input = match key {
                KEY_ENTER | KEY_KPENTER => Input::Enter,
                KEY_ESC => Input::Escape,
                _ => return false,
            };
            self.modal_key(input);
            return false;
        }
        if let Some(ch) = &self.chooser {
            if ch.typing {
                self.load_xkb();
                return self.text_key(key);
            }
            if !self.ctrl && !self.shift && !self.alt {
                match key {
                    KEY_ESC => self.exit = true,
                    KEY_ENTER | KEY_KPENTER => self.dialog_accept(),
                    _ => {}
                }
                if matches!(key, KEY_ESC | KEY_ENTER | KEY_KPENTER) {
                    return false;
                }
            }
        }
        if self.alt {
            if !self.ctrl && !self.shift && matches!(key, KEY_ENTER | KEY_KPENTER) {
                if let Some(file) = self.view.open_with_target() {
                    ops::spawn_open_with(file, self.jobs.clone());
                }
            }
            return false;
        }
        let effect = match (self.ctrl, self.shift, key) {
            (false, false, KEY_ENTER | KEY_KPENTER) => {
                let effect = self.view.open_selected_any();
                self.apply(effect);
                return false;
            }
            (false, false, KEY_DELETE) => {
                if let Some(modal) = self.view.confirm_delete() {
                    self.push_modal(modal);
                }
                return false;
            }
            (true, _, KEY_A) => {
                let effect = self.view.select_all();
                self.apply(effect);
                return false;
            }
            (true, false, KEY_C | KEY_X) => {
                self.copy_selection(key == KEY_X);
                return false;
            }
            (true, false, KEY_V) => {
                self.paste_files();
                return false;
            }
            (true, false, KEY_N) => {
                self.open_text_modal(Modal::new_folder());
                return false;
            }
            (false, false, KEY_P) => {
                if let Some(path) = self.view.play_target() {
                    self.apply(Effect::TogglePlay(path));
                }
                return false;
            }
            (false, false, KEY_E) => {
                if let Some(dir) = self.view.edit_target() {
                    ops::spawn_editor(dir, self.jobs.clone());
                }
                return false;
            }
            (false, false, KEY_Z) | (true, false, KEY_K) => {
                self.open_text_modal(Modal::go_to());
                return false;
            }
            (false, false, KEY_DOT) => {
                self.toggle_dotfiles();
                return false;
            }
            (false, false, KEY_F2) => {
                if let Some(modal) = self.view.rename_target() {
                    self.open_text_modal(modal);
                }
                return false;
            }
            (false, false, KEY_RIGHT) => {
                let effect = self.view.open_selected();
                self.apply(effect);
                return false;
            }
            (false, false, KEY_LEFT) => {
                let effect = self.view.go_up();
                self.apply(effect);
                return false;
            }
            (false, false, KEY_UP) => self.view.step(false),
            (false, false, KEY_DOWN) => self.view.step(true),
            (false, true, KEY_UP) => self.view.extend(false),
            (false, true, KEY_DOWN) => self.view.extend(true),
            _ => return false,
        };
        self.apply(effect);
        self.sync_save_name();
        true
    }

    /// Loads the keyboard layout on first use, then shows the modal.
    pub(super) fn open_text_modal(&mut self, modal: Modal) {
        self.load_xkb();
        self.push_modal(modal);
    }

    /// Text entry needs the keyboard layout; it's compiled on first use.
    fn load_xkb(&mut self) {
        if self.xkb.is_none() {
            if let Some(keymap) = &self.keymap {
                self.xkb = Xkb::load(keymap, self.mods);
            }
        }
    }

    fn text_key(&mut self, key: u32) -> bool {
        let Some(xkb) = self.xkb.as_mut() else {
            return false;
        };
        let typed = xkb.key(key);
        let repeats = xkb.repeats(key);
        let input = match typed.sym {
            k::Return | k::KP_Enter => Input::Enter,
            k::Escape => Input::Escape,
            k::BackSpace => Input::Backspace { word: self.ctrl },
            k::Delete | k::KP_Delete => Input::Delete,
            k::Left | k::KP_Left => Input::Left,
            k::Right | k::KP_Right => Input::Right,
            k::Home | k::KP_Home => Input::Home,
            k::Up | k::KP_Up => Input::Up,
            k::Down | k::KP_Down => Input::Down,
            k::End | k::KP_End => Input::End,
            k::v | k::V if self.ctrl => {
                self.paste_text();
                return false;
            }
            _ if self.ctrl => return false,
            _ => match typed.text {
                Some(t) if t.chars().any(|c| !c.is_control()) => Input::Text(t),
                _ => return false,
            },
        };
        let repeat = repeats && !matches!(input, Input::Enter | Input::Escape);
        self.text_input(input);
        repeat
    }

    /// A text key goes to the modal's field, or else the dialog's name field.
    fn text_input(&mut self, input: Input) {
        if !self.modals.is_empty() {
            self.modal_key(input);
        } else if let Some(ch) = self.chooser.as_mut() {
            let action = ch.key(input);
            self.dialog_action(action);
        }
    }

    fn modal_key(&mut self, input: Input) {
        let Some(modal) = self.modals.front_mut() else {
            return;
        };
        match modal.key(input) {
            (_, Some(outcome)) => self.modal_outcome(outcome),
            (true, None) => self.request_redraw(),
            (false, None) => {}
        }
    }

    /// Reads the clipboard off-thread; the text arrives in `pasted`.
    fn paste_text(&mut self) {
        let Some(offer) = self
            .data_device
            .as_ref()
            .and_then(|d| d.data().selection_offer())
        else {
            return;
        };
        let mime = offer.with_mime_types(|types| {
            PASTE_TYPES
                .iter()
                .find(|t| types.iter().any(|m| m == *t))
                .map(|t| t.to_string())
        });
        let Some(Ok(pipe)) = mime.map(|m| offer.receive(m)) else {
            return;
        };
        // The source only starts writing once our request reaches the compositor.
        let _ = self.conn.flush();
        let tx = self.pastes.clone();
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.take(PASTE_MAX).read_to_end(&mut buf);
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
    }

    /// Names are one line, so only the clipboard's first line is inserted.
    pub(super) fn pasted(&mut self, text: String) {
        let line = text.lines().next().unwrap_or_default();
        let typing = match self.modals.front() {
            Some(modal) => modal.takes_text(),
            None => self.chooser.as_ref().is_some_and(|c| c.typing),
        };
        if !line.is_empty() && typing {
            self.text_input(Input::Text(line.to_string()));
        }
    }
}
