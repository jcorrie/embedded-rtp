//! RTP for microcontrollers, usable unchanged on a server.
//!
//! `no_std`, no allocator, no floating point (the RP2040 has no FPU) and no
//! dependencies unless the `srtp` feature is on. All state is fixed size and
//! can live in a `static`. The crate never touches sockets or clocks: callers
//! hand in datagrams and the current time, so it works the same under
//! embassy, RTIC, tokio or a bare loop.
//!
//! - [`packet`]: RTP packets (RFC 3550 §5) and RTP/RTCP demultiplexing on one
//!   port (RFC 5761).
//! - [`stats`]: sequence validation, loss and interarrival jitter (RFC 3550
//!   appendix A).
//! - [`jitter`]: a reordering, adaptive jitter buffer.
//! - [`Receiver`] and [`Sender`]: the pieces above wired together for one
//!   incoming and one outgoing stream - what a client actually uses.
//! - [`rtcp`]: sender and receiver reports, source descriptions and
//!   goodbyes (RFC 3550 §6).
//! - [`l16`]: the L16 payload format and packet loss concealment.
//! - [`red`]: redundant audio (RFC 2198), to recover lost frames exactly.
//! - [`profile`]: an intercom media format: L16, 16 kHz mono, 20 ms frames.
//! - `srtp` (with the `srtp` feature): SRTP and SRTCP (RFC 3711), to
//!   authenticate and encrypt both.
//!
//! See `docs/design.md` for the design, measurements and roadmap.

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod jitter;
pub mod l16;
pub mod packet;
pub mod profile;
mod receiver;
pub mod red;
pub mod rtcp;
mod sender;
#[cfg(feature = "srtp")]
pub mod srtp;
pub mod stats;

pub use receiver::{Received, Receiver};
pub use sender::Sender;

/// Why a packet could not be read or written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The packet ends before its headers say it should.
    Truncated,
    /// Not RTP version 2.
    BadVersion,
    /// The padding count is zero or longer than the packet, or an RTCP
    /// packet other than the last in a compound packet is padded.
    BadPadding,
    /// The output buffer cannot hold what is being written.
    BufferTooSmall,
    /// More items than a 5-bit count field can hold: over 31 report
    /// blocks, sources or chunks in one RTCP packet.
    TooMany,
    /// A value longer than its length field can hold: an SDES item or BYE
    /// reason over 255 bytes.
    TooLong,
    /// A compound RTCP packet breaks RFC 3550's structure rules: it doesn't
    /// start with an SR or RR, or an SDES item has type 0.
    BadCompound,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "packet truncated",
            Self::BadVersion => "not RTP version 2",
            Self::BadPadding => "invalid padding",
            Self::BufferTooSmall => "output buffer too small",
            Self::TooMany => "too many items for a count field",
            Self::TooLong => "value too long for its length field",
            Self::BadCompound => "malformed compound RTCP packet",
        })
    }
}

impl core::error::Error for Error {}

/// Read a big endian `u16` at `at`. Callers have already bounds checked.
fn be16(buf: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([buf[at], buf[at + 1]])
}

/// Read a big endian `u32` at `at`. Callers have already bounds checked.
fn be32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

// Compile the README's examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
