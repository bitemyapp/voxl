//! Bounded validation of the existing browser-compatible video wire format.
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Hello {
    pub w: u32,
    pub h: u32,
    #[serde(default)]
    pub stripes: Vec<(u32, u32)>,
    #[serde(default, rename = "readOnly")]
    pub read_only: bool,
}
impl Hello {
    pub fn validate(mut self) -> Result<Self, String> {
        if self.w < 2
            || self.h < 2
            || self.w > 16384
            || self.h > 16384
            || !self.w.is_multiple_of(2)
            || !self.h.is_multiple_of(2)
            || u64::from(self.w) * u64::from(self.h) > 64 * 1024 * 1024
        {
            return Err("invalid stream dimensions".into());
        }
        if self.stripes.is_empty() {
            self.stripes.push((0, self.h));
        }
        if self.stripes.len() > 16 {
            return Err("too many video stripes".into());
        }
        let mut next = 0;
        for &(y, h) in &self.stripes {
            if y != next || h < 2 || h % 2 != 0 || h > self.h - next {
                return Err(
                    "video stripes must evenly cover the output without gaps or overlap".into(),
                );
            }
            next += h;
        }
        if next != self.h {
            return Err("incomplete video stripe layout".into());
        }
        Ok(self)
    }
}
#[derive(Debug)]
pub struct Header {
    pub id: u32,
    pub stripe: usize,
    pub parts: usize,
    pub key: bool,
}
impl Header {
    pub fn parse(bytes: &[u8], hello: &Hello) -> Result<Self, String> {
        if bytes.len() <= 16 || bytes.len() > 64 * 1024 * 1024 || bytes[0] != 1 {
            return Err("invalid video packet length/type".into());
        }
        let stripe = bytes[10] as usize;
        let parts = bytes[12] as usize;
        if stripe >= hello.stripes.len()
            || bytes[11] as usize != hello.stripes.len()
            || parts == 0
            || parts > hello.stripes.len()
            || u16::from_be_bytes([bytes[14], bytes[15]]) as u32 != hello.stripes[stripe].0
        {
            return Err("video packet does not match the announced stripe layout".into());
        }
        Ok(Self {
            id: u32::from_be_bytes(bytes[2..6].try_into().unwrap()),
            stripe,
            parts,
            key: bytes[1] & 1 != 0,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn hello(stripes: Vec<(u32, u32)>) -> Hello {
        Hello {
            w: 1920,
            h: 1080,
            stripes,
            read_only: false,
        }
    }
    #[test]
    fn rejects_layouts_that_could_overrun_or_leave_uninitialized_pixels() {
        for layout in [
            vec![(0, 1082)],
            vec![(0, 540), (539, 540)],
            vec![(0, 540)],
            vec![(0, u32::MAX)],
            vec![(0, 539), (539, 541)],
        ] {
            assert!(hello(layout).validate().is_err());
        }
        assert_eq!(hello(vec![]).validate().unwrap().stripes, vec![(0, 1080)]);
        assert!(hello(vec![(0, 544), (544, 536)]).validate().is_ok());
    }
    #[test]
    fn validates_header_before_decoding_untrusted_payload() {
        let hello = hello(vec![(0, 544), (544, 536)]).validate().unwrap();
        let mut packet = vec![0; 17];
        packet[0] = 1;
        packet[10] = 1;
        packet[11] = 2;
        packet[12] = 1;
        packet[14..16].copy_from_slice(&544u16.to_be_bytes());
        assert!(Header::parse(&packet, &hello).is_ok());
        for (index, value) in [(10, 2), (11, 3), (12, 0), (12, 3), (14, 255)] {
            let mut bad = packet.clone();
            bad[index] = value;
            assert!(Header::parse(&bad, &hello).is_err());
        }
        for n in 0..17 {
            assert!(Header::parse(&packet[..n], &hello).is_err());
        }
    }
}
