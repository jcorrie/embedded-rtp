//! Redundant audio (RFC 2198): the payload format against the RFC's
//! diagram, the encoder's choice of blocks, the receiver's recovery, and a
//! simulated lossy network that knows which frames were lost.
//!
//! Payloads are assembled here from the diagram's fields with plain integer
//! arithmetic, not with the crate's writer, so a misreading of the format
//! can't hide itself by being undone in the parser.

mod common;

use std::collections::BTreeMap;

use common::Rng;
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::{Packet, HEADER_LEN};
use embedded_rtp::red::{self, Block, Encoder, Payload, BLOCK_HEADER_LEN};
use embedded_rtp::{Error, Received, Receiver, Sender};

const PT: u8 = 96;
const RED: u8 = 97;
const TICKS: u32 = 320;

/// A redundant block header, from RFC 2198 §3:
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |F|   block PT  |  timestamp offset         |   block length    |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
fn block_header(pt: u8, offset: u32, len: usize) -> [u8; 4] {
    assert!(pt < 128 && offset < 1 << 14 && len < 1 << 10);
    ((1u32 << 31) | (u32::from(pt) << 24) | (offset << 10) | len as u32).to_be_bytes()
}

/// The final header, for the primary: F = 0 and the block PT.
fn primary_header(pt: u8) -> u8 {
    pt
}

/// RFC 2198 §3's example: a primary frame plus one redundant block,
/// headers first, then the block's data, then the primary's.
fn rfc_example() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&block_header(5, 640, 3)); // DVI4, 640 ticks back
    out.push(primary_header(0)); // PCMU
    out.extend_from_slice(&[0xd1, 0xd2, 0xd3]);
    out.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    out
}

#[test]
fn parses_the_rfc_layout() {
    let bytes = rfc_example();
    let payload = Payload::parse(&bytes).unwrap();
    assert_eq!(payload.payload_type, 0);
    assert_eq!(payload.primary, &[1, 2, 3, 4]);
    let blocks: Vec<Block> = payload.blocks().collect();
    assert_eq!(
        blocks,
        [Block {
            payload_type: 5,
            timestamp_offset: 640,
            data: &[0xd1, 0xd2, 0xd3],
        }]
    );
}

#[test]
fn writes_the_rfc_layout() {
    let block = Block {
        payload_type: 5,
        timestamp_offset: 640,
        data: &[0xd1, 0xd2, 0xd3],
    };
    let mut out = [0u8; 64];
    let len = red::write(&[block], 0, &[1, 2, 3, 4], &mut out).unwrap();
    assert_eq!(out[..len], rfc_example());
}

/// Every field at its extremes, two blocks deep, so a field that spills into
/// its neighbour shows.
#[test]
fn fields_are_independent() {
    for (pt, offset, len) in [
        (0, 0, 0),
        (127, 0x3fff, 1023),
        (0x55, 0x2aaa, 0x155),
        (0x2a, 0x1555, 0x2aa),
    ] {
        let data = vec![0xa5; len];
        let mut expected = Vec::new();
        expected.extend_from_slice(&block_header(pt, offset, len));
        expected.extend_from_slice(&block_header(127 - pt, 0x3fff - offset, 0));
        expected.push(primary_header(pt));
        expected.extend_from_slice(&data);
        expected.push(0x77);

        let blocks = [
            Block {
                payload_type: pt,
                timestamp_offset: offset as u16,
                data: &data,
            },
            Block {
                payload_type: 127 - pt,
                timestamp_offset: (0x3fff - offset) as u16,
                data: &[],
            },
        ];
        let mut out = vec![0u8; expected.len()];
        assert_eq!(
            red::write(&blocks, pt, &[0x77], &mut out),
            Ok(expected.len())
        );
        assert_eq!(out, expected);

        let payload = Payload::parse(&expected).unwrap();
        assert!(payload.blocks().eq(blocks));
        assert_eq!(payload.payload_type, pt);
        assert_eq!(payload.primary, &[0x77]);
    }
}

#[test]
fn malformed_payloads_are_rejected() {
    let bytes = rfc_example();
    // Cut anywhere before the primary's data and the block no longer fits.
    for end in 0..BLOCK_HEADER_LEN + 1 + 3 {
        assert_eq!(
            Payload::parse(&bytes[..end]),
            Err(Error::Truncated),
            "cut at {end}"
        );
    }
    // An empty primary is fine.
    assert_eq!(Payload::parse(&bytes[..8]).unwrap().primary, &[] as &[u8]);
}

