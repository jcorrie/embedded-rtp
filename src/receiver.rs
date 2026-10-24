//! One incoming stream: statistics and jitter buffer together.

use crate::jitter::{Config, Counters, JitterBuffer, Playout, Push};
use crate::packet::Packet;
use crate::red;
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
    /// A redundant payload (RFC 2198) whose headers run past its end.
    /// Dropped.
    Malformed,
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
///
/// Set up with [`with_redundancy`](Self::with_redundancy), it also takes
/// redundant packets (RFC 2198, see [`red`]) and recovers lost frames from
/// them.
pub struct Receiver<const SLOTS: usize, const FRAME: usize> {
    payload_type: u8,
    frame_ticks: u32,
    /// The `red` payload type, if redundant packets are expected.
    red_payload_type: Option<u8>,
    /// Frames added to the target depth while redundant packets arrive.
    red_wait: u8,
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
            red_payload_type: None,
            red_wait: 0,
            ssrc: None,
            last_sr: None,
            stats: SourceStats::new(),
            buffer: JitterBuffer::new(config),
            warmup: 0,
        }
    }

    /// Also take redundant packets (RFC 2198) of `payload_type`, whose
    /// primary frames are of the receiver's own payload type, and recover
    /// lost frames from their redundant blocks. Plain packets are still
    /// taken, so a peer that doesn't send redundancy works as before.
    ///
    /// A lost frame is only recovered if the packet carrying it arrives
    /// before the frame's turn to play, so while redundant packets arrive
    /// the target depth is raised by `wait` frames: 1 for redundancy depth
    /// 1, 2 for depth 2 and so on, at that many frame periods of latency.
    /// With `wait` at 0, only frames whose next packet happens to be in
    /// time are recovered. `max_depth` limits the total.
    #[must_use]
    pub const fn with_redundancy(mut self, payload_type: u8, wait: u8) -> Self {
        self.red_payload_type = Some(payload_type);
        self.red_wait = wait;
        self
    }

    /// Take a packet. `arrival` is the local receive time in RTP clock ticks,
    /// from any origin; only differences matter.
    ///
    /// Packets from a source still on probation (RFC 3550 A.1) are buffered
    /// anyway: they have already passed the version, payload type and size
    /// checks, and dropping the start of every stream would be audible.
    ///
    /// For a redundant packet, the result is the primary frame's. A
    /// redundant block is used if its frame is missing, still due to play,
    /// and its offset is `k` whole frames, `k` at most `SLOTS`: it is then
    /// taken to be frame `sequence - k`. That holds as long as the sender,
    /// like [`red::Encoder`], never carries a frame across a silence.
    pub fn receive(&mut self, packet: &Packet<'_>, arrival: u32) -> Received {
        let header = packet.header;
        let (primary, redundant) = if Some(header.payload_type) == self.red_payload_type {
            let Ok(red) = red::Payload::parse(packet.payload) else {
                return Received::Malformed;
            };
            if red.payload_type != self.payload_type {
                return Received::WrongPayloadType;
            }
            (red.primary, Some(red))
        } else if header.payload_type == self.payload_type {
            (packet.payload, None)
        } else {
            return Received::WrongPayloadType;
        };
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
        let wait = if redundant.is_some() {
            self.red_wait
        } else {
            0
        };
        if self.warmup < WARMUP_PACKETS {
            self.warmup += 1;
            if wait > 0 {
                let initial = self.buffer.config().initial_depth;
                self.buffer.set_target(initial.saturating_add(wait));
            }
        } else {
            self.buffer
                .set_target_from_jitter(self.stats.jitter(), self.frame_ticks);
            if wait > 0 {
                self.buffer
                    .set_target(self.buffer.target().saturating_add(wait));
            }
        }
        let received = self.buffer.push(header.sequence, primary).into();
        if let Some(red) = redundant {
            for block in red.blocks() {
                if block.payload_type != self.payload_type {
                    continue;
                }
                if let Some(back) = self.frames_in(block.timestamp_offset) {
                    self.buffer
                        .recover(header.sequence.wrapping_sub(back), block.data);
                }
            }
        }
        received
    }

    /// `ticks` as a whole number of frames, from 1 to `SLOTS`; anything
    /// else can't be placed in the buffer. Counts up rather than dividing,
    /// for the Cortex-M0+.
    fn frames_in(&self, ticks: u16) -> Option<u16> {
        let ticks = u32::from(ticks);
        if self.frame_ticks == 0 {
            return None;
        }
        let mut frames = 0u16;
        let mut covered = 0u32;
        while covered < ticks && usize::from(frames) < SLOTS {
            covered += self.frame_ticks;
            frames += 1;
        }
        (frames > 0 && covered == ticks).then_some(frames)
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
