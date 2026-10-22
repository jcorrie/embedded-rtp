//! RTP wire format conformance: RFC 3550 §5.1 and §5.3.1, RFC 5761 §4.
//!
//! Packets are built from the RFC diagrams in `common`, never with the
//! crate's writer, and the parser's view of them is checked against how they
//! were built. The writer is checked against the same hand-built bytes.

mod common;

use common::{rtp_header, Rng};
use embedded_rtp::packet::{self, Header, Packet, HEADER_LEN};
use embedded_rtp::Error;

/// Everything that went into a hand-built packet.
struct Built {
    bytes: Vec<u8>,
    header: Header,
    csrcs: Vec<u32>,
    extension: Option<(u16, Vec<u8>)>,
    payload: Vec<u8>,
    padding: usize,
}

/// Assemble a packet field by field per RFC 3550 §5.1 (CSRC list), §5.3.1
/// (extension: 16-bit profile, 16-bit length in 32-bit words excluding the
/// four-byte extension header) and §5.1 P bit (the last octet counts the
/// padding, itself included).
fn build(
    rng: &mut Rng,
    csrc_count: usize,
    extension_words: Option<usize>,
    payload_len: usize,
    padding: usize,
) -> Built {
    let header = Header {
        marker: rng.chance(500),
        payload_type: rng.below(128) as u8,
        sequence: rng.next_u32() as u16,
        timestamp: rng.next_u32(),
        ssrc: rng.next_u32(),
    };
    let mut bytes = rtp_header(
        header.marker,
        header.payload_type,
        header.sequence,
        header.timestamp,
        header.ssrc,
    );
    bytes[0] |= csrc_count as u8;
    if extension_words.is_some() {
        bytes[0] |= 0x10;
    }
    if padding > 0 {
        bytes[0] |= 0x20;
    }

    let csrcs: Vec<u32> = (0..csrc_count).map(|_| rng.next_u32()).collect();
    for csrc in &csrcs {
        bytes.extend_from_slice(&csrc.to_be_bytes());
    }

    let extension = extension_words.map(|words| {
        let profile = rng.next_u32() as u16;
        let data = rng.bytes(words * 4);
        bytes.extend_from_slice(&profile.to_be_bytes());
        bytes.extend_from_slice(&(words as u16).to_be_bytes());
        bytes.extend_from_slice(&data);
        (profile, data)
    });

    let payload = rng.bytes(payload_len);
    bytes.extend_from_slice(&payload);
    if padding > 0 {
        bytes.extend(core::iter::repeat(0).take(padding - 1));
        bytes.push(padding as u8);
    }

    Built {
        bytes,
        header,
        csrcs,
        extension,
        payload,
        padding,
    }
}

fn assert_parses_as_built(built: &Built) {
    let packet =
        Packet::parse(&built.bytes).unwrap_or_else(|e| panic!("{e:?} for {:02x?}", built.bytes));
    assert_eq!(packet.header, built.header);
    assert_eq!(packet.csrcs().collect::<Vec<_>>(), built.csrcs);
    assert_eq!(
        packet.extension.map(|e| (e.profile, e.data.to_vec())),
        built.extension
    );
    assert_eq!(packet.payload, built.payload.as_slice());
    assert_eq!(
        HEADER_LEN
            + 4 * built.csrcs.len()
            + built.extension.as_ref().map_or(0, |(_, d)| 4 + d.len())
            + packet.payload.len()
            + built.padding,
        built.bytes.len()
    );
}

/// Every slice a parse returns must lie inside the input, and the pieces must
/// add up to exactly the input length.
fn assert_well_formed(input: &[u8], packet: &Packet<'_>) {
    let bounds = input.as_ptr_range();
    let inside = |s: &[u8]| {
        let r = s.as_ptr_range();
        bounds.start <= r.start && r.end <= bounds.end
    };
    assert!(inside(packet.payload));
    let extension = packet.extension.map_or(0, |e| {
        assert!(inside(e.data));
        assert_eq!(e.data.len() % 4, 0);
        4 + e.data.len()
    });
    let consumed = HEADER_LEN + 4 * packet.csrcs().count() + extension + packet.payload.len();
    let padding = input.len() - consumed;
    if input[0] & 0x20 != 0 {
        assert_eq!(padding, usize::from(input[input.len() - 1]));
        assert!(padding >= 1);
    } else {
        assert_eq!(padding, 0);
    }
}

