//! RTP packets (RFC 3550 §5.1).
//!
//! Parsing borrows from the datagram, so reading a packet copies nothing.

use crate::{be16, be32, Error};

/// The only RTP version there is.
pub const VERSION: u8 = 2;
/// Size of the fixed header, before any CSRCs or extension.
pub const HEADER_LEN: usize = 12;

/// The fixed RTP header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    /// For audio, set on the first packet of a talkspurt.
    pub marker: bool,
    /// Payload type, 0..=127.
    pub payload_type: u8,
    /// Increments by one per packet, wrapping.
    pub sequence: u16,
    /// Sampling instant of the first sample, in clock-rate units.
    pub timestamp: u32,
    /// Identifies the stream.
    pub ssrc: u32,
}

impl Header {
    /// Write a bare 12-byte header - no padding, extension or CSRCs.
    pub fn write(&self, buf: &mut [u8]) -> Result<usize, Error> {
        let out = buf.get_mut(..HEADER_LEN).ok_or(Error::BufferTooSmall)?;
        out[0] = VERSION << 6;
        out[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7f);
        out[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        Ok(HEADER_LEN)
    }
}

/// A header extension (RFC 3550 §5.3.1). Carried through, not interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extension<'a> {
    /// Profile-defined identifier, e.g. `0xBEDE` for RFC 8285.
    pub profile: u16,
    /// The extension body, a whole number of 32-bit words.
    pub data: &'a [u8],
}

/// An RTP packet borrowed from a receive buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    /// The fixed header.
    pub header: Header,
    /// Header extension, if the X bit was set.
    pub extension: Option<Extension<'a>>,
    /// The media, with any padding already removed.
    pub payload: &'a [u8],
    csrc: &'a [u8],
}

impl<'a> Packet<'a> {
    /// Parse a datagram. Checks the structure only; whether the payload type
    /// or size is wanted is up to the caller.
    pub fn parse(buf: &'a [u8]) -> Result<Self, Error> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Truncated);
        }
        let first = buf[0];
        if first >> 6 != VERSION {
            return Err(Error::BadVersion);
        }
        let padded = first & 0x20 != 0;
        let extended = first & 0x10 != 0;
        let csrc_count = usize::from(first & 0x0f);

        let header = Header {
            marker: buf[1] & 0x80 != 0,
            payload_type: buf[1] & 0x7f,
            sequence: be16(buf, 2),
            timestamp: be32(buf, 4),
            ssrc: be32(buf, 8),
        };

        let mut offset = HEADER_LEN + 4 * csrc_count;
        let csrc = buf.get(HEADER_LEN..offset).ok_or(Error::Truncated)?;

        let extension = if extended {
            let head = buf.get(offset..offset + 4).ok_or(Error::Truncated)?;
            let words = usize::from(be16(head, 2));
            let start = offset + 4;
            offset = start + 4 * words;
            Some(Extension {
                profile: be16(head, 0),
                data: buf.get(start..offset).ok_or(Error::Truncated)?,
            })
        } else {
            None
        };

        let mut end = buf.len();
        if padded {
            // The last byte counts the padding, itself included.
            let padding = usize::from(buf[end - 1]);
            if padding == 0 || offset + padding > end {
                return Err(Error::BadPadding);
            }
            end -= padding;
        }

        Ok(Self {
            header,
            extension,
            payload: &buf[offset..end],
            csrc,
        })
    }

    /// Contributing sources, for packets a mixer produced.
    pub fn csrcs(&self) -> impl Iterator<Item = u32> + 'a {
        self.csrc.chunks_exact(4).map(|word| be32(word, 0))
    }
}

/// Write a header and payload into `buf`, returning the packet length.
pub fn write(header: &Header, payload: &[u8], buf: &mut [u8]) -> Result<usize, Error> {
    let len = HEADER_LEN + payload.len();
    let out = buf.get_mut(..len).ok_or(Error::BufferTooSmall)?;
    header.write(out)?;
    out[HEADER_LEN..].copy_from_slice(payload);
    Ok(len)
}

/// Whether a datagram is RTCP rather than RTP, for when both share a port
/// (RFC 5761 §4). RTCP packet types land the second byte in 192..=223, a range
/// RTP payload types are kept out of for exactly this reason.
#[must_use]
pub fn is_rtcp(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] >> 6 == VERSION && (192..=223).contains(&buf[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let header = Header {
            marker: true,
            payload_type: 96,
            sequence: 0xfffe,
            timestamp: 0xdead_beef,
            ssrc: 0x0102_0304,
        };
        let mut buf = [0u8; 32];
        let len = write(&header, &[1, 2, 3, 4], &mut buf).unwrap();
        assert_eq!(len, 16);
        assert_eq!(buf[0], 0x80);
        assert_eq!(buf[1], 0x80 | 96);

        let packet = Packet::parse(&buf[..len]).unwrap();
        assert_eq!(packet.header, header);
        assert_eq!(packet.payload, &[1, 2, 3, 4]);
        assert_eq!(packet.extension, None);
        assert_eq!(packet.csrcs().count(), 0);
        assert!(!is_rtcp(&buf[..len]));
    }

    #[test]
    fn csrc_extension_and_padding() {
        let mut buf = [0u8; 12 + 8 + 8 + 3 + 1];
        buf[0] = 0x80 | 0x20 | 0x10 | 2; // padding, extension, two CSRCs
        buf[1] = 96;
        buf[12..16].copy_from_slice(&7u32.to_be_bytes());
        buf[16..20].copy_from_slice(&9u32.to_be_bytes());
        buf[20..22].copy_from_slice(&0xbedeu16.to_be_bytes());
        buf[22..24].copy_from_slice(&1u16.to_be_bytes());
        buf[24..28].copy_from_slice(&[0xaa; 4]);
        buf[28] = 0x55; // payload
        buf[29..32].copy_from_slice(&[0, 0, 3]); // three bytes of padding

        let packet = Packet::parse(&buf).unwrap();
        let mut csrcs = packet.csrcs();
        assert_eq!(csrcs.next(), Some(7));
        assert_eq!(csrcs.next(), Some(9));
        assert_eq!(csrcs.next(), None);
        let extension = packet.extension.unwrap();
        assert_eq!(extension.profile, 0xbede);
        assert_eq!(extension.data, &[0xaa; 4]);
        assert_eq!(packet.payload, &[0x55]);
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(Packet::parse(&[0x80; 11]), Err(Error::Truncated));
        assert_eq!(Packet::parse(&[0x40; 12]), Err(Error::BadVersion));

        let mut csrc = [0u8; 12];
        csrc[0] = 0x81; // claims a CSRC that is not there
        assert_eq!(Packet::parse(&csrc), Err(Error::Truncated));

        let mut padding = [0u8; 13];
        padding[0] = 0xa0;
        padding[12] = 2; // more padding than payload
        assert_eq!(Packet::parse(&padding), Err(Error::BadPadding));
        padding[12] = 0;
        assert_eq!(Packet::parse(&padding), Err(Error::BadPadding));
    }

    #[test]
    fn small_buffer() {
        let mut buf = [0u8; 13];
        assert_eq!(
            write(&Header::default(), &[0; 2], &mut buf),
            Err(Error::BufferTooSmall)
        );
    }

    #[test]
    fn rtcp_demux() {
        assert!(is_rtcp(&[0x80, 200]));
        assert!(is_rtcp(&[0x81, 201]));
        assert!(!is_rtcp(&[0x80, 96]));
        assert!(!is_rtcp(&[0x80, 0x80 | 96]));
        assert!(!is_rtcp(&[0x80]));
    }
}
