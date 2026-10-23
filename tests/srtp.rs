//! SRTP and SRTCP (RFC 3711) conformance. Known answers come from outside
//! the crate: libsrtp's `srtp_validate` packets and an AES key wrap computed
//! by OpenSSL. The RFC 3711 appendix B keystream and key derivation vectors
//! are unit tests in `src/srtp.rs`, since they need private functions.
//!
//! Run with `cargo test --features srtp`.

#![cfg(feature = "srtp")]

mod common;

use common::{l16_packet, test_signal, Rng};
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::Packet;
use embedded_rtp::rtcp::{self, Compound, ReportBlock, Writer};
use embedded_rtp::srtp::{
    self, Error, Inbound, MasterKey, Outbound, REPLAY_WINDOW, RTCP_OVERHEAD, RTP_OVERHEAD, TAG_LEN,
};
use embedded_rtp::{Received, Receiver, Sender};

/// libsrtp's `test_key`, which is also RFC 3711 appendix B.3's master key
/// and salt.
fn libsrtp_key() -> MasterKey {
    MasterKey::from_bytes(&[
        0xe1, 0xf9, 0x7a, 0x0d, 0x3e, 0x01, 0x8b, 0xe0, 0xd6, 0x4f, 0xa3, 0x2c, 0x06, 0xde, 0x41,
        0x39, 0x0e, 0xc6, 0x75, 0xad, 0x49, 0x8a, 0xfe, 0xeb, 0xb6, 0x96, 0x0b, 0x3a, 0xab, 0xe6,
    ])
}

fn key(seed: u8) -> MasterKey {
    let mut bytes = [0u8; 30];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_mul(31).wrapping_add(i as u8);
    }
    MasterKey::from_bytes(&bytes)
}

/// Protect an RTP packet, returning the SRTP packet.
fn protect(outbound: &mut Outbound, packet: &[u8]) -> Vec<u8> {
    let mut buf = packet.to_vec();
    buf.resize(packet.len() + RTP_OVERHEAD, 0);
    let len = outbound.protect_rtp(&mut buf, packet.len()).unwrap();
    assert_eq!(len, buf.len());
    buf
}

