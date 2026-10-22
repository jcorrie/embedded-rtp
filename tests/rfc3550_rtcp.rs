//! RTCP conformance: RFC 3550 §6.1, §6.4, §6.5, §6.6 and appendix A.2.
//!
//! Expected bytes are written out by hand from the RFC diagrams, the
//! validity rules are tested one by one, the round-trip arithmetic is
//! checked against the RFC's own worked example, and packets captured from
//! ffmpeg are parsed and checked against what ffmpeg was streaming.

mod common;

use common::Rng;
use embedded_rtp::rtcp::{
    self, ntp_from_micros, ntp_short, round_trip, sdes, Compound, Packet, ReportBlock, SenderInfo,
    Writer,
};
use embedded_rtp::Error;

fn write(build: impl FnOnce(&mut Writer<'_>)) -> Vec<u8> {
    let mut buf = [0u8; 1500];
    let mut writer = Writer::new(&mut buf);
    build(&mut writer);
    writer.finish().to_vec()
}

fn packets(bytes: &[u8]) -> Vec<Packet<'_>> {
    Compound::parse(bytes).unwrap().packets().collect()
}

/// §6.4.1: SR header, sender info, then report blocks.
///
/// ```text
/// |V=2|P|    RC   |   PT=SR=200   |             length            |
/// |                         SSRC of sender                        |
/// |              NTP timestamp, most significant word             |
/// |             NTP timestamp, least significant word             |
/// |                         RTP timestamp                         |
/// |                     sender's packet count                     |
/// |                      sender's octet count                     |
/// |                 SSRC_1 (SSRC of first source)                 |
/// | fraction lost |       cumulative number of packets lost       |
/// |           extended highest sequence number received           |
/// |                      interarrival jitter                      |
/// |                         last SR (LSR)                         |
/// |                   delay since last SR (DLSR)                  |
/// ```
#[test]
fn sender_report_matches_the_diagram() {
    let info = SenderInfo {
        ntp_timestamp: 0xb705_2000_1122_3344,
        rtp_timestamp: 0x0a0b_0c0d,
        packet_count: 47,
        octet_count: 48_000,
    };
    let block = ReportBlock {
        ssrc: 0xfeed_beef,
        fraction_lost: 0x40,
        cumulative_lost: 0x01_0203,
        extended_highest: 0x0001_0005,
        jitter: 0x99,
        last_sr: 0xb705_2000,
        delay_since_last_sr: 0x0005_4000,
    };
    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        0x81, 200, 0x00, 0x0c,            // V=2 RC=1, SR, length 13 words - 1
        0xa0, 0xee, 0x87, 0xc8,           // sender SSRC
        0xb7, 0x05, 0x20, 0x00,           // NTP, most significant word
        0x11, 0x22, 0x33, 0x44,           // NTP, least significant word
        0x0a, 0x0b, 0x0c, 0x0d,           // RTP timestamp
        0x00, 0x00, 0x00, 47,             // packet count
        0x00, 0x00, 0xbb, 0x80,           // octet count: 48000
        0xfe, 0xed, 0xbe, 0xef,           // SSRC_1
        0x40, 0x01, 0x02, 0x03,           // fraction lost, cumulative lost
        0x00, 0x01, 0x00, 0x05,           // extended highest sequence
        0x00, 0x00, 0x00, 0x99,           // jitter
        0xb7, 0x05, 0x20, 0x00,           // LSR
        0x00, 0x05, 0x40, 0x00,           // DLSR
    ];
    let bytes = write(|w| w.sender_report(0xa0ee_87c8, &info, &[block]).unwrap());
    assert_eq!(bytes, expected);
    match packets(&expected)[..] {
        [Packet::SenderReport {
            ssrc,
            info: got,
            reports,
        }] => {
            assert_eq!(ssrc, 0xa0ee_87c8);
            assert_eq!(got, info);
            assert_eq!(reports.collect::<Vec<_>>(), [block]);
        }
        ref other => panic!("{other:?}"),
    }
}

