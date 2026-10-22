//! Reception statistics for one source (RFC 3550 appendix A).
//!
//! A transcription of the RFC's reference code: sequence validation (A.1),
//! loss accounting (A.3) and interarrival jitter (A.8). All integer, with the
//! jitter kept scaled by 16 exactly as the RFC suggests, so no FPU is needed.

const SEQ_MOD: u32 = 1 << 16;

/// Largest forward jump still treated as the same stream (A.1).
pub const MAX_DROPOUT: u16 = 3000;
/// Largest backward step still treated as reordering rather than a restart.
pub const MAX_MISORDER: u16 = 100;
/// In-order packets a new source must send before it is trusted.
pub const MIN_SEQUENTIAL: u8 = 2;

/// Jitter differences are clamped to this many clock ticks, so a garbage
/// timestamp cannot overflow the estimate. At 16 kHz this is over 15 minutes,
/// far beyond any real jitter.
const MAX_TRANSIT_DELTA: u32 = 1 << 24;

/// What a sequence number says about a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqStatus {
    /// In order, a tolerable jump forward, or a reordered or duplicate packet.
    Valid,
    /// The source has not yet sent [`MIN_SEQUENTIAL`] packets in a row.
    Probation,
    /// A large jump has been confirmed by the packet after it: the sender
    /// restarted. Statistics start again from this packet; anything buffered
    /// from before should be discarded.
    Restarted,
    /// A large jump not yet confirmed. Drop the packet.
    Invalid,
}

/// The numbers an RTCP report block carries about one source (RFC 3550
/// §6.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    /// Fraction of packets lost since the previous report, out of 256.
    pub fraction_lost: u8,
    /// Packets lost since the start, clamped to 24-bit signed. Negative when
    /// duplicates outnumber losses.
    pub cumulative_lost: i32,
    /// Highest sequence number seen, with wraparounds in the upper 16 bits.
    pub extended_highest: u32,
    /// Interarrival jitter, in RTP clock ticks.
    pub jitter: u32,
}

/// Sequence, loss and jitter tracking for one source.
#[derive(Debug, Clone)]
pub struct SourceStats {
    started: bool,
    max_seq: u16,
    /// Wraparound count, shifted into the upper 16 bits.
    cycles: u32,
    base_seq: u32,
    /// The sequence number a confirming packet must carry after a big jump.
    bad_seq: u32,
    probation: u8,
    received: u32,
    expected_prior: u32,
    received_prior: u32,
    /// Relative transit time of the previous packet, if there was one.
    transit: Option<u32>,
    /// Interarrival jitter scaled by 16.
    jitter: u32,
}

