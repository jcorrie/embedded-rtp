//! Reception statistics conformance: RFC 3550 §6.4.1 and appendix A.
//!
//! The expected numbers come from a simulated network that knows exactly what
//! was sent, lost, duplicated and reordered, and from the §6.4.1 definitions
//! evaluated directly, not from the algorithm under test.

mod common;

use common::Rng;
use embedded_rtp::stats::{SeqStatus, SourceStats, MAX_DROPOUT, MAX_MISORDER, MIN_SEQUENTIAL};

/// A network between one sender and our receiver.
struct Channel {
    /// Loss probability, per mille.
    loss: u32,
    /// Duplication probability, per mille.
    duplicate: u32,
    /// Probability that a packet is held back and delivered late, per mille.
    reorder: u32,
    /// How many later packets a held-back packet can be overtaken by.
    max_delay: usize,
}

/// The sender's packet indices in the order they reach the receiver.
///
/// The first `MIN_SEQUENTIAL` packets always arrive first and in order. That
/// pins down which packet starts the RFC's count, so ground truth does not
/// depend on how probation is implemented.
fn deliver(rng: &mut Rng, channel: &Channel, packets: usize) -> Vec<usize> {
    let clean = usize::from(MIN_SEQUENTIAL);
    let mut arrivals: Vec<usize> = (0..clean).collect();
    let mut held: Vec<(usize, usize)> = Vec::new(); // (index, deliver after this many more)
    for index in clean..packets {
        if rng.chance(channel.loss) {
            continue;
        }
        let copies = if rng.chance(channel.duplicate) { 2 } else { 1 };
        for _ in 0..copies {
            if rng.chance(channel.reorder) {
                held.push((index, 1 + rng.below(channel.max_delay as u32) as usize));
            } else {
                arrivals.push(index);
            }
        }
        held.retain_mut(|(late, wait)| {
            *wait -= 1;
            if *wait == 0 {
                arrivals.push(*late);
            }
            *wait > 0
        });
    }
    arrivals.extend(held.into_iter().map(|(index, _)| index));
    arrivals
}

/// Ground truth for a report, per RFC 3550 §6.4.1:
///
/// - extended highest: the highest sequence number received, extended;
/// - expected: extended highest minus the initial sequence number, plus one,
///   where the initial number is the one the source was validated on;
/// - received: every packet after that, late and duplicate ones included;
/// - cumulative lost: expected minus received;
/// - fraction lost: lost / expected over the interval since the last report,
///   as a fixed point number with the binary point at the left edge,
///   or zero when the interval lost nothing (or gained through duplicates).
#[derive(Debug, Default)]
struct Truth {
    max_index: usize,
    received: i64,
    prior_expected: i64,
    prior_received: i64,
}

impl Truth {
    /// The packet `MIN_SEQUENTIAL - 1` is the one that validates the source.
    const BASE: usize = MIN_SEQUENTIAL as usize - 1;

    fn arrive(&mut self, index: usize) {
        self.max_index = self.max_index.max(index);
        if index >= Self::BASE {
            self.received += 1;
        }
    }

    fn check(&mut self, first_seq: u16, stats: &mut SourceStats, context: &str) {
        let expected = (self.max_index - Self::BASE + 1) as i64;
        let lost = expected - self.received;
        let expected_interval = expected - self.prior_expected;
        let lost_interval = lost - (self.prior_expected - self.prior_received);
        self.prior_expected = expected;
        self.prior_received = self.received;
        let fraction = if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            (lost_interval * 256 / expected_interval) as u8
        };

        let report = stats.report();
        assert_eq!(
            report.extended_highest,
            u32::from(first_seq) + self.max_index as u32,
            "extended highest, {context}"
        );
        assert_eq!(
            i64::from(report.cumulative_lost),
            lost,
            "cumulative lost, {context}"
        );
        assert_eq!(report.fraction_lost, fraction, "fraction lost, {context}");
    }
}

fn simulate(seed: u64, first_seq: u16, packets: usize, channel: &Channel, report_every: usize) {
    let mut rng = Rng::new(seed);
    let arrivals = deliver(&mut rng, channel, packets);
    let mut stats = SourceStats::new();
    let mut truth = Truth::default();

    for (position, &index) in arrivals.iter().enumerate() {
        let seq = first_seq.wrapping_add(index as u16);
        let status = stats.update(seq);
        let expected_status = if position < Truth::BASE {
            SeqStatus::Probation
        } else {
            SeqStatus::Valid
        };
        assert_eq!(
            status, expected_status,
            "seed {seed}, arrival {position}, seq {seq}"
        );
        truth.arrive(index);
        if position >= Truth::BASE && position % report_every == 0 {
            truth.check(
                first_seq,
                &mut stats,
                &format!("seed {seed}, arrival {position}"),
            );
        }
    }
    truth.check(first_seq, &mut stats, &format!("seed {seed}, end"));
}