/// §6.4.2: an RR with no report blocks is legal, and is how a compound
/// packet starts when there is nothing to report.
#[test]
fn empty_receiver_report() {
    let expected = [0x80, 201, 0x00, 0x01, 0x00, 0x00, 0x00, 0x07];
    assert_eq!(write(|w| w.receiver_report(7, &[]).unwrap()), expected);
    match packets(&expected)[..] {
        [Packet::ReceiverReport { ssrc: 7, reports }] => assert_eq!(reports.len(), 0),
        ref other => panic!("{other:?}"),
    }
}

/// §6.5: items end with one or more zero octets, padding the chunk to a
/// 32-bit boundary - so items that end exactly on a boundary still need a
/// whole word of zeros.
#[test]
fn sdes_chunks_end_with_zeros_to_a_word_boundary() {
    #[rustfmt::skip]
    let aligned = [
        0x81, 202, 0x00, 0x03,            // SC=1, SDES, length 4 words - 1
        0x00, 0x00, 0x00, 0x09,           // SSRC
        1, 2, b'a', b'b',                 // CNAME "ab": ends on a boundary...
        0, 0, 0, 0,                       // ...so a full word of zeros follows
    ];
    assert_eq!(
        write(|w| w.source_description(9, &[(sdes::CNAME, b"ab")]).unwrap()),
        aligned
    );

    #[rustfmt::skip]
    let padded = [
        0x81, 202, 0x00, 0x03,
        0x00, 0x00, 0x00, 0x09,
        1, 3, b'a', b'b', b'c', 0, 0, 0,  // CNAME "abc", then three zeros
    ];
    assert_eq!(
        write(|w| w.source_description(9, &[(sdes::CNAME, b"abc")]).unwrap()),
        padded
    );

    for bytes in [&aligned[..], &padded[..]] {
        // A compound packet must start with a report (A.2).
        let compound = [empty_rr(), bytes.to_vec()].concat();
        let chunk = match packets(&compound)[..] {
            [_, Packet::SourceDescription(mut chunks)] => chunks.next().unwrap(),
            ref other => panic!("{other:?}"),
        };
        assert_eq!(chunk.ssrc, 9);
        assert!(chunk.cname().unwrap().starts_with(b"ab"));
        assert_eq!(chunk.name(), None);
    }
}

#[test]
fn sdes_with_several_items_in_every_alignment() {
    for name_len in 0..12 {
        let name = vec![b'n'; name_len];
        let bytes = write(|w| {
            w.receiver_report(1, &[]).unwrap();
            w.source_description(
                1,
                &[
                    (sdes::CNAME, b"x@y"),
                    (sdes::NAME, &name),
                    (sdes::TOOL, b"t"),
                ],
            )
            .unwrap();
        });
        assert_eq!(bytes.len() % 4, 0);
        let chunk = match packets(&bytes)[..] {
            [_, Packet::SourceDescription(mut chunks)] => chunks.next().unwrap(),
            ref other => panic!("{other:?}"),
        };
        let items: Vec<_> = chunk.items.map(|i| (i.kind, i.value.to_vec())).collect();
        assert_eq!(
            items,
            [
                (sdes::CNAME, b"x@y".to_vec()),
                (sdes::NAME, name.clone()),
                (sdes::TOOL, b"t".to_vec())
            ]
        );
    }
}

