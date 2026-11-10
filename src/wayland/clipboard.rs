//! Clipboard exchange through wlr-data-control, with bounded pipe I/O away from
//! the Wayland dispatch thread. No X11 clipboard or external command is involved.

use crate::desktop::{ClipContent, ClipboardMode};
use crate::hub::{Hub, Out};
use bytes::Bytes;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as device, zwlr_data_control_manager_v1 as manager,
    zwlr_data_control_offer_v1 as offer, zwlr_data_control_source_v1 as source,
};

const MAX_BYTES: usize = 15 << 20;
type Request = (ClipContent, mpsc::SyncSender<()>);

#[derive(Clone)]
pub struct Handle(mpsc::Sender<Request>);
impl Handle {
    pub fn set_blocking(&self, content: ClipContent) {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.0.send((content, tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
    }
}

#[derive(Default)]
struct State {
    offers:
        HashMap<wayland_client::backend::ObjectId, (offer::ZwlrDataControlOfferV1, Vec<String>)>,
    selection: Option<Option<offer::ZwlrDataControlOfferV1>>,
    finished: bool,
}

pub fn spawn(hub: Arc<Hub>, mode: ClipboardMode) -> Result<Handle, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).map_err(|e| e.to_string())?;
    let manager: manager::ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wlr-data-control clipboard: {e}"))?;
    let device = manager.get_data_device(&seat, &qh, ());
    let (tx, rx) = mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("wayland-clipboard".into())
        .spawn(move || {
            let mut state = State::default();
            let mut owned: Option<source::ZwlrDataControlSourceV1> = None;
            let (incoming_tx, incoming_rx) = mpsc::channel::<(u64, String, Vec<u8>)>();
            let mut generation = 0u64;
            let mut last = Vec::new();
            loop {
                loop {
                    match rx.try_recv() {
                        Ok((content, done)) => {
                            if mode.to_server {
                                let (mime, bytes) = match content {
                                    ClipContent::Text(s) => {
                                        ("text/plain;charset=utf-8", s.into_bytes())
                                    }
                                    ClipContent::Image(b) => ("image/png", b),
                                };
                                if bytes.len() <= MAX_BYTES {
                                    last.clone_from(&bytes);
                                    let source = manager.create_data_source(&qh, Arc::new(bytes));
                                    source.offer(mime.into());
                                    if mime.starts_with("text/") {
                                        source.offer("text/plain".into());
                                    }
                                    device.set_selection(Some(&source));
                                    if let Some(old) = owned.replace(source) {
                                        old.destroy();
                                    }
                                    // Ownership is acknowledged before the following paste key.
                                    if queue.roundtrip(&mut state).is_err() {
                                        return;
                                    }
                                }
                            }
                            let _ = done.send(());
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                if let Err(e) = super::dispatch(&conn, &mut queue, &mut state, 20) {
                    log::error!("Wayland clipboard: {e}");
                    return;
                }
                if state.finished {
                    return;
                }
                if let Some(selection) = state.selection.take() {
                    generation = generation.wrapping_add(1);
                    if let Some(selected) = &selection {
                        if mode.to_client {
                            let mimes = state
                                .offers
                                .get(&selected.id())
                                .map(|o| o.1.as_slice())
                                .unwrap_or_default();
                            let mime = [
                                "image/png",
                                "text/plain;charset=utf-8",
                                "text/plain",
                                "UTF8_STRING",
                            ]
                            .into_iter()
                            .find(|m| mimes.iter().any(|s| s == m));
                            if let Some(mime) = mime {
                                if let Ok((read, write)) = nix::unistd::pipe() {
                                    selected.receive(mime.into(), write.as_fd());
                                    drop(write);
                                    let _ = conn.flush();
                                    let tx = incoming_tx.clone();
                                    let mime = mime.to_string();
                                    std::thread::spawn(move || {
                                        if let Ok(data) = read_pipe(read) {
                                            let _ = tx.send((generation, mime, data));
                                        }
                                    });
                                }
                            }
                        }
                    }
                    // Offers cease to be useful after receive has marshalled its fd.
                    for (_, (offer, _)) in state.offers.drain() {
                        offer.destroy();
                    }
                }
                while let Ok((id, mime, data)) = incoming_rx.try_recv() {
                    if id != generation || data == last {
                        continue;
                    }
                    last.clone_from(&data);
                    if mime == "image/png" {
                        let mut message = Vec::with_capacity(data.len() + 1);
                        message.push(4); // Bin.ClipImage
                        message.extend_from_slice(&data);
                        let message = Bytes::from(message);
                        hub.broadcast(|| Out::Bin(message.clone()));
                    } else if let Ok(text) = String::from_utf8(data) {
                        *hub.last_clipboard.lock() = Some(text.clone());
                        let message = serde_json::json!({"t":"clip", "d":text}).to_string();
                        hub.broadcast(|| Out::Text(message.clone()));
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Handle(tx))
}

fn nonblocking(fd: &OwnedFd) -> std::io::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    fcntl(fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(Into::into)
}
fn ready(file: &File, events: i16, until: Instant) -> std::io::Result<()> {
    if Instant::now() >= until {
        return Err(std::io::ErrorKind::TimedOut.into());
    }
    let mut p = libc::pollfd {
        fd: file.as_raw_fd(),
        events,
        revents: 0,
    };
    // Safety: a valid descriptor owned by file and one initialized pollfd.
    let n = unsafe { libc::poll(&mut p, 1, 100) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn read_pipe(fd: OwnedFd) -> std::io::Result<Vec<u8>> {
    nonblocking(&fd)?;
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    let until = Instant::now() + Duration::from_secs(5);
    let mut buf = [0; 8192];
    loop {
        ready(&file, libc::POLLIN, until)?;
        match file.read(&mut buf) {
            Ok(0) => return Ok(bytes),
            Ok(n) if bytes.len() + n <= MAX_BYTES => bytes.extend_from_slice(&buf[..n]),
            Ok(_) => return Err(std::io::ErrorKind::InvalidData.into()),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
}
fn write_pipe(fd: OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    nonblocking(&fd)?;
    let mut file = File::from(fd);
    let until = Instant::now() + Duration::from_secs(5);
    let mut pos = 0;
    while pos < bytes.len() {
        ready(&file, libc::POLLOUT, until)?;
        match file.write(&bytes[pos..]) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => pos += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl Dispatch<device::ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &device::ZwlrDataControlDeviceV1,
        e: device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            device::Event::DataOffer { id } => {
                s.offers.insert(id.id(), (id, Vec::new()));
            }
            device::Event::Selection { id } => s.selection = Some(id),
            device::Event::Finished => s.finished = true,
            _ => {}
        }
    }
    event_created_child!(State, device::ZwlrDataControlDeviceV1, [0 => (offer::ZwlrDataControlOfferV1, ())]);
}
impl Dispatch<offer::ZwlrDataControlOfferV1, ()> for State {
    fn event(
        s: &mut Self,
        offer: &offer::ZwlrDataControlOfferV1,
        e: offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = e {
            if let Some(o) = s.offers.get_mut(&offer.id()) {
                o.1.push(mime_type);
            }
        }
    }
}
impl Dispatch<source::ZwlrDataControlSourceV1, Arc<Vec<u8>>> for State {
    fn event(
        _: &mut Self,
        _: &source::ZwlrDataControlSourceV1,
        e: source::Event,
        data: &Arc<Vec<u8>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let source::Event::Send { fd, .. } = e {
            let data = data.clone();
            std::thread::spawn(move || {
                let _ = write_pipe(fd, &data);
            });
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore manager::ZwlrDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transfers_larger_than_the_pipe_buffer_complete() {
        let (r, w) = nix::unistd::pipe().unwrap();
        let bytes = vec![71; 300_000];
        let expected = bytes.clone();
        let writer = std::thread::spawn(move || write_pipe(w, &bytes).unwrap());
        assert_eq!(read_pipe(r).unwrap(), expected);
        writer.join().unwrap();
    }
}
