//! A local, optional renderer ABI. The network supplies only bounded data;
//! library paths and executable code are never taken from a remote peer.
use glow::HasContext;
use std::ffi::{c_char, c_void, CStr};
#[repr(C)]
#[derive(Default)]
struct View {
    width: u32,
    height: u32,
    stride: u32,
    rgba: *const u8,
    bytes: usize,
    damage: *const u32,
    damage_count: usize,
}
type New = unsafe extern "C" fn() -> *mut c_void;
type Free = unsafe extern "C" fn(*mut c_void);
type Paint = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;
type GetView = unsafe extern "C" fn(*mut c_void, *mut View) -> i32;
type Error = unsafe extern "C" fn(*mut c_void) -> *const c_char;
pub struct Scene {
    _library: libloading::Library,
    handle: *mut c_void,
    new: New,
    free: Free,
    paint: Paint,
    view: GetView,
    error: Error,
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    texture: glow::Texture,
    size: (u32, u32),
    pub active: bool,
    pub frames: u64,
    pub bytes: u64,
    pub paint_us: u128,
    pub uploaded_bytes: u64,
}
impl Scene {
    pub fn new(video: &sdl2::VideoSubsystem, path: &str) -> Result<Self, String> {
        unsafe {
            let library = libloading::Library::new(path).map_err(|e| e.to_string())?;
            let version = library
                .get::<unsafe extern "C" fn() -> u32>(b"aizome_remote_renderer_version\0")
                .map_err(|e| e.to_string())?;
            if version() != 1 {
                return Err("unsupported Aizome renderer ABI".into());
            }
            let new = *library
                .get::<New>(b"aizome_remote_renderer_new\0")
                .map_err(|e| e.to_string())?;
            let free = *library
                .get::<Free>(b"aizome_remote_renderer_free\0")
                .map_err(|e| e.to_string())?;
            let paint = *library
                .get::<Paint>(b"aizome_remote_renderer_paint\0")
                .map_err(|e| e.to_string())?;
            let view = *library
                .get::<GetView>(b"aizome_remote_renderer_view\0")
                .map_err(|e| e.to_string())?;
            let error = *library
                .get::<Error>(b"aizome_remote_renderer_error\0")
                .map_err(|e| e.to_string())?;
            let gl =
                glow::Context::from_loader_function(|s| video.gl_get_proc_address(s) as *const _);
            let program = gl.create_program()?;
            for (kind, source) in [
                (
                    glow::VERTEX_SHADER,
                    r#"#version 330 core
out vec2 uv;
void main() { vec2 v=vec2((gl_VertexID<<1)&2,gl_VertexID&2); uv=vec2(v.x,1.0-v.y); gl_Position=vec4(v*2.0-1.0,0.0,1.0); }"#,
                ),
                (
                    glow::FRAGMENT_SHADER,
                    r#"#version 330 core
in vec2 uv; out vec4 color; uniform sampler2D pixels;
void main() { color=vec4(texture(pixels,uv).rgb,1.0); }"#,
                ),
            ] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let error = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    gl.delete_program(program);
                    return Err(error);
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                let e = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(e);
            }
            let vao = gl.create_vertex_array()?;
            let texture = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            let handle = new();
            if handle.is_null() {
                gl.delete_texture(texture);
                gl.delete_vertex_array(vao);
                gl.delete_program(program);
                return Err("Aizome renderer allocation failed".into());
            }
            Ok(Self {
                _library: library,
                handle,
                new,
                free,
                paint,
                view,
                error,
                gl,
                program,
                vao,
                texture,
                size: (0, 0),
                active: false,
                frames: 0,
                bytes: 0,
                paint_us: 0,
                uploaded_bytes: 0,
            })
        }
    }
    pub fn reset(&mut self) {
        self.active = false;
        unsafe {
            (self.free)(self.handle);
            self.handle = (self.new)();
        }
    }
    pub fn frame(&mut self, bytes: &[u8], expected: (u32, u32)) -> Result<(), String> {
        if bytes.len() > 16 * 1024 * 1024 || self.handle.is_null() {
            return Err("retained frame bounds".into());
        }
        let start = std::time::Instant::now();
        unsafe {
            if (self.paint)(self.handle, bytes.as_ptr(), bytes.len()) != 0 {
                return Err(CStr::from_ptr((self.error)(self.handle))
                    .to_string_lossy()
                    .into_owned());
            }
            let mut view = View::default();
            if (self.view)(self.handle, &mut view) != 0 {
                return Err("retained renderer returned no image".into());
            }
            let pixels = u64::from(view.width) * u64::from(view.height);
            if (view.width, view.height) != expected
                || pixels == 0
                || pixels > 16 * 1024 * 1024
                || view.stride != view.width * 4
                || view.bytes != pixels as usize * 4
                || view.rgba.is_null()
                || view.damage_count > 64 * 256
                || (view.damage_count > 0 && view.damage.is_null())
            {
                return Err("invalid retained renderer image".into());
            }
            let data = std::slice::from_raw_parts(view.rgba, view.bytes);
            let damage = if view.damage_count == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(view.damage, view.damage_count * 4)
            };
            for r in damage.as_chunks::<4>().0 {
                if r[0] > view.width
                    || r[1] > view.height
                    || r[2] > view.width - r[0]
                    || r[3] > view.height - r[1]
                {
                    return Err("invalid retained renderer damage".into());
                }
            }
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl
                .pixel_store_i32(glow::UNPACK_ROW_LENGTH, view.width as i32);
            if self.size != expected || !self.active {
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    view.width as i32,
                    view.height as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(data)),
                );
                self.uploaded_bytes += view.bytes as u64;
                self.size = expected;
            } else {
                for r in damage
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|r| r[2] > 0 && r[3] > 0)
                {
                    let offset = (r[1] as usize * view.width as usize + r[0] as usize) * 4;
                    self.gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        r[0] as i32,
                        r[1] as i32,
                        r[2] as i32,
                        r[3] as i32,
                        glow::RGBA,
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(&data[offset..])),
                    );
                    self.uploaded_bytes += u64::from(r[2]) * u64::from(r[3]) * 4;
                }
            }
            self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            let error = self.gl.get_error();
            if error != glow::NO_ERROR {
                return Err(format!("retained GL upload: {error:#x}"));
            }
        }
        self.active = true;
        self.frames += 1;
        self.bytes += bytes.len() as u64;
        self.paint_us += start.elapsed().as_micros();
        Ok(())
    }
    pub fn draw(&self, size: (u32, u32)) {
        unsafe {
            self.gl.viewport(0, 0, size.0 as i32, size.1 as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            let r = crate::render::viewport(size, self.size);
            self.gl
                .viewport(r[0], size.1 as i32 - r[1] - r[3], r[2], r[3]);
            self.gl.use_program(Some(self.program));
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            self.gl.uniform_1_i32(
                self.gl
                    .get_uniform_location(self.program, "pixels")
                    .as_ref(),
                0,
            );
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }
}
impl Drop for Scene {
    fn drop(&mut self) {
        unsafe {
            (self.free)(self.handle);
            self.gl.delete_texture(self.texture);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_program(self.program);
        }
    }
}