#[test]
fn perfect_network() {
    let channel = Channel {
        loss: 0,
        duplicate: 0,
        reorder: 0,
        max_delay: 1,
    };
    simulate(1, 1000, 5_000, &channel, 250);
}

#[test]
fn lossy_network() {
    let channel = Channel {
        loss: 50,
        duplicate: 0,
        reorder: 0,
        max_delay: 1,
    };
    for seed in 0..20 {
        simulate(seed, Rng::new(seed).next_u32() as u16, 5_000, &channel, 250);
    }
}

#[test]
fn duplicating_network() {
    let channel = Channel {
        loss: 0,
        duplicate: 50,
        reorder: 0,
        max_delay: 1,
    };
    for seed in 0..20 {
        simulate(seed, Rng::new(seed).next_u32() as u16, 5_000, &channel, 250);
    }
}

#[test]
fn reordering_network() {
    let channel = Channel {
        loss: 0,
        duplicate: 0,
        reorder: 100,
        max_delay: 20,
    };
    for seed in 0..20 {
        simulate(seed, Rng::new(seed).next_u32() as u16, 5_000, &channel, 250);
    }
}

#[test]
fn hostile_network_across_many_wraps() {
    // Loss, duplication and reordering at once, for 4.5 wraps of the 16-bit
    // sequence number, starting just short of a wrap.
    let channel = Channel {
        loss: 100,
        duplicate: 30,
        reorder: 150,
        max_delay: 50,
    };
    for seed in 0..5 {
        simulate(100 + seed, 65_530, 300_000, &channel, 1_000);
    }
}

#[test]
fn reordering_up_to_the_misorder_limit_is_still_counted() {
    // Held back by up to MAX_MISORDER - 1 packets: the furthest the A.1 code
    // accepts as reordering (see the boundary test below).
    let channel = Channel {
        loss: 0,
        duplicate: 0,
        reorder: 20,
        max_delay: usize::from(MAX_MISORDER) - 1,
    };
    for seed in 0..10 {
        simulate(200 + seed, 0, 20_000, &channel, 500);
    }
}

/// A source with `max_seq` 1000, validated and in steady state.
fn steady_at(seq: u16) -> SourceStats {
    let mut stats = SourceStats::new();
    for s in seq - 10..=seq {
        stats.update(s);
    }
    assert!(stats.is_valid());
    stats
}

#[test]
fn forward_jumps_up_to_max_dropout_are_loss_not_restart() {
    // A.1: `udelta < MAX_DROPOUT` is in order with a permissible gap.
    let mut stats = steady_at(1000);
    assert_eq!(stats.update(1000 + MAX_DROPOUT - 1), SeqStatus::Valid);
    assert_eq!(stats.report().cumulative_lost, i32::from(MAX_DROPOUT) - 2);

    // `udelta == MAX_DROPOUT` is a very large jump.
    let mut stats = steady_at(1000);
    assert_eq!(stats.update(1000 + MAX_DROPOUT), SeqStatus::Invalid);
}

#[test]
fn backward_steps_within_max_misorder_are_reordering() {
    // A.1 treats `udelta > RTP_SEQ_MOD - MAX_MISORDER` as reordering, so a
    // step back of MAX_MISORDER - 1 is the largest accepted and a step of
    // exactly MAX_MISORDER is already a "very large jump".
    let mut stats = steady_at(1000);
    assert_eq!(stats.update(1000 - (MAX_MISORDER - 1)), SeqStatus::Valid);
    assert_eq!(stats.report().extended_highest, 1000, "max unchanged");

    let mut stats = steady_at(1000);
    assert_eq!(stats.update(1000 - MAX_MISORDER), SeqStatus::Invalid);
}

#[test]
fn restart_is_confirmed_by_the_following_packet_only() {
    let mut stats = steady_at(1000);
    assert_eq!(stats.update(30_000), SeqStatus::Invalid);
    // Not the packet after the jump: the old stream carries on.
    assert_eq!(stats.update(1001), SeqStatus::Valid);
    // A second jump to the same place still needs its own confirmation.
    assert_eq!(stats.update(30_000), SeqStatus::Invalid);
    assert_eq!(stats.update(30_001), SeqStatus::Restarted);

    // After a restart, counting starts again from the confirming packet.
    for seq in 30_002..30_010 {
        assert_eq!(stats.update(seq), SeqStatus::Valid);
    }
    let report = stats.report();
    assert_eq!(report.extended_highest, 30_009);
    assert_eq!(report.cumulative_lost, 0);
}

