use glow::HasContext;
use voxl_media::{CudaGl, Format, Frame};

pub struct Renderer {
    pub gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    textures: [glow::Texture; 3],
    cuda: Option<CudaGl>,
    allocated: Option<(usize, usize, Format)>,
}
impl Renderer {
    pub fn new(video: &sdl2::VideoSubsystem) -> Result<Self, String> {
        let gl = unsafe {
            glow::Context::from_loader_function(|s| video.gl_get_proc_address(s) as *const _)
        };
        unsafe {
            log::info!(
                "OpenGL {} / {}",
                gl.get_parameter_string(glow::VERSION),
                gl.get_parameter_string(glow::RENDERER)
            );
            let program = gl.create_program()?;
            for (kind, source) in [
                (
                    glow::VERTEX_SHADER,
                    r#"#version 330 core
out vec2 uv;
void main() {
    vec2 v = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
    uv = vec2(v.x, 1.0 - v.y);
    gl_Position = vec4(v * 2.0 - 1.0, 0.0, 1.0);
}"#,
                ),
                (
                    glow::FRAGMENT_SHADER,
                    r#"#version 330 core
in vec2 uv; out vec4 color;
uniform sampler2D y_tex; uniform sampler2D u_tex; uniform sampler2D v_tex;
uniform int nv12;
uniform vec3 range_scale;
uniform vec4 matrix;
void main() {
    float y = (texture(y_tex, uv).r - range_scale.x) * range_scale.y;
    vec2 chroma = nv12 == 1 ? texture(u_tex, uv).rg : vec2(texture(u_tex, uv).r, texture(v_tex, uv).r);
    vec2 c = (chroma - vec2(128.0/255.0)) * range_scale.z;
    color = vec4(y + matrix.x*c.y, y - matrix.y*c.x - matrix.z*c.y, y + matrix.w*c.x, 1.0);
}"#,
                ),
            ] {
                let shader = gl.create_shader(kind)?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    return Err(gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(gl.get_program_info_log(program));
            }
            let vao = gl.create_vertex_array()?;
            let textures = [
                gl.create_texture()?,
                gl.create_texture()?,
                gl.create_texture()?,
            ];
            for (i, texture) in textures.iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(*texture));
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MIN_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_MAG_FILTER,
                    glow::LINEAR as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );
            }
            Ok(Self {
                gl,
                program,
                vao,
                textures,
                cuda: None,
                allocated: None,
            })
        }
    }
    pub fn upload(&mut self, frame: &Frame) -> Result<(), String> {
        let format = frame.format()?;
        let layout = (frame.width(), frame.height(), format);
        let nv12 = format != Format::I420;
        unsafe {
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            if self.allocated != Some(layout) {
                self.cuda = None;
            }
            for i in 0..if nv12 { 2 } else { 3 } {
                let channels = if nv12 && i == 1 { 2 } else { 1 };
                let (w, h) = if i == 0 {
                    (frame.width(), frame.height())
                } else {
                    (frame.width().div_ceil(2), frame.height().div_ceil(2))
                };
                self.gl.active_texture(glow::TEXTURE0 + i as u32);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(self.textures[i]));
                if self.allocated != Some(layout) {
                    self.gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        if channels == 2 { glow::RG8 } else { glow::R8 } as i32,
                        w as i32,
                        h as i32,
                        0,
                        if channels == 2 { glow::RG } else { glow::RED },
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(None),
                    );
                }
                if format != Format::Cuda {
                    let (bytes, stride) = frame.plane(i)?;
                    self.gl
                        .pixel_store_i32(glow::UNPACK_ROW_LENGTH, (stride / channels) as i32);
                    self.gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        0,
                        0,
                        w as i32,
                        h as i32,
                        if channels == 2 { glow::RG } else { glow::RED },
                        glow::UNSIGNED_BYTE,
                        glow::PixelUnpackData::Slice(Some(bytes)),
                    );
                }
            }
            self.allocated = Some(layout);
            if format == Format::Cuda {
                if self.cuda.is_none() {
                    self.cuda = Some(CudaGl::new(
                        frame,
                        self.textures[0].0.get(),
                        self.textures[1].0.get(),
                    )?);
                    log::info!("CUDA/GL device-memory presentation enabled");
                }
                self.cuda.as_mut().unwrap().copy(frame)?;
            }
            self.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
            self.gl.use_program(Some(self.program));
            let (kr, kb, full) = frame.color();
            let kg = 1.0 - kr - kb;
            self.gl.uniform_4_f32(
                self.gl
                    .get_uniform_location(self.program, "matrix")
                    .as_ref(),
                2.0 * (1.0 - kr),
                2.0 * kb * (1.0 - kb) / kg,
                2.0 * kr * (1.0 - kr) / kg,
                2.0 * (1.0 - kb),
            );
            let (black, y_scale, c_scale) = if full {
                (0.0, 1.0, 1.0)
            } else {
                (16.0 / 255.0, 255.0 / 219.0, 255.0 / 224.0)
            };
            self.gl.uniform_3_f32(
                self.gl
                    .get_uniform_location(self.program, "range_scale")
                    .as_ref(),
                black,
                y_scale,
                c_scale,
            );
            self.gl.uniform_1_i32(
                self.gl.get_uniform_location(self.program, "nv12").as_ref(),
                i32::from(nv12),
            );
        }
        Ok(())
    }
    pub fn draw_band(&self, size: (u32, u32), source: (u32, u32), band: (u32, u32), clear: bool) {
        let rect = viewport(size, source);
        unsafe {
            if clear {
                self.gl.viewport(0, 0, size.0 as i32, size.1 as i32);
                self.gl.clear_color(0.03, 0.03, 0.04, 1.0);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
            }
            let top = (band.0 as f64 * rect[3] as f64 / source.1 as f64).round() as i32;
            let bottom =
                ((band.0 + band.1) as f64 * rect[3] as f64 / source.1 as f64).round() as i32;
            self.gl
                .viewport(rect[0], rect[1] + rect[3] - bottom, rect[2], bottom - top);
            self.gl.use_program(Some(self.program));
            self.gl.bind_vertex_array(Some(self.vao));
            for (i, name) in ["y_tex", "u_tex", "v_tex"].iter().enumerate() {
                self.gl.active_texture(glow::TEXTURE0 + i as u32);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(self.textures[i]));
                self.gl.uniform_1_i32(
                    self.gl.get_uniform_location(self.program, name).as_ref(),
                    i as i32,
                );
            }
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.cuda = None;
        unsafe {
            for texture in self.textures {
                self.gl.delete_texture(texture);
            }
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_program(self.program);
        }
    }
}
pub fn viewport(size: (u32, u32), source: (u32, u32)) -> [i32; 4] {
    let scale =
        (size.0 as f64 / source.0.max(1) as f64).min(size.1 as f64 / source.1.max(1) as f64);
    let w = (source.0 as f64 * scale).round() as i32;
    let h = (source.1 as f64 * scale).round() as i32;
    [(size.0 as i32 - w) / 2, (size.1 as i32 - h) / 2, w, h]
}
