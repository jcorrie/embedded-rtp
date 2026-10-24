//! Redundant audio (RFC 2198): each packet also carries the frame or frames
//! before it, so an isolated loss is recovered exactly from the next packet
//! instead of being concealed.
//!
//! A redundant payload has its own dynamic payload type (in SDP,
//! `a=rtpmap:97 red/16000/1` and `a=fmtp:97 96/96`). It starts with a
//! 4-byte header per redundant block and a 1-byte header for the primary,
//! then the blocks' data in the same order, then the primary frame:
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |1|   block PT  |  timestamp offset         |   block length    |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |0|   block PT  |  (primary: no offset or length)
//! +-+-+-+-+-+-+-+-+
//! ```
//!
//! - Sending: an [`Encoder`] remembers the last frames sent and writes each
//!   payload with them; a [`Sender`](crate::Sender) set up with the `red`
//!   payload type sends it.
//! - Receiving: [`Receiver::with_redundancy`](crate::Receiver::with_redundancy)
//!   takes both plain and redundant packets and fills frames missing from
//!   its jitter buffer from the redundant blocks.
//! - [`Payload`] and [`write`](fn@write) read and write the format directly.
//!
//! The cost is bandwidth: at depth 1 every frame is sent twice. A frame
//! can only be recovered if the next packet arrives before its turn to
//! play, so the receiver also waits a frame longer.

use crate::Error;

/// Size of a redundant block's header.
pub const BLOCK_HEADER_LEN: usize = 4;
/// Largest timestamp offset a block header can hold (14 bits).
pub const MAX_OFFSET: u32 = 0x3fff;
/// Largest block a block header can describe (10 bits).
pub const MAX_BLOCK_LEN: usize = 0x3ff;

/// A redundant block: an earlier frame, carried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block<'a> {
    /// The frame's own payload type, 0..=127.
    pub payload_type: u8,
    /// How far before the packet's timestamp the frame starts, in RTP clock
    /// ticks.
    pub timestamp_offset: u16,
    /// The frame's payload.
    pub data: &'a [u8],
}

/// A redundant payload borrowed from a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Payload<'a> {
    /// The primary frame's payload type, 0..=127.
    pub payload_type: u8,
    /// The primary frame: the one the packet's own timestamp and sequence
    /// number describe.
    pub primary: &'a [u8],
    /// The redundant blocks' headers.
    headers: &'a [u8],
    /// The redundant blocks' data, back to back.
    data: &'a [u8],
}

impl<'a> Payload<'a> {
    /// Parse an RTP payload carried with the `red` payload type. Fails with
    /// [`Error::Truncated`] if the headers, or the block lengths they give,
    /// run past the end.
    pub fn parse(payload: &'a [u8]) -> Result<Self, Error> {
        let mut at = 0;
        let mut data_len = 0;
        loop {
            let first = *payload.get(at).ok_or(Error::Truncated)?;
            if first & 0x80 == 0 {
                break;
            }
            let header = payload
                .get(at..at + BLOCK_HEADER_LEN)
                .ok_or(Error::Truncated)?;
            data_len += block_len(header);
            at += BLOCK_HEADER_LEN;
        }
        let headers = &payload[..at];
        let payload_type = payload[at] & 0x7f;
        let rest = &payload[at + 1..];
        if data_len > rest.len() {
            return Err(Error::Truncated);
        }
        let (data, primary) = rest.split_at(data_len);
        Ok(Self {
            payload_type,
            primary,
            headers,
            data,
        })
    }

    /// The redundant blocks, in the order they were written (usually oldest
    /// first).
    pub fn blocks(&self) -> impl Iterator<Item = Block<'a>> + 'a {
        let mut data = self.data;
        self.headers
            .chunks_exact(BLOCK_HEADER_LEN)
            .map(move |header| {
                let (block, rest) = data.split_at(block_len(header));
                data = rest;
                Block {
                    payload_type: header[0] & 0x7f,
                    timestamp_offset: (u16::from(header[1]) << 6) | u16::from(header[2] >> 2),
                    data: block,
                }
            })
    }
}

fn block_len(header: &[u8]) -> usize {
    (usize::from(header[2] & 0x03) << 8) | usize::from(header[3])
}

