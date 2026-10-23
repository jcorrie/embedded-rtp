//! What SRTP adds to the per-frame cost, sized as the intercom uses it:
//! 640-byte L16 frames, 50 a second each way. Compare `srtp_frame/*` with
//! `frame/*` in `hot_path`.
//!
//! ```sh
//! cargo bench --features srtp --bench srtp
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use embedded_rtp::jitter::Config;
use embedded_rtp::packet::Packet;
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::srtp::{self, Inbound, MasterKey, Outbound, RTCP_OVERHEAD, RTP_OVERHEAD};
use embedded_rtp::{Receiver, Sender};

const TICKS: u32 = FRAME_SAMPLES as u32;
const SRTP_BYTES: usize = PACKET_BYTES + RTP_OVERHEAD;

fn master_key() -> MasterKey {
    MasterKey::from_bytes(&[7; 30])
}

fn packets(c: &mut Criterion) {
    let mut group = c.benchmark_group("srtp");
    group.throughput(Throughput::Bytes(PACKET_BYTES as u64));

    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut outbound = Outbound::new(&master_key());
    let mut buf = [0u8; SRTP_BYTES];
    group.bench_function("protect_rtp", |b| {
        b.iter(|| {
            let len = sender.send_in_place(&mut buf, FRAME_BYTES).unwrap();
            outbound.protect_rtp(black_box(&mut buf), len).unwrap()
        })
    });

    // Unprotecting needs a fresh packet each time (replays are rejected), so
    // this is protect plus unprotect; subtract `protect_rtp`.
    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut outbound = Outbound::new(&master_key());
    let mut inbound = Inbound::new(&master_key());
    group.bench_function("protect_unprotect_rtp", |b| {
        b.iter(|| {
            let len = sender.send_in_place(&mut buf, FRAME_BYTES).unwrap();
            let len = outbound.protect_rtp(&mut buf, len).unwrap();
            inbound.unprotect_rtp(black_box(&mut buf[..len])).unwrap()
        })
    });

    // A forged packet: the cost of rejecting one.
    let mut outbound = Outbound::new(&MasterKey::from_bytes(&[8; 30]));
    let mut forged = [0u8; SRTP_BYTES];
    let len = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0)
        .send_in_place(&mut forged, FRAME_BYTES)
        .unwrap();
    outbound.protect_rtp(&mut forged, len).unwrap();
    let mut inbound = Inbound::new(&master_key());
    group.bench_function("reject_forged_rtp", |b| {
        b.iter(|| {
            let mut copy = forged;
            inbound.unprotect_rtp(black_box(&mut copy)).unwrap_err()
        })
    });

    // A typical compound report: RR with one block plus SDES, 60 bytes.
    let mut outbound = Outbound::new(&master_key());
    let mut inbound = Inbound::new(&master_key());
    let mut report = [0u8; 60 + RTCP_OVERHEAD];
    report[0] = 0x81;
    report[1] = 201;
    group.throughput(Throughput::Bytes(60));
    group.bench_function("protect_unprotect_rtcp", |b| {
        b.iter(|| {
            let len = outbound.protect_rtcp(&mut report, 60).unwrap();
            inbound
                .unprotect_rtcp(black_box(&mut report[..len]))
                .unwrap()
        })
    });
    group.finish();

    let mut group = c.benchmark_group("srtp_setup");
    group.bench_function("new_outbound", |b| {
        b.iter(|| Outbound::new(black_box(&master_key())))
    });
    let kek = [3; 16];
    let wrapped = srtp::wrap(&kek, &master_key());
    group.bench_function("unwrap_key", |b| {
        b.iter(|| srtp::unwrap(black_box(&kek), black_box(&wrapped)).unwrap())
    });
    group.finish();
}

/// One frame through one direction with SRTP: send, protect, unprotect,
/// parse, receive, play. Compare with `frame/send_in_place_receive_play`.
fn frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("srtp_frame");
    group.throughput(Throughput::Bytes(FRAME_BYTES as u64));
    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut outbound = Outbound::new(&master_key());
    let mut inbound = Inbound::new(&master_key());
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT);
    let mut wire = [0u8; SRTP_BYTES];
    let mut i = 0u32;
    group.bench_function("send_in_place_receive_play", |b| {
        b.iter(|| {
            let len = sender.send_in_place(&mut wire, FRAME_BYTES).unwrap();
            let len = outbound.protect_rtp(&mut wire, len).unwrap();
            let len = inbound.unprotect_rtp(&mut wire[..len]).unwrap();
            let packet = Packet::parse(black_box(&wire[..len])).unwrap();
            i = i.wrapping_add(1);
            receiver.receive(&packet, i * TICKS);
            black_box(receiver.play());
        })
    });
    group.finish();
}

criterion_group!(benches, packets, frame);
criterion_main!(benches);
