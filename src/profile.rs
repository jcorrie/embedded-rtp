//! An intercom media format.
//!
//! L16 (RFC 3551 §4.5.11: big endian PCM16), mono, 16 kHz, 20 ms per packet,
//! on a dynamic payload type. In SDP terms `a=rtpmap:96 L16/16000/1` and
//! `a=ptime:20`, so Wireshark, ffmpeg and GStreamer decode it as is.
//!
//! [`Codec`] switches to G.722 (`a=rtpmap:9 G722/8000`), which carries the
//! same 16 kHz audio in a quarter of the bandwidth.
//!
//! Nothing else in the crate depends on these values; they are here so the
//! server and devices agree on them.

use crate::g722;
use crate::packet::HEADER_LEN;

/// Dynamic payload type for L16/16000/1.
pub const PAYLOAD_TYPE: u8 = 96;
/// RTP clock rate, which for L16 is the sample rate.
pub const CLOCK_RATE: u32 = 16_000;
/// Samples in one 20 ms frame, and so the timestamp step between packets.
pub const FRAME_SAMPLES: usize = 320;
/// Payload bytes in one frame.
pub const FRAME_BYTES: usize = FRAME_SAMPLES * 2;
/// One packet as the intercom sends it: a bare header plus one frame.
pub const PACKET_BYTES: usize = HEADER_LEN + FRAME_BYTES;

/// How a 20 ms frame of 16 kHz mono audio goes on the wire.
///
/// ```
/// use embedded_rtp::profile::Codec;
///
/// let codec = Codec::G722;
/// assert_eq!(codec.payload_type(), 9);
/// assert_eq!(codec.frame_ticks(), 160); // the RTP clock runs at 8 kHz
/// assert_eq!(codec.frame_bytes(), 160);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Codec {
    /// L16: 640 bytes a frame, 256 kbit/s. The default.
    #[default]
    L16,
    /// G.722 at 64 kbit/s: 160 bytes a frame. See [`g722`].
    G722,
}

impl Codec {
    /// The RTP payload type.
    #[must_use]
    pub const fn payload_type(self) -> u8 {
        match self {
            Self::L16 => PAYLOAD_TYPE,
            Self::G722 => g722::PAYLOAD_TYPE,
        }
    }

    /// The RTP clock rate. For G.722 it is half the sample rate (RFC 3551
    /// §4.5.2).
    #[must_use]
    pub const fn clock_rate(self) -> u32 {
        match self {
            Self::L16 => CLOCK_RATE,
            Self::G722 => g722::CLOCK_RATE,
        }
    }

    /// RTP clock ticks in one frame: the timestamp step between packets,
    /// and what `Sender` and `Receiver` take as `frame_ticks`.
    #[must_use]
    pub const fn frame_ticks(self) -> u32 {
        match self {
            Self::L16 => FRAME_SAMPLES as u32,
            Self::G722 => FRAME_SAMPLES as u32 / 2,
        }
    }

    /// Payload bytes in one frame.
    #[must_use]
    pub const fn frame_bytes(self) -> usize {
        match self {
            Self::L16 => FRAME_BYTES,
            Self::G722 => FRAME_SAMPLES / 2,
        }
    }

    /// A bare-header packet of one frame.
    #[must_use]
    pub const fn packet_bytes(self) -> usize {
        HEADER_LEN + self.frame_bytes()
    }
}
