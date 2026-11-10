//! The existing 48 kHz stereo Opus stream, with a bounded playback queue.
//! A late network burst must not turn into seconds of stale desktop audio.
use sdl2::audio::{AudioQueue, AudioSpecDesired};
use std::{ffi::c_void, ptr::NonNull};

const RATE: usize = 48_000;
const CHANNELS: usize = 2;
const MAX_SAMPLES: usize = RATE * 120 / 1000;
const MAX_QUEUED_BYTES: u32 = (RATE * CHANNELS * 4 * 80 / 1000) as u32;

#[link(name = "opus")]
unsafe extern "C" {
    fn opus_decoder_create(rate: i32, channels: i32, error: *mut i32) -> *mut c_void;
    fn opus_decoder_destroy(decoder: *mut c_void);
    fn opus_decode_float(
        decoder: *mut c_void,
        data: *const u8,
        len: i32,
        pcm: *mut f32,
        samples: i32,
        fec: i32,
    ) -> i32;
}

struct Decoder(NonNull<c_void>);
impl Decoder {
    fn new() -> Result<Self, String> {
        let mut error = 0;
        // SAFETY: valid error storage, supported Opus rate/channel count.
        let ptr = unsafe { opus_decoder_create(RATE as i32, CHANNELS as i32, &mut error) };
        NonNull::new(ptr)
            .map(Self)
            .ok_or_else(|| format!("Opus decoder initialization: {error}"))
    }
    fn decode<'a>(&mut self, packet: &[u8], pcm: &'a mut [f32]) -> Result<&'a [f32], String> {
        if packet.is_empty() || packet.len() > 8192 || pcm.len() < MAX_SAMPLES * CHANNELS {
            return Err("invalid Opus packet or output capacity".into());
        }
        // SAFETY: the decoder is uniquely owned, and both slices have validated
        // lengths. libopus writes at most samples * channels floats.
        let n = unsafe {
            opus_decode_float(
                self.0.as_ptr(),
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                MAX_SAMPLES as i32,
                0,
            )
        };
        if n < 0 {
            return Err(format!("invalid Opus packet: {n}"));
        }
        Ok(&pcm[..n as usize * CHANNELS])
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: this handle is owned and freed exactly once.
        unsafe { opus_decoder_destroy(self.0.as_ptr()) };
    }
}

pub struct Playback {
    decoder: Decoder,
    device: AudioQueue<f32>,
    pcm: Vec<f32>,
    pub packets: u64,
    pub discarded_queues: u64,
}
impl Playback {
    pub fn new(sdl: &sdl2::Sdl) -> Result<Self, String> {
        let device = sdl.audio()?.open_queue::<f32, _>(
            None,
            &AudioSpecDesired {
                freq: Some(RATE as i32),
                channels: Some(CHANNELS as u8),
                samples: Some(480),
            },
        )?;
        Ok(Self {
            decoder: Decoder::new()?,
            device,
            pcm: vec![0.0; MAX_SAMPLES * CHANNELS],
            packets: 0,
            discarded_queues: 0,
        })
    }
    pub fn reset(&mut self) -> Result<(), String> {
        self.device.pause();
        self.device.clear();
        self.decoder = Decoder::new()?;
        Ok(())
    }
    pub fn packet(&mut self, packet: &[u8]) -> Result<(), String> {
        let pcm = self.decoder.decode(packet, &mut self.pcm)?;
        // Bound the entire queued duration, including this packet. Decode
        // precedes dropping so the predictive codec's state remains current.
        if self
            .device
            .size()
            .saturating_add(std::mem::size_of_val(pcm) as u32)
            > MAX_QUEUED_BYTES
        {
            self.device.clear();
            self.discarded_queues += 1;
        }
        let keep = (MAX_QUEUED_BYTES as usize / 4).min(pcm.len());
        self.device.queue_audio(&pcm[pcm.len() - keep..])?;
        self.device.resume();
        self.packets += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_stereo_silence_and_rejects_invalid_packets() {
        let mut decoder = Decoder::new().unwrap();
        let mut pcm = vec![0.0; MAX_SAMPLES * CHANNELS];
        // RFC 6716's commonly used 20 ms Opus silence packet.
        let samples = decoder.decode(&[0xf8, 0xff, 0xfe], &mut pcm).unwrap();
        assert_eq!(samples.len(), 960 * CHANNELS);
        assert!(samples.iter().all(|s| s.is_finite() && s.abs() < 0.001));
        assert!(decoder.decode(&[], &mut pcm).is_err());
        assert!(decoder.decode(&vec![0; 8193], &mut pcm).is_err());
        assert!(decoder.decode(&[0xf8, 0xff, 0xfe], &mut [0.0; 2]).is_err());
    }
}
