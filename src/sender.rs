//! Outgoing stream bookkeeping (RFC 3550 §5.1, RFC 3551 §4.1).

use crate::packet::{self, Header, HEADER_LEN};
use crate::rtcp::SenderInfo;
use crate::Error;

/// Packetizes one outgoing stream of fixed-duration frames.
///
/// Sequence numbers count packets sent. Timestamps count media time, so they
/// keep advancing through silence: call [`Sender::skip`] for every frame
/// period that sends nothing. The first packet after a silence (and the very
/// first packet) carries the marker bit, as RFC 3551 §4.1 asks of audio
/// senders that suppress silence.
#[derive(Debug, Clone)]
pub struct Sender {
    ssrc: u32,
    payload_type: u8,
    frame_ticks: u32,
    sequence: u16,
    timestamp: u32,
    talkspurt_start: bool,
    packets: u32,
    octets: u32,
}

impl Sender {
    /// A sender for `ssrc` and `payload_type`, whose frames each last
    /// `frame_ticks` of the RTP clock.
    ///
    /// RFC 3550 §5.1 asks for a random SSRC and random starting sequence
    /// number and timestamp. There is no random number generator in `core`,
    /// so the caller supplies them.
    #[must_use]
    pub const fn new(
        ssrc: u32,
        payload_type: u8,
        frame_ticks: u32,
        initial_sequence: u16,
        initial_timestamp: u32,
    ) -> Self {
        Self {
            ssrc,
            payload_type: payload_type & 0x7f,
            frame_ticks,
            sequence: initial_sequence,
            timestamp: initial_timestamp,
            talkspurt_start: true,
            packets: 0,
            octets: 0,
        }
    }

    /// Write one frame as a packet into `buf`, returning its length, and
    /// advance to the next frame. Nothing advances if `buf` is too small.
    pub fn send(&mut self, payload: &[u8], buf: &mut [u8]) -> Result<usize, Error> {
        let len = packet::write(&self.header(), payload, buf)?;
        self.advance(payload.len());
        Ok(len)
    }

    /// Like [`send`](Self::send), for a payload already in place at
    /// `buf[HEADER_LEN..HEADER_LEN + payload_len]`: only the header is
    /// written, saving a copy of the payload. Capture audio (or point a DMA
    /// transfer) straight into the packet buffer after its header.
    pub fn send_in_place(&mut self, buf: &mut [u8], payload_len: usize) -> Result<usize, Error> {
        let len = HEADER_LEN + payload_len;
        if buf.len() < len {
            return Err(Error::BufferTooSmall);
        }
        self.header().write(buf)?;
        self.advance(payload_len);
        Ok(len)
    }

    fn header(&self) -> Header {
        Header {
            marker: self.talkspurt_start,
            payload_type: self.payload_type,
            sequence: self.sequence,
            timestamp: self.timestamp,
            ssrc: self.ssrc,
        }
    }

    fn advance(&mut self, payload_len: usize) {
        self.talkspurt_start = false;
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.frame_ticks);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload_len as u32);
    }

    /// Let one frame period pass without sending: the timestamp advances, the
    /// sequence number does not, and the next packet starts a talkspurt.
    pub fn skip(&mut self) {
        self.timestamp = self.timestamp.wrapping_add(self.frame_ticks);
        self.talkspurt_start = true;
    }

    /// The stream's SSRC.
    #[must_use]
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Sequence number the next packet will carry.
    #[must_use]
    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    /// Timestamp the next packet will carry.
    #[must_use]
    pub fn timestamp(&self) -> u32 {
        self.timestamp
    }

    /// Packets sent, wrapping (the RTCP sender's packet count).
    #[must_use]
    pub fn packet_count(&self) -> u32 {
        self.packets
    }

    /// Payload octets sent, excluding headers, wrapping (the RTCP sender's
    /// octet count).
    #[must_use]
    pub fn octet_count(&self) -> u32 {
        self.octets
    }

    /// The sender information for an RTCP sender report sent at
    /// `ntp_timestamp`. The RTP timestamp is the next frame's, which is
    /// "now" on the media clock when called at a frame boundary.
    #[must_use]
    pub fn sender_info(&self, ntp_timestamp: u64) -> SenderInfo {
        SenderInfo {
            ntp_timestamp,
            rtp_timestamp: self.timestamp,
            packet_count: self.packets,
            octet_count: self.octets,
        }
    }
}
