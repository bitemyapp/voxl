//! Optional Aizome retained paint. An independent IPC subscriber is bound to the
//! already authorized capture object's id and this process's Unix credentials.
use crate::hub::{Client, Hub, Out};
use bytes::Bytes;
use parking_lot::Mutex;
use serde_json::json;
use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq)]
struct Identity {
    epoch: u64,
    object: u32,
    output: String,
}
static IDENTITY: Mutex<Option<Identity>> = Mutex::new(None);
static NEXT: AtomicU64 = AtomicU64::new(1);
pub struct Registration(u64);
pub fn register(object: u32, output: String) -> Registration {
    let epoch = NEXT.fetch_add(1, Ordering::Relaxed);
    *IDENTITY.lock() = Some(Identity {
        epoch,
        object,
        output,
    });
    Registration(epoch)
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut current = IDENTITY.lock();
        if current.as_ref().is_some_and(|c| c.epoch == self.0) {
            *current = None;
        }
    }
}
pub fn command(hub: &Arc<Hub>, client: &Arc<Client>, enabled: bool, nonce: u32, compressed: bool) {
    // At most one worker per connection, even if a peer toggles rapidly. Its
    // one-slot notification coalesces changes; the atomics hold the latest one.
    let mut notify = client.scene_notify.lock();
    client.scene_nonce.store(nonce, Ordering::Release);
    client.scene_compressed.store(compressed, Ordering::Release);
    client.scene_ack.store(0, Ordering::Release);
    client.scene_written.store(0, Ordering::Release);
    client.scene_enabled.store(enabled, Ordering::Release);
    client.scene_generation.fetch_add(1, Ordering::AcqRel);
    if notify.is_none() && enabled {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let owner = client.clone();
        let hub = hub.clone();
        match std::thread::Builder::new()
            .name("aizome-retained".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    if hub.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let (generation, enabled, nonce, compressed) = {
                        let _guard = owner.scene_notify.lock();
                        (
                            owner.scene_generation.load(Ordering::Acquire),
                            owner.scene_enabled.load(Ordering::Acquire),
                            owner.scene_nonce.load(Ordering::Acquire),
                            owner.scene_compressed.load(Ordering::Acquire),
                        )
                    };
                    if !enabled {
                        continue;
                    }
                    if let Err(error) = run(&hub, &owner, generation, nonce, compressed) {
                        if owner.scene_generation.load(Ordering::Acquire) == generation {
                            log::info!("retained replay uses video: {error}");
                            fallback(&hub, &owner, nonce, &error);
                        }
                    }
                    if hub.stopping.load(Ordering::Acquire) {
                        break;
                    }
                }
            }) {
            Ok(_) => *notify = Some(tx),
            Err(error) => status(client, nonce, &format!("retained worker: {error}")),
        }
    }
    if let Some(tx) = notify.as_ref() {
        let _ = tx.try_send(());
    }
}

fn status(c: &Client, nonce: u32, reason: &str) {
    c.send(Out::Text(
        json!({"t":"scene", "nonce":nonce, "reason":reason}).to_string(),
    ));
}
fn fallback(hub: &Hub, c: &Client, nonce: u32, reason: &str) {
    c.video.store(true, Ordering::Relaxed);
    c.started.store(false, Ordering::Relaxed);
    c.needs_keyframe.store(true, Ordering::Relaxed);
    c.inflight.lock().clear();
    status(c, nonce, reason);
    hub.wake();
}
fn run(hub: &Hub, c: &Client, generation: u64, nonce: u32, compressed: bool) -> Result<(), String> {
    let identity = IDENTITY
        .lock()
        .clone()
        .ok_or("native Aizome capture unavailable")?;
    log::info!(
        "retained capture object={} output={}",
        identity.object,
        identity.output
    );
    let mut ipc = super::ipc::Client::connect()?;
    ipc.request(json!({"request":"subscribe"}))?;
    // No events can precede the acknowledgement until the compositor processes
    // its next loop iteration. The normal request reader preserves its buffer.
    ipc.request(json!({"request":"remote-scene", "capture":identity.object, "output":identity.output, "enabled":true}))?;
    let (mut stream, mut pending) = ipc.into_stream();
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let mut outstanding: VecDeque<(u32, usize, Instant)> = VecDeque::new();
    let mut active = false;
    let mut serial = 0u32;
    let mut codec = crate::retained::Encoder::new(compressed);
    loop {
        if c.scene_generation.load(Ordering::Acquire) != generation
            || hub.stopping.load(Ordering::Acquire)
        {
            return Ok(());
        }
        if IDENTITY.lock().as_ref() != Some(&identity) {
            return Err("capture changed".into());
        }
        let ack = c.scene_ack.load(Ordering::Acquire);
        let written = c.scene_written.load(Ordering::Acquire);
        if written >> 32 == u64::from(nonce) && (written as u32).wrapping_sub(ack) as i32 >= 0 {
            if let Some(index) = outstanding.iter().position(|(s, _, _)| *s == ack) {
                outstanding.drain(..=index);
            }
        }
        if outstanding
            .front()
            .is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(2))
        {
            return Err("retained viewer acknowledgement timed out".into());
        }
        let mut buf = [0u8; 65536];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return Err("Aizome state stream closed".into()),
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    if pending.len() > 32 * 1024 * 1024 {
                        return Err("retained IPC backlog exceeded".into());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
        while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<_> = pending.drain(..=nl).collect();
            let event: serde_json::Value =
                serde_json::from_slice(&line).map_err(|e| e.to_string())?;
            if event["event"] != "remote-scene"
                || event["capture"].as_u64() != Some(u64::from(identity.object))
            {
                continue;
            }
            if let Some(payload) = event["payload"].as_str() {
                if payload.len() > 16 * 1024 * 1024 {
                    return Err("retained frame exceeds limit".into());
                }
                #[derive(serde::Deserialize)]
                struct Header {
                    full: bool,
                }
                let header: Header = serde_json::from_str(payload).map_err(|e| e.to_string())?;
                if !active && !header.full {
                    continue;
                }
                if outstanding.len() >= 8
                    || outstanding.iter().map(|(_, n, _)| n).sum::<usize>() + payload.len()
                        > 32 * 1024 * 1024
                {
                    return Err("retained viewer fell behind; video fallback".into());
                }
                serial = serial.wrapping_add(1);
                let packet = codec.packet(nonce, serial, payload.as_bytes())?;
                outstanding.push_back((serial, payload.len(), Instant::now()));
                let bytes = packet.len();
                c.send_scene(Bytes::from(packet), nonce, serial)?;
                hub.stats.retained_frames.fetch_add(1, Ordering::Relaxed);
                hub.stats
                    .retained_raw_bytes
                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                hub.stats
                    .retained_bytes
                    .fetch_add(bytes as u64, Ordering::Relaxed);
                active = true;
            } else {
                let reason = event["reason"].as_str().unwrap_or("scene requires pixels");
                fallback(hub, c, nonce, reason);
                active = false;
                if reason == "capture-stopped" {
                    return Ok(());
                }
            }
        }
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // A bounded wait observes cancellation and stalled acknowledgements even
        // when the shared scene is idle; no busy polling or unbounded queue.
        unsafe {
            libc::poll(&mut poll, 1, 200);
        }
    }
}
