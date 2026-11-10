//! Keep physical key lifetimes while using SDL text events for layout/IME.
use sdl2::{
    event::Event,
    keyboard::{Keycode, Mod, Scancode},
};
use std::collections::HashMap;

#[derive(Default)]
pub struct Keyboard {
    held: HashMap<Scancode, u32>,
    pending: Option<Scancode>,
    ignore_repeat_text: bool,
}
impl Keyboard {
    pub fn clear(&mut self) {
        self.held.clear();
        self.pending = None;
        self.ignore_repeat_text = false;
    }
    pub fn event(&mut self, event: &Event) -> Vec<String> {
        let mut commands = Vec::new();
        match event {
            Event::KeyDown { repeat: true, .. } => self.ignore_repeat_text = true,
            Event::KeyDown {
                keycode: Some(key),
                scancode: Some(scan),
                keymod,
                repeat: false,
                ..
            } => {
                self.ignore_repeat_text = false;
                if self.held.contains_key(scan) {
                    return commands;
                }
                let shortcut = keymod.intersects(
                    Mod::LCTRLMOD | Mod::RCTRLMOD | Mod::LALTMOD | Mod::LGUIMOD | Mod::RGUIMOD,
                ) && !keymod.intersects(Mod::RALTMOD);
                if let Some(sym) = keysym(*key, shortcut) {
                    self.held.insert(*scan, sym);
                    commands.push(format!("k,{sym},1"));
                    self.pending = None;
                } else {
                    self.pending = Some(*scan);
                }
            }
            Event::KeyUp {
                scancode: Some(scan),
                ..
            } => {
                if self.pending == Some(*scan) {
                    self.pending = None;
                }
                if let Some(sym) = self.held.remove(scan) {
                    commands.push(format!("k,{sym},0"));
                }
            }
            Event::TextInput { text, .. } => {
                if self.ignore_repeat_text {
                    self.ignore_repeat_text = false;
                    return commands;
                }
                let mut chars = text.chars();
                let first = chars.next();
                let single = chars.next().is_none();
                if let (Some(c), Some(scan), true) = (first, self.pending.take(), single) {
                    let sym = unicode(c);
                    self.held.insert(scan, sym);
                    commands.push(format!("k,{sym},1"));
                } else {
                    // IME commits and pasted text can contain several characters
                    // and need not correspond to a currently held physical key.
                    for c in text.chars() {
                        let sym = unicode(c);
                        commands.push(format!("k,{sym},1"));
                        commands.push(format!("k,{sym},0"));
                    }
                }
            }
            _ => {}
        }
        commands
    }
}
fn unicode(c: char) -> u32 {
    if c as u32 <= 255 {
        c as u32
    } else {
        0x01000000 | c as u32
    }
}
fn keysym(k: Keycode, shortcut: bool) -> Option<u32> {
    Some(match k {
        Keycode::Backspace => 0xff08,
        Keycode::Tab => 0xff09,
        Keycode::Return => 0xff0d,
        Keycode::Escape => 0xff1b,
        Keycode::Delete => 0xffff,
        Keycode::Home => 0xff50,
        Keycode::Left => 0xff51,
        Keycode::Up => 0xff52,
        Keycode::Right => 0xff53,
        Keycode::Down => 0xff54,
        Keycode::PageUp => 0xff55,
        Keycode::PageDown => 0xff56,
        Keycode::End => 0xff57,
        Keycode::Insert => 0xff63,
        Keycode::F1 => 0xffbe,
        Keycode::F2 => 0xffbf,
        Keycode::F3 => 0xffc0,
        Keycode::F4 => 0xffc1,
        Keycode::F5 => 0xffc2,
        Keycode::F6 => 0xffc3,
        Keycode::F7 => 0xffc4,
        Keycode::F8 => 0xffc5,
        Keycode::F9 => 0xffc6,
        Keycode::F10 => 0xffc7,
        Keycode::F11 => 0xffc8,
        Keycode::F12 => 0xffc9,
        Keycode::F13 => 0xffca,
        Keycode::F14 => 0xffcb,
        Keycode::F15 => 0xffcc,
        Keycode::F16 => 0xffcd,
        Keycode::F17 => 0xffce,
        Keycode::F18 => 0xffcf,
        Keycode::F19 => 0xffd0,
        Keycode::F20 => 0xffd1,
        Keycode::F21 => 0xffd2,
        Keycode::F22 => 0xffd3,
        Keycode::F23 => 0xffd4,
        Keycode::F24 => 0xffd5,
        Keycode::NumLockClear => 0xff7f,
        Keycode::ScrollLock => 0xff14,
        Keycode::Pause => 0xff13,
        Keycode::PrintScreen => 0xff61,
        Keycode::Application => 0xff67,
        Keycode::KpEnter => 0xff8d,
        Keycode::Kp0 => 0xffb0,
        Keycode::Kp1 => 0xffb1,
        Keycode::Kp2 => 0xffb2,
        Keycode::Kp3 => 0xffb3,
        Keycode::Kp4 => 0xffb4,
        Keycode::Kp5 => 0xffb5,
        Keycode::Kp6 => 0xffb6,
        Keycode::Kp7 => 0xffb7,
        Keycode::Kp8 => 0xffb8,
        Keycode::Kp9 => 0xffb9,
        Keycode::KpPeriod => 0xffae,
        Keycode::KpDivide => 0xffaf,
        Keycode::KpMultiply => 0xffaa,
        Keycode::KpMinus => 0xffad,
        Keycode::KpPlus => 0xffab,
        Keycode::LShift => 0xffe1,
        Keycode::RShift => 0xffe2,
        Keycode::LCtrl => 0xffe3,
        Keycode::RCtrl => 0xffe4,
        Keycode::CapsLock => 0xffe5,
        Keycode::LAlt => 0xffe9,
        Keycode::RAlt => 0xffea,
        Keycode::LGui => 0xffeb,
        Keycode::RGui => 0xffec,
        _ => {
            let raw = i32::from(k);
            if shortcut && (32..=126).contains(&raw) {
                raw as u32
            } else {
                return None;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(down: bool, key: Keycode, scan: Scancode, keymod: Mod) -> Event {
        if down {
            Event::KeyDown {
                timestamp: 0,
                window_id: 1,
                keycode: Some(key),
                scancode: Some(scan),
                keymod,
                repeat: false,
            }
        } else {
            Event::KeyUp {
                timestamp: 0,
                window_id: 1,
                keycode: Some(key),
                scancode: Some(scan),
                keymod,
                repeat: false,
            }
        }
    }
    fn text(s: &str) -> Event {
        Event::TextInput {
            timestamp: 0,
            window_id: 1,
            text: s.into(),
        }
    }
    #[test]
    fn printable_keys_stay_held_until_physical_release() {
        let mut k = Keyboard::default();
        assert!(k
            .event(&key(true, Keycode::A, Scancode::A, Mod::LSHIFTMOD))
            .is_empty());
        assert_eq!(k.event(&text("A")), ["k,65,1"]);
        // Releasing Shift first must not change the symbol used for key-up.
        assert_eq!(
            k.event(&key(false, Keycode::A, Scancode::A, Mod::NOMOD)),
            ["k,65,0"]
        );
    }
    #[test]
    fn shortcut_release_survives_modifier_change_and_ime_commits() {
        let mut k = Keyboard::default();
        assert_eq!(
            k.event(&key(true, Keycode::A, Scancode::A, Mod::LCTRLMOD)),
            ["k,97,1"]
        );
        assert_eq!(
            k.event(&key(false, Keycode::A, Scancode::A, Mod::NOMOD)),
            ["k,97,0"]
        );
        let commands = k.event(&text("日本"));
        assert_eq!(commands.len(), 4);
        assert!(commands[0].ends_with(",1") && commands[1].ends_with(",0"));
        assert!(k.held.is_empty());
    }
}
