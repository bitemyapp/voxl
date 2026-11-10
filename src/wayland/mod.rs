//! Aizome's native Wayland backend. The server, codec and browser protocol stay
//! shared with X11, providing a baseline for a future native Remote Desks viewer.

pub mod capture;
pub mod clipboard;
#[cfg(feature = "gpu")]
pub mod gpu_capture;
pub mod input;
mod ipc;
pub mod scene;

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
    /// Drawing assets can contain more data than the visible pixel image.
    /// Only the source owner can authorize that richer sharing capability.
    pub native_widgets: bool,
    pub video_backend: String,
    // Parse and validate the same options in CPU-only builds as GPU builds.
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    pub gpu_preset: u32,
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    pub gpu_quality: f32,
}
static SETTINGS: Mutex<Settings> = Mutex::new(Settings {
    enabled: false,
    output: None,
    socket: None,
    cursor: false,
    native_widgets: false,
    video_backend: String::new(),
    gpu_preset: 1,
    gpu_quality: 24.0,
});

pub fn init(args: &HashMap<String, String>) -> Result<bool, String> {
    let video_backend = args
        .get("video-backend")
        .map(String::as_str)
        .unwrap_or("cpu");
    if !matches!(video_backend, "cpu" | "gpu" | "auto") {
        return Err("--video-backend must be cpu, gpu or auto".into());
    }
    let gpu_preset = args
        .get("gpu-preset")
        .map(String::as_str)
        .unwrap_or("1")
        .trim_start_matches(['P', 'p'])
        .parse::<u32>()
        .ok()
        .filter(|p| (1..=7).contains(p))
        .ok_or("--gpu-preset must be P1 through P7")?;
    let gpu_quality = args
        .get("gpu-quality")
        .map(String::as_str)
        .unwrap_or("24")
        .parse::<f32>()
        .ok()
        .filter(|q| q.is_finite() && (1.0..=51.0).contains(q))
        .ok_or("--gpu-quality must be between 1 and 51 (lower is higher quality)")?;
    let enabled = select_backend(
        args.get("capture-backend")
            .map(String::as_str)
            .unwrap_or("auto"),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        args.contains_key("display"),
    )?;
    *SETTINGS.lock() = Settings {
        enabled,
        video_backend: video_backend.into(),
        gpu_preset,
        gpu_quality,
        native_widgets: crate::truthy(args.get("native-widgets"), false),
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
        match ready.cmp(&0) {
            std::cmp::Ordering::Less => {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e.to_string());
                }
            }
            std::cmp::Ordering::Greater => match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            },
            std::cmp::Ordering::Equal => {}
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