/// Random payloads built from the diagram parse back to their fields, and
/// random bytes either fail to parse or write back byte for byte: every bit
/// of the format is a field, so there is nothing to lose.
#[test]
fn random_payloads_round_trip() {
    let mut rng = Rng::new(2198);
    for _ in 0..20_000 {
        let count = rng.below(4) as usize;
        let fields: Vec<(u8, u32, Vec<u8>)> = (0..count)
            .map(|_| {
                let len = rng.below(40) as usize;
                (rng.below(128) as u8, rng.below(1 << 14), rng.bytes(len))
            })
            .collect();
        let primary_type = rng.below(128) as u8;
        let primary_len = rng.below(40) as usize;
        let primary = rng.bytes(primary_len);
        let mut bytes = Vec::new();
        for (pt, offset, data) in &fields {
            bytes.extend_from_slice(&block_header(*pt, *offset, data.len()));
        }
        bytes.push(primary_header(primary_type));
        for (_, _, data) in &fields {
            bytes.extend_from_slice(data);
        }
        bytes.extend_from_slice(&primary);

        let payload = Payload::parse(&bytes).unwrap();
        assert_eq!(payload.payload_type, primary_type);
        assert_eq!(payload.primary, primary);
        let parsed: Vec<_> = payload
            .blocks()
            .map(|b| {
                (
                    b.payload_type,
                    u32::from(b.timestamp_offset),
                    b.data.to_vec(),
                )
            })
            .collect();
        assert_eq!(parsed, fields);

        // Corrupt a byte or cut it short: no panic, and whatever parses
        // writes back as it was.
        let mut garbled = bytes.clone();
        let at = rng.below(garbled.len() as u32) as usize;
        garbled[at] = rng.next_u32() as u8;
        garbled.truncate(garbled.len() - rng.below(garbled.len() as u32 + 1) as usize);
        if let Ok(payload) = Payload::parse(&garbled) {
            let blocks: Vec<Block> = payload.blocks().collect();
            let mut out = vec![0u8; garbled.len()];
            assert_eq!(
                red::write(&blocks, payload.payload_type, payload.primary, &mut out),
                Ok(garbled.len())
            );
            assert_eq!(out, garbled);
        }
    }
}

fn frame(index: u32) -> [u8; 8] {
    let [a, b, c, d] = index.to_be_bytes();
    [a, b, c, d, !a, !b, !c, !d]
}

/// The blocks an encoder wrote: (offset, data).
fn blocks_of(payload: &[u8]) -> (Vec<(u16, Vec<u8>)>, Vec<u8>) {
    let payload = Payload::parse(payload).unwrap();
    assert_eq!(payload.payload_type, PT);
    let blocks = payload
        .blocks()
        .map(|b| {
            assert_eq!(b.payload_type, PT);
            (b.timestamp_offset, b.data.to_vec())
        })
        .collect();
    (blocks, payload.primary.to_vec())
}

#[test]
fn encoder_carries_the_frames_before_oldest_first() {
    let mut encoder: Encoder<2, 8> = Encoder::new(PT, TICKS);
    let mut out = [0u8; Encoder::<2, 8>::MAX_PAYLOAD];
    let ts = u32::MAX - TICKS; // wraps on the third frame

    let len = encoder.encode(ts, &frame(0), &mut out).unwrap();
    assert_eq!(blocks_of(&out[..len]), (vec![], frame(0).to_vec()));
    assert_eq!(len, 1 + 8);

    let len = encoder
        .encode(ts.wrapping_add(TICKS), &frame(1), &mut out)
        .unwrap();
    assert_eq!(
        blocks_of(&out[..len]),
        (vec![(320, frame(0).to_vec())], frame(1).to_vec())
    );

    let len = encoder
        .encode(ts.wrapping_add(2 * TICKS), &frame(2), &mut out)
        .unwrap();
    assert_eq!(
        blocks_of(&out[..len]),
        (
            vec![(640, frame(0).to_vec()), (320, frame(1).to_vec())],
            frame(2).to_vec()
        )
    );
    assert_eq!(len, Encoder::<2, 8>::MAX_PAYLOAD);

    let len = encoder
        .encode(ts.wrapping_add(3 * TICKS), &frame(3), &mut out)
        .unwrap();
    assert_eq!(
        blocks_of(&out[..len]),
        (
            vec![(640, frame(1).to_vec()), (320, frame(2).to_vec())],
            frame(3).to_vec()
        )
    );
}