/// Unprotect an SRTP packet, returning the RTP packet.
fn unprotect(inbound: &mut Inbound, packet: &[u8]) -> Result<Vec<u8>, Error> {
    let mut buf = packet.to_vec();
    let len = inbound.unprotect_rtp(&mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

fn protect_rtcp(outbound: &mut Outbound, packet: &[u8]) -> Vec<u8> {
    let mut buf = packet.to_vec();
    buf.resize(packet.len() + RTCP_OVERHEAD, 0);
    let len = outbound.protect_rtcp(&mut buf, packet.len()).unwrap();
    assert_eq!(len, buf.len());
    buf
}

fn unprotect_rtcp(inbound: &mut Inbound, packet: &[u8]) -> Result<Vec<u8>, Error> {
    let mut buf = packet.to_vec();
    let len = inbound.unprotect_rtcp(&mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

fn audio_packet(seq: u16) -> Vec<u8> {
    let samples = test_signal(320, usize::from(seq) * 320);
    l16_packet(seq, u32::from(seq) * 320, 0x5eed_cafe, &samples)
}

const LIBSRTP_RTP: [u8; 28] = [
    0x80, 0x0f, 0x12, 0x34, 0xde, 0xca, 0xfb, 0xad, 0xca, 0xfe, 0xba, 0xbe, 0xab, 0xab, 0xab, 0xab,
    0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
];
const LIBSRTP_SRTP: [u8; 38] = [
    0x80, 0x0f, 0x12, 0x34, 0xde, 0xca, 0xfb, 0xad, 0xca, 0xfe, 0xba, 0xbe, 0x4e, 0x55, 0xdc, 0x4c,
    0xe7, 0x99, 0x78, 0xd8, 0x8c, 0xa4, 0xd2, 0x15, 0x94, 0x9d, 0x24, 0x02, 0xb7, 0x8d, 0x6a, 0xcc,
    0x99, 0xea, 0x17, 0x9b, 0x8d, 0xbb,
];
const LIBSRTP_RTCP: [u8; 24] = [
    0x81, 0xc8, 0x00, 0x0b, 0xca, 0xfe, 0xba, 0xbe, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
    0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab, 0xab,
];
/// SRTCP index 1: libsrtp numbers its first SRTCP packet 1, where RFC 3711
/// §3.4 starts at 0.
const LIBSRTP_SRTCP: [u8; 38] = [
    0x81, 0xc8, 0x00, 0x0b, 0xca, 0xfe, 0xba, 0xbe, 0x71, 0x28, 0x03, 0x5b, 0xe4, 0x87, 0xb9, 0xbd,
    0xbe, 0xf8, 0x90, 0x41, 0xf9, 0x77, 0xa5, 0xa8, 0x80, 0x00, 0x00, 0x01, 0x99, 0x3e, 0x08, 0xcd,
    0x54, 0xd6, 0xc1, 0x23, 0x07, 0x98,
];

/// libsrtp's `srtp_validate`: an RTP packet protected byte for byte as
/// libsrtp does it, covering key derivation, the IV, the cipher and the tag.
#[test]
fn rtp_matches_libsrtp() {
    let mut outbound = Outbound::new(&libsrtp_key());
    assert_eq!(protect(&mut outbound, &LIBSRTP_RTP), LIBSRTP_SRTP);

    let mut inbound = Inbound::new(&libsrtp_key());
    assert_eq!(
        unprotect(&mut inbound, &LIBSRTP_SRTP),
        Ok(LIBSRTP_RTP.to_vec())
    );
}

#[test]
fn rtcp_matches_libsrtp() {
    let mut outbound = Outbound::new(&libsrtp_key());
    let first = protect_rtcp(&mut outbound, &LIBSRTP_RTCP);
    assert_eq!(&first[24..28], &[0x80, 0, 0, 0], "E flag, index 0");
    assert_eq!(protect_rtcp(&mut outbound, &LIBSRTP_RTCP), LIBSRTP_SRTCP);

    let mut inbound = Inbound::new(&libsrtp_key());
    assert_eq!(
        unprotect_rtcp(&mut inbound, &LIBSRTP_SRTCP),
        Ok(LIBSRTP_RTCP.to_vec())
    );
}

/// The whole path: Sender, protect, unprotect, parse, Receiver. Audio comes
/// out exactly as it went in, and the wire carries none of it in the clear.
#[test]
fn audio_round_trip_through_sender_and_receiver() {
    let mut sender = Sender::new(0x1234_5678, 96, 320, 40_000, 7);
    let mut outbound = Outbound::new(&key(1));
    let mut inbound = Inbound::new(&key(1));
    let mut receiver = Receiver::<16, 640>::new(96, 320, Config::DEFAULT);

    let mut played = Vec::new();
    let mut sent = Vec::new();
    for i in 0..50 {
        let audio: Vec<u8> = (0..640).map(|j| (i * 7 + j) as u8).collect();
        let mut buf = [0u8; 12 + 640 + RTP_OVERHEAD];
        let len = sender.send(&audio, &mut buf).unwrap();
        let len = outbound.protect_rtp(&mut buf, len).unwrap();
        assert_eq!(len, buf.len());
        assert!(
            !buf.windows(16).any(|w| audio.windows(16).any(|a| a == w)),
            "audio visible on the wire"
        );

        let len = inbound.unprotect_rtp(&mut buf).unwrap();
        let packet = Packet::parse(&buf[..len]).unwrap();
        assert_eq!(receiver.receive(&packet, i * 320), Received::Accepted);
        sent.push(audio);
        if let Playout::Frame { payload, .. } = receiver.play() {
            played.push(payload.to_vec());
        }
    }
    assert!(!played.is_empty());
    assert_eq!(played, sent[..played.len()]);
}

/// Every single-bit change anywhere in a packet is rejected, and a rejected
/// packet leaves the context untouched: the genuine packet is still accepted
/// afterwards.
#[test]
fn any_altered_bit_is_rejected() {
    let mut outbound = Outbound::new(&key(2));
    let first = protect(&mut outbound, &audio_packet(10));
    let packet = protect(&mut outbound, &audio_packet(11));
    let mut inbound = Inbound::new(&key(2));
    unprotect(&mut inbound, &first).unwrap();

    for bit in 0..packet.len() * 8 {
        let mut altered = packet.clone();
        altered[bit / 8] ^= 1 << (bit % 8);
        let result = unprotect(&mut inbound, &altered);
        assert!(result.is_err(), "bit {bit} flipped and accepted");
    }
    assert_eq!(unprotect(&mut inbound, &packet), Ok(audio_packet(11)));
}

#[test]
fn any_altered_rtcp_bit_is_rejected() {
    let mut writer_buf = [0u8; 128];
    let mut writer = Writer::new(&mut writer_buf);
    writer.receiver_report(0x5eed_cafe, &[]).unwrap();
    writer
        .source_description(0x5eed_cafe, &[(rtcp::sdes::CNAME, b"kitchen")])
        .unwrap();
    let report = writer.finish().to_vec();

    let mut outbound = Outbound::new(&key(3));
    let packet = protect_rtcp(&mut outbound, &report);
    let mut inbound = Inbound::new(&key(3));
    for bit in 0..packet.len() * 8 {
        let mut altered = packet.clone();
        altered[bit / 8] ^= 1 << (bit % 8);
        assert!(
            unprotect_rtcp(&mut inbound, &altered).is_err(),
            "bit {bit} flipped and accepted"
        );
    }
    let plain = unprotect_rtcp(&mut inbound, &packet).unwrap();
    assert_eq!(plain, report);
    assert!(Compound::parse(&plain).is_ok());
}

#[test]
fn wrong_key_is_rejected() {
    let mut outbound = Outbound::new(&key(4));
    let packet = protect(&mut outbound, &audio_packet(1));
    let report = protect_rtcp(&mut outbound, &LIBSRTP_RTCP);

    let mut inbound = Inbound::new(&key(5));
    assert_eq!(unprotect(&mut inbound, &packet), Err(Error::Authentication));
    assert_eq!(
        unprotect_rtcp(&mut inbound, &report),
        Err(Error::Authentication)
    );
}

/// RFC 3711 §3.3.2: each packet is accepted once; anything older than the
/// replay window is rejected; anything newer, or inside the window and
/// unseen, is accepted.
#[test]
fn replays_are_rejected() {
    let mut outbound = Outbound::new(&key(6));
    let packets: Vec<Vec<u8>> = (0..200)
        .map(|seq| protect(&mut outbound, &audio_packet(seq)))
        .collect();
    let mut inbound = Inbound::new(&key(6));

    // Everything but packet 100 and the last one, then a replay of each.
    for (seq, packet) in packets.iter().enumerate().take(199) {
        if seq != 100 {
            unprotect(&mut inbound, packet).unwrap();
        }
    }
    for packet in &packets[..199] {
        if packet != &packets[100] {
            assert_eq!(unprotect(&mut inbound, packet), Err(Error::Replayed));
        }
    }
    // 98 behind the newest (198): outside the window though never seen.
    const { assert!(198 - 100 >= REPLAY_WINDOW) };
    assert_eq!(unprotect(&mut inbound, &packets[100]), Err(Error::Replayed));

    // Inside the window and unseen: accepted, once.
    let mut inbound = Inbound::new(&key(6));
    unprotect(&mut inbound, &packets[0]).unwrap();
    let edge = REPLAY_WINDOW as usize - 1;
    unprotect(&mut inbound, &packets[edge]).unwrap();
    assert_eq!(unprotect(&mut inbound, &packets[0]), Err(Error::Replayed));
    unprotect(&mut inbound, &packets[1]).unwrap();
    assert_eq!(unprotect(&mut inbound, &packets[1]), Err(Error::Replayed));
    unprotect(&mut inbound, &packets[edge + 1]).unwrap();
    // The window has slid by one: packet 2 is 62 behind and unseen.
    unprotect(&mut inbound, &packets[2]).unwrap();
    assert_eq!(unprotect(&mut inbound, &packets[2]), Err(Error::Replayed));

    // The boundary, unseen on both sides of it.
    let mut inbound = Inbound::new(&key(6));
    unprotect(&mut inbound, &packets[REPLAY_WINDOW as usize]).unwrap();
    assert_eq!(unprotect(&mut inbound, &packets[0]), Err(Error::Replayed));
    unprotect(&mut inbound, &packets[1]).unwrap();
}

#[test]
fn rtcp_replays_are_rejected() {
    let mut outbound = Outbound::new(&key(7));
    let reports: Vec<Vec<u8>> = (0..100)
        .map(|_| protect_rtcp(&mut outbound, &LIBSRTP_RTCP))
        .collect();
    let mut inbound = Inbound::new(&key(7));
    unprotect_rtcp(&mut inbound, &reports[0]).unwrap();
    unprotect_rtcp(&mut inbound, &reports[5]).unwrap();
    assert_eq!(
        unprotect_rtcp(&mut inbound, &reports[5]),
        Err(Error::Replayed)
    );
    unprotect_rtcp(&mut inbound, &reports[3]).unwrap();
    unprotect_rtcp(&mut inbound, &reports[99]).unwrap();
    assert_eq!(
        unprotect_rtcp(&mut inbound, &reports[4]),
        Err(Error::Replayed)
    );
}

/// RFC 3711 §3.3.1: the rollover counter, which the tag covers, follows the
/// sequence number across its wrap, including packets reordered over it.
/// A wrong rollover counter would fail authentication.
#[test]
fn rollover_across_the_sequence_wrap() {
    let mut outbound = Outbound::new(&key(8));
    let mut inbound = Inbound::new(&key(8));
    let first: u16 = 65_000;
    let count = 70_000 + 1_000; // wraps twice
    let mut rng = Rng::new(8);
    let mut pending: Option<(u16, Vec<u8>)> = None;
    for i in 0..count {
        let seq = first.wrapping_add(i as u16);
        let packet = protect(&mut outbound, &audio_packet(seq));
        // Swap neighbours now and then, including right at each wrap.
        if pending.is_none() && (seq == 0xffff || rng.below(10) == 0) {
            pending = Some((seq, packet));
            continue;
        }
        assert_eq!(unprotect(&mut inbound, &packet), Ok(audio_packet(seq)));
        if let Some((seq, packet)) = pending.take() {
            assert_eq!(unprotect(&mut inbound, &packet), Ok(audio_packet(seq)));
        }
    }
}

/// The sender never uses an index twice: repeating a sequence number moves
/// to the next rollover, so the keystream differs, and the receiver sees a
/// replay rather than decrypting reused keystream.
#[test]
fn sender_never_reuses_an_index() {
    let mut outbound = Outbound::new(&key(9));
    let once = protect(&mut outbound, &audio_packet(5));
    let again = protect(&mut outbound, &audio_packet(5));
    assert_ne!(once[12..], again[12..]);

    let mut inbound = Inbound::new(&key(9));
    unprotect(&mut inbound, &once).unwrap();
    assert_eq!(unprotect(&mut inbound, &again), Err(Error::Replayed));
}

#[test]
fn a_context_belongs_to_one_ssrc() {
    let mut outbound = Outbound::new(&key(10));
    let packet = protect(&mut outbound, &audio_packet(1));
    let mut other = l16_packet(2, 640, 0x0bad_f00d, &[0; 4]);
    other.resize(other.len() + TAG_LEN, 0);
    let len = other.len() - TAG_LEN;
    assert_eq!(outbound.protect_rtp(&mut other, len), Err(Error::WrongSsrc));

    let mut stranger = Outbound::new(&key(10));
    let foreign = protect(&mut stranger, &l16_packet(2, 640, 0x0bad_f00d, &[0; 4]));
    let mut inbound = Inbound::new(&key(10));
    unprotect(&mut inbound, &packet).unwrap();
    assert_eq!(unprotect(&mut inbound, &foreign), Err(Error::WrongSsrc));
}

/// Rekeying mid-stream: packets already in flight under the old key are
/// still accepted until the first one under the new key arrives; after that
/// the old key is gone.
#[test]
fn rekey_without_dropping_audio() {
    let mut outbound = Outbound::new(&key(11));
    let mut inbound = Inbound::new(&key(11));
    for seq in 0..10 {
        unprotect(&mut inbound, &protect(&mut outbound, &audio_packet(seq))).unwrap();
    }
    let in_flight = protect(&mut outbound, &audio_packet(10));
    let late = protect(&mut outbound, &audio_packet(11));
    let mut report = LIBSRTP_RTCP;
    report[4..8].copy_from_slice(&0x5eed_cafe_u32.to_be_bytes());
    let late_report = protect_rtcp(&mut outbound, &report);

    inbound.rekey(&key(12));
    outbound.rekey(&key(12));
    assert_eq!(unprotect(&mut inbound, &in_flight), Ok(audio_packet(10)));
    let fresh = protect(&mut outbound, &audio_packet(12));
    assert_eq!(unprotect(&mut inbound, &fresh), Ok(audio_packet(12)));
    assert_eq!(unprotect(&mut inbound, &late), Err(Error::Authentication));
    assert_eq!(
        unprotect_rtcp(&mut inbound, &late_report),
        Err(Error::Authentication)
    );
    for seq in 13..20 {
        unprotect(&mut inbound, &protect(&mut outbound, &audio_packet(seq))).unwrap();
    }
}

/// Header extensions and CSRCs stay in the clear but are authenticated.
#[test]
fn extension_and_csrcs_are_authenticated_not_encrypted() {
    let mut packet = vec![0x90 | 2, 96, 0, 1, 0, 0, 0, 0, 0x5e, 0xed, 0xca, 0xfe];
    packet.extend_from_slice(&[1, 1, 1, 1, 2, 2, 2, 2]); // two CSRCs
    packet.extend_from_slice(&[0xbe, 0xde, 0, 1, 0x10, 0xaa, 0, 0]); // extension
    let header_len = packet.len();
    packet.extend_from_slice(&[9; 32]);

    let mut outbound = Outbound::new(&key(13));
    let protected = protect(&mut outbound, &packet);
    assert_eq!(protected[..header_len], packet[..header_len]);
    assert_ne!(protected[header_len..header_len + 32], packet[header_len..]);

    let mut inbound = Inbound::new(&key(13));
    let mut altered = protected.clone();
    altered[header_len - 3] ^= 1; // inside the extension
    assert_eq!(
        unprotect(&mut inbound, &altered),
        Err(Error::Authentication)
    );
    assert_eq!(unprotect(&mut inbound, &protected), Ok(packet));
}

#[test]
fn malformed_input() {
    let mut outbound = Outbound::new(&key(14));
    let mut inbound = Inbound::new(&key(14));
    let packet = audio_packet(1);

    let mut small = packet.clone();
    assert_eq!(
        outbound.protect_rtp(&mut small, packet.len()),
        Err(Error::BufferTooSmall)
    );
    let mut short = [0x80u8; 11 + TAG_LEN];
    assert_eq!(outbound.protect_rtp(&mut short, 11), Err(Error::Truncated));
    assert_eq!(inbound.unprotect_rtp(&mut [0x80; 9]), Err(Error::Truncated));
    assert_eq!(
        inbound.unprotect_rtp(&mut [0x80; 11 + TAG_LEN]),
        Err(Error::Truncated)
    );
    let mut version1 = protect(&mut outbound, &packet);
    version1[0] = 0x40;
    assert_eq!(inbound.unprotect_rtp(&mut version1), Err(Error::BadVersion));
    // Claims 15 CSRCs it doesn't have.
    let mut csrcs = protect(&mut outbound, &audio_packet(3))[..30].to_vec();
    csrcs[0] |= 0x0f;
    assert_eq!(inbound.unprotect_rtp(&mut csrcs), Err(Error::Truncated));

    let mut report = LIBSRTP_RTCP.to_vec();
    assert_eq!(
        outbound.protect_rtcp(&mut report, 24),
        Err(Error::BufferTooSmall)
    );
    assert_eq!(
        inbound.unprotect_rtcp(&mut [0x81; 7 + RTCP_OVERHEAD]),
        Err(Error::Truncated)
    );

    // Authentication-only SRTCP (E flag clear) is not accepted.
    let mut cleared = protect_rtcp(&mut outbound, &LIBSRTP_RTCP);
    cleared[24] &= 0x7f;
    assert_eq!(
        inbound.unprotect_rtcp(&mut cleared),
        Err(Error::Authentication)
    );
}

/// Random bytes never panic and never pass.
#[test]
fn random_input_is_rejected() {
    let mut inbound = Inbound::new(&key(15));
    let mut rng = Rng::new(15);
    for _ in 0..20_000 {
        let len = rng.below(120) as usize;
        let mut buf: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        if let Some(first) = buf.first_mut() {
            *first = 0x80 | (*first & 0x3f);
        }
        assert!(inbound.unprotect_rtp(&mut buf.clone()).is_err());
        assert!(inbound.unprotect_rtcp(&mut buf).is_err());
    }
}

/// RFC 5649 key wrap of a session key under a device's key encryption key,
/// against a value computed by OpenSSL
/// (`openssl enc -id-aes128-wrap-pad -iv a65959a6`).
#[test]
fn key_wrap_matches_openssl() {
    let kek = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let mut bytes = [0u8; 30];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = i as u8;
    }
    let session = MasterKey::from_bytes(&bytes);
    let wrapped = srtp::wrap(&kek, &session);
    assert_eq!(
        wrapped,
        [
            0xc6, 0x8a, 0x2b, 0x34, 0x21, 0x7a, 0x42, 0xc6, 0x90, 0xd4, 0x61, 0xc9, 0x9d, 0xdf,
            0x04, 0xdf, 0x42, 0x9d, 0xe1, 0x42, 0x94, 0x1b, 0x88, 0x68, 0x9c, 0xad, 0x65, 0x8f,
            0x74, 0xab, 0xb1, 0x7e, 0x76, 0x27, 0x90, 0xf9, 0xf3, 0x76, 0x6a, 0x2a,
        ]
    );
    assert_eq!(srtp::unwrap(&kek, &wrapped).unwrap().to_bytes(), bytes);

    for bit in 0..wrapped.len() * 8 {
        let mut altered = wrapped;
        altered[bit / 8] ^= 1 << (bit % 8);
        assert!(srtp::unwrap(&kek, &altered).is_err(), "bit {bit}");
    }
    let mut other = kek;
    other[0] ^= 1;
    assert_eq!(
        srtp::unwrap(&other, &wrapped).err(),
        Some(Error::Authentication)
    );
}

/// Keys never reach a log.
#[test]
fn master_keys_are_not_printed() {
    assert_eq!(format!("{:?}", key(16)), "MasterKey(..)");
}

/// An SR's report block survives protection, so receivers can still compute
/// round-trip times.
#[test]
fn sender_reports_round_trip() {
    let mut buf = [0u8; 128];
    let mut writer = Writer::new(&mut buf);
    let block = ReportBlock {
        ssrc: 9,
        jitter: 40,
        ..ReportBlock::default()
    };
    writer.receiver_report(0x5eed_cafe, &[block]).unwrap();
    let report = writer.finish().to_vec();

    let mut outbound = Outbound::new(&key(17));
    let mut inbound = Inbound::new(&key(17));
    let plain = unprotect_rtcp(&mut inbound, &protect_rtcp(&mut outbound, &report)).unwrap();
    let compound = Compound::parse(&plain).unwrap();
    match compound.packets().next() {
        Some(rtcp::Packet::ReceiverReport { mut reports, .. }) => {
            assert_eq!(reports.next(), Some(block));
        }
        other => panic!("{other:?}"),
    }
}
