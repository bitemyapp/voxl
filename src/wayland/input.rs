//! Native Aizome input. Pointer coordinates are mapped from capture pixels to the
//! selected output's logical rectangle. Keysyms use the compositor's actual XKB
//! keymap; rare text symbols absent from it use a virtual keyboard, without
//! changing the compositor's physical keyboard layout.

use super::ipc::{Client, Output};
use crate::input::InputEvent;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as manager, zwp_virtual_keyboard_v1 as keyboard,
};
use xkbcommon::xkb;

static OUTPUT: Mutex<Option<(String, u32, u32)>> = Mutex::new(None);
pub fn set_output(name: String, w: u32, h: u32) {
    *OUTPUT.lock() = Some((name, w, h));
}

#[derive(Default)]
struct Heard {
    keymap: Option<xkb::Keymap>,
    state: Option<xkb::State>,
}

struct Input {
    ipc: Client,
    conn: Connection,
    queue: EventQueue<Heard>,
    heard: Heard,
    virtual_keyboard: keyboard::ZwpVirtualKeyboardV1,
    held: HashMap<u32, u32>,
    buttons: HashSet<u32>,
    output: Output,
    output_checked: Instant,
    epoch: Instant,
    clipboard: Option<crate::desktop::ClipboardHandle>,
    px_per_notch: f64,
}

pub fn spawn(
    clipboard: Option<crate::desktop::ClipboardHandle>,
    px_per_notch: f64,
) -> Result<mpsc::Sender<InputEvent>, String> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("wayland-input".into())
        .spawn(move || {
            let mut input = match Input::new(clipboard, px_per_notch) {
                Ok(input) => {
                    let _ = ready_tx.send(Ok(()));
                    input
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            while let Ok(event) = rx.recv() {
                if matches!(event, InputEvent::Shutdown) {
                    break;
                }
                if let Err(e) = input.event(event) {
                    log::error!("Wayland input: {e}");
                    break;
                }
            }
            let _ = input.release_all();
        })
        .map_err(|e| e.to_string())?;
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|e| e.to_string())??;
    Ok(tx)
}