#[test]
fn probation_needs_min_sequential_packets_in_a_row() {
    let mut stats = SourceStats::new();
    let mut seq = 500u16;
    for _ in 1..MIN_SEQUENTIAL {
        assert_eq!(stats.update(seq), SeqStatus::Probation);
        seq += 1;
    }
    // A gap during probation starts it again.
    seq += 5;
    for _ in 1..MIN_SEQUENTIAL {
        assert_eq!(stats.update(seq), SeqStatus::Probation);
        seq += 1;
    }
    assert_eq!(stats.update(seq), SeqStatus::Valid);
    assert!(stats.is_valid());
}

#[test]
fn reports_before_validation_carry_no_loss_figures() {
    let mut stats = SourceStats::new();
    assert_eq!(stats.report().cumulative_lost, 0);
    stats.update(7);
    let report = stats.report();
    assert_eq!(
        (
            report.cumulative_lost,
            report.fraction_lost,
            report.extended_highest
        ),
        (0, 0, 0)
    );
}

#[test]
fn cumulative_loss_saturates_at_24_bits() {
    // §6.4.1: cumulative lost is a signed 24-bit field. Push loss past it
    // with jumps just under MAX_DROPOUT; A.3 clamps rather than wrapping.
    let mut stats = steady_at(1000);
    let mut seq = 1000u16;
    for _ in 0..3_000 {
        seq = seq.wrapping_add(MAX_DROPOUT - 1);
        assert_eq!(stats.update(seq), SeqStatus::Valid);
    }
    assert_eq!(stats.report().cumulative_lost, 0x7f_ffff);
}

/// §6.4.1 / A.8 evaluated directly in floating point:
/// `D(i-1, i) = (Rj - Ri) - (Sj - Si)`, `J(i) = J(i-1) + (|D| - J(i-1)) / 16`.
fn reference_jitter(packets: &[(u32, u32)]) -> f64 {
    let mut jitter = 0.0f64;
    for pair in packets.windows(2) {
        let (s_i, r_i) = pair[0];
        let (s_j, r_j) = pair[1];
        let d = (f64::from(r_j) - f64::from(r_i)) - (f64::from(s_j) - f64::from(s_i));
        jitter += (d.abs() - jitter) / 16.0;
    }
    jitter
}

/// The A.8 integer method keeps 16·J and subtracts `round(16·J / 16)` each
/// step. Each rounding is off by at most half a unit of 16·J, and the error
/// decays by 15/16 per step, so the stored value stays within 8 units (half a
/// tick) of exact. Reporting `16·J >> 4` then truncates by up to one more
/// tick, so the reported jitter minus the exact value lies in (-1.5, 0.5].
#[test]
fn jitter_tracks_the_floating_point_definition() {
    for seed in 0..20 {
        let mut rng = Rng::new(seed);
        let mut stats = SourceStats::new();
        let mut packets = Vec::new();
        let mut timestamp = 0u32;
        for i in 0..20_000u32 {
            // 20 ms frames with occasional silence gaps in the timestamps.
            timestamp += if rng.chance(20) {
                320 * (2 + rng.below(50))
            } else {
                320
            };
            // Network delay: a base, a random spread, and the odd spike.
            let mut delay = 800 + rng.below(1 + seed as u32 * 40);
            if rng.chance(5) {
                delay += rng.below(4_000);
            }
            let arrival = timestamp + 10_000 + delay;
            stats.record_arrival(timestamp, arrival);
            packets.push((timestamp, arrival));

            if i % 1_000 == 999 {
                let expected = reference_jitter(&packets);
                let ours = f64::from(stats.jitter());
                let error = ours - expected;
                assert!(
                    -1.5 < error && error <= 0.5,
                    "seed {seed}, packet {i}: ours {ours}, reference {expected:.2}"
                );
            }
        }
    }
}

#[test]
fn jitter_is_independent_of_clock_offset_and_wrap() {
    // Only differences matter (A.8): shifting both clocks, even across the
    // 32-bit wrap, must not change the estimate.
    let delays = [0u32, 37, 5, 210, 90, 3, 400, 12];
    let estimate = |ts_offset: u32, arrival_offset: u32| {
        let mut stats = SourceStats::new();
        for i in 0..2_000u32 {
            let ts = ts_offset.wrapping_add(i * 320);
            let arrival = arrival_offset.wrapping_add(i * 320 + delays[i as usize % delays.len()]);
            stats.record_arrival(ts, arrival);
        }
        stats.jitter()
    };
    let baseline = estimate(0, 0);
    assert!(baseline > 0);
    assert_eq!(estimate(u32::MAX - 100_000, 7), baseline);
    assert_eq!(estimate(12_345, u32::MAX - 50), baseline);
}