/// §6.6: SSRCs, then an optional length-prefixed reason padded to a word.
#[test]
fn goodbye_matches_the_diagram() {
    let plain = [0x81, 203, 0x00, 0x01, 0xa0, 0xee, 0x87, 0xc8];
    assert_eq!(write(|w| w.goodbye(&[0xa0ee_87c8], None).unwrap()), plain);

    #[rustfmt::skip]
    let with_reason = [
        0x82, 203, 0x00, 0x04,            // SC=2, BYE, length 5 words - 1
        0, 0, 0, 1,
        0, 0, 0, 2,
        5, b'l', b'e', b'a',              // reason: length 5, "leave"...
        b'v', b'e', 0, 0,                 // ...padded to a word
    ];
    assert_eq!(
        write(|w| w.goodbye(&[1, 2], Some(b"leave")).unwrap()),
        with_reason
    );

    match packets(&[&[0x80, 201, 0, 1, 0, 0, 0, 1][..], &with_reason].concat())[..] {
        [_, Packet::Goodbye { sources, reason }] => {
            assert_eq!(sources.collect::<Vec<_>>(), [1, 2]);
            assert_eq!(reason, Some(&b"leave"[..]));
        }
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn cumulative_loss_is_24_bit_signed() {
    let block = |lost| ReportBlock {
        cumulative_lost: lost,
        ..ReportBlock::default()
    };
    for (written, read) in [
        (-1, -1),
        (0x7f_ffff, 0x7f_ffff),
        (-0x80_0000, -0x80_0000),
        (50_000_000, 0x7f_ffff),
        (-50_000_000, -0x80_0000),
    ] {
        let bytes = write(|w| w.receiver_report(1, &[block(written)]).unwrap());
        // The field is the low 24 bits of the second word of the block.
        if written == -1 {
            assert_eq!(bytes[13..16], [0xff, 0xff, 0xff]);
        }
        match packets(&bytes)[..] {
            [Packet::ReceiverReport { mut reports, .. }] => {
                assert_eq!(
                    reports.next().unwrap().cumulative_lost,
                    read,
                    "wrote {written}"
                );
            }
            ref other => panic!("{other:?}"),
        }
    }
}

/// RFC 3550 §6.4.1, figure 2: an SR's NTP middle bits are echoed as LSR,
/// the receiver adds how long it held it as DLSR, and the sender subtracts
/// both from the arrival time.
#[test]
fn round_trip_from_the_rfc_example() {
    let arrival = 0xb710_8000; // 46864.500 s
    let last_sr = 0xb705_2000; // 46853.125 s
    let delay = 0x0005_4000; // 5.250 s
    assert_eq!(round_trip(arrival, last_sr, delay), Some(0x0006_2000)); // 6.125 s
    assert_eq!(round_trip(arrival, 0, delay), None, "no SR received yet");
    assert_eq!(round_trip(last_sr, last_sr, delay), None, "inconsistent");
}

#[test]
fn ntp_formats() {
    assert_eq!(ntp_from_micros(0), 0);
    assert_eq!(ntp_from_micros(1_500_000), 0x0000_0001_8000_0000);
    assert_eq!(ntp_from_micros(46_864_500_000), 0x0000_b710_8000_0000);
    assert_eq!(ntp_short(0x0000_b710_8000_0000), 0xb710_8000);
}

fn empty_rr() -> Vec<u8> {
    vec![0x80, 201, 0, 1, 0, 0, 0, 1]
}

/// RFC 3550 A.2: the checks a receiver applies to a compound packet.
#[test]
fn appendix_a2_validity_checks() {
    let sdes = write(|w| w.source_description(1, &[(sdes::CNAME, b"c")]).unwrap());
    let good = [empty_rr(), sdes.clone()].concat();
    assert!(Compound::parse(&good).is_ok());

    // The first packet must be SR or RR.
    assert_eq!(Compound::parse(&sdes), Err(Error::BadCompound));
    // Every packet must be version 2.
    let mut bad = good.clone();
    bad[8] = (bad[8] & 0x3f) | 0x40;
    assert_eq!(Compound::parse(&bad), Err(Error::BadVersion));
    // Only the last packet may be padded.
    let mut bad = good.clone();
    bad[0] |= 0x20;
    assert_eq!(Compound::parse(&bad), Err(Error::BadPadding));
    // The lengths must add up to the datagram: neither short nor long.
    assert_eq!(
        Compound::parse(&good[..good.len() - 4]),
        Err(Error::Truncated)
    );
    assert_eq!(
        Compound::parse(&[good.clone(), vec![0x80, 201]].concat()),
        Err(Error::Truncated)
    );
    assert_eq!(Compound::parse(&[]), Err(Error::Truncated));
}

#[test]
fn padding_on_the_last_packet() {
    // An empty RR padded with one word: length 2 words - 1, last octet 4.
    let padded = [0xa0, 201, 0, 2, 0, 0, 0, 1, 0, 0, 0, 4];
    match packets(&padded)[..] {
        [Packet::ReceiverReport { ssrc: 1, reports }] => assert_eq!(reports.len(), 0),
        ref other => panic!("{other:?}"),
    }
    let mut zero = padded;
    zero[11] = 0;
    assert_eq!(Compound::parse(&zero), Err(Error::BadPadding));
    let mut too_much = padded;
    too_much[11] = 9;
    assert_eq!(Compound::parse(&too_much), Err(Error::BadPadding));
}

#[test]
fn counts_that_overrun_the_packet_are_rejected() {
    // RR claiming one report block in an 8-byte packet.
    assert_eq!(
        Compound::parse(&[0x81, 201, 0, 1, 0, 0, 0, 1]),
        Err(Error::Truncated)
    );
    // SDES claiming two chunks with room for one.
    let one = write(|w| w.source_description(1, &[(sdes::CNAME, b"c")]).unwrap());
    let mut two = one.clone();
    two[0] = 0x82;
    assert_eq!(
        Compound::parse(&[empty_rr(), two].concat()),
        Err(Error::Truncated)
    );
    // BYE claiming two sources with one.
    assert_eq!(
        Compound::parse(&[empty_rr(), vec![0x82, 203, 0, 1, 0, 0, 0, 1]].concat()),
        Err(Error::Truncated)
    );
}

#[test]
fn other_packet_types_pass_through() {
    let app = [0x81, 204, 0, 2, 0, 0, 0, 1, b'n', b'a', b'm', b'e'];
    match packets(&[empty_rr(), app.to_vec()].concat())[..] {
        [_, Packet::Other {
            packet_type: 204,
            count: 1,
            body,
        }] => assert_eq!(body, &app[4..]),
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn every_written_packet_has_a_correct_length_field() {
    let mut rng = Rng::new(3);
    for _ in 0..2_000 {
        let blocks: Vec<ReportBlock> = (0..rng.below(32))
            .map(|_| ReportBlock {
                ssrc: rng.next_u32(),
                jitter: rng.next_u32(),
                ..ReportBlock::default()
            })
            .collect();
        let (name_len, reason_len) = (rng.below(256) as usize, rng.below(256) as usize);
        let name = rng.bytes(name_len);
        let reason = rng.bytes(reason_len);
        let sources: Vec<u32> = (0..rng.below(32)).map(|_| rng.next_u32()).collect();
        let bytes = write(|w| {
            w.sender_report(1, &SenderInfo::default(), &blocks).unwrap();
            w.source_description(1, &[(sdes::NAME, &name)]).unwrap();
            w.goodbye(&sources, Some(&reason)).unwrap();
        });
        // Walk the headers: each length field is the packet's words minus one.
        let mut at = 0;
        let mut seen = 0;
        while at < bytes.len() {
            let words = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]])) + 1;
            at += 4 * words;
            seen += 1;
        }
        assert_eq!((at, seen), (bytes.len(), 3));
        match packets(&bytes)[..] {
            [Packet::SenderReport { reports, .. }, Packet::SourceDescription(mut chunks), Packet::Goodbye {
                sources: got,
                reason: r,
            }] => {
                assert_eq!(reports.collect::<Vec<_>>(), blocks);
                assert_eq!(chunks.next().unwrap().name(), Some(&name[..]));
                assert_eq!(got.collect::<Vec<_>>(), sources);
                assert_eq!(
                    r,
                    if reason.is_empty() {
                        None
                    } else {
                        Some(&reason[..])
                    }
                );
            }
            ref other => panic!("{other:?}"),
        }
    }
}

