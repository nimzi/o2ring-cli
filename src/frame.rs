// SPDX-License-Identifier: AGPL-3.0-or-later
//! OxyII frame codec (O2Ring S generation).
//!
//! ```text
//! A5 | cmd | ~cmd | flag | seq | len u16 LE | payload[len] | crc8
//! ```
//!
//! `flag` is 0 in requests; in replies 1 means OK and anything else is an error code.
//! The device echoes `seq`. CRC-8 is poly 0x07, init 0, not reflected, no xorout,
//! computed over every byte before the CRC.

pub const HEAD: u8 = 0xA5;
pub const OVERHEAD: usize = 8;
/// Longest payload we accept (GET_INFO reports a max packet length of 2016 on the O2Ring S).
pub const MAX_PAYLOAD: usize = 4096;

pub fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

pub fn encode(cmd: u8, payload: &[u8], seq: u8) -> Vec<u8> {
    assert!(payload.len() <= u16::MAX as usize, "payload too large");
    let mut out = Vec::with_capacity(OVERHEAD + payload.len());
    out.extend_from_slice(&[HEAD, cmd, !cmd, 0x00, seq]);
    out.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    out.extend_from_slice(payload);
    out.push(crc8(&out));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub cmd: u8,
    pub flag: u8,
    pub seq: u8,
    pub payload: Vec<u8>,
}

/// Accumulates notification chunks and yields complete, CRC-valid frames.
///
/// An incomplete candidate doesn't block scanning for a complete frame further on,
/// and bytes before a decoded frame are dropped (same behaviour as the vendor SDK).
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(chunk);
        let b = &self.buf;
        let mut frames = Vec::new();
        let mut i = 0;
        let mut keep_from: Option<usize> = None;
        while b.len() - i >= OVERHEAD {
            if b[i] != HEAD || (b[i + 1] ^ b[i + 2]) != 0xFF {
                i += 1;
                continue;
            }
            let len = u16::from_le_bytes([b[i + 5], b[i + 6]]) as usize;
            if len > MAX_PAYLOAD {
                i += 1;
                continue;
            }
            let total = OVERHEAD + len;
            if b.len() - i < total {
                keep_from.get_or_insert(i);
                i += 1;
                continue;
            }
            if crc8(&b[i..i + total - 1]) != b[i + total - 1] {
                i += 1;
                continue;
            }
            frames.push(Frame {
                cmd: b[i + 1],
                flag: b[i + 3],
                seq: b[i + 4],
                payload: b[i + 7..i + 7 + len].to_vec(),
            });
            i += total;
            keep_from = None;
        }
        let from = keep_from.unwrap_or(i);
        self.buf.drain(..from);
        frames
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc8(b"123456789"), 0xF4);
    }

    #[test]
    fn round_trip_split_across_chunks() {
        let f = encode(0xF3, &[1, 2, 3, 4], 7);
        let mut d = Decoder::default();
        // Leading garbage, then the frame split in two notifications.
        assert!(d.push(&[0x00, 0x11]).is_empty());
        assert!(d.push(&f[..5]).is_empty());
        let out = d.push(&f[5..]);
        assert_eq!(
            out,
            vec![Frame { cmd: 0xF3, flag: 0, seq: 7, payload: vec![1, 2, 3, 4] }]
        );
    }

    #[test]
    fn rejects_bad_crc() {
        let mut f = encode(0xE1, &[9, 9], 1);
        *f.last_mut().unwrap() ^= 0xFF;
        assert!(Decoder::default().push(&f).is_empty());
    }
}
