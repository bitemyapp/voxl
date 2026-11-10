//! Two persistent GBM capture DMA-BUFs copied on-device into NVENC GL inputs.
//! This object is deliberately thread-bound: its EGL context stays current on
//! the creating thread, and buffers may be encoded only after capture completion.
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::fd::{FromRawFd, OwnedFd};
use std::ptr::NonNull;
#[repr(C)]
struct RawDmaBuf {
    fd: [c_int; 4],
    stride: [u32; 4],
    offset: [u32; 4],
    modifier: u64,
    planes: u32,
    width: u32,
    height: u32,
    format: u32,
}
unsafe extern "C" {
    fn voxl_gpu_new(
        path: *const c_char,
        w: u32,
        h: u32,
        format: u32,
        mods: *const u64,
        n: usize,
        fps: u32,
        kbps: u32,
        quality: f32,
        preset: u32,
        error: *mut c_char,
        cap: usize,
    ) -> *mut c_void;
    fn voxl_gpu_free(g: *mut c_void);
    fn voxl_gpu_export(g: *mut c_void, index: c_int, out: *mut RawDmaBuf) -> c_int;
    fn voxl_gpu_encode(
        g: *mut c_void,
        index: c_int,
        key: c_int,
        kbps: u32,
        quality: f32,
        out: *mut u8,
        cap: usize,
        len: *mut usize,
        is_key: *mut c_int,
        error: *mut c_char,
        errcap: usize,
    ) -> c_int;
    fn voxl_gpu_readback(g: *mut c_void, index: c_int, out: *mut u8, len: usize) -> c_int;
}
pub struct DmaPlane {
    pub fd: OwnedFd,
    pub stride: u32,
    pub offset: u32,
}
pub struct DmaBuf {
    pub planes: Vec<DmaPlane>,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub format: u32,
}
pub struct Gpu {
    raw: NonNull<c_void>,
    width: u32,
    height: u32,
}
#[derive(Clone, Copy)]
pub struct EncoderOptions {
    pub fps: u32,
    pub kbps: u32,
    pub quality: f32,
    pub preset: u32,
}
impl Gpu {
    pub fn new(
        device: &str,
        width: u32,
        height: u32,
        format: u32,
        modifiers: &[u64],
        options: EncoderOptions,
    ) -> Result<Self, String> {
        let EncoderOptions {
            fps,
            kbps,
            quality,
            preset,
        } = options;
        if width < 2
            || height < 2
            || width % 2 != 0
            || height % 2 != 0
            || u64::from(width) * u64::from(height) * 4 > 256 * 1024 * 1024
            || fps == 0
            || fps > 1000
            || kbps == 0
            || kbps > 1_000_000
            || !quality.is_finite()
            || !(0.0..=51.0).contains(&quality)
            || !(1..=7).contains(&preset)
        {
            return Err("invalid GPU encoder dimensions/rate".into());
        }
        let path = CString::new(device).map_err(|e| e.to_string())?;
        let mut error = [0i8; 512];
        let raw = unsafe {
            voxl_gpu_new(
                path.as_ptr(),
                width,
                height,
                format,
                modifiers.as_ptr(),
                modifiers.len(),
                fps,
                kbps,
                quality,
                preset,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        NonNull::new(raw)
            .map(|raw| Self { raw, width, height })
            .ok_or_else(|| unsafe {
                CStr::from_ptr(error.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            })
    }
    pub fn export(&self, index: usize) -> Result<DmaBuf, String> {
        if index >= 2 {
            return Err("invalid GPU buffer index".into());
        }
        let mut raw = RawDmaBuf {
            fd: [-1; 4],
            stride: [0; 4],
            offset: [0; 4],
            modifier: 0,
            planes: 0,
            width: 0,
            height: 0,
            format: 0,
        };
        if unsafe { voxl_gpu_export(self.raw.as_ptr(), index as i32, &mut raw) } != 0 {
            return Err("exporting capture DMA-BUF".into());
        }
        let planes = (0..raw.planes as usize)
            .map(|i| DmaPlane {
                fd: unsafe { OwnedFd::from_raw_fd(raw.fd[i]) },
                stride: raw.stride[i],
                offset: raw.offset[i],
            })
            .collect();
        Ok(DmaBuf {
            planes,
            modifier: raw.modifier,
            width: raw.width,
            height: raw.height,
            format: raw.format,
        })
    }
    /// Call only after the compositor reports this buffer ready. Encoding is
    /// complete on return, so that buffer may then be reused for capture.
    pub fn encode(
        &mut self,
        index: usize,
        key: bool,
        kbps: u32,
        quality: f32,
        out: &mut Vec<u8>,
    ) -> Result<bool, String> {
        if index >= 2
            || kbps > 1_000_000
            || !quality.is_finite()
            || !(0.0..=51.0).contains(&quality)
        {
            return Err("invalid GPU encoding request".into());
        }
        // A single bounded allocation is retained by the caller across frames.
        let capacity =
            (self.width as usize * self.height as usize * 4).clamp(1024 * 1024, 64 * 1024 * 1024);
        out.clear();
        out.reserve(capacity);
        let mut len = 0;
        let mut is_key = 0;
        let mut error = [0i8; 512];
        let r = unsafe {
            voxl_gpu_encode(
                self.raw.as_ptr(),
                index as i32,
                i32::from(key),
                kbps,
                quality,
                out.as_mut_ptr(),
                capacity,
                &mut len,
                &mut is_key,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if r < 0 {
            return Err(unsafe {
                CStr::from_ptr(error.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            });
        }
        // The shim writes exactly len bytes after checking the supplied capacity.
        unsafe {
            out.set_len(len);
        }
        Ok(is_key != 0)
    }
    /// Explicit screenshot support. The streaming path never calls this.
    pub fn snapshot(&self, index: usize) -> Result<Vec<u8>, String> {
        if index >= 2 {
            return Err("invalid GPU buffer index".into());
        }
        let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
        if unsafe {
            voxl_gpu_readback(
                self.raw.as_ptr(),
                index as i32,
                pixels.as_mut_ptr(),
                pixels.len(),
            )
        } != 0
        {
            return Err("GPU screenshot readback failed".into());
        }
        Ok(pixels)
    }
}
impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe { voxl_gpu_free(self.raw.as_ptr()) };
    }
}