#[test]
fn fixed_header_fields_sit_where_the_diagram_puts_them() {
    // V=2 P=0 X=0 CC=0 | M=1 PT=0x5a | seq 0x1234 | ts 0x89abcdef | ssrc 0x0badf00d
    let bytes = [
        0x80, 0xda, 0x12, 0x34, 0x89, 0xab, 0xcd, 0xef, 0x0b, 0xad, 0xf0, 0x0d, 0xff,
    ];
    let packet = Packet::parse(&bytes).unwrap();
    assert_eq!(
        packet.header,
        Header {
            marker: true,
            payload_type: 0x5a,
            sequence: 0x1234,
            timestamp: 0x89ab_cdef,
            ssrc: 0x0bad_f00d
        }
    );
    assert_eq!(packet.payload, &[0xff]);

    // The same header, written.
    let mut out = [0u8; HEADER_LEN];
    assert_eq!(packet.header.write(&mut out), Ok(HEADER_LEN));
    assert_eq!(out, bytes[..HEADER_LEN]);
}

#[test]
fn every_combination_of_padding_extension_and_csrc_count() {
    let mut rng = Rng::new(1);
    for csrc_count in 0..=15 {
        for extension_words in [None, Some(0), Some(1), Some(7)] {
            for padding in [0, 1, 3, 4, 255] {
                for payload_len in [0, 1, 640] {
                    let built = build(&mut rng, csrc_count, extension_words, payload_len, padding);
                    assert_parses_as_built(&built);
                }
            }
        }
    }
}

#[test]
fn random_packets_parse_as_built() {
    let mut rng = Rng::new(2);
    for _ in 0..20_000 {
        let csrc_count = rng.below(16) as usize;
        let extension = rng.chance(300).then(|| rng.below(20) as usize);
        let payload_len = rng.below(1400) as usize;
        let padding = if rng.chance(300) {
            1 + rng.below(255) as usize
        } else {
            0
        };
        assert_parses_as_built(&build(
            &mut rng,
            csrc_count,
            extension,
            payload_len,
            padding,
        ));
    }
}

#[test]
fn writer_matches_the_diagram() {
    let mut rng = Rng::new(3);
    for _ in 0..10_000 {
        let header = Header {
            marker: rng.chance(500),
            payload_type: rng.below(128) as u8,
            sequence: rng.next_u32() as u16,
            timestamp: rng.next_u32(),
            ssrc: rng.next_u32(),
        };
        let payload_len = rng.below(700) as usize;
        let payload = rng.bytes(payload_len);
        let mut out = vec![0u8; HEADER_LEN + payload.len()];
        let len = packet::write(&header, &payload, &mut out).unwrap();

        let mut expected = rtp_header(
            header.marker,
            header.payload_type,
            header.sequence,
            header.timestamp,
            header.ssrc,
        );
        expected.extend_from_slice(&payload);
        assert_eq!(&out[..len], expected.as_slice());
    }
}

#[test]
fn writer_rejects_short_buffers_without_writing_past_them() {
    let header = Header::default();
    for space in 0..HEADER_LEN + 4 {
        let mut out = vec![0xee; space];
        let result = packet::write(&header, &[1, 2, 3, 4], &mut out);
        assert_eq!(result, Err(Error::BufferTooSmall), "space {space}");
    }
}

#[test]
fn only_version_2_is_accepted() {
    let mut rng = Rng::new(4);
    let built = build(&mut rng, 2, Some(1), 20, 0);
    for version in [0u8, 1, 3] {
        let mut bytes = built.bytes.clone();
        bytes[0] = (bytes[0] & 0x3f) | (version << 6);
        assert_eq!(Packet::parse(&bytes), Err(Error::BadVersion));
    }
}

#[test]
fn shorter_than_a_fixed_header_is_truncated() {
    for len in 0..HEADER_LEN {
        let bytes = vec![0x80; len];
        assert_eq!(Packet::parse(&bytes), Err(Error::Truncated), "len {len}");
    }
}

#[test]
fn csrcs_or_extension_past_the_end_are_truncated() {
    let mut rng = Rng::new(5);
    let built = build(&mut rng, 3, Some(2), 0, 0);
    // Cutting anywhere inside the CSRC list or the extension must fail;
    // there is no payload or padding to absorb the loss.
    for len in HEADER_LEN..built.bytes.len() {
        assert_eq!(
            Packet::parse(&built.bytes[..len]),
            Err(Error::Truncated),
            "len {len}"
        );
    }
    assert!(Packet::parse(&built.bytes).is_ok());
}