/// After a frame period with nothing sent, earlier frames are a different
/// number of sequence numbers back than their offset says: leave them out.
#[test]
fn encoder_carries_nothing_across_a_silence() {
    let mut encoder: Encoder<2, 8> = Encoder::new(PT, TICKS);
    let mut out = [0u8; Encoder::<2, 8>::MAX_PAYLOAD];
    encoder.encode(0, &frame(0), &mut out).unwrap();
    encoder.encode(TICKS, &frame(1), &mut out).unwrap();
    // Frame period 2 is skipped.
    let len = encoder.encode(3 * TICKS, &frame(3), &mut out).unwrap();
    assert_eq!(blocks_of(&out[..len]), (vec![], frame(3).to_vec()));
    let len = encoder.encode(4 * TICKS, &frame(4), &mut out).unwrap();
    assert_eq!(
        blocks_of(&out[..len]),
        (vec![(320, frame(3).to_vec())], frame(4).to_vec())
    );

    encoder.reset();
    let len = encoder.encode(5 * TICKS, &frame(5), &mut out).unwrap();
    assert_eq!(blocks_of(&out[..len]), (vec![], frame(5).to_vec()));
}

#[test]
fn encoder_errors_change_nothing() {
    let mut encoder: Encoder<1, 8> = Encoder::new(PT, TICKS);
    let mut out = [0u8; Encoder::<1, 8>::MAX_PAYLOAD];
    encoder.encode(0, &frame(0), &mut out).unwrap();
    assert_eq!(
        encoder.encode(TICKS, &[0; 9], &mut out),
        Err(Error::BufferTooSmall)
    );
    let mut short = [0u8; Encoder::<1, 8>::MAX_PAYLOAD - 1];
    assert_eq!(
        encoder.encode(TICKS, &frame(1), &mut short),
        Err(Error::BufferTooSmall)
    );
    // Frame 0 is still the one remembered.
    let len = encoder.encode(TICKS, &frame(1), &mut out).unwrap();
    assert_eq!(
        blocks_of(&out[..len]),
        (vec![(320, frame(0).to_vec())], frame(1).to_vec())
    );
}

/// Frames of different lengths come back at their own lengths.
#[test]
fn encoder_keeps_each_frames_length() {
    let mut encoder: Encoder<2, 8> = Encoder::new(PT, TICKS);
    let mut out = [0u8; Encoder::<2, 8>::MAX_PAYLOAD];
    encoder.encode(0, &[1], &mut out).unwrap();
    encoder.encode(TICKS, &[], &mut out).unwrap();
    let len = encoder.encode(2 * TICKS, &[3, 3, 3], &mut out).unwrap();
    assert_eq!(
        blocks_of(&out[..len]),
        (vec![(640, vec![1]), (320, vec![])], vec![3, 3, 3])
    );
}

type TestReceiver = Receiver<16, 8>;

fn red_receiver(wait: u8) -> TestReceiver {
    Receiver::new(PT, TICKS, Config::DEFAULT).with_redundancy(RED, wait)
}

/// A sender and encoder producing redundant packets of frame `index`.
struct RedSource<const DEPTH: usize> {
    sender: Sender,
    encoder: Encoder<DEPTH, 8>,
}

impl<const DEPTH: usize> RedSource<DEPTH> {
    fn new(first_sequence: u16) -> Self {
        Self {
            sender: Sender::new(0xfeed, RED, TICKS, first_sequence, 99),
            encoder: Encoder::new(PT, TICKS),
        }
    }

    fn send(&mut self, index: u32) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_LEN + Encoder::<DEPTH, 8>::MAX_PAYLOAD];
        let len = self
            .encoder
            .encode(
                self.sender.timestamp(),
                &frame(index),
                &mut buf[HEADER_LEN..],
            )
            .unwrap();
        let len = self.sender.send_in_place(&mut buf, len).unwrap();
        buf.truncate(len);
        buf
    }

    fn skip(&mut self) {
        self.sender.skip();
    }
}

/// Deliver a packet as if it arrived exactly on time: no jitter.
fn deliver(receiver: &mut TestReceiver, bytes: &[u8]) -> Received {
    let packet = Packet::parse(bytes).unwrap();
    receiver.receive(&packet, packet.header.timestamp)
}