impl Default for SourceStats {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceStats {
    /// Statistics for a source not yet heard from.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            started: false,
            max_seq: 0,
            cycles: 0,
            base_seq: 0,
            bad_seq: SEQ_MOD + 1,
            probation: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            transit: None,
            jitter: 0,
        }
    }

    /// Start counting afresh from `seq` (A.1 `init_seq`).
    fn init(&mut self, seq: u16) {
        self.base_seq = u32::from(seq);
        self.max_seq = seq;
        self.bad_seq = SEQ_MOD + 1;
        self.cycles = 0;
        self.received = 0;
        self.received_prior = 0;
        self.expected_prior = 0;
        // Timestamps are unrelated across a restart.
        self.transit = None;
    }

    /// Account for a packet's sequence number (A.1 `update_seq`).
    pub fn update(&mut self, seq: u16) -> SeqStatus {
        if !self.started {
            self.started = true;
            self.init(seq);
            self.max_seq = seq.wrapping_sub(1);
            self.probation = MIN_SEQUENTIAL;
        }

        if self.probation > 0 {
            if seq == self.max_seq.wrapping_add(1) {
                self.probation -= 1;
                self.max_seq = seq;
                if self.probation == 0 {
                    self.init(seq);
                    self.received += 1;
                    return SeqStatus::Valid;
                }
            } else {
                self.probation = MIN_SEQUENTIAL - 1;
                self.max_seq = seq;
            }
            return SeqStatus::Probation;
        }

        let delta = seq.wrapping_sub(self.max_seq);
        if delta < MAX_DROPOUT {
            // In order, with a permissible gap.
            if seq < self.max_seq {
                self.cycles = self.cycles.wrapping_add(SEQ_MOD);
            }
            self.max_seq = seq;
        } else if u32::from(delta) <= SEQ_MOD - u32::from(MAX_MISORDER) {
            // A very large jump. Believe it only if the next packet follows on.
            if u32::from(seq) == self.bad_seq {
                self.init(seq);
                self.received += 1;
                return SeqStatus::Restarted;
            }
            self.bad_seq = (u32::from(seq) + 1) & (SEQ_MOD - 1);
            return SeqStatus::Invalid;
        }
        // Otherwise a duplicate or reordered packet: counted, max unchanged.
        self.received = self.received.wrapping_add(1);
        SeqStatus::Valid
    }

    /// Feed a packet's arrival into the jitter estimate (A.8).
    ///
    /// `arrival` is the local receive time in the same clock units as the RTP
    /// timestamp, from any origin - only differences matter. Call it for
    /// packets [`update`](Self::update) did not mark [`SeqStatus::Invalid`].
    pub fn record_arrival(&mut self, rtp_timestamp: u32, arrival: u32) {
        let transit = arrival.wrapping_sub(rtp_timestamp);
        if let Some(previous) = self.transit {
            let d = (transit.wrapping_sub(previous) as i32)
                .unsigned_abs()
                .min(MAX_TRANSIT_DELTA);
            // J += (|D| - J) / 16, with J held as 16 * J.
            self.jitter = self.jitter + d - ((self.jitter + 8) >> 4);
        }
        self.transit = Some(transit);
    }

    /// Current interarrival jitter, in RTP clock ticks.
    #[must_use]
    pub fn jitter(&self) -> u32 {
        self.jitter >> 4
    }

    /// Whether the source has passed probation.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.started && self.probation == 0
    }

    /// Packets received since the stream (re)started, duplicates included.
    #[must_use]
    pub fn received(&self) -> u32 {
        self.received
    }

    /// Highest sequence number seen, with wraparounds in the upper 16 bits.
    #[must_use]
    pub fn extended_highest(&self) -> u32 {
        self.cycles.wrapping_add(u32::from(self.max_seq))
    }

    /// Loss and jitter figures for an RTCP report block (A.3), starting a new
    /// reporting interval. Before the source is valid, everything but the
    /// jitter is zero.
    pub fn report(&mut self) -> Report {
        if !self.is_valid() {
            return Report {
                jitter: self.jitter(),
                ..Report::default()
            };
        }

        let extended_highest = self.extended_highest();
        let expected = extended_highest.wrapping_sub(self.base_seq).wrapping_add(1);
        let lost = i64::from(expected) - i64::from(self.received);

        let expected_interval = expected.wrapping_sub(self.expected_prior);
        self.expected_prior = expected;
        let received_interval = self.received.wrapping_sub(self.received_prior);
        self.received_prior = self.received;
        let lost_interval = i64::from(expected_interval) - i64::from(received_interval);

        let fraction_lost = if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            ((lost_interval << 8) / i64::from(expected_interval)).min(255) as u8
        };

        Report {
            fraction_lost,
            cumulative_lost: lost.clamp(-0x80_0000, 0x7f_ffff) as i32,
            extended_highest,
            jitter: self.jitter(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `seqs` in order, returning the statuses.
    fn feed<const N: usize>(stats: &mut SourceStats, seqs: [u16; N]) -> [SeqStatus; N] {
        seqs.map(|seq| stats.update(seq))
    }

    #[test]
    fn probation_then_valid() {
        let mut stats = SourceStats::new();
        assert!(!stats.is_valid());
        assert_eq!(
            feed(&mut stats, [10, 11, 12]),
            [SeqStatus::Probation, SeqStatus::Valid, SeqStatus::Valid]
        );
        assert!(stats.is_valid());
        assert_eq!(stats.received(), 2);
        let report = stats.report();
        assert_eq!(report.cumulative_lost, 0);
        assert_eq!(report.fraction_lost, 0);
        assert_eq!(report.extended_highest, 12);
    }

    #[test]
    fn broken_probation_restarts_it() {
        let mut stats = SourceStats::new();
        assert_eq!(
            feed(&mut stats, [10, 50, 51]),
            [SeqStatus::Probation, SeqStatus::Probation, SeqStatus::Valid]
        );
        assert_eq!(stats.report().extended_highest, 51);
    }

    #[test]
    fn loss_and_fraction() {
        let mut stats = SourceStats::new();
        feed(&mut stats, [0, 1, 2, 3]);
        // 4..=7 never arrive.
        feed(&mut stats, [8, 9, 10, 11]);
        let report = stats.report();
        assert_eq!(report.cumulative_lost, 4);
        // Interval since start: 11 expected, 7 received.
        assert_eq!(report.fraction_lost, ((4 << 8) / 11) as u8);

        // A clean interval reports no new loss but keeps the total.
        feed(&mut stats, [12, 13, 14, 15]);
        let report = stats.report();
        assert_eq!(report.fraction_lost, 0);
        assert_eq!(report.cumulative_lost, 4);
    }

    #[test]
    fn wraparound_extends_the_sequence() {
        let mut stats = SourceStats::new();
        feed(&mut stats, [65533, 65534, 65535, 0, 1]);
        let report = stats.report();
        assert_eq!(report.extended_highest, (1 << 16) + 1);
        assert_eq!(report.cumulative_lost, 0);
    }

    #[test]
    fn reordering_is_not_loss() {
        let mut stats = SourceStats::new();
        assert_eq!(
            feed(&mut stats, [0, 1, 3, 2, 4]),
            [
                SeqStatus::Probation,
                SeqStatus::Valid,
                SeqStatus::Valid,
                SeqStatus::Valid,
                SeqStatus::Valid
            ]
        );
        let report = stats.report();
        assert_eq!(report.cumulative_lost, 0);
        assert_eq!(report.extended_highest, 4);
    }

    #[test]
    fn duplicates_make_loss_negative() {
        let mut stats = SourceStats::new();
        feed(&mut stats, [0, 1, 2, 2]);
        assert_eq!(stats.report().cumulative_lost, -1);
    }

    #[test]
    fn big_jump_needs_confirmation() {
        let mut stats = SourceStats::new();
        feed(&mut stats, [100, 101, 102]);
        assert_eq!(stats.update(40_000), SeqStatus::Invalid);
        assert_eq!(stats.update(40_001), SeqStatus::Restarted);
        assert_eq!(stats.update(40_002), SeqStatus::Valid);
        let report = stats.report();
        assert_eq!(report.extended_highest, 40_002);
        assert_eq!(report.cumulative_lost, 0);
    }

    #[test]
    fn unconfirmed_jump_leaves_stream_alone() {
        let mut stats = SourceStats::new();
        feed(&mut stats, [100, 101, 102]);
        assert_eq!(stats.update(40_000), SeqStatus::Invalid);
        assert_eq!(stats.update(103), SeqStatus::Valid);
        assert_eq!(stats.report().extended_highest, 103);
    }

    #[test]
    fn jitter_is_zero_for_a_steady_sender() {
        let mut stats = SourceStats::new();
        for i in 0..100u32 {
            stats.record_arrival(i * 320, 5_000 + i * 320);
        }
        assert_eq!(stats.jitter(), 0);
    }

    #[test]
    fn jitter_converges_on_the_deviation() {
        // Arrivals alternate 40 ticks early and late, so every |D| is 80.
        let mut stats = SourceStats::new();
        for i in 0..400u32 {
            let wobble = if i % 2 == 0 { 0 } else { 80 };
            stats.record_arrival(i * 320, i * 320 + wobble);
        }
        assert!((79..=80).contains(&stats.jitter()), "{}", stats.jitter());
    }

    #[test]
    fn jitter_ignores_timestamp_wrap() {
        let mut stats = SourceStats::new();
        let start = u32::MAX - 640;
        for i in 0..10u32 {
            let ts = start.wrapping_add(i * 320);
            stats.record_arrival(ts, ts.wrapping_add(1234));
        }
        assert_eq!(stats.jitter(), 0);
    }

    #[test]
    fn garbage_timestamps_cannot_overflow() {
        let mut stats = SourceStats::new();
        for i in 0..1_000u32 {
            stats.record_arrival(i.wrapping_mul(0x9e37_79b9), 0);
        }
        assert!(stats.jitter() <= MAX_TRANSIT_DELTA);
    }
}
