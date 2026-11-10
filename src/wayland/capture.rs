//! ext-image-copy-capture-v1 into a reusable wl_shm buffer. Only one compositor
//! write is outstanding. Pixels are read after `ready`, never while it writes;
//! changed rows are copied to a stable image before asking for the next frame.

use crate::capture::{Band, DAMAGE_EVENTS};
use memmap2::MmapMut;
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd};
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1 as source, ext_output_image_capture_source_manager_v1 as sources,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as manager,
    ext_image_copy_capture_session_v1 as session,
};

#[derive(Default)]
struct Heard {
    outputs: Vec<(wl_output::WlOutput, String)>,
    size: (u32, u32),
    formats: Vec<wl_shm::Format>,
    constraints: u64,
    in_constraints: bool,
    stopped: bool,
    removed: Vec<u32>,
    ready: bool,
    failed: Option<String>,
    transform: Option<WEnum<wl_output::Transform>>,
    damage: Vec<(i32, i32, i32, i32)>,
}

struct Buffer {
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    map: MmapMut,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

pub fn memory_file(name: &std::ffi::CStr, len: usize) -> Result<File, String> {
    // Safety: a valid NUL-terminated name, no inherited descriptors.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // Safety: memfd_create returned a new descriptor owned by this File.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(len as u64).map_err(|e| e.to_string())?;
    Ok(file)
}

pub struct Capturer {
    conn: Connection,
    queue: EventQueue<Heard>,
    heard: Heard,
    session: session::ExtImageCopyCaptureSessionV1,
    source: source::ExtImageCaptureSourceV1,
    frame: Option<frame::ExtImageCopyCaptureFrameV1>,
    buffer: Buffer,
    output_id: u32,
    constraints: u64,
    pub width: u32,
    pub height: u32,
    pixels: Vec<u8>,
    pub damaged: bool,
    pub damaged_at: Option<Instant>,
    pub changes: Vec<(u32, u32, u32, u32)>,
    dirty: Vec<u8>,
    band: Option<Band>,
    rebuild: bool,
}

impl Capturer {
    pub fn new() -> Result<Self, String> {
        let settings = super::settings();
        let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connection: {e}"))?;
        let (globals, mut queue) =
            registry_queue_init::<Heard>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let manager: manager::ExtImageCopyCaptureManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("ext-image-copy-capture-v1 is required (Aizome): {e}"))?;
        let sources: sources::ExtOutputImageCaptureSourceManagerV1 =
            globals
                .bind(&qh, 1..=1, ())
                .map_err(|e| format!("ext-output-image-capture-source-v1 is required: {e}"))?;
        let mut heard = Heard::default();
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                let output = globals
                    .registry()
                    .bind(g.name, g.version.min(4), &qh, g.name);
                heard.outputs.push((output, String::new()));
            }
        });
        queue.roundtrip(&mut heard).map_err(|e| e.to_string())?;
        let (output, name) = heard
            .outputs
            .iter()
            .find(|(_, name)| settings.output.as_ref().is_none_or(|wanted| wanted == name))
            .ok_or_else(|| {
                format!(
                    "Wayland output {:?} not found; available: {}",
                    settings.output,
                    heard
                        .outputs
                        .iter()
                        .map(|o| o.1.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let output_id = *output.data::<u32>().ok_or("output registry id missing")?;
        let output_name = name.clone();
        let src = sources.create_source(output, &qh, ());
        let options = if settings.cursor {
            manager::Options::PaintCursors
        } else {
            manager::Options::empty()
        };
        let session = manager.create_session(&src, options, &qh, ());
        let deadline = Instant::now() + Duration::from_secs(5);
        while heard.constraints == 0 && !heard.stopped {
            if Instant::now() >= deadline {
                return Err("Wayland capture constraints timed out".into());
            }
            super::dispatch(&conn, &mut queue, &mut heard, 100)?;
        }
        if heard.stopped {
            return Err("Wayland capture was stopped by the compositor".into());
        }
        let (width, height) = heard.size;
        let len = image_len(width, height)?;
        if !heard.formats.contains(&wl_shm::Format::Xrgb8888) {
            return Err("Wayland capture requires wl_shm XRGB8888".into());
        }
        // x264 I420 requires even dimensions; never silently crop a display.
        if width % 2 != 0 || height % 2 != 0 {
            return Err(format!(
                "Wayland output must have even dimensions for H.264: {width}x{height}"
            ));
        }
        let file = memory_file(c"voxl-wayland-frame", len)?;
        // Safety: this fd belongs to us and has exactly len bytes; only the
        // compositor writes it and we read exclusively after a ready event.
        let map = unsafe { MmapMut::map_mut(&file) }.map_err(|e| e.to_string())?;
        let pool = shm.create_pool(file.as_fd(), len as i32, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            width as i32 * 4,
            wl_shm::Format::Xrgb8888,
            &qh,
            (),
        );
        let constraints = heard.constraints;
        let mut me = Self {
            conn,
            queue,
            heard,
            session,
            source: src,
            frame: None,
            buffer: Buffer { buffer, pool, map },
            output_id,
            constraints,
            width,
            height,
            pixels: vec![0; len],
            damaged: false,
            damaged_at: None,
            changes: Vec::new(),
            dirty: vec![0; width.div_ceil(16) as usize * height.div_ceil(16) as usize],
            band: None,
            rebuild: false,
        };
        me.request_frame();
        while !me.damaged {
            if Instant::now() >= deadline {
                return Err("first Wayland capture timed out".into());
            }
            super::dispatch(&me.conn, &mut me.queue, &mut me.heard, 100)?;
            me.pump()?;
        }
        super::input::set_output(output_name.clone(), width, height);
        log::info!("native Wayland capture: {output_name}, {width}x{height}, ext-image-copy-capture + wl_shm");
        Ok(me)
    }

    fn request_frame(&mut self) {
        let frame = self.session.create_frame(&self.queue.handle(), ());
        frame.attach_buffer(&self.buffer.buffer);
        // The compositor may replace any pixel; this is buffer validity, distinct
        // from the damage events telling us which scene pixels actually changed.
        frame.damage_buffer(0, 0, self.width as i32, self.height as i32);
        frame.capture();
        self.frame = Some(frame);
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.as_fd()
    }
    pub fn needs_rebuild(&self) -> bool {
        self.rebuild
    }

    pub fn pump(&mut self) -> Result<(), String> {
        super::dispatch(&self.conn, &mut self.queue, &mut self.heard, 0)?;
        if self.heard.stopped || self.heard.removed.contains(&self.output_id) {
            return Err("Wayland sharing stopped or the selected output was removed; restart voxl to share again".into());
        }
        if self.heard.constraints != self.constraints {
            self.rebuild = true;
            return Ok(());
        }
        if let Some(error) = self.heard.failed.take() {
            return Err(format!("Wayland capture failed: {error}"));
        }
        if !self.heard.ready {
            return Ok(());
        }
        if self.heard.transform != Some(WEnum::Value(wl_output::Transform::Normal)) {
            return Err(
                "capture must be upright (Aizome); this compositor returned a transformed buffer"
                    .into(),
            );
        }
        self.heard.ready = false;
        self.heard.transform = None;
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        let mut rects = std::mem::take(&mut self.heard.damage);
        if rects.is_empty() {
            rects.push((0, 0, self.width as i32, self.height as i32));
        }
        for (x, y, w, h) in rects {
            if let Some((x0, y0, x1, y1)) = clip_damage(x, y, w, h, self.width, self.height) {
                for row in y0..y1 {
                    let a = (row as usize * self.width as usize + x0 as usize) * 4;
                    let b = a + (x1 - x0) as usize * 4;
                    self.pixels[a..b].copy_from_slice(&self.buffer.map[a..b]);
                }
                let cols = self.width.div_ceil(16) as usize;
                for row in y0 / 16..y1.div_ceil(16) {
                    self.dirty[row as usize * cols + (x0 / 16) as usize
                        ..row as usize * cols + x1.div_ceil(16) as usize]
                        .fill(1);
                }
                let band = Band {
                    y0: y0 & !1,
                    y1: ((y1 + 1) & !1).min(self.height),
                };
                self.band = Some(match self.band {
                    Some(old) => Band {
                        y0: old.y0.min(band.y0),
                        y1: old.y1.max(band.y1),
                    },
                    None => band,
                });
                self.changes.push((x0, y0, x1, y1));
                self.damaged = true;
                self.damaged_at.get_or_insert_with(Instant::now);
                DAMAGE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.request_frame();
        self.conn.flush().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        self.damaged = false;
        self.damaged_at = None;
        Ok(self.band.take())
    }
    pub fn take_mb_map(&mut self) -> Vec<u8> {
        let len = self.dirty.len();
        std::mem::replace(&mut self.dirty, vec![0; len])
    }
    pub fn bgrx(&self) -> (&[u8], usize) {
        (&self.pixels, self.width as usize * 4)
    }
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        self.pump()?;
        Ok(self.pixels.clone())
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
        self.source.destroy();
        let _ = self.conn.flush();
    }
}

fn image_len(w: u32, h: u32) -> Result<usize, String> {
    let len = u64::from(w)
        .checked_mul(u64::from(h))
        .and_then(|n| n.checked_mul(4))
        .ok_or("capture dimensions overflow")?;
    if w < 2 || h < 2 || w > 16384 || h > 16384 || len > 256 * 1024 * 1024 {
        return Err(format!(
            "unsupported Wayland capture dimensions {w}x{h} (maximum 256 MiB)"
        ));
    }
    Ok(len as usize)
}

fn clip_damage(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    width: u32,
    height: u32,
) -> Option<(u32, u32, u32, u32)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let x0 = (x as i64).clamp(0, width as i64) as u32;
    let y0 = (y as i64).clamp(0, height as i64) as u32;
    let x1 = (x as i64 + w as i64).clamp(0, width as i64) as u32;
    let y1 = (y as i64 + h as i64).clamp(0, height as i64) as u32;
    (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Heard {
    fn event(
        h: &mut Self,
        _: &wl_registry::WlRegistry,
        e: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = e {
            h.removed.push(name);
        }
    }
}
impl Dispatch<wl_output::WlOutput, u32> for Heard {
    fn event(
        h: &mut Self,
        output: &wl_output::WlOutput,
        e: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = e {
            if let Some(o) = h.outputs.iter_mut().find(|o| o.0 == *output) {
                o.1 = name;
            }
        }
    }
}
impl Dispatch<session::ExtImageCopyCaptureSessionV1, ()> for Heard {
    fn event(
        h: &mut Self,
        _: &session::ExtImageCopyCaptureSessionV1,
        e: session::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(e, session::Event::Stopped) && !h.in_constraints {
            h.in_constraints = true;
            h.formats.clear();
        }
        match e {
            session::Event::BufferSize { width, height } => h.size = (width, height),
            session::Event::ShmFormat {
                format: WEnum::Value(format),
            } => h.formats.push(format),
            session::Event::Done => {
                h.constraints += 1;
                h.in_constraints = false;
            }
            session::Event::Stopped => h.stopped = true,
            _ => {}
        }
    }
}
impl Dispatch<frame::ExtImageCopyCaptureFrameV1, ()> for Heard {
    fn event(
        h: &mut Self,
        _: &frame::ExtImageCopyCaptureFrameV1,
        e: frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            frame::Event::Ready => h.ready = true,
            frame::Event::Failed { reason } => h.failed = Some(format!("{reason:?}")),
            frame::Event::Damage {
                x,
                y,
                width,
                height,
            } => h.damage.push((x, y, width, height)),
            frame::Event::Transform { transform } => h.transform = Some(transform),
            _ => {}
        }
    }
}
delegate_noop!(Heard: ignore wl_shm::WlShm);
delegate_noop!(Heard: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Heard: ignore wl_buffer::WlBuffer);
delegate_noop!(Heard: ignore manager::ExtImageCopyCaptureManagerV1);
delegate_noop!(Heard: ignore sources::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(Heard: ignore source::ExtImageCaptureSourceV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn damage_is_clipped_without_overflow() {
        assert_eq!(clip_damage(-5, -3, 10, 8, 100, 80), Some((0, 0, 5, 5)));
        assert_eq!(
            clip_damage(90, 70, i32::MAX, i32::MAX, 100, 80),
            Some((90, 70, 100, 80))
        );
        assert_eq!(clip_damage(200, 0, 10, 10, 100, 80), None);
        assert_eq!(clip_damage(0, 0, -1, 10, 100, 80), None);
    }
    #[test]
    fn dimensions_are_bounded_before_allocating() {
        assert_eq!(image_len(2560, 1600).unwrap(), 16_384_000);
        assert!(image_len(u32::MAX, u32::MAX).is_err());
        assert!(image_len(1, 100).is_err());
        assert!(image_len(16384, 16384).is_err());
    }
}