/// The frame index played, `None` for concealment, panicking on silence.
fn play(receiver: &mut TestReceiver) -> Option<u32> {
    match receiver.play() {
        Playout::Frame { payload, .. } => {
            let index = u32::from_be_bytes(payload[..4].try_into().unwrap());
            assert_eq!(payload, frame(index), "payload belongs to its frame");
            Some(index)
        }
        Playout::Lost { .. } => None,
        Playout::Silence => panic!("silence"),
    }
}

#[test]
fn an_isolated_loss_is_recovered_exactly() {
    let mut source = RedSource::<1>::new(65_534);
    let mut receiver = red_receiver(1);
    let packets: Vec<_> = (0..6).map(|i| source.send(i)).collect();
    for (i, packet) in packets.iter().enumerate() {
        if i != 2 {
            assert_eq!(deliver(&mut receiver, packet), Received::Accepted);
        }
    }
    let played: Vec<_> = (0..6).map(|_| play(&mut receiver)).collect();
    assert_eq!(played, (0..6).map(Some).collect::<Vec<_>>());
    let counters = receiver.counters();
    assert_eq!(
        (
            counters.recovered,
            counters.concealed,
            counters.late,
            counters.duplicate
        ),
        (1, 0, 0, 0)
    );
}

#[test]
fn depth_two_recovers_a_burst_of_two() {
    let mut source = RedSource::<2>::new(100);
    let mut receiver = red_receiver(2);
    for i in 0..8 {
        let packet = source.send(i);
        if !(3..=4).contains(&i) {
            deliver(&mut receiver, &packet);
        }
    }
    let played: Vec<_> = (0..8).map(|_| play(&mut receiver)).collect();
    assert_eq!(played, (0..8).map(Some).collect::<Vec<_>>());
    assert_eq!(receiver.counters().recovered, 2);
}

#[test]
fn depth_one_recovers_only_the_second_of_a_burst_of_two() {
    let mut source = RedSource::<1>::new(100);
    let mut receiver = red_receiver(1);
    for i in 0..8 {
        let packet = source.send(i);
        if !(3..=4).contains(&i) {
            deliver(&mut receiver, &packet);
        }
    }
    let played: Vec<_> = (0..8).map(|_| play(&mut receiver)).collect();
    let mut expected: Vec<_> = (0..8).map(Some).collect();
    expected[3] = None;
    assert_eq!(played, expected);
}

/// The first frame of a talkspurt has nothing before it to come back with,
/// but its own redundant copy arrives with the second.
#[test]
fn the_first_frame_of_a_talkspurt_is_recovered() {
    let mut source = RedSource::<1>::new(7);
    // No extra wait, so the two frames are enough to start playing.
    let mut receiver = red_receiver(0);
    let first = source.send(0);
    let second = source.send(1);
    drop(first);
    deliver(&mut receiver, &second);
    assert_eq!(play(&mut receiver), Some(0));
    assert_eq!(play(&mut receiver), Some(1));
}

/// Redundant copies of frames already played or buffered are expected and
/// aren't counted as late or duplicate packets.
#[test]
fn copies_of_frames_already_seen_are_not_counted() {
    let mut source = RedSource::<2>::new(0);
    let mut receiver = red_receiver(2);
    for i in 0..20 {
        deliver(&mut receiver, &source.send(i));
        // The target is the initial depth, 2, plus the wait.
        if i >= 3 {
            play(&mut receiver);
        }
    }
    let counters = receiver.counters();
    assert_eq!(
        (counters.late, counters.duplicate, counters.recovered),
        (0, 0, 0)
    );
    assert!(counters.played > 10);

    // Played out as fast as they come, the copies arrive after their
    // frames have played.
    let mut source = RedSource::<2>::new(0);
    let mut receiver = red_receiver(0);
    for i in 0..20 {
        deliver(&mut receiver, &source.send(i));
        for _ in 0..3 {
            receiver.play();
        }
    }
    let counters = receiver.counters();
    assert_eq!(
        (counters.late, counters.duplicate, counters.recovered),
        (0, 0, 0)
    );
    assert_eq!(counters.played, 20);
}

#[test]
fn plain_packets_still_play() {
    let mut sender = Sender::new(0xfeed, PT, TICKS, 0, 0);
    let mut receiver = red_receiver(1);
    for i in 0..4 {
        let mut buf = [0u8; HEADER_LEN + 8];
        let len = sender.send(&frame(i), &mut buf).unwrap();
        assert_eq!(deliver(&mut receiver, &buf[..len]), Received::Accepted);
    }
    for i in 0..4 {
        assert_eq!(play(&mut receiver), Some(i));
    }
}

