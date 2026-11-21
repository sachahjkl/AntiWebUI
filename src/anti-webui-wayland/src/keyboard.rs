//! xkbcommon keyboard handling.

use anti_webui_core::event::{Key, KeyEvent, Modifiers, NamedKey};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};
use xkbcommon::xkb;
use xkbcommon::xkb::keysyms as ks;

pub struct Keyboard {
    context: xkb::Context,
    state: Option<xkb::State>,
    pub modifiers: Modifiers,
    /// Characters per second and initial delay.
    pub repeat_rate: u32,
    pub repeat_delay: Duration,
    pub repeat: Option<Repeat>,
}

pub struct Repeat {
    pub keycode: u32,
    pub next: Instant,
}

impl Keyboard {
    pub fn new() -> Self {
        Self {
            context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            state: None,
            modifiers: Modifiers::default(),
            repeat_rate: 25,
            repeat_delay: Duration::from_millis(600),
            repeat: None,
        }
    }

    pub fn set_keymap(&mut self, fd: OwnedFd, size: u32) {
        // SAFETY: the compositor hands us a read-only keymap mapping of `size` bytes.
        let keymap = unsafe {
            xkb::Keymap::new_from_fd(
                &self.context,
                fd,
                size as usize,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        };
        if let Ok(Some(keymap)) = keymap {
            self.state = Some(xkb::State::new(&keymap));
        }
    }

    pub fn update_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        let Some(state) = &mut self.state else { return };
        state.update_mask(depressed, latched, locked, 0, 0, group);
        let active = |name: &str| state.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE);
        self.modifiers = Modifiers {
            shift: active(xkb::MOD_NAME_SHIFT),
            ctrl: active(xkb::MOD_NAME_CTRL),
            alt: active(xkb::MOD_NAME_ALT),
            logo: active(xkb::MOD_NAME_LOGO),
        };
    }

    /// Resets held modifiers when keyboard focus leaves so a key released while unfocused
    /// cannot stay stuck on subsequent pointer events.
    pub fn clear_modifiers(&mut self) {
        self.modifiers = Modifiers::default();
        if let Some(state) = &mut self.state {
            state.update_mask(0, 0, 0, 0, 0, 0);
        }
    }

    pub fn repeats(&self, keycode: u32) -> bool {
        self.state
            .as_ref()
            .is_some_and(|s| s.get_keymap().key_repeats(xkb::Keycode::new(keycode + 8)))
    }

    /// Translates an evdev keycode into a key event.
    pub fn key_event(&self, keycode: u32, pressed: bool, repeat: bool) -> Option<KeyEvent> {
        let state = self.state.as_ref()?;
        let code = xkb::Keycode::new(keycode + 8);
        let sym = state.key_get_one_sym(code);
        let key = map_keysym(sym);
        let text = if pressed && !self.modifiers.ctrl && !self.modifiers.alt && !self.modifiers.logo
        {
            let t = state.key_get_utf8(code);
            (!t.is_empty() && !t.chars().any(char::is_control)).then_some(t)
        } else {
            None
        };
        Some(KeyEvent {
            key,
            text,
            modifiers: self.modifiers,
            pressed,
            repeat,
        })
    }
}

fn map_keysym(sym: xkb::Keysym) -> Key {
    let named = match sym.raw() {
        ks::KEY_Return | ks::KEY_KP_Enter => NamedKey::Enter,
        ks::KEY_Tab | ks::KEY_ISO_Left_Tab => NamedKey::Tab,
        ks::KEY_space => NamedKey::Space,
        ks::KEY_BackSpace => NamedKey::Backspace,
        ks::KEY_Delete | ks::KEY_KP_Delete => NamedKey::Delete,
        ks::KEY_Escape => NamedKey::Escape,
        ks::KEY_Left | ks::KEY_KP_Left => NamedKey::ArrowLeft,
        ks::KEY_Right | ks::KEY_KP_Right => NamedKey::ArrowRight,
        ks::KEY_Up | ks::KEY_KP_Up => NamedKey::ArrowUp,
        ks::KEY_Down | ks::KEY_KP_Down => NamedKey::ArrowDown,
        ks::KEY_Home | ks::KEY_KP_Home => NamedKey::Home,
        ks::KEY_End | ks::KEY_KP_End => NamedKey::End,
        ks::KEY_Page_Up | ks::KEY_KP_Page_Up => NamedKey::PageUp,
        ks::KEY_Page_Down | ks::KEY_KP_Page_Down => NamedKey::PageDown,
        ks::KEY_Insert | ks::KEY_KP_Insert => NamedKey::Insert,
        ks::KEY_Shift_L | ks::KEY_Shift_R => NamedKey::Shift,
        ks::KEY_Control_L | ks::KEY_Control_R => NamedKey::Control,
        ks::KEY_Alt_L | ks::KEY_Alt_R => NamedKey::Alt,
        ks::KEY_Super_L | ks::KEY_Super_R => NamedKey::Super,
        ks::KEY_Caps_Lock => NamedKey::CapsLock,
        s @ ks::KEY_F1..=ks::KEY_F24 => NamedKey::F((s - ks::KEY_F1 + 1) as u8),
        _ => {
            let c = xkb::keysym_to_utf32(sym);
            return match char::from_u32(c).filter(|c| *c != '\0' && !c.is_control()) {
                Some(c) => Key::Character(c.to_string()),
                None => Key::Unidentified,
            };
        }
    };
    Key::Named(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_modifiers_resets_active_modifiers() {
        let mut kb = Keyboard::new();
        let keymap = xkb::Keymap::new_from_names(
            &kb.context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("default us keymap");
        let shift_bit = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let ctrl_bit = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_CTRL);
        kb.state = Some(xkb::State::new(&keymap));

        kb.update_modifiers(shift_bit | ctrl_bit, 0, 0, 0);
        assert!(kb.modifiers.shift);
        assert!(kb.modifiers.ctrl);

        kb.clear_modifiers();
        assert_eq!(kb.modifiers, Modifiers::default());
    }
}