/// Walk everything a parsed compound offers, checking slices stay inside.
fn exercise(input: &[u8]) {
    let Ok(compound) = Compound::parse(input) else {
        return;
    };
    let bounds = input.as_ptr_range();
    let inside = |s: &[u8]| {
        let r = s.as_ptr_range();
        bounds.start <= r.start && r.end <= bounds.end
    };
    for packet in compound.packets() {
        match packet {
            Packet::SenderReport { reports, .. } | Packet::ReceiverReport { reports, .. } => {
                reports.for_each(drop);
            }
            Packet::SourceDescription(chunks) => {
                for chunk in chunks {
                    for item in chunk.items {
                        assert!(inside(item.value));
                    }
                }
            }
            Packet::Goodbye { sources, reason } => {
                sources.for_each(drop);
                assert!(reason.map_or(true, inside));
            }
            Packet::Other { body, .. } => assert!(inside(body)),
        }
    }
}

#[test]
fn malformed_input_never_panics() {
    let mut rng = Rng::new(9);
    let sample = write(|w| {
        w.sender_report(1, &SenderInfo::default(), &[ReportBlock::default(); 2])
            .unwrap();
        w.source_description(1, &[(sdes::CNAME, b"abc"), (sdes::NAME, b"de")])
            .unwrap();
        w.goodbye(&[1, 2], Some(b"x")).unwrap();
    });
    for len in 0..=sample.len() {
        exercise(&sample[..len]);
    }
    for bit in 0..sample.len() * 8 {
        let mut flipped = sample.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        exercise(&flipped);
    }
    for _ in 0..100_000 {
        let len = 4 * rng.below(20) as usize;
        let mut input = rng.bytes(len);
        if let Some(first) = input.first_mut() {
            *first = (*first & 0x1f) | 0x80;
        }
        if input.len() > 1 {
            input[1] = 200 + input[1] % 5;
        }
        exercise(&input);
    }
}

