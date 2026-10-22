//! Jitter buffer conformance, by simulation.
//!
//! A sender produces frames on its clock, a simulated network delays, loses,
//! duplicates and stalls them, and a receiver plays out on its own clock.
//! The simulation knows what really arrived and when, so it can check the
//! buffer's every decision:
//!
//! - a played frame carries exactly that frame's bytes, and plays once;
//! - a frame is reported lost only if it had not arrived in time;
//! - playout runs in sequence order, and every sequence number it passes
//!   without playing or concealing is accounted for in the counters;
//! - accepted frames are conserved: played, discarded, or still buffered;
//! - depth never exceeds `max_depth`, and no frame waits longer than prefill
//!   plus that.

mod common;

use std::collections::{HashMap, HashSet};

use common::Rng;
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::Packet;
use embedded_rtp::{Received, Receiver, Sender};

const FRAME_TICKS: u32 = 320;
const PT: u8 = 96;
type TestReceiver = Receiver<16, 8>;

/// Frame payloads are the frame's index, so any mix-up shows.
fn payload(index: u64) -> [u8; 8] {
    index.to_be_bytes()
}

#[derive(Clone)]
struct Scenario {
    seed: u64,
    frames: u64,
    first_sequence: u16,
    /// Fixed network delay, in ticks.
    base_delay: f64,
    /// Extra delay, uniformly random up to this many ticks.
    spread: f64,
    /// Per mille.
    loss: u32,
    /// Per mille.
    duplicate: u32,
    /// Per mille chance per frame of a stall that holds packets back.
    stall: u32,
    /// How long a stall lasts, in ticks.
    stall_ticks: f64,
    /// Per mille chance per frame that the sender goes quiet.
    silence: u32,
    /// Receiver clock rate relative to the sender's, e.g. 1.01 is 1% slow.
    receiver_period: f64,
    config: Config,
}

impl Scenario {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            frames: 5_000,
            first_sequence: 1_000,
            base_delay: 2_000.0,
            spread: 0.0,
            loss: 0,
            duplicate: 0,
            stall: 0,
            stall_ticks: 0.0,
            silence: 0,
            receiver_period: 1.0,
            config: Config::DEFAULT,
        }
    }
}

