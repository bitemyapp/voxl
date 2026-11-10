//! Reference-counted FFmpeg frames with an explicit GPU/CPU boundary.
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::NonNull;

unsafe extern "C" {
    fn voxl_decoder_new(mode: c_int, error: *mut c_char, cap: usize) -> *mut c_void;
    fn voxl_decoder_free(d: *mut c_void);
    fn voxl_decoder_hardware(d: *mut c_void) -> c_int;
    fn voxl_decoder_send(d: *mut c_void, data: *const u8, len: usize, pts: i64) -> c_int;
    fn voxl_decoder_receive(d: *mut c_void, result: *mut c_int) -> *mut c_void;
    fn voxl_frame_free(f: *mut c_void);
    fn voxl_frame_width(f: *const c_void) -> c_int;
    fn voxl_frame_height(f: *const c_void) -> c_int;
    fn voxl_frame_format(f: *const c_void) -> c_int;
    fn voxl_frame_pts(f: *const c_void) -> i64;
    fn voxl_frame_color(f: *const c_void) -> c_int;
    fn voxl_frame_plane(f: *const c_void, i: c_int, stride: *mut c_int) -> *const u8;
    fn voxl_frame_download(f: *const c_void, out: *mut *mut c_void) -> c_int;
    fn voxl_cuda_gl_new(
        f: *const c_void,
        y: u32,
        uv: u32,
        error: *mut c_char,
        cap: usize,
    ) -> *mut c_void;
    fn voxl_cuda_gl_copy(p: *mut c_void, f: *const c_void) -> c_int;
    fn voxl_cuda_gl_free(p: *mut c_void);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decode {
    Cpu,
    Auto,
    Cuda,
}
pub struct Decoder(NonNull<c_void>);
pub struct Frame(NonNull<c_void>);
pub struct CudaGl(NonNull<c_void>);
impl CudaGl {
    /// Register decoded-frame destination textures with CUDA.
    ///
    /// # Safety
    /// The owning GL context must be current when creating, copying or dropping
    /// this object. Textures must outlive it and must not be deleted or reallocated
    /// while registered with CUDA.
    pub unsafe fn new(frame: &Frame, y: u32, uv: u32) -> Result<Self, String> {
        let mut error = [0i8; 512];
        let p = voxl_cuda_gl_new(frame.0.as_ptr(), y, uv, error.as_mut_ptr(), error.len());
        NonNull::new(p).map(Self).ok_or_else(|| {
            CStr::from_ptr(error.as_ptr())
                .to_string_lossy()
                .into_owned()
        })
    }
    pub fn copy(&mut self, frame: &Frame) -> Result<(), String> {
        let r = unsafe { voxl_cuda_gl_copy(self.0.as_ptr(), frame.0.as_ptr()) };
        if r == 0 {
            Ok(())
        } else {
            Err(format!("CUDA/GL transfer failed ({r})"))
        }
    }
}
impl Drop for CudaGl {
    fn drop(&mut self) {
        unsafe { voxl_cuda_gl_free(self.0.as_ptr()) };
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    I420,
    Nv12,
    Cuda,
}
impl Decoder {
    pub fn new(mode: Decode) -> Result<Self, String> {
        let mut error = [0i8; 512];
        // The shim owns its codec/device and copies the error within this buffer.
        let d = unsafe {
            voxl_decoder_new(
                match mode {
                    Decode::Cpu => 0,
                    Decode::Auto => 1,
                    Decode::Cuda => 2,
                },
                error.as_mut_ptr(),
                error.len(),
            )
        };
        NonNull::new(d).map(Self).ok_or_else(|| unsafe {
            CStr::from_ptr(error.as_ptr())
                .to_string_lossy()
                .into_owned()
        })
    }
    pub fn hardware(&self) -> bool {
        unsafe { voxl_decoder_hardware(self.0.as_ptr()) != 0 }
    }
    pub fn send(&mut self, bytes: &[u8], pts: i64) -> Result<(), String> {
        let r = unsafe { voxl_decoder_send(self.0.as_ptr(), bytes.as_ptr(), bytes.len(), pts) };
        if r < 0 {
            Err(format!("H.264 packet rejected ({r})"))
        } else {
            Ok(())
        }
    }
    pub fn receive(&mut self) -> Result<Option<Frame>, String> {
        let mut r = 0;
        let f = unsafe { voxl_decoder_receive(self.0.as_ptr(), &mut r) };
        if let Some(f) = NonNull::new(f) {
            return Ok(Some(Frame(f)));
        }
        if r == -11 {
            Ok(None)
        } else {
            Err(format!("H.264 decode failed ({r})"))
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { voxl_decoder_free(self.0.as_ptr()) };
    }
}
impl Frame {
    /// YUV conversion coefficients (Kr, Kb, full range), defaulting to the
    /// original Voxl BT.601 stream when a source omits color metadata.
    pub fn color(&self) -> (f32, f32, bool) {
        let color = unsafe { voxl_frame_color(self.0.as_ptr()) };
        let (kr, kb) = match color & 255 {
            1 => (0.2126, 0.0722),
            2 => (0.2627, 0.0593),
            _ => (0.299, 0.114),
        };
        (kr, kb, color & 256 != 0)
    }
    pub fn width(&self) -> usize {
        unsafe { voxl_frame_width(self.0.as_ptr()) as usize }
    }
    pub fn height(&self) -> usize {
        unsafe { voxl_frame_height(self.0.as_ptr()) as usize }
    }
    pub fn pts(&self) -> i64 {
        unsafe { voxl_frame_pts(self.0.as_ptr()) }
    }
    pub fn format(&self) -> Result<Format, String> {
        match unsafe { voxl_frame_format(self.0.as_ptr()) } {
            0 => Ok(Format::I420),
            1 => Ok(Format::Nv12),
            2 => Ok(Format::Cuda),
            _ => Err("unsupported decoded pixel format".into()),
        }
    }
    pub fn plane(&self, i: usize) -> Result<(&[u8], usize), String> {
        if i > 2 {
            return Err("invalid plane".into());
        }
        let mut stride = 0;
        let p = unsafe { voxl_frame_plane(self.0.as_ptr(), i as i32, &mut stride) };
        if p.is_null() || stride <= 0 {
            return Err("plane is not CPU-readable".into());
        }
        let rows = if i == 0 {
            self.height()
        } else {
            self.height().div_ceil(2)
        };
        let format = self.format()?;
        let row_bytes = if i == 0 {
            self.width()
        } else if format == Format::Nv12 {
            self.width().div_ceil(2) * 2
        } else {
            self.width().div_ceil(2)
        };
        if rows == 0 || row_bytes == 0 || (stride as usize) < row_bytes {
            return Err("invalid decoded plane layout".into());
        }
        let len = (rows - 1)
            .checked_mul(stride as usize)
            .and_then(|n| n.checked_add(row_bytes))
            .ok_or("decoded plane is too large")?;
        Ok((
            unsafe { std::slice::from_raw_parts(p, len) },
            stride as usize,
        ))
    }
    pub fn download(&self) -> Result<Frame, String> {
        let mut p = std::ptr::null_mut();
        let ret = unsafe { voxl_frame_download(self.0.as_ptr(), &mut p) };
        if ret < 0 {
            return Err(format!("download decoded frame ({ret})"));
        }
        NonNull::new(p)
            .map(Frame)
            .ok_or_else(|| "missing downloaded frame".into())
    }
}
impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { voxl_frame_free(self.0.as_ptr()) };
    }
}