#[test]
fn without_redundancy_red_packets_are_another_payload_type() {
    let mut source = RedSource::<1>::new(0);
    let mut receiver: TestReceiver = Receiver::new(PT, TICKS, Config::DEFAULT);
    assert_eq!(
        deliver(&mut receiver, &source.send(0)),
        Received::WrongPayloadType
    );
}

fn red_packet(sequence: u16, timestamp: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = common::rtp_header(false, RED, sequence, timestamp, 0xfeed);
    out.extend_from_slice(payload);
    out
}

#[test]
fn malformed_or_foreign_red_payloads_are_dropped() {
    let mut receiver = red_receiver(1);
    let truncated = red_packet(0, 0, &rfc_example()[..6]);
    assert_eq!(deliver(&mut receiver, &truncated), Received::Malformed);
    // A primary that isn't ours.
    let mut foreign = vec![primary_header(0)];
    foreign.extend_from_slice(&frame(0));
    assert_eq!(
        deliver(&mut receiver, &red_packet(0, 0, &foreign)),
        Received::WrongPayloadType
    );
    assert_eq!(
        receiver.stats().received(),
        0,
        "neither reached the statistics"
    );
}

/// Only blocks of our payload type, a whole number of frames back, can be
/// placed; anything else is ignored.
#[test]
fn unplaceable_blocks_are_ignored() {
    let mut receiver = red_receiver(1);
    let mut payload = Vec::new();
    payload.extend_from_slice(&block_header(0, TICKS, 8)); // not our type
    payload.extend_from_slice(&block_header(PT, TICKS + 1, 8)); // not whole frames
    payload.extend_from_slice(&block_header(PT, 0, 8)); // no frames back
    payload.extend_from_slice(&block_header(PT, 2 * TICKS, 8)); // frame 8
    payload.push(primary_header(PT));
    for index in [100, 101, 102, 8] {
        payload.extend_from_slice(&frame(index));
    }
    payload.extend_from_slice(&frame(10));
    deliver(&mut receiver, &red_packet(10, 10 * TICKS, &payload));
    assert_eq!(receiver.counters().recovered, 1);
    assert_eq!(play(&mut receiver), Some(8));
    assert_eq!(play(&mut receiver), None);
    assert_eq!(play(&mut receiver), Some(10));
}

/// While redundant packets arrive, the target depth includes the wait.
#[test]
fn the_wait_raises_the_target() {
    let mut source = RedSource::<1>::new(0);
    let mut receiver = red_receiver(2);
    deliver(&mut receiver, &source.send(0));
    assert_eq!(
        receiver.buffer().target(),
        Config::DEFAULT.initial_depth + 2
    );
    for i in 1..40 {
        deliver(&mut receiver, &source.send(i));
    }
    // No jitter: the jitter target is the minimum, 1.
    assert_eq!(receiver.buffer().target(), 1 + 2);
}

/// Independent loss or bursts on a network with some delay spread. The
/// simulation knows which frames were sent, so it checks every frame played
/// is the right one.
struct Network {
    seed: u64,
    frames: u32,
    /// Per mille chance per packet of starting a burst of losses.
    loss: u32,
    burst: u32,
    /// Delay spread, uniformly random, in ticks.
    spread: u32,
    /// Per mille chance per frame that the sender goes quiet.
    silence: u32,
}

/// Frames that never played are what a listener hears as loss: concealed,
/// or passed over in silence when a burst left the buffer empty.
#[derive(Debug, Default)]
struct Outcome {
    sent: u32,
    played: u32,
    concealed: u32,
    recovered: u32,
    lost_on_network: u32,
}