#[test]
fn invalid_padding_counts_are_rejected() {
    let mut rng = Rng::new(6);
    let built = build(&mut rng, 1, Some(1), 10, 4);
    let mut bytes = built.bytes.clone();
    let last = bytes.len() - 1;

    bytes[last] = 0; // RFC 3550 §5.1: the count includes itself, so 0 is impossible.
    assert_eq!(Packet::parse(&bytes), Err(Error::BadPadding));

    // Padding may eat the payload but not the headers before it.
    let headers = HEADER_LEN + 4 + 8;
    bytes[last] = (bytes.len() - headers) as u8;
    assert!(Packet::parse(&bytes).unwrap().payload.is_empty());
    bytes[last] = (bytes.len() - headers + 1) as u8;
    assert_eq!(Packet::parse(&bytes), Err(Error::BadPadding));
}

#[test]
fn truncations_and_bit_flips_never_panic_or_escape_the_buffer() {
    let mut rng = Rng::new(7);
    for _ in 0..300 {
        let csrc_count = rng.below(4) as usize;
        let extension = rng.chance(500).then(|| rng.below(3) as usize);
        let padding = if rng.chance(500) {
            1 + rng.below(8) as usize
        } else {
            0
        };
        let payload_len = rng.below(40) as usize;
        let built = build(&mut rng, csrc_count, extension, payload_len, padding);

        for len in 0..=built.bytes.len() {
            let input = &built.bytes[..len];
            if let Ok(packet) = Packet::parse(input) {
                assert_well_formed(input, &packet);
            }
        }
        for bit in 0..built.bytes.len() * 8 {
            let mut input = built.bytes.clone();
            input[bit / 8] ^= 1 << (bit % 8);
            if let Ok(packet) = Packet::parse(&input) {
                assert_well_formed(&input, &packet);
            }
        }
    }
}

#[test]
fn random_bytes_never_panic_or_escape_the_buffer() {
    let mut rng = Rng::new(8);
    for _ in 0..100_000 {
        let len = rng.below(80) as usize;
        let mut input = rng.bytes(len);
        if let Some(first) = input.first_mut() {
            // Mostly version 2, so the deeper paths get exercised.
            *first = (*first & 0x3f) | 0x80;
        }
        if let Ok(packet) = Packet::parse(&input) {
            assert_well_formed(&input, &packet);
        }
    }
}

/// RFC 5761 §4: RTP and RTCP on one port are told apart by the second byte.
/// RTCP packet types occupy 192..=223, which RTP produces only for payload
/// types 64..=95 with the marker set - and §4 forbids those payload types.
#[test]
fn rtp_and_rtcp_demultiplex_on_one_port() {
    for payload_type in 0..128u8 {
        for marker in [false, true] {
            let bytes = rtp_header(marker, payload_type, 1, 2, 3);
            let collides = marker && (64..=95).contains(&payload_type);
            assert_eq!(
                packet::is_rtcp(&bytes),
                collides,
                "PT {payload_type} M {marker}"
            );
        }
    }
    // SR, RR, SDES, BYE, APP and the rest of the RTCP range.
    for packet_type in 192..=223u8 {
        assert!(packet::is_rtcp(&[0x80, packet_type, 0, 1, 0, 0, 0, 0]));
        // Count bits in the first byte do not matter.
        assert!(packet::is_rtcp(&[0x9f, packet_type, 0, 1, 0, 0, 0, 0]));
        // Not version 2: neither.
        assert!(!packet::is_rtcp(&[0x40, packet_type, 0, 1]));
    }
    assert!(!packet::is_rtcp(&[]));
    assert!(!packet::is_rtcp(&[0x80]));
}

/// Packets captured from ffmpeg's RTP muxer. They are an independent
/// implementation's idea of L16/16000/1. See `tests/fixtures/README.md`.
#[test]
fn ffmpeg_packets_parse_to_the_audio_ffmpeg_was_given() {
    let fixture = include_bytes!("fixtures/ffmpeg-l16-16000.bin");
    let packets = common::split_fixture(fixture);
    assert!(packets.len() >= 3);

    let first = Packet::parse(packets[0]).unwrap().header;
    let mut samples = Vec::new();
    let mut expected_seq = first.sequence;
    let mut expected_ts = first.timestamp;
    for bytes in &packets {
        let packet = Packet::parse(bytes).unwrap();
        assert_eq!(packet.header.payload_type, 96);
        assert_eq!(packet.header.ssrc, first.ssrc);
        assert_eq!(packet.header.sequence, expected_seq);
        assert_eq!(packet.header.timestamp, expected_ts);
        assert!(packet.extension.is_none());
        assert_eq!(packet.csrcs().count(), 0);
        assert!(!packet::is_rtcp(bytes));

        let chunk = common::l16_samples(packet.payload);
        expected_seq = expected_seq.wrapping_add(1);
        // L16's clock is the sample rate: one tick per sample.
        expected_ts = expected_ts.wrapping_add(chunk.len() as u32);
        samples.extend(chunk);
    }
    assert_eq!(samples, common::test_signal(samples.len(), 0));
}