impl Input {
    fn new(
        clipboard: Option<crate::desktop::ClipboardHandle>,
        px_per_notch: f64,
    ) -> Result<Self, String> {
        let mut ipc = Client::connect()?;
        let name = OUTPUT
            .lock()
            .as_ref()
            .ok_or("capture output is not ready")?
            .0
            .clone();
        let output = ipc.output(&name)?;
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<Heard>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).map_err(|e| e.to_string())?;
        seat.get_keyboard(&qh, ());
        let manager: manager::ZwpVirtualKeyboardManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let virtual_keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
        let mut heard = Heard::default();
        queue.roundtrip(&mut heard).map_err(|e| e.to_string())?;
        if heard.keymap.is_none() {
            return Err("the compositor supplied no XKB keymap".into());
        }
        Ok(Self {
            ipc,
            conn,
            queue,
            heard,
            virtual_keyboard,
            held: HashMap::new(),
            buttons: HashSet::new(),
            output,
            output_checked: Instant::now(),
            epoch: Instant::now(),
            clipboard,
            px_per_notch,
        })
    }

    fn event(&mut self, event: InputEvent) -> Result<(), String> {
        super::dispatch(&self.conn, &mut self.queue, &mut self.heard, 0)?;
        match event {
            InputEvent::Move { x, y } => {
                let (name, w, h) = OUTPUT.lock().clone().ok_or("capture output unavailable")?;
                // Positions and scale can change without changing capture dimensions.
                if name != self.output.name
                    || self.output_checked.elapsed() >= Duration::from_millis(250)
                {
                    self.output = self.ipc.output(&name)?;
                    self.output_checked = Instant::now();
                }
                let (x, y) = map_pointer(x, y, w, h, &self.output.logical);
                self.ipc
                    .input(vec![json!({"type":"pointer-motion", "x":x, "y":y})])
            }
            InputEvent::Button { button, down } => {
                let code = match button {
                    1 => 272,
                    2 => 274,
                    3 => 273,
                    8 => 275,
                    9 => 276,
                    _ => return Ok(()),
                };
                if down {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                }
                self.ipc.input(vec![
                    json!({"type":"pointer-button", "button":code.to_string(), "pressed":down}),
                ])
            }
            InputEvent::Scroll { dx, dy } => {
                self.scroll(dx as f64 * self.px_per_notch, dy as f64 * self.px_per_notch)
            }
            InputEvent::ScrollPx { dx, dy } => self.scroll(dx, dy),
            InputEvent::Key { keysym, down } => self.key(keysym, down),
            InputEvent::Clipboard(content) => {
                if let Some(c) = &self.clipboard {
                    c.set_blocking(content);
                }
                Ok(())
            }
            InputEvent::ReleaseAll | InputEvent::Shutdown => self.release_all(),
        }
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        if !dx.is_finite() || !dy.is_finite() {
            return Ok(());
        }
        let (_, w, h) = OUTPUT.lock().clone().ok_or("capture output unavailable")?;
        self.ipc.input(vec![json!({"type":"pointer-scroll", "dx":dx.clamp(-10000.0,10000.0) * self.output.logical.w as f64 / w as f64,
            "dy":dy.clamp(-10000.0,10000.0) * self.output.logical.h as f64 / h as f64})])
    }

    fn key(&mut self, sym: u32, down: bool) -> Result<(), String> {
        if !down {
            if let Some(code) = self.held.remove(&sym) {
                return self.send_keys(vec![key_event(code, false)]);
            }
            return Ok(());
        }
        if self.held.contains_key(&sym) {
            return Ok(());
        } // Wayland clients repeat held keys.
        let map = self.heard.keymap.as_ref().ok_or("no keymap")?;
        let state = self.heard.state.as_ref().ok_or("no keyboard state")?;
        let shift_mask = 1u32
            .checked_shl(map.mod_get_index(xkb::MOD_NAME_SHIFT))
            .unwrap_or(0);
        let depressed = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        let mut alternate = xkb::State::new(map);
        alternate.update_mask(
            depressed ^ shift_mask,
            state.serialize_mods(xkb::STATE_MODS_LATCHED),
            state.serialize_mods(xkb::STATE_MODS_LOCKED),
            0,
            0,
            state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        );
        let mut found = None;
        // Resolve with the actual modifier state, including Caps Lock and AltGr.
        // Only try changing Shift if the symbol isn't reachable as held.
        'search: for (candidate, adjusted) in [(state, false), (&alternate, true)] {
            for raw in map.min_keycode().raw()..=map.max_keycode().raw() {
                let code = xkb::Keycode::new(raw);
                if candidate.key_get_one_sym(code).raw() == sym {
                    found = Some((raw.saturating_sub(8), adjusted));
                    break 'search;
                }
            }
        }
        let Some((code, adjusted)) = found else {
            // IME/Unicode text has no physical key in many layouts. Send that
            // symbol through a private virtual keyboard; never remap real keys.
            if xkb::keysym_to_utf32(xkb::Keysym::new(sym)) >= 0x20
                && !self
                    .held
                    .keys()
                    .any(|k| matches!(k, 0xffe3 | 0xffe4 | 0xffe9 | 0xffea | 0xffeb | 0xffec))
            {
                return self.text_symbol(sym);
            }
            log::warn!("keysym {sym:#x} is absent from the compositor keymap");
            return Ok(());
        };
        let shifts: Vec<u32> = self
            .held
            .iter()
            .filter(|(s, _)| matches!(**s, 0xffe1 | 0xffe2))
            .map(|(_, &c)| c)
            .collect();
        let mut events = Vec::new();
        if adjusted && shifts.is_empty() {
            events.push(key_event(42, true));
        }
        if adjusted && !shifts.is_empty() {
            events.extend(shifts.iter().map(|&c| key_event(c, false)));
        }
        events.push(key_event(code, true));
        // A temporary Shift must cover both edges; otherwise client key repeat
        // would change case. These adjusted symbols are taps, as IME text is.
        if adjusted {
            events.push(key_event(code, false));
        } else {
            self.held.insert(sym, code);
        }
        if adjusted && shifts.is_empty() {
            events.push(key_event(42, false));
        }
        if adjusted && !shifts.is_empty() {
            events.extend(shifts.iter().map(|&c| key_event(c, true)));
        }
        self.send_keys(events)
    }

    fn send_keys(&mut self, events: Vec<Value>) -> Result<(), String> {
        self.ipc.input(events.clone())?;
        if let Some(state) = &mut self.heard.state {
            for event in events.iter().filter(|e| e["type"] == "key") {
                if let Some(code) = event["key"].as_str().and_then(|s| s.parse::<u32>().ok()) {
                    state.update_key(
                        xkb::Keycode::new(code + 8),
                        if event["pressed"] == true {
                            xkb::KeyDirection::Down
                        } else {
                            xkb::KeyDirection::Up
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn text_symbol(&mut self, sym: u32) -> Result<(), String> {
        let keymap = format!("xkb_keymap {{ xkb_keycodes {{ minimum=8; maximum=255; <VTXT>=10; }}; xkb_types {{ type \"ONE_LEVEL\" {{ modifiers=None; map[None]=Level1; level_name[Level1]=\"Any\"; }}; }}; xkb_compatibility {{}}; xkb_symbols {{ key <VTXT> {{ type=\"ONE_LEVEL\", [ 0x{sym:08x} ] }}; }}; }};\0");
        let mut file = super::capture::memory_file(c"voxl-keymap", keymap.len())?;
        file.write_all(keymap.as_bytes())
            .map_err(|e| e.to_string())?;
        self.virtual_keyboard
            .keymap(1, file.as_fd(), keymap.len() as u32);
        self.virtual_keyboard.modifiers(0, 0, 0, 0);
        let time = self.epoch.elapsed().as_millis() as u32;
        self.virtual_keyboard.key(time, 2, 1);
        self.virtual_keyboard.key(time, 2, 0);
        // Ensure the native compositor receives this before subsequent IPC keys.
        self.queue
            .roundtrip(&mut self.heard)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn release_all(&mut self) -> Result<(), String> {
        let mut events: Vec<Value> = self
            .held
            .drain()
            .map(|(_, code)| key_event(code, false))
            .collect();
        events.extend(self.buttons.drain().map(
            |code| json!({"type":"pointer-button", "button":code.to_string(), "pressed":false}),
        ));
        self.send_keys(events)
    }
}

fn key_event(code: u32, down: bool) -> Value {
    json!({"type":"key", "key":code.to_string(), "pressed":down})
}
fn map_pointer(x: i32, y: i32, w: u32, h: u32, r: &super::ipc::Rect) -> (f64, f64) {
    (
        r.x as f64 + x.clamp(0, w.saturating_sub(1) as i32) as f64 * r.w as f64 / w.max(1) as f64,
        r.y as f64 + y.clamp(0, h.saturating_sub(1) as i32) as f64 * r.h as f64 / h.max(1) as f64,
    )
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Heard {
    fn event(
        h: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        e: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = e
        {
            if size > 16 * 1024 * 1024 {
                return;
            }
            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            // Safety: protocol supplies a keymap fd of size bytes, consumed here.
            if let Ok(Some(map)) = unsafe {
                xkb::Keymap::new_from_fd(
                    &context,
                    fd,
                    size as usize,
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                )
            } {
                // Aizome forwards a virtual keyboard's map to clients too. It is
                // not the physical layout used by IPC key events; keep the last
                // real layout when our one-symbol text map comes back to us.
                if map.key_by_name("VTXT").is_none() {
                    let mut state = xkb::State::new(&map);
                    if let Some(old) = &h.state {
                        state.update_mask(
                            old.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                            old.serialize_mods(xkb::STATE_MODS_LATCHED),
                            old.serialize_mods(xkb::STATE_MODS_LOCKED),
                            0,
                            0,
                            old.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
                        );
                    }
                    h.state = Some(state);
                    h.keymap = Some(map);
                }
            }
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Heard {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Heard: ignore wl_seat::WlSeat);
delegate_noop!(Heard: ignore manager::ZwpVirtualKeyboardManagerV1);
delegate_noop!(Heard: ignore keyboard::ZwpVirtualKeyboardV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_maps_pixels_to_scaled_offset_output() {
        let r = super::super::ipc::Rect {
            x: -960,
            y: 100,
            w: 960,
            h: 540,
        };
        assert_eq!(map_pointer(960, 540, 1920, 1080, &r), (-480.0, 370.0));
        assert_eq!(map_pointer(-10, -10, 1920, 1080, &r), (-960.0, 100.0));
        assert_eq!(map_pointer(9999, 9999, 1920, 1080, &r), (-0.5, 639.5));
    }
}