fn simulate<const DEPTH: usize>(network: &Network, redundancy: bool) -> Outcome {
    let mut rng = Rng::new(network.seed);
    let mut source = RedSource::<DEPTH>::new(rng.next_u32() as u16);
    let mut plain = Sender::new(0xfeed, PT, TICKS, 0, 99);
    let wait = if redundancy { DEPTH as u8 } else { 0 };
    let config = Config {
        initial_depth: 2,
        min_depth: 1,
        max_depth: 16,
    };
    let mut receiver: TestReceiver = Receiver::new(PT, TICKS, config).with_redundancy(RED, wait);

    let mut arrivals: BTreeMap<(u64, u32), Vec<u8>> = BTreeMap::new();
    let mut result = Outcome::default();
    let mut burst_left = 0;
    let mut quiet = 0;
    for index in 0..network.frames {
        if quiet == 0 && rng.chance(network.silence) {
            quiet = 5 + rng.below(50);
        }
        if quiet > 0 {
            quiet -= 1;
            source.skip();
            plain.skip();
            continue;
        }
        let bytes = if redundancy {
            source.send(index)
        } else {
            let mut buf = vec![0u8; HEADER_LEN + 8];
            plain.send(&frame(index), &mut buf).unwrap();
            buf
        };
        result.sent += 1;
        if burst_left == 0 && rng.chance(network.loss) {
            burst_left = network.burst;
        }
        if burst_left > 0 {
            burst_left -= 1;
            result.lost_on_network += 1;
            continue;
        }
        let arrival = u64::from(index * TICKS) + 600 + u64::from(rng.below(network.spread + 1));
        arrivals.insert((arrival, index), bytes);
    }

    // The receiver plays on its own clock, half a frame out of step.
    let mut last_index = None;
    let mut pending = arrivals.into_iter().peekable();
    for period in 0..u64::from(network.frames) + 40 {
        let now = period * u64::from(TICKS) + u64::from(TICKS / 2);
        while let Some(((arrival, _), bytes)) = pending.next_if(|((at, _), _)| *at <= now) {
            receiver.receive(&Packet::parse(&bytes).unwrap(), arrival as u32);
        }
        match receiver.play() {
            Playout::Frame { payload, .. } => {
                let index = u32::from_be_bytes(payload[..4].try_into().unwrap());
                assert_eq!(payload, frame(index));
                assert!(
                    last_index.map_or(true, |last| index > last),
                    "plays in order"
                );
                last_index = Some(index);
                result.played += 1;
            }
            Playout::Lost { .. } => result.concealed += 1,
            Playout::Silence => {}
        }
    }
    result.recovered = receiver.counters().recovered;
    result
}

impl Outcome {
    fn missing(&self) -> u32 {
        self.sent - self.played
    }
}

/// The issue's target: with independent 5% loss, depth 1 cuts the frames
/// lost to the listener by over 90%. What it can't recover is a loss whose
/// next packet is lost too, about 5% of them.
#[test]
fn depth_one_recovers_nearly_all_independent_losses() {
    for seed in 0..4 {
        let network = Network {
            seed,
            frames: 10_000,
            loss: 50,
            burst: 1,
            spread: 200,
            silence: 0,
        };
        let without = simulate::<1>(&network, false);
        let with = simulate::<1>(&network, true);
        assert_eq!(without.missing(), without.lost_on_network, "{without:?}");
        assert!(
            with.missing() * 10 < without.missing(),
            "seed {seed}: {with:?} vs {without:?}"
        );
        assert_eq!(with.missing(), with.lost_on_network - with.recovered);
    }
}

/// Bursts of two: depth 2 recovers them, depth 1 only their second frame.
#[test]
fn depth_two_recovers_bursts_of_two() {
    for seed in 0..4 {
        let network = Network {
            seed,
            frames: 10_000,
            loss: 25,
            burst: 2,
            spread: 200,
            silence: 0,
        };
        let without = simulate::<2>(&network, false);
        let one = simulate::<1>(&network, true);
        let two = simulate::<2>(&network, true);
        assert!(
            two.missing() * 10 < without.missing(),
            "seed {seed}: {two:?} vs {without:?}"
        );
        assert!(
            one.missing() * 3 > without.missing(),
            "seed {seed}: {one:?} vs {without:?}"
        );
        assert!(
            two.missing() * 10 < one.missing(),
            "seed {seed}: {two:?} vs {one:?}"
        );
    }
}

/// Talkspurts start and stop, with loss and more delay spread. Redundancy
/// never carries a frame across a silence, and `simulate` checks frame by
/// frame that none is played in the wrong place.
#[test]
fn silences_do_not_misplace_frames() {
    for seed in 0..4 {
        let network = Network {
            seed,
            frames: 10_000,
            loss: 50,
            burst: 2,
            spread: 400,
            silence: 20,
        };
        let without = simulate::<2>(&network, false);
        let with = simulate::<2>(&network, true);
        assert!(
            with.missing() * 10 < without.missing(),
            "seed {seed}: {with:?} vs {without:?}"
        );
    }
}
