//! Runtime selection between Linux capture backends. Encoding sees the same damage
//! and pixel interface on X11 and Wayland.

use std::os::fd::BorrowedFd;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

pub static DAMAGE_EVENTS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y0: u32,
    pub y1: u32,
}

enum Backend {
    X11(Box<crate::x11_capture::Capturer>),
    Wayland(Box<crate::wayland::capture::Capturer>),
}

pub struct Capturer {
    backend: Backend,
    pub width: u32,
    pub height: u32,
    pub damaged: bool,
    pub damaged_at: Option<Instant>,
    pub changes: Vec<(u32, u32, u32, u32)>,
}

macro_rules! backend {
    ($self:expr, $c:ident, $body:expr) => {
        match &mut $self.backend {
            Backend::X11($c) => $body,
            Backend::Wayland($c) => $body,
        }
    };
}

impl Capturer {
    pub fn new(display: Option<&str>) -> Result<Self, String> {
        let backend = if crate::wayland::enabled() {
            Backend::Wayland(Box::new(crate::wayland::capture::Capturer::new()?))
        } else {
            Backend::X11(Box::new(crate::x11_capture::Capturer::new(display)?))
        };
        let mut me = Self {
            backend,
            width: 0,
            height: 0,
            damaged: false,
            damaged_at: None,
            changes: Vec::new(),
        };
        me.sync();
        Ok(me)
    }

    fn sync(&mut self) {
        backend!(self, c, {
            self.width = c.width;
            self.height = c.height;
            self.damaged = c.damaged;
            self.damaged_at = c.damaged_at;
            self.changes.append(&mut c.changes);
        });
    }

    pub fn pump(&mut self) -> Result<(), String> {
        backend!(self, c, c.pump())?;
        self.sync();
        Ok(())
    }

    pub fn take_damage(&mut self) -> Result<Option<Band>, String> {
        let band = backend!(self, c, c.take_damage())?;
        self.sync();
        Ok(band)
    }

    pub fn take_mb_map(&mut self) -> Vec<u8> {
        backend!(self, c, c.take_mb_map())
    }

    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        let pixels = backend!(self, c, c.snapshot())?;
        self.sync();
        Ok(pixels)
    }

    pub fn full(&self) -> Band {
        Band {
            y0: 0,
            y1: self.height,
        }
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        match &self.backend {
            Backend::X11(c) => c.fd(),
            Backend::Wayland(c) => c.fd(),
        }
    }

    pub fn needs_rebuild(&self) -> bool {
        match &self.backend {
            Backend::X11(c) => c.needs_rebuild(),
            Backend::Wayland(c) => c.needs_rebuild(),
        }
    }

    pub fn geometry_changed(&self) -> bool {
        match &self.backend {
            Backend::X11(c) => c.geometry_changed(),
            Backend::Wayland(c) => c.needs_rebuild(),
        }
    }

    pub fn bgrx(&self) -> Option<(&[u8], usize)> {
        match &self.backend {
            Backend::X11(c) => c.bgrx(),
            Backend::Wayland(c) => Some(c.bgrx()),
        }
    }

    pub fn grab(&self, band: Band) -> Result<(), String> {
        match &self.backend {
            Backend::X11(c) => c.grab(band),
            Backend::Wayland(_) => Ok(()),
        }
    }

    pub fn convert(&self, dst: &mut crate::convert::I420, y0: usize, y1: usize, threads: usize) {
        match &self.backend {
            Backend::X11(c) => c.convert(dst, y0, y1, threads),
            Backend::Wayland(c) => {
                let (pixels, stride) = c.bgrx();
                crate::convert::convert_band(pixels, stride, dst, y0, y1, threads);
            }
        }
    }
}