/// Write a redundant payload: `blocks`, then the primary frame of type
/// `payload_type`. Returns its length.
///
/// Fails with [`Error::TooLong`] if a block's offset or length does not fit
/// its header, and with [`Error::BufferTooSmall`] if `out` cannot hold it.
pub fn write(
    blocks: &[Block<'_>],
    payload_type: u8,
    primary: &[u8],
    out: &mut [u8],
) -> Result<usize, Error> {
    let mut len = 1 + primary.len();
    for block in blocks {
        if u32::from(block.timestamp_offset) > MAX_OFFSET || block.data.len() > MAX_BLOCK_LEN {
            return Err(Error::TooLong);
        }
        len += BLOCK_HEADER_LEN + block.data.len();
    }
    let out = out.get_mut(..len).ok_or(Error::BufferTooSmall)?;
    let mut at = 0;
    for block in blocks {
        let offset = block.timestamp_offset;
        let block_len = block.data.len();
        out[at] = 0x80 | (block.payload_type & 0x7f);
        out[at + 1] = (offset >> 6) as u8;
        out[at + 2] = ((offset << 2) as u8) | (block_len >> 8) as u8;
        out[at + 3] = block_len as u8;
        at += BLOCK_HEADER_LEN;
    }
    out[at] = payload_type & 0x7f;
    at += 1;
    for block in blocks {
        out[at..at + block.data.len()].copy_from_slice(block.data);
        at += block.data.len();
    }
    out[at..].copy_from_slice(primary);
    Ok(len)
}

#[derive(Clone, Copy)]
struct Past<const FRAME: usize> {
    sent: bool,
    timestamp: u32,
    len: u16,
    data: [u8; FRAME],
}

/// Writes each frame together with the `DEPTH` frames sent before it, for
/// frames of up to `FRAME` bytes. Memory is roughly `DEPTH * FRAME` bytes.
///
/// A frame is carried again only if it was sent exactly `k` frames earlier
/// on the media clock, for `k` up to `DEPTH`. So after a silence, or at the
/// start, the first packets carry less or no redundancy: the receiver maps
/// an offset of `k` frames to `k` sequence numbers back, which is only right
/// if no frame period between them went unsent.
///
/// ```
/// use embedded_rtp::red::Encoder;
/// use embedded_rtp::packet::HEADER_LEN;
/// use embedded_rtp::Sender;
///
/// const RED: u8 = 97;
/// let mut encoder: Encoder<1, 640> = Encoder::new(96, 320);
/// let mut sender = Sender::new(0x1234_5678, RED, 320, 0, 0);
/// let mut buf = [0u8; HEADER_LEN + Encoder::<1, 640>::MAX_PAYLOAD];
/// # let frame = [0u8; 640];
/// // Every 20 ms:
/// let len = encoder.encode(sender.timestamp(), &frame, &mut buf[HEADER_LEN..])?;
/// let len = sender.send_in_place(&mut buf, len)?;
/// // Send buf[..len].
/// # Ok::<(), embedded_rtp::Error>(())
/// ```
pub struct Encoder<const DEPTH: usize, const FRAME: usize> {
    payload_type: u8,
    frame_ticks: u32,
    /// The last frames sent, a ring; `newest` is the latest.
    past: [Past<FRAME>; DEPTH],
    newest: usize,
}

impl<const DEPTH: usize, const FRAME: usize> Encoder<DEPTH, FRAME> {
    /// The largest payload [`encode`](Self::encode) writes: `DEPTH` blocks
    /// with their headers, the primary header and the primary.
    pub const MAX_PAYLOAD: usize = DEPTH * (BLOCK_HEADER_LEN + FRAME) + 1 + FRAME;

    /// An encoder for frames of `payload_type`, each lasting `frame_ticks`
    /// of the RTP clock, with nothing sent yet.
    ///
    /// # Panics
    ///
    /// If `DEPTH` is zero, `FRAME` is over [`MAX_BLOCK_LEN`], or `DEPTH`
    /// frames are longer than [`MAX_OFFSET`] ticks. In a `const` or `static`
    /// these are compile errors.
    #[must_use]
    pub const fn new(payload_type: u8, frame_ticks: u32) -> Self {
        assert!(DEPTH >= 1, "DEPTH must be at least one");
        assert!(FRAME <= MAX_BLOCK_LEN, "FRAME must be at most 1023 bytes");
        assert!(
            (DEPTH as u64) * (frame_ticks as u64) <= MAX_OFFSET as u64,
            "DEPTH frames must span at most 16383 ticks"
        );
        Self {
            payload_type: payload_type & 0x7f,
            frame_ticks,
            past: [Past {
                sent: false,
                timestamp: 0,
                len: 0,
                data: [0; FRAME],
            }; DEPTH],
            newest: 0,
        }
    }

    /// Write the payload for `primary`, the frame starting at `timestamp`,
    /// into `out` and remember the frame for the next packets. Returns the
    /// payload's length, at most [`MAX_PAYLOAD`](Self::MAX_PAYLOAD).
    ///
    /// Nothing changes on an error: [`Error::BufferTooSmall`] if `primary`
    /// is longer than `FRAME` or `out` cannot hold the payload.
    pub fn encode(
        &mut self,
        timestamp: u32,
        primary: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Error> {
        if primary.len() > FRAME {
            return Err(Error::BufferTooSmall);
        }
        let mut blocks = [Block {
            payload_type: 0,
            timestamp_offset: 0,
            data: &[],
        }; DEPTH];
        let mut count = 0;
        // Oldest first, as RFC 2198's examples order them. The frame sent k
        // packets ago goes in only if it is also k frames ago on the media
        // clock, so that no frame period between went unsent.
        for k in (1..=DEPTH).rev() {
            let past = &self.past[(self.newest + DEPTH + 1 - k) % DEPTH];
            let offset = k as u32 * self.frame_ticks;
            if past.sent && past.timestamp == timestamp.wrapping_sub(offset) {
                blocks[count] = Block {
                    payload_type: self.payload_type,
                    timestamp_offset: offset as u16,
                    data: &past.data[..usize::from(past.len)],
                };
                count += 1;
            }
        }
        let len = write(&blocks[..count], self.payload_type, primary, out)?;

        self.newest = (self.newest + 1) % DEPTH;
        let past = &mut self.past[self.newest];
        past.sent = true;
        past.timestamp = timestamp;
        past.len = primary.len() as u16;
        past.data[..primary.len()].copy_from_slice(primary);
        Ok(len)
    }

    /// Forget the frames sent so far, as for a new stream.
    pub fn reset(&mut self) {
        for past in &mut self.past {
            past.sent = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let blocks = [
            Block {
                payload_type: 96,
                timestamp_offset: 640,
                data: &[1, 2, 3],
            },
            Block {
                payload_type: 0,
                timestamp_offset: MAX_OFFSET as u16,
                data: &[4; MAX_BLOCK_LEN],
            },
        ];
        let mut out = [0u8; 2 * BLOCK_HEADER_LEN + 1 + 3 + MAX_BLOCK_LEN + 2];
        let len = write(&blocks, 96, &[5, 6], &mut out).unwrap();
        assert_eq!(len, out.len());
        let payload = Payload::parse(&out).unwrap();
        assert_eq!(payload.payload_type, 96);
        assert_eq!(payload.primary, &[5, 6]);
        assert!(payload.blocks().eq(blocks));
    }

    #[test]
    fn no_blocks() {
        let mut out = [0u8; 3];
        assert_eq!(write(&[], 96, &[7, 8], &mut out), Ok(3));
        assert_eq!(out, [96, 7, 8]);
        let payload = Payload::parse(&out).unwrap();
        assert_eq!(payload.primary, &[7, 8]);
        assert_eq!(payload.blocks().count(), 0);
    }

    #[test]
    fn rejects_what_does_not_fit() {
        let mut out = [0u8; 2048];
        let too_far = Block {
            payload_type: 96,
            timestamp_offset: MAX_OFFSET as u16 + 1,
            data: &[],
        };
        assert_eq!(write(&[too_far], 96, &[], &mut out), Err(Error::TooLong));
        let too_long = Block {
            payload_type: 96,
            timestamp_offset: 0,
            data: &[0; MAX_BLOCK_LEN + 1],
        };
        assert_eq!(write(&[too_long], 96, &[], &mut out), Err(Error::TooLong));
        assert_eq!(
            write(&[], 96, &[0; 4], &mut out[..4]),
            Err(Error::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(Payload::parse(&[]), Err(Error::Truncated));
        // A block header with no primary header after it.
        assert_eq!(Payload::parse(&[0x80 | 96, 0, 0, 0]), Err(Error::Truncated));
        // A block header cut short.
        assert_eq!(Payload::parse(&[0x80 | 96, 0, 0]), Err(Error::Truncated));
        // A block longer than what follows.
        assert_eq!(
            Payload::parse(&[0x80 | 96, 0, 0, 2, 96, 1]),
            Err(Error::Truncated)
        );
        assert!(Payload::parse(&[0x80 | 96, 0, 0, 2, 96, 1, 2]).is_ok());
    }

    #[test]
    fn usable_in_a_static() {
        static ENCODER: Encoder<2, 640> = Encoder::new(96, 320);
        assert_eq!(ENCODER.frame_ticks, 320);
    }
}
