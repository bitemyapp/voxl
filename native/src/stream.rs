use crate::{
    protocol::{Header, Hello},
    render::Renderer,
};
use std::collections::VecDeque;
use voxl_media::{Decode, Decoder, Frame};

struct Stripe {
    // GL registrations are destroyed before the decoder device.
    renderer: Renderer,
    decoder: Decoder,
    waiting_key: bool,
    visible: bool,
}
struct Pending {
    id: u32,
    need: usize,
    received: u16,
    decoded: Vec<(usize, Frame)>,
    skipped: usize,
}
pub struct PacketResult {
    pub acks: Vec<u32>,
    pub request_key: bool,
}
pub struct Stream {
    pub hello: Hello,
    stripes: Vec<Stripe>,
    pending: VecDeque<Pending>,
    pub frames: u64,
}
impl Stream {
    pub fn new(video: &sdl2::VideoSubsystem, hello: Hello, mode: Decode) -> Result<Self, String> {
        let mut stripes = Vec::new();
        for _ in &hello.stripes {
            stripes.push(Stripe {
                renderer: Renderer::new(video)?,
                decoder: Decoder::new(mode)?,
                waiting_key: true,
                visible: false,
            });
        }
        Ok(Self {
            hello,
            stripes,
            pending: VecDeque::new(),
            frames: 0,
        })
    }
    pub fn hardware(&self) -> bool {
        self.stripes.iter().all(|s| s.decoder.hardware())
    }
    pub fn visible(&self) -> bool {
        self.stripes.iter().all(|s| s.visible)
    }
    /// Decode each H.264 stream independently, then atomically publish all the
    /// changed bands of one desktop frame. Unchanged bands retain their texture.
    pub fn packet(&mut self, bytes: &[u8]) -> Result<PacketResult, String> {
        let h = Header::parse(bytes, &self.hello)?;
        if !self.pending.iter().any(|p| p.id == h.id) {
            if self.pending.len() >= 8 {
                return Err("too many incomplete video frames".into());
            }
            self.pending.push_back(Pending {
                id: h.id,
                need: h.parts,
                received: 0,
                decoded: Vec::new(),
                skipped: 0,
            });
        }
        let pending = self.pending.iter_mut().find(|p| p.id == h.id).unwrap();
        if pending.need != h.parts
            || pending.received & (1 << h.stripe) != 0
            || pending.received.count_ones() as usize >= pending.need
        {
            return Err("inconsistent or repeated video stripe".into());
        }
        pending.received |= 1 << h.stripe;
        let stripe = &mut self.stripes[h.stripe];
        let request_key = stripe.waiting_key && !h.key;
        if request_key {
            pending.skipped += 1;
        } else {
            stripe.waiting_key = false;
            stripe.decoder.send(&bytes[16..], i64::from(h.id))?;
            while let Some(frame) = stripe.decoder.receive()? {
                if frame.width() != self.hello.w as usize
                    || frame.height() != self.hello.stripes[h.stripe].1 as usize
                {
                    return Err("decoded dimensions differ from hello".into());
                }
                let pending = self
                    .pending
                    .iter_mut()
                    .find(|p| i64::from(p.id) == frame.pts())
                    .ok_or("unexpected decoded frame timestamp")?;
                if pending.decoded.iter().any(|(i, _)| *i == h.stripe) {
                    return Err("duplicate decoded stripe".into());
                }
                pending.decoded.push((h.stripe, frame));
            }
        }
        let mut acks = Vec::new();
        while self
            .pending
            .front()
            .is_some_and(|p| p.decoded.len() + p.skipped == p.need)
        {
            let pending = self.pending.pop_front().unwrap();
            if !pending.decoded.is_empty() {
                self.frames += 1;
            }
            for (index, frame) in pending.decoded {
                let stripe = &mut self.stripes[index];
                stripe.renderer.upload(&frame)?;
                stripe.visible = true;
            }
            acks.push(pending.id);
        }
        Ok(PacketResult { acks, request_key })
    }
    pub fn draw(&self, size: (u32, u32)) {
        for (i, stripe) in self.stripes.iter().enumerate() {
            stripe.renderer.draw_band(
                size,
                (self.hello.w, self.hello.h),
                self.hello.stripes[i],
                i == 0,
            );
        }
    }
}