struct Delivery {
    arrival: f64,
    order: usize,
    index: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct Outcome {
    played: u64,
    concealed: u64,
    max_target: u8,
}

fn run(scenario: &Scenario) -> (Outcome, embedded_rtp::jitter::Counters) {
    let mut rng = Rng::new(scenario.seed);
    let mut sender = Sender::new(0xfeed, PT, FRAME_TICKS, scenario.first_sequence, 12_345);

    // Sender side: frame `index` is produced at `index * FRAME_TICKS`.
    let mut deliveries = Vec::new();
    let mut stall_until = f64::MIN;
    let mut quiet_for = 0u32;
    let mut sequence_of = HashMap::new();
    for index in 0..scenario.frames {
        let sent_at = index as f64 * f64::from(FRAME_TICKS);
        if quiet_for == 0 && rng.chance(scenario.silence) {
            quiet_for = 10 + rng.below(100);
        }
        if quiet_for > 0 {
            quiet_for -= 1;
            sender.skip();
            continue;
        }
        let mut buf = [0u8; 64];
        let len = sender.send(&payload(index), &mut buf).unwrap();
        let bytes = buf[..len].to_vec();
        sequence_of.insert(Packet::parse(&bytes).unwrap().header.sequence, index);

        if rng.chance(scenario.stall) {
            stall_until = sent_at + scenario.stall_ticks;
        }
        if rng.chance(scenario.loss) {
            continue;
        }
        let copies = if rng.chance(scenario.duplicate) { 2 } else { 1 };
        for _ in 0..copies {
            let jitter = f64::from(rng.below(1_000_000)) / 1e6 * scenario.spread;
            let arrival = (sent_at + scenario.base_delay + jitter).max(stall_until);
            deliveries.push(Delivery {
                arrival,
                order: deliveries.len(),
                index,
                bytes: bytes.clone(),
            });
        }
    }
    deliveries.sort_by(|a, b| a.arrival.total_cmp(&b.arrival).then(a.order.cmp(&b.order)));

    // Receiver side: plays a frame every `receiver_period` frame periods.
    let mut receiver = TestReceiver::new(PT, FRAME_TICKS, scenario.config);
    let max_depth = usize::from(scenario.config.max_depth);
    let mut accepted: HashSet<u16> = HashSet::new();
    let mut accepted_at: HashMap<u16, u64> = HashMap::new();
    let mut played: HashSet<u16> = HashSet::new();
    let mut accepted_count = 0u64;
    let mut last_output: Option<u16> = None;
    let mut passed_before = 0u32;
    let mut outcome = Outcome::default();
    let mut pending = deliveries.into_iter().peekable();
    let end = scenario.frames as f64 * f64::from(FRAME_TICKS)
        + scenario.base_delay
        + scenario.spread
        + scenario.stall_ticks
        + 100.0 * f64::from(FRAME_TICKS);

    let mut tick = 0u64;
    loop {
        let now = tick as f64 * f64::from(FRAME_TICKS) * scenario.receiver_period;
        if now > end {
            break;
        }
        while let Some(delivery) = pending.next_if(|d| d.arrival <= now) {
            let packet = Packet::parse(&delivery.bytes).unwrap();
            let sequence = packet.header.sequence;
            let result = receiver.receive(&packet, delivery.arrival as u32);
            assert_ne!(
                result,
                Received::Invalid,
                "seed {}: index {}",
                scenario.seed,
                delivery.index
            );
            if result == Received::Accepted {
                assert!(accepted.insert(sequence) || !played.contains(&sequence));
                accepted_at.insert(sequence, tick);
                accepted_count += 1;
            }
            assert!(receiver.buffer().depth() <= max_depth, "depth after push");
        }

        // Copy the borrowed payload out so the receiver can be inspected.
        let playout = match receiver.play() {
            Playout::Frame { sequence, payload } => Some((sequence, Some(payload.to_vec()))),
            Playout::Lost { sequence } => Some((sequence, None)),
            Playout::Silence => None,
        };
        let counters = receiver.counters();
        assert!(receiver.buffer().depth() <= max_depth, "depth after pop");
        assert_eq!(
            accepted_count,
            u64::from(counters.played + counters.overflow + counters.drained)
                + receiver.buffer().buffered() as u64,
            "seed {}: accepted frames are conserved",
            scenario.seed
        );
        outcome.max_target = outcome.max_target.max(receiver.buffer().target());

        let output = match playout {
            Some((sequence, Some(payload))) => {
                assert_eq!(payload.len(), 8);
                assert_eq!(
                    u64::from_be_bytes(payload.try_into().unwrap()),
                    sequence_of[&sequence],
                    "seed {}: wrong bytes for {sequence}",
                    scenario.seed
                );
                assert!(
                    played.insert(sequence),
                    "seed {}: {sequence} played twice",
                    scenario.seed
                );
                // While playing, a frame at most `max_depth - 1` ahead plays
                // within that many periods; prefilling adds at most the
                // target, itself at most `max_depth`.
                let waited = tick - accepted_at[&sequence];
                assert!(
                    waited < 2 * max_depth as u64,
                    "seed {}: {sequence} waited {waited} frames",
                    scenario.seed
                );
                outcome.played += 1;
                Some(sequence)
            }
            Some((sequence, None)) => {
                assert!(
                    !accepted.contains(&sequence),
                    "seed {}: {sequence} reported lost but it had arrived",
                    scenario.seed
                );
                outcome.concealed += 1;
                Some(sequence)
            }
            None => None,
        };

        if let Some(sequence) = output {
            let passed = counters.overflow + counters.drained + counters.skipped;
            if let Some(last) = last_output {
                let gap = u32::from(sequence.wrapping_sub(last).wrapping_sub(1));
                assert_eq!(
                    gap,
                    passed - passed_before,
                    "seed {}: sequence numbers between {last} and {sequence} unaccounted for",
                    scenario.seed
                );
            }
            last_output = Some(sequence);
            passed_before = passed;
        }
        tick += 1;
    }
    assert!(pending.peek().is_none());
    (outcome, receiver.counters())
}

#[test]
fn clean_network_plays_everything_at_steady_latency() {
    let scenario = Scenario::new(1);
    let (outcome, counters) = run(&scenario);
    assert_eq!(outcome.played, scenario.frames);
    assert_eq!(counters.concealed, 0);
    assert_eq!(counters.overflow + counters.drained + counters.skipped, 0);
    // The one underrun is the end of the stream.
    assert_eq!(counters.underruns, 1);
}

#[test]
fn jittery_network_conceals_only_what_is_truly_late() {
    for seed in 0..20 {
        let scenario = Scenario {
            spread: 600.0,
            ..Scenario::new(seed)
        };
        let (outcome, _) = run(&scenario);
        assert!(
            outcome.played > scenario.frames * 9 / 10,
            "seed {seed}: {outcome:?}"
        );
    }
}

#[test]
fn target_adapts_to_jitter() {
    let calm = run(&Scenario {
        spread: 50.0,
        ..Scenario::new(1)
    })
    .0;
    let rough = run(&Scenario {
        spread: 900.0,
        ..Scenario::new(1)
    })
    .0;
    assert!(calm.max_target <= 2, "{calm:?}");
    assert!(rough.max_target >= 4, "{rough:?}");
}

#[test]
fn lossy_duplicating_network() {
    for seed in 0..20 {
        let scenario = Scenario {
            spread: 400.0,
            loss: 50,
            duplicate: 30,
            ..Scenario::new(seed)
        };
        let (outcome, counters) = run(&scenario);
        assert!(outcome.concealed > 0, "seed {seed}");
        assert!(counters.duplicate > 0 || counters.late > 0, "seed {seed}");
    }
}

#[test]
fn silence_suppression_is_not_loss() {
    for seed in 0..10 {
        let scenario = Scenario {
            silence: 10,
            spread: 100.0,
            ..Scenario::new(seed)
        };
        let (_, counters) = run(&scenario);
        assert_eq!(
            counters.concealed, 0,
            "seed {seed}: silence reported as loss"
        );
        assert_eq!(counters.skipped, 0, "seed {seed}");
        assert!(
            counters.underruns > 1,
            "seed {seed}: each silence ends playout"
        );
    }
}

#[test]
fn stalls_and_bursts_keep_latency_bounded() {
    for seed in 0..20 {
        let scenario = Scenario {
            spread: 200.0,
            stall: 5,
            stall_ticks: 8_000.0, // half a second
            ..Scenario::new(seed)
        };
        run(&scenario);
    }
}

#[test]
fn slow_receiver_clock_drains_instead_of_growing() {
    for seed in 0..5 {
        let scenario = Scenario {
            receiver_period: 1.01,
            frames: 20_000,
            ..Scenario::new(seed)
        };
        let (_, counters) = run(&scenario);
        assert!(counters.drained > 0, "seed {seed}: {counters:?}");
        assert_eq!(counters.concealed, 0, "seed {seed}");
    }
}

#[test]
fn fast_receiver_clock_underruns_and_recovers() {
    for seed in 0..5 {
        let scenario = Scenario {
            receiver_period: 0.99,
            frames: 20_000,
            ..Scenario::new(seed)
        };
        let (outcome, counters) = run(&scenario);
        assert!(counters.underruns > 1, "seed {seed}");
        assert_eq!(counters.concealed, 0, "seed {seed}");
        assert_eq!(outcome.played, scenario.frames, "seed {seed}");
    }
}

#[test]
fn everything_at_once_across_the_sequence_wrap() {
    for seed in 0..10 {
        let scenario = Scenario {
            first_sequence: 60_000,
            frames: 30_000,
            spread: 700.0,
            loss: 30,
            duplicate: 20,
            stall: 2,
            stall_ticks: 5_000.0,
            silence: 3,
            receiver_period: 1.002,
            ..Scenario::new(100 + seed)
        };
        run(&scenario);
    }
}
