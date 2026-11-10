//! DMA-BUF capture feeding two persistent NVENC OpenGL textures. A frame is never
//! reused while being encoded; the other buffer may be filled concurrently.
use crate::gpu::InitError;
use crate::gpu_media::{EncoderOptions, Gpu};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1 as source, ext_output_image_capture_source_manager_v1 as sources,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1 as frame, ext_image_copy_capture_manager_v1 as manager,
    ext_image_copy_capture_session_v1 as session,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_v1 as dmabuf,
};

#[derive(Default)]
struct Heard {
    outputs: Vec<(wl_output::WlOutput, String)>,
    size: (u32, u32),
    device: Option<u64>,
    formats: Vec<(u32, Vec<u64>)>,
    constraints: u64,
    in_constraints: bool,
    stopped: bool,
    removed: Vec<u32>,
    ready: bool,
    failed: Option<String>,
    transform: Option<WEnum<wl_output::Transform>>,
    damage: Vec<(i32, i32, i32, i32)>,
}
pub struct Capturer {
    pub output_name: String,
    conn: Connection,
    queue: EventQueue<Heard>,
    heard: Heard,
    session: session::ExtImageCopyCaptureSessionV1,
    source: source::ExtImageCaptureSourceV1,
    frame: Option<frame::ExtImageCopyCaptureFrameV1>,
    buffers: Vec<wl_buffer::WlBuffer>,
    pub gpu: Gpu,
    pub width: u32,
    pub height: u32,
    pub current: usize,
    pending: usize,
    output_id: u32,
    constraints: u64,
    pub changed: bool,
    pub changed_at: Instant,
    pub changes: Vec<(u32, u32, u32, u32)>,
}
impl Capturer {
    pub fn new(encode_options: EncoderOptions) -> Result<Self, InitError> {
        let settings = super::settings();
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<Heard>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let manager: manager::ExtImageCopyCaptureManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let sources: sources::ExtOutputImageCaptureSourceManagerV1 =
            globals.bind(&qh, 1..=1, ()).map_err(|e| e.to_string())?;
        let dmabuf: dmabuf::ZwpLinuxDmabufV1 = globals
            .bind(&qh, 3..=5, ())
            .map_err(|e| format!("linux-dmabuf required: {e}"))?;
        let mut heard = Heard::default();
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                heard.outputs.push((
                    globals
                        .registry()
                        .bind(g.name, g.version.min(4), &qh, g.name),
                    String::new(),
                ));
            }
        });
        queue.roundtrip(&mut heard).map_err(|e| e.to_string())?;
        let (output, name) = heard
            .outputs
            .iter()
            .find(|(_, name)| settings.output.as_ref().is_none_or(|wanted| wanted == name))
            .ok_or("selected Wayland output not found")?;
        let name = name.clone();
        let output_id = *output.data::<u32>().ok_or("missing output id")?;
        let source = sources.create_source(output, &qh, ());
        let options = if settings.cursor {
            manager::Options::PaintCursors
        } else {
            manager::Options::empty()
        };
        let session = manager.create_session(&source, options, &qh, ());
        let deadline = Instant::now() + Duration::from_secs(5);
        while heard.constraints == 0 && !heard.stopped {
            if Instant::now() >= deadline {
                return Err("GPU capture constraints timed out".into());
            }
            super::dispatch(&conn, &mut queue, &mut heard, 100)?;
        }
        if heard.stopped {
            return Err(InitError::Revoked);
        }
        let device = heard
            .device
            .ok_or("compositor did not offer GPU capture (use its GLES renderer)")?;
        let path = std::fs::read_dir("/dev/dri")
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .find(|p| {
                p.file_name().to_string_lossy().starts_with("renderD")
                    && p.metadata().is_ok_and(|m| m.rdev() == device)
            })
            .ok_or_else(|| format!("capture render device {device} not found"))?
            .path();
        let (width, height) = heard.size;
        let format = u32::from_le_bytes(*b"XR24");
        let modifiers = &heard
            .formats
            .iter()
            .find(|f| f.0 == format)
            .ok_or("capture does not offer XRGB8888 DMA-BUFs")?
            .1;
        log::info!(
            "GPU capture: {} {width}x{height}, {} advertised modifiers",
            path.display(),
            modifiers.len()
        );
        let gpu = Gpu::new(
            path.to_str().ok_or("invalid render path")?,
            width,
            height,
            format,
            modifiers,
            encode_options,
        )?;
        let mut buffers = Vec::new();
        for i in 0..2 {
            let b = gpu.export(i)?;
            let params = dmabuf.create_params(&qh, ());
            for (i, p) in b.planes.iter().enumerate() {
                params.add(
                    p.fd.as_fd(),
                    i as u32,
                    p.offset,
                    p.stride,
                    (b.modifier >> 32) as u32,
                    b.modifier as u32,
                );
            }
            buffers.push(params.create_immed(
                b.width as i32,
                b.height as i32,
                b.format,
                params::Flags::empty(),
                &qh,
                (),
            ));
            params.destroy();
        }
        queue
            .roundtrip(&mut heard)
            .map_err(|e| format!("DMA-BUF import: {e}"))?;
        let constraints = heard.constraints;
        let mut me = Self {
            output_name: name,
            conn,
            queue,
            heard,
            session,
            source,
            frame: None,
            buffers,
            gpu,
            width,
            height,
            current: 0,
            pending: 0,
            output_id,
            constraints,
            changed: false,
            changed_at: Instant::now(),
            changes: Vec::new(),
        };
        me.request(0)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !me.changed {
            if Instant::now() >= deadline {
                return Err("first GPU capture timed out".into());
            }
            if let Err(error) = me.pump(100) {
                if me.heard.stopped || me.heard.removed.contains(&me.output_id) {
                    return Err(InitError::Revoked);
                }
                return Err(error.into());
            }
        }
        Ok(me)
    }
    fn request(&mut self, index: usize) -> Result<(), String> {
        let f = self.session.create_frame(&self.queue.handle(), ());
        f.attach_buffer(&self.buffers[index]);
        f.damage_buffer(0, 0, self.width as i32, self.height as i32);
        f.capture();
        self.frame = Some(f);
        self.pending = index;
        self.conn.flush().map_err(|e| e.to_string())
    }
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.as_fd()
    }
    pub fn resized(&self) -> bool {
        self.constraints != self.heard.constraints
    }
    pub fn pump(&mut self, timeout: i32) -> Result<(), String> {
        super::dispatch(&self.conn, &mut self.queue, &mut self.heard, timeout)?;
        if self.heard.stopped || self.heard.removed.contains(&self.output_id) {
            return Err("Wayland sharing stopped or selected output removed".into());
        }
        if self.resized() {
            return Ok(());
        }
        if let Some(error) = self.heard.failed.take() {
            return Err(format!("GPU capture failed: {error}"));
        }
        if !self.heard.ready {
            return Ok(());
        }
        if self.heard.transform != Some(WEnum::Value(wl_output::Transform::Normal)) {
            return Err("GPU capture requires upright compositor buffers".into());
        }
        self.heard.ready = false;
        self.heard.transform = None;
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
        self.current = self.pending;
        if !self.changed {
            self.changed_at = Instant::now();
        }
        self.changed = true;
        let mut damage = std::mem::take(&mut self.heard.damage);
        if damage.is_empty() {
            damage.push((0, 0, self.width as i32, self.height as i32));
        }
        for (x, y, w, h) in damage {
            if w <= 0 || h <= 0 {
                continue;
            }
            let x0 = (x as i64).clamp(0, self.width as i64) as u32;
            let y0 = (y as i64).clamp(0, self.height as i64) as u32;
            let x1 = (x as i64 + w as i64).clamp(0, self.width as i64) as u32;
            let y1 = (y as i64 + h as i64).clamp(0, self.height as i64) as u32;
            if x1 > x0 && y1 > y0 {
                self.changes.push((x0, y0, x1, y1));
            }
        }
        crate::capture::DAMAGE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.request(1 - self.current)
    }
}
impl Drop for Capturer {
    fn drop(&mut self) {
        if let Some(f) = self.frame.take() {
            f.destroy();
        }
        self.session.destroy();
        self.source.destroy();
        for b in &self.buffers {
            b.destroy();
        }
        let _ = self.conn.flush();
    }
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
        o: &wl_output::WlOutput,
        e: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = e {
            if let Some(p) = h.outputs.iter_mut().find(|p| p.0 == *o) {
                p.1 = name;
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
            h.device = None;
        }
        match e {
            session::Event::BufferSize { width, height } => h.size = (width, height),
            session::Event::DmabufDevice { device } => {
                if let Ok(bytes) = device.try_into() {
                    h.device = Some(u64::from_ne_bytes(bytes));
                }
            }
            session::Event::DmabufFormat { format, modifiers } => {
                if modifiers.len() % 8 == 0 && modifiers.len() <= 8192 {
                    h.formats.push((
                        format,
                        modifiers
                            .chunks_exact(8)
                            .map(|m| u64::from_ne_bytes(m.try_into().unwrap()))
                            .collect(),
                    ));
                }
            }
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
            frame::Event::Transform { transform } => h.transform = Some(transform),
            frame::Event::Damage {
                x,
                y,
                width,
                height,
            } => h.damage.push((x, y, width, height)),
            _ => {}
        }
    }
}
delegate_noop!(Heard:ignore wl_buffer::WlBuffer);
delegate_noop!(Heard:ignore manager::ExtImageCopyCaptureManagerV1);
delegate_noop!(Heard:ignore sources::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(Heard:ignore source::ExtImageCaptureSourceV1);
delegate_noop!(Heard:ignore dmabuf::ZwpLinuxDmabufV1);
delegate_noop!(Heard:ignore params::ZwpLinuxBufferParamsV1);
