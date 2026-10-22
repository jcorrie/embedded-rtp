//! Helpers shared by the integration tests.
//!
//! Packets here are assembled byte by byte from the RFC 3550 diagrams rather
//! than with the crate's own writer, so a misreading of the spec in
//! `packet::write` cannot hide itself by being undone in `Packet::parse`.

#![allow(dead_code)]

/// Deterministic xorshift64* generator, so every failure reproduces from its
/// seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 32) as u32
    }

    pub fn below(&mut self, n: u32) -> u32 {
        self.next_u32() % n
    }

    /// True with probability `per_mille` / 1000.
    pub fn chance(&mut self, per_mille: u32) -> bool {
        self.below(1000) < per_mille
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf {
            *byte = self.next_u32() as u8;
        }
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = vec![0; len];
        self.fill(&mut out);
        out
    }
}

/// The RFC 3550 §5.1 fixed header with no CSRCs, extension or padding:
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |V=2|P|X|  CC   |M|     PT      |       sequence number         |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                           timestamp                           |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |           synchronization source (SSRC) identifier            |
/// +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
/// ```
pub fn rtp_header(marker: bool, payload_type: u8, seq: u16, timestamp: u32, ssrc: u32) -> Vec<u8> {
    let mut out = vec![0b1000_0000, (u8::from(marker) << 7) | payload_type];
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
    out
}

/// Samples as L16: network byte order (RFC 3551 §4.5.11).
pub fn l16_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_be_bytes()).collect()
}

/// L16 payload back to samples.
pub fn l16_samples(bytes: &[u8]) -> Vec<i16> {
    assert_eq!(bytes.len() % 2, 0, "odd L16 payload length");
    bytes
        .chunks_exact(2)
        .map(|pair| i16::from_be_bytes([pair[0], pair[1]]))
        .collect()
}

/// A bare-header L16 packet.
pub fn l16_packet(seq: u16, timestamp: u32, ssrc: u32, samples: &[i16]) -> Vec<u8> {
    let mut out = rtp_header(false, 96, seq, timestamp, ssrc);
    out.extend_from_slice(&l16_bytes(samples));
    out
}

/// Audio whose two bytes differ in almost every sample and span the whole
/// i16 range, so a byte-order mistake changes nearly every sample instead of
/// passing unnoticed.
pub fn test_signal(len: usize, offset: usize) -> Vec<i16> {
    (0..len)
        .map(|k| {
            ((k + offset) as u32)
                .wrapping_mul(7919)
                .wrapping_add(0x1234) as i16
        })
        .collect()
}

/// Split a fixture file: each packet prefixed with its length as a big
/// endian u16 (see `tests/fixtures/README.md`).
pub fn split_fixture(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut packets = Vec::new();
    while !bytes.is_empty() {
        let len = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
        packets.push(&bytes[2..2 + len]);
        bytes = &bytes[2 + len..];
    }
    packets
}
