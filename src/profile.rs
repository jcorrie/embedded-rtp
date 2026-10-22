//! An intercom media format.
//!
//! L16 (RFC 3551 §4.5.11: big endian PCM16), mono, 16 kHz, 20 ms per packet,
//! on a dynamic payload type. In SDP terms `a=rtpmap:96 L16/16000/1` and
//! `a=ptime:20`, so Wireshark, ffmpeg and GStreamer decode it as is.
//!
//! Nothing else in the crate depends on these values; they are here so the
//! server and devices agree on them.

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
