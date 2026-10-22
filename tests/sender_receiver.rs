//! Sender conformance (RFC 3550 §5.1, RFC 3551 §4.1) and receiver stream
//! handling. Sender output is compared with headers assembled from the RFC
//! diagram in `common`, not with the crate's own writer.

mod common;

use common::rtp_header;
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::Packet;
use embedded_rtp::{Error, Received, Receiver, Sender};

const TICKS: u32 = 320;

fn send(sender: &mut Sender, payload: &[u8]) -> Vec<u8> {
    let mut buf = [0u8; 1500];
    let len = sender.send(payload, &mut buf).unwrap();
    buf[..len].to_vec()
}

fn expected(marker: bool, seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = rtp_header(marker, 96, seq, ts, 0xdead_beef);
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn packets_follow_the_header_diagram() {
    let mut sender = Sender::new(0xdead_beef, 96, TICKS, 7, 1_000);
    assert_eq!(
        send(&mut sender, &[1, 2]),
        expected(true, 7, 1_000, &[1, 2])
    );
    assert_eq!(
        send(&mut sender, &[3, 4]),
        expected(false, 8, 1_320, &[3, 4])
    );
    assert_eq!(send(&mut sender, &[]), expected(false, 9, 1_640, &[]));
}

#[test]
fn sequence_numbers_step_by_one_and_wrap() {
    let mut sender = Sender::new(0xdead_beef, 96, TICKS, 65_534, 0);
    let seqs: Vec<u16> = (0..4)
        .map(|_| {
            Packet::parse(&send(&mut sender, &[0]))
                .unwrap()
                .header
                .sequence
        })
        .collect();
    assert_eq!(seqs, [65_534, 65_535, 0, 1]);
}

#[test]
fn timestamps_step_by_frame_duration_and_wrap() {
    let mut sender = Sender::new(0xdead_beef, 96, TICKS, 0, u32::MAX - 500);
    let stamps: Vec<u32> = (0..3)
        .map(|_| {
            Packet::parse(&send(&mut sender, &[0]))
                .unwrap()
                .header
                .timestamp
        })
        .collect();
    assert_eq!(stamps, [u32::MAX - 500, u32::MAX - 180, 139]);
}

/// RFC 3551 §4.1: silence suppression keeps sequence numbers contiguous,
/// lets the timestamp run on, and marks the first packet of each talkspurt.
#[test]
fn silence_advances_time_not_sequence_and_marks_the_next_talkspurt() {
    let mut sender = Sender::new(0xdead_beef, 96, TICKS, 100, 0);
    assert_eq!(send(&mut sender, &[0]), expected(true, 100, 0, &[0]));
    assert_eq!(send(&mut sender, &[0]), expected(false, 101, 320, &[0]));
    for _ in 0..5 {
        sender.skip();
    }
    assert_eq!(
        send(&mut sender, &[0]),
        expected(true, 102, 640 + 5 * 320, &[0])
    );
    assert_eq!(
        send(&mut sender, &[0]),
        expected(false, 103, 960 + 5 * 320, &[0])
    );
}

#[test]
fn ssrc_and_payload_type_are_constant() {
    let mut sender = Sender::new(0x0102_0304, 0xff, TICKS, 0, 0);
    for _ in 0..10 {
        let header = Packet::parse(&send(&mut sender, &[0; 4])).unwrap().header;
        assert_eq!(header.ssrc, 0x0102_0304);
        assert_eq!(header.payload_type, 0x7f, "payload type is 7 bits");
    }
}

/// RFC 3550 §6.4.1: the sender's packet and octet counts. Octets are payload
/// only, "not including header or padding".
#[test]
fn packet_and_octet_counts_cover_payload_only() {
    let mut sender = Sender::new(1, 96, TICKS, 0, 0);
    send(&mut sender, &[0; 640]);
    sender.skip();
    send(&mut sender, &[0; 100]);
    assert_eq!(sender.packet_count(), 2);
    assert_eq!(sender.octet_count(), 740);
}

#[test]
fn sending_in_place_produces_the_same_packets() {
    let mut copying = Sender::new(0xdead_beef, 96, TICKS, 65_535, 9);
    let mut in_place = copying.clone();
    for i in 0..5u8 {
        if i == 3 {
            copying.skip();
            in_place.skip();
        }
        let payload = [i; 7];
        let mut buf = [0u8; 64];
        buf[12..19].copy_from_slice(&payload);
        let len = in_place.send_in_place(&mut buf, payload.len()).unwrap();
        assert_eq!(&buf[..len], send(&mut copying, &payload).as_slice());
    }
    assert_eq!(in_place.octet_count(), copying.octet_count());
}

#[test]
fn sending_in_place_into_a_short_buffer_changes_nothing() {
    let mut sender = Sender::new(0xdead_beef, 96, TICKS, 5, 50);
    let mut buf = [0u8; 14];
    assert_eq!(
        sender.send_in_place(&mut buf, 3),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(buf, [0; 14], "nothing written");
    assert_eq!(sender.sequence(), 5);
}

#[test]
fn a_failed_send_changes_nothing() {
    let mut sender = Sender::new(0xdead_beef, 96, TICKS, 5, 50);
    let mut small = [0u8; 12];
    assert_eq!(
        sender.send(&[1, 2, 3], &mut small),
        Err(Error::BufferTooSmall)
    );
    assert_eq!((sender.packet_count(), sender.octet_count()), (0, 0));
    assert_eq!(
        send(&mut sender, &[1, 2, 3]),
        expected(true, 5, 50, &[1, 2, 3])
    );
}

type TestReceiver = Receiver<8, 4>;

fn receiver() -> TestReceiver {
    Receiver::new(
        96,
        TICKS,
        Config {
            initial_depth: 1,
            min_depth: 1,
            max_depth: 8,
        },
    )
}

fn packet(ssrc: u32, seq: u16, payload: [u8; 4]) -> Vec<u8> {
    let mut bytes = rtp_header(false, 96, seq, u32::from(seq) * TICKS, ssrc);
    bytes.extend_from_slice(&payload);
    bytes
}

fn deliver(receiver: &mut TestReceiver, bytes: &[u8]) -> Received {
    receiver.receive(&Packet::parse(bytes).unwrap(), 0)
}

fn play(receiver: &mut TestReceiver) -> Option<[u8; 4]> {
    match receiver.play() {
        Playout::Frame { payload, .. } => Some(payload.try_into().unwrap()),
        _ => None,
    }
}

#[test]
fn first_packets_play_while_the_source_is_on_probation() {
    let mut receiver = receiver();
    assert_eq!(
        deliver(&mut receiver, &packet(1, 10, [10; 4])),
        Received::Accepted
    );
    assert!(!receiver.stats().is_valid(), "still on probation");
    assert_eq!(play(&mut receiver), Some([10; 4]));
}

#[test]
fn other_payload_types_are_ignored() {
    let mut receiver = receiver();
    let mut bytes = packet(1, 10, [0; 4]);
    bytes[1] = 0; // PT 0, PCMU
    assert_eq!(deliver(&mut receiver, &bytes), Received::WrongPayloadType);
    assert_eq!(receiver.ssrc(), None);
    assert!(receiver.report().is_none());
}

#[test]
fn a_new_ssrc_is_a_new_stream() {
    let mut receiver = receiver();
    deliver(&mut receiver, &packet(1, 10, [1; 4]));
    deliver(&mut receiver, &packet(1, 11, [1; 4]));
    // Another sender takes over the address. The old stream's frames go.
    assert_eq!(
        deliver(&mut receiver, &packet(2, 5_000, [2; 4])),
        Received::Accepted
    );
    assert_eq!(receiver.ssrc(), Some(2));
    assert_eq!(play(&mut receiver), Some([2; 4]));
    assert_eq!(play(&mut receiver), None);
    assert!(!receiver.stats().is_valid(), "statistics restarted");
}

#[test]
fn a_confirmed_restart_discards_the_old_stream() {
    let mut receiver = receiver();
    for seq in 10..13 {
        deliver(&mut receiver, &packet(1, seq, [1; 4]));
    }
    // Same SSRC, sequence numbers far away: dropped until confirmed.
    assert_eq!(
        deliver(&mut receiver, &packet(1, 40_000, [2; 4])),
        Received::Invalid
    );
    assert_eq!(
        deliver(&mut receiver, &packet(1, 40_001, [3; 4])),
        Received::Accepted
    );
    assert_eq!(play(&mut receiver), Some([3; 4]), "old frames discarded");
}

#[test]
fn reports_describe_the_stream() {
    let mut receiver = receiver();
    for seq in [1, 2, 3, 5, 6] {
        deliver(&mut receiver, &packet(9, seq, [0; 4]));
    }
    let report = receiver.report().unwrap();
    assert_eq!(report.extended_highest, 6);
    assert_eq!(report.cumulative_lost, 1);
}

#[test]
fn sender_to_receiver_is_bit_exact() {
    let mut sender = Sender::new(77, 96, TICKS, 65_000, 0);
    let mut receiver = receiver();
    let mut sent = Vec::new();
    let mut heard = Vec::new();
    for i in 0..2_000u32 {
        // Talk for 50 frames, pause for 10.
        if i % 60 < 50 {
            let frame = i.to_be_bytes();
            let mut buf = [0u8; 16];
            let len = sender.send(&frame, &mut buf).unwrap();
            assert_eq!(
                receiver.receive(&Packet::parse(&buf[..len]).unwrap(), i * TICKS),
                Received::Accepted
            );
            sent.push(frame);
        } else {
            sender.skip();
        }
        heard.extend(play(&mut receiver));
    }
    while let Some(frame) = play(&mut receiver) {
        heard.push(frame);
    }
    assert_eq!(heard, sent);
    assert_eq!(receiver.counters().concealed, 0);
}

/// RFC 3550 §6.4.1: a receiver echoes the last SR it got from a source as
/// LSR, with how long it held it as DLSR, so the sender can work out the
/// round trip.
#[test]
fn report_blocks_echo_the_last_sender_report() {
    use embedded_rtp::rtcp::{ntp_from_micros, ntp_short, round_trip};

    let mut sender = Sender::new(0x5555, 96, TICKS, 100, 0);
    let mut receiver = receiver();
    assert!(receiver.report_block(0).is_none(), "nothing received yet");

    for _ in 0..10 {
        let mut buf = [0u8; 32];
        let len = sender.send(&[0; 4], &mut buf).unwrap();
        receiver.receive(&Packet::parse(&buf[..len]).unwrap(), 0);
    }
    // No SR yet: the last-SR fields are zero.
    let block = receiver.report_block(1_000).unwrap();
    assert_eq!(
        (block.ssrc, block.last_sr, block.delay_since_last_sr),
        (0x5555, 0, 0)
    );
    assert_eq!(block.extended_highest, 109);

    // The sender reports at t = 10 s on its clock; it arrives at t = 10.2 s
    // on the receiver's, which reports back at t = 10.7 s.
    let sent_at = ntp_from_micros(10_000_000);
    let info = sender.sender_info(sent_at);
    assert_eq!((info.packet_count, info.octet_count), (10, 40));
    receiver.sender_report(0x5555, &info, ntp_short(ntp_from_micros(10_200_000)));
    let block = receiver
        .report_block(ntp_short(ntp_from_micros(10_700_000)))
        .unwrap();
    assert_eq!(block.last_sr, ntp_short(sent_at));
    assert_eq!(block.delay_since_last_sr, 0x8000, "held for 0.5 s");

    // Back at the sender at t = 10.9 s: 0.9 s since sending, 0.5 s of it
    // held, so 0.4 s of round trip.
    let rtt = round_trip(
        ntp_short(ntp_from_micros(10_900_000)),
        block.last_sr,
        block.delay_since_last_sr,
    );
    let rtt = rtt.unwrap();
    assert!((26_213..=26_215).contains(&rtt), "{rtt} / 65536 s"); // 0.4 * 65536

    // An SR from some other source is not echoed.
    receiver.sender_report(0x9999, &info, 0);
    assert_eq!(receiver.report_block(0).unwrap().last_sr, 0);
}
