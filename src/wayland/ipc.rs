//! Small wire client for Aizome's public, same-user Unix-socket IPC. Keeping the
//! wire boundary here avoids importing the compositor's Cargo workspace.

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub struct Client(BufReader<UnixStream>);

#[derive(Clone, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
#[derive(Clone, Deserialize)]
pub struct Output {
    pub name: String,
    pub logical: Rect,
    pub enabled: bool,
}

impl Client {
    pub fn connect() -> Result<Self, String> {
        let path = super::settings()
            .socket
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("AIZOME_SOCKET").map(PathBuf::from))
            .or_else(|| {
                let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
                let display = std::env::var("WAYLAND_DISPLAY").ok()?;
                let name = std::path::Path::new(&display).file_name()?.to_str()?;
                Some(PathBuf::from(runtime).join(format!("aizome.{name}.sock")))
            })
            .ok_or("set AIZOME_SOCKET or --aizome-socket for native input")?;
        let stream = UnixStream::connect(&path)
            .map_err(|e| format!("Aizome IPC {}: {e}", path.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        let mut me = Self(BufReader::new(stream));
        let version = me.request(json!({"request": "version"}))?;
        if version["response"] != "version" {
            return Err("not an Aizome IPC socket".into());
        }
        Ok(me)
    }

    pub fn request(&mut self, value: Value) -> Result<Value, String> {
        let stream = self.0.get_mut();
        serde_json::to_writer(&mut *stream, &value).map_err(|e| e.to_string())?;
        stream.write_all(b"\n").map_err(|e| e.to_string())?;
        let mut line = Vec::new();
        // Bound even a malfunctioning peer's response before parsing JSON.
        use std::io::Read;
        self.0
            .by_ref()
            .take(1024 * 1024)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if line.last() != Some(&b'\n') {
            return Err("Aizome IPC closed or response too large".into());
        }
        let response: Value = serde_json::from_slice(&line).map_err(|e| e.to_string())?;
        if response["response"] == "error" {
            return Err(format!("Aizome: {}", response["message"]));
        }
        Ok(response)
    }

    pub fn output(&mut self, name: &str) -> Result<Output, String> {
        let response = self.request(json!({"request": "outputs"}))?;
        let outputs: Vec<Output> =
            serde_json::from_value(response["outputs"].clone()).map_err(|e| e.to_string())?;
        outputs
            .into_iter()
            .find(|o| o.enabled && o.name == name && o.logical.w > 0 && o.logical.h > 0)
            .ok_or_else(|| format!("captured output {name} is absent from Aizome's layout"))
    }

    pub fn input(&mut self, events: Vec<Value>) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let response = self.request(json!({"request": "synthesize", "events": events}))?;
        if response["response"] != "ok" {
            return Err(format!("unexpected input response: {response}"));
        }
        Ok(())
    }
}
