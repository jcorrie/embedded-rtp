//! One incoming stream: statistics and jitter buffer together.

use crate::jitter::{Config, Counters, JitterBuffer, Playout, Push};
use crate::packet::Packet;
use crate::rtcp::{self, ReportBlock, SenderInfo};
use crate::stats::{Report, SeqStatus, SourceStats};

/// Packets the jitter estimate needs before it steers the buffer. The A.8
/// estimate moves 1/16 of the way per packet, so it means little before this.
const WARMUP_PACKETS: u8 = 16;

/// What happened to a received packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// Buffered for playout.
    Accepted,
    /// Not the payload type this receiver plays. Dropped.
    WrongPayloadType,
    /// A sequence number jump not (yet) confirmed as a restart (RFC 3550
    /// A.1). Dropped.
    Invalid,
    /// Its turn to play has passed. Dropped.
    Late,
    /// Already buffered. Dropped.
    Duplicate,
    /// Larger than a frame slot. Dropped.
    Oversized,
}

impl From<Push> for Received {
    fn from(push: Push) -> Self {
        match push {
            Push::Accepted => Self::Accepted,
            Push::Late => Self::Late,
            Push::Duplicate => Self::Duplicate,
            Push::Oversized => Self::Oversized,
        }
    }
}

/// Receives one RTP stream and plays it out at a steady frame rate.
///
/// Feed every packet from the remote end to [`Receiver::receive`] as it
/// arrives, and call [`Receiver::play`] once per frame period. A new SSRC, or
/// a sequence number jump confirmed as a sender restart, starts the stream
/// afresh.
pub struct Receiver<const SLOTS: usize, const FRAME: usize> {
    payload_type: u8,
    frame_ticks: u32,
    ssrc: Option<u32>,
    /// The last sender report from the remote end: its SSRC, the NTP short
    /// form of its timestamp, and when it arrived (NTP short, our clock).
    last_sr: Option<(u32, u32, u32)>,
    stats: SourceStats,
    buffer: JitterBuffer<SLOTS, FRAME>,
    warmup: u8,
}

impl<const SLOTS: usize, const FRAME: usize> Receiver<SLOTS, FRAME> {
    /// A receiver for `payload_type`, whose frames each last `frame_ticks`
    /// of the RTP clock. See [`JitterBuffer::new`] for the panics.
    #[must_use]
    pub const fn new(payload_type: u8, frame_ticks: u32, config: Config) -> Self {
        Self {
            payload_type,
            frame_ticks,
            ssrc: None,
            last_sr: None,
            stats: SourceStats::new(),
            buffer: JitterBuffer::new(config),
            warmup: 0,
        }
    }

    /// Take a packet. `arrival` is the local receive time in RTP clock ticks,
    /// from any origin; only differences matter.
    ///
    /// Packets from a source still on probation (RFC 3550 A.1) are buffered
    /// anyway: they have already passed the version, payload type and size
    /// checks, and dropping the start of every stream would be audible.
    pub fn receive(&mut self, packet: &Packet<'_>, arrival: u32) -> Received {
        let header = packet.header;
        if header.payload_type != self.payload_type {
            return Received::WrongPayloadType;
        }
        if self.ssrc != Some(header.ssrc) {
            self.ssrc = Some(header.ssrc);
            self.stats = SourceStats::new();
            self.buffer.reset();
            self.warmup = 0;
        }
        match self.stats.update(header.sequence) {
            SeqStatus::Invalid => return Received::Invalid,
            SeqStatus::Restarted => {
                self.buffer.reset();
                self.warmup = 0;
            }
            SeqStatus::Valid | SeqStatus::Probation => {}
        }
        self.stats.record_arrival(header.timestamp, arrival);
        if self.warmup < WARMUP_PACKETS {
            self.warmup += 1;
        } else {
            self.buffer
                .set_target_from_jitter(self.stats.jitter(), self.frame_ticks);
        }
        self.buffer.push(header.sequence, packet.payload).into()
    }

    /// The next frame period's output. Call exactly once per frame period.
    /// A frame's payload is borrowed from the jitter buffer, not copied.
    pub fn play(&mut self) -> Playout<'_> {
        self.buffer.pop()
    }

    /// The SSRC being received, once a packet has arrived.
    #[must_use]
    pub fn ssrc(&self) -> Option<u32> {
        self.ssrc
    }

    /// Reception statistics for the current stream.
    #[must_use]
    pub fn stats(&self) -> &SourceStats {
        &self.stats
    }

    /// Figures for an RTCP report block about the current stream, starting a
    /// new reporting interval. `None` before any packet has arrived.
    pub fn report(&mut self) -> Option<Report> {
        self.ssrc.map(|_| self.stats.report())
    }

    /// Note a sender report from the remote end, so the next report block
    /// can echo it (RFC 3550 §6.4.1) and the sender can work out the round
    /// trip. `now` is the local time in NTP short format (1/65536 s, see
    /// [`rtcp::ntp_from_micros`] and [`rtcp::ntp_short`]); any epoch works.
    pub fn sender_report(&mut self, ssrc: u32, info: &SenderInfo, now: u32) {
        self.last_sr = Some((ssrc, rtcp::ntp_short(info.ntp_timestamp), now));
    }

    /// An RTCP report block about the current stream, starting a new
    /// reporting interval, with the last-SR fields filled in if a sender
    /// report from this stream has arrived. `now` is on the same clock as for
    /// [`sender_report`](Self::sender_report). `None` before any packet.
    pub fn report_block(&mut self, now: u32) -> Option<ReportBlock> {
        let ssrc = self.ssrc?;
        let (last_sr, delay) = match self.last_sr {
            Some((from, last_sr, at)) if from == ssrc => (last_sr, now.wrapping_sub(at)),
            _ => (0, 0),
        };
        Some(ReportBlock::new(ssrc, &self.stats.report(), last_sr, delay))
    }

    /// The jitter buffer, for its depth, target and counters.
    #[must_use]
    pub fn buffer(&self) -> &JitterBuffer<SLOTS, FRAME> {
        &self.buffer
    }

    /// Jitter buffer totals.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.buffer.counters()
    }
}