/// RTCP from an independent implementation: what ffmpeg's RTP muxer sent
/// while streaming 1.5 s of L16 at 16 kHz (`tests/fixtures/README.md`).
#[test]
fn ffmpeg_rtcp_parses_and_adds_up() {
    let datagrams = common::split_fixture(include_bytes!("fixtures/ffmpeg-rtcp.bin"));
    assert_eq!(datagrams.len(), 2);
    let mut reports = Vec::new();
    for (i, datagram) in datagrams.iter().enumerate() {
        let packets = packets(datagram);
        let (ssrc, info) = match packets[0] {
            Packet::SenderReport {
                ssrc,
                info,
                reports,
            } => {
                assert_eq!(reports.len(), 0, "ffmpeg only sends");
                (ssrc, info)
            }
            ref other => panic!("{other:?}"),
        };
        match packets[1] {
            Packet::SourceDescription(mut chunks) => {
                let chunk = chunks.next().unwrap();
                assert_eq!(chunk.ssrc, ssrc);
                assert_eq!(chunk.cname(), Some(&b"embedded-rtp-test"[..]));
            }
            ref other => panic!("{other:?}"),
        }
        if i == 1 {
            match packets[2] {
                Packet::Goodbye { sources, reason } => {
                    assert_eq!(sources.collect::<Vec<_>>(), [ssrc]);
                    assert_eq!(reason, None);
                }
                ref other => panic!("{other:?}"),
            }
        }
        reports.push(info);
    }
    let (first, last) = (reports[0], reports[1]);
    assert_eq!((first.packet_count, first.octet_count), (0, 0));
    // 1.5 s of 16-bit mono at 16 kHz.
    assert_eq!(last.octet_count, 48_000);
    assert_eq!(last.packet_count, 47);
    // The NTP and RTP clocks advance together: RTP ticks at 16 kHz.
    let ntp_seconds = (last.ntp_timestamp - first.ntp_timestamp) as f64 / 2f64.powi(32);
    let rtp_seconds = f64::from(last.rtp_timestamp.wrapping_sub(first.rtp_timestamp)) / 16_000.0;
    assert!(
        (ntp_seconds - rtp_seconds).abs() < 0.01,
        "NTP {ntp_seconds} s vs RTP {rtp_seconds} s"
    );
}

#[test]
fn module_constants() {
    assert_eq!(
        (rtcp::SR, rtcp::RR, rtcp::SDES, rtcp::BYE, rtcp::APP),
        (200, 201, 202, 203, 204)
    );
}
