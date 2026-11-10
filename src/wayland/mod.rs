//! Aizome's native Wayland backend. The server, codec and browser protocol stay
//! shared with X11, providing a baseline for a future native Remote Desks viewer.

pub mod capture;
pub mod clipboard;
pub mod input;
mod ipc;

use parking_lot::Mutex;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use wayland_client::{Connection, EventQueue};

#[derive(Clone, Default)]
pub struct Settings {
    pub enabled: bool,
    pub output: Option<String>,
    pub socket: Option<String>,
    pub cursor: bool,
}
static SETTINGS: Mutex<Settings> = Mutex::new(Settings {
    enabled: false,
    output: None,
    socket: None,
    cursor: false,
});

pub fn init(args: &HashMap<String, String>) -> Result<bool, String> {
    let enabled = select_backend(
        args.get("capture-backend")
            .map(String::as_str)
            .unwrap_or("auto"),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        args.contains_key("display"),
    )?;
    *SETTINGS.lock() = Settings {
        enabled,
        output: args.get("wayland-output").cloned(),
        socket: args.get("aizome-socket").cloned(),
        cursor: crate::truthy(
            args.get("show-cursor"),
            crate::truthy(args.get("read-only"), false),
        ),
    };
    Ok(enabled)
}

fn select_backend(name: &str, wayland: bool, explicit_x11: bool) -> Result<bool, String> {
    match name {
        "auto" => Ok(wayland && !explicit_x11),
        "wayland" => Ok(true),
        "x11" => Ok(false),
        _ => Err(format!(
            "unknown --capture-backend={name}; use auto, x11 or wayland"
        )),
    }
}

pub fn enabled() -> bool {
    SETTINGS.lock().enabled
}
pub fn settings() -> Settings {
    SETTINGS.lock().clone()
}

/// Read available events without blocking the encoder's pacing loop. A prepared
/// read is cancelled when the socket isn't ready; no round-trip per frame.
pub fn dispatch<S: 'static>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
    timeout: i32,
) -> Result<(), String> {
    queue.dispatch_pending(state).map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    if let Some(guard) = queue.prepare_read() {
        let mut p = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // Safety: one initialized pollfd, borrowed from the live connection.
        let ready = unsafe { libc::poll(&mut p, 1, timeout) };
        if ready < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.to_string());
            }
        } else if ready > 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    queue.dispatch_pending(state).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_backend_wins_over_environment() {
        assert!(select_backend("auto", true, false).unwrap());
        assert!(!select_backend("auto", true, true).unwrap());
        assert!(!select_backend("auto", false, false).unwrap());
        assert!(!select_backend("x11", true, false).unwrap());
        assert!(select_backend("wayland", false, true).unwrap());
        assert!(select_backend("typo", true, false).is_err());
    }
}
