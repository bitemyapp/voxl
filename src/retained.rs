//! Bounded retained-paint framing shared by the server and native viewer.
//! A zlib stream keeps a small dictionary across messages; each sync-flushed
//! message is independently length-bounded and belongs to one viewer epoch.
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use std::borrow::Cow;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
const MAX_WIRE: usize = MAX_PAYLOAD + MAX_PAYLOAD / 100 + 1024 + 13;
pub struct Encoder {
    compression: Option<Compress>,
}
impl Encoder {
    pub fn new(compressed: bool) -> Self {
        Self {
            compression: compressed.then(|| Compress::new(Compression::fast(), true)),
        }
    }
    pub fn packet(&mut self, nonce: u32, sequence: u32, payload: &[u8]) -> Result<Vec<u8>, String> {
        if payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return Err("retained payload bounds".into());
        }
        let mut packet = Vec::with_capacity(payload.len() + payload.len() / 100 + 1024 + 13);
        packet.push(if self.compression.is_some() {
            0x21
        } else {
            0x20
        });
        packet.extend_from_slice(&nonce.to_le_bytes());
        packet.extend_from_slice(&sequence.to_le_bytes());
        if let Some(codec) = &mut self.compression {
            packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            let input = codec.total_in();
            let result = codec
                .compress_vec(payload, &mut packet, FlushCompress::Sync)
                .map_err(|e| e.to_string())?;
            if result != Status::Ok
                || codec.total_in() - input != payload.len() as u64
                || !packet.ends_with(&[0, 0, 255, 255])
            {
                return Err("retained compression did not finish its message".into());
            }
        } else {
            packet.extend_from_slice(payload);
        }
        Ok(packet)
    }
}
#[derive(Default)]
pub struct Decoder {
    nonce: Option<u32>,
    sequence: u32,
    compression: Option<Decompress>,
}
pub struct Decoded<'a> {
    pub nonce: u32,
    pub sequence: u32,
    pub payload: Cow<'a, [u8]>,
}
impl Decoder {
    pub fn packet<'a>(&mut self, packet: &'a [u8]) -> Result<Decoded<'a>, String> {
        if packet.len() < 9 || packet.len() > MAX_WIRE || !matches!(packet[0], 0x20 | 0x21) {
            return Err("invalid retained packet bounds/type".into());
        }
        let nonce = u32::from_le_bytes(packet[1..5].try_into().unwrap());
        let sequence = u32::from_le_bytes(packet[5..9].try_into().unwrap());
        let compressed = packet[0] == 0x21;
        if self.nonce != Some(nonce) {
            self.nonce = Some(nonce);
            self.sequence = 0;
            self.compression = compressed.then(|| Decompress::new(true));
        }
        if sequence != self.sequence.wrapping_add(1) || compressed != self.compression.is_some() {
            return Err("retained transport sequence/codec changed".into());
        }
        let payload = if let Some(codec) = &mut self.compression {
            if packet.len() < 17 || !packet.ends_with(&[0, 0, 255, 255]) {
                return Err("truncated retained compressed message".into());
            }
            let size = u32::from_le_bytes(packet[9..13].try_into().unwrap()) as usize;
            if size == 0 || size > MAX_PAYLOAD {
                return Err("retained expanded length exceeds limit".into());
            }
            // One extra byte detects a false length without allocating or
            // decompressing the rest of an oversized expansion.
            let mut out = Vec::with_capacity(size + 1);
            let input = codec.total_in();
            let result = codec
                .decompress_vec(&packet[13..], &mut out, FlushDecompress::Sync)
                .map_err(|e| e.to_string())?;
            if result != Status::Ok
                || out.len() != size
                || codec.total_in() - input != (packet.len() - 13) as u64
            {
                return Err("retained compressed length/state mismatch".into());
            }
            Cow::Owned(out)
        } else {
            if packet.len() - 9 > MAX_PAYLOAD {
                return Err("retained payload exceeds limit".into());
            }
            Cow::Borrowed(&packet[9..])
        };
        self.sequence = sequence;
        Ok(Decoded {
            nonce,
            sequence,
            payload,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_drawing_reuses_dictionary_and_epochs_reset_it() {
        for compressed in [false, true] {
            let mut encoder = Encoder::new(compressed);
            let mut decoder = Decoder::default();
            let mut data = vec![0; 48 * 1024];
            let mut random = 0x12345678u32;
            for byte in &mut data {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                *byte = random as u8;
            }
            for sequence in 1..=12 {
                let data = &data[sequence as usize..sequence as usize + 8 * 1024];
                let packet = encoder.packet(7, sequence, data).unwrap();
                let out = decoder.packet(&packet).unwrap();
                assert_eq!((out.nonce, out.sequence), (7, sequence));
                assert_eq!(out.payload.as_ref(), data);
                if compressed && sequence > 1 {
                    // Compressor versions and backends choose different
                    // matches. Test the dictionary's benefit against the same
                    // payload with a fresh stream, not one encoded byte count.
                    let fresh = Encoder::new(true).packet(7, 1, data).unwrap();
                    assert!(
                        packet.len() * 2 < fresh.len(),
                        "dictionary not reused: {} versus {} fresh",
                        packet.len(),
                        fresh.len()
                    );
                }
            }
            let packet = Encoder::new(compressed).packet(8, 1, b"new epoch").unwrap();
            assert_eq!(
                decoder.packet(&packet).unwrap().payload.as_ref(),
                b"new epoch"
            );
        }
    }
    #[test]
    fn gaps_duplicates_truncation_and_expansion_lies_are_rejected() {
        let mut encoder = Encoder::new(true);
        let first = encoder.packet(1, 1, &vec![b'a'; 4096]).unwrap();
        let second = encoder.packet(1, 2, b"second").unwrap();
        assert!(Decoder::default().packet(&second).is_err());
        let mut decoder = Decoder::default();
        decoder.packet(&first).unwrap();
        assert!(decoder.packet(&first).is_err());
        assert!(Decoder::default()
            .packet(&first[..first.len() - 1])
            .is_err());
        for size in [1u32, 4095, 4097, u32::MAX] {
            let mut forged = first.clone();
            forged[9..13].copy_from_slice(&size.to_le_bytes());
            assert!(
                Decoder::default().packet(&forged).is_err(),
                "accepted wrong expanded size {size}"
            );
        }
        let mut ended = first.clone();
        ended[0] = 0x20;
        let mut decoder = Decoder::default();
        decoder.packet(&first).unwrap();
        ended[5..9].copy_from_slice(&2u32.to_le_bytes());
        assert!(
            decoder.packet(&ended).is_err(),
            "codec changed inside epoch"
        );
    }
}
