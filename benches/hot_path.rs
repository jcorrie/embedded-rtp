//! Per-operation cost of everything on the audio hot path.
//!
//! Each benchmark is one operation in steady state, sized as the intercom
//! uses it: 640-byte frames, 20 ms apart. `frame/*` is the whole per-frame
//! cost of one direction. Everything else breaks that down.
//!
//! These are host (x86-64) timings. On a Cortex-M0+ the ranking holds but the
//! constants do not: memory copies and integer division cost far more there.
//!
//! ```sh
//! cargo bench --bench hot_path                       # run
//! cargo bench --bench hot_path -- --save-baseline a  # record
//! cargo bench --bench hot_path -- --baseline a       # compare
//! ```

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use embedded_rtp::duck::{self, Ducker};
use embedded_rtp::g722;
use embedded_rtp::jitter::{Config, JitterBuffer, Playout};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::{self, Header, Packet, HEADER_LEN};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::red::{Encoder, Payload};
use embedded_rtp::rtcp::{self, Compound, ReportBlock, SenderInfo, Writer};
use embedded_rtp::stats::SourceStats;
use embedded_rtp::{Receiver, Sender};

const TICKS: u32 = FRAME_SAMPLES as u32;

fn intercom_packet(sequence: u16) -> [u8; PACKET_BYTES] {
    let header = Header {
        marker: false,
        payload_type: PAYLOAD_TYPE,
        sequence,
        timestamp: u32::from(sequence) * TICKS,
        ssrc: 0x1234_5678,
    };
    let mut buf = [0u8; PACKET_BYTES];
    packet::write(&header, &[0x5a; FRAME_BYTES], &mut buf).unwrap();
    buf
}

fn packets(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet");
    group.throughput(Throughput::Bytes(PACKET_BYTES as u64));
    let bytes = intercom_packet(1);
    group.bench_function("parse", |b| {
        b.iter(|| Packet::parse(black_box(&bytes)).unwrap().payload.len())
    });
    let header = Packet::parse(&bytes).unwrap().header;
    let payload = [0x5a; FRAME_BYTES];
    let mut out = [0u8; PACKET_BYTES];
    group.bench_function("write", |b| {
        b.iter(|| packet::write(black_box(&header), black_box(&payload), &mut out).unwrap())
    });
    group.bench_function("is_rtcp", |b| b.iter(|| packet::is_rtcp(black_box(&bytes))));
    group.finish();
}

fn stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("stats");
    let mut source = SourceStats::new();
    let mut sequence = 0u16;
    group.bench_function("update", |b| {
        b.iter(|| {
            sequence = sequence.wrapping_add(1);
            source.update(black_box(sequence))
        })
    });
    let mut source = SourceStats::new();
    let mut i = 0u32;
    group.bench_function("record_arrival", |b| {
        b.iter(|| {
            i = i.wrapping_add(1);
            source.record_arrival(black_box(i * TICKS), black_box(i * TICKS + (i & 31)));
        })
    });
    let mut source = SourceStats::new();
    for s in 0..1000 {
        source.update(s);
    }
    group.bench_function("report", |b| b.iter(|| source.report()));
    group.finish();
}

fn sender(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender");
    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let payload = [0x5a; FRAME_BYTES];
    let mut out = [0u8; PACKET_BYTES];
    group.bench_function("send", |b| {
        b.iter(|| sender.send(black_box(&payload), &mut out).unwrap())
    });
    // Audio already captured into the packet buffer: header only.
    group.bench_function("send_in_place", |b| {
        b.iter(|| {
            sender
                .send_in_place(black_box(&mut out), FRAME_BYTES)
                .unwrap()
        })
    });
    group.finish();
}

/// A buffer in steady state: `depth` frames banked, one in and one out per
/// iteration, as at 50 frames a second.
fn jitter<const SLOTS: usize>(c: &mut Criterion, depth: u8) {
    let config = Config {
        initial_depth: depth,
        min_depth: 1,
        max_depth: SLOTS as u8,
    };
    let mut group = c.benchmark_group("jitter");
    let payload = [0x5a; FRAME_BYTES];

    let mut buffer = JitterBuffer::<SLOTS, FRAME_BYTES>::new(config);
    let mut next = 0u16;
    for _ in 0..depth {
        buffer.push(next, &payload);
        next += 1;
    }
    group.bench_function(BenchmarkId::new("push_pop", format!("{SLOTS}slots")), |b| {
        b.iter(|| {
            buffer.push(black_box(next), &payload);
            next = next.wrapping_add(1);
            black_box(buffer.pop());
        })
    });
    group.bench_function(BenchmarkId::new("depth", format!("{SLOTS}slots")), |b| {
        b.iter(|| black_box(&buffer).depth())
    });

    // One frame in ten lost: exercises the concealment path.
    let mut buffer = JitterBuffer::<SLOTS, FRAME_BYTES>::new(config);
    let mut next = 0u16;
    for _ in 0..depth {
        buffer.push(next, &payload);
        next += 1;
    }
    group.bench_function(
        BenchmarkId::new("push_pop_10pc_loss", format!("{SLOTS}slots")),
        |b| {
            b.iter(|| {
                if next % 10 != 0 {
                    buffer.push(black_box(next), &payload);
                }
                next = next.wrapping_add(1);
                black_box(buffer.pop());
            })
        },
    );
    group.finish();
}

/// G.722, one 20 ms frame: 320 samples to 160 bytes and back.
fn g722_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("g722");
    group.throughput(Throughput::Bytes(FRAME_BYTES as u64));
    let samples: [i16; FRAME_SAMPLES] =
        core::array::from_fn(|i| ((i as i32 * 97) % 8_000 - 4_000) as i16);
    let mut encoder = g722::Encoder::new();
    let mut codes = [0u8; FRAME_SAMPLES / 2];
    group.bench_function("encode", |b| {
        b.iter(|| encoder.encode(black_box(&samples), &mut codes))
    });
    let mut decoder = g722::Decoder::new();
    let mut out = [0i16; FRAME_SAMPLES];
    group.bench_function("decode", |b| {
        b.iter(|| decoder.decode(black_box(&codes), &mut out))
    });
    group.finish();
}

fn concealment(c: &mut Criterion) {
    let mut group = c.benchmark_group("l16");
    let bytes = [0x5a; FRAME_BYTES];
    let mut samples = [0i16; FRAME_SAMPLES];
    group.bench_function("decode", |b| {
        b.iter(|| l16::decode(black_box(&bytes), &mut samples))
    });
    let mut out = [0u8; FRAME_BYTES];
    group.bench_function("encode", |b| {
        b.iter(|| l16::encode(black_box(&samples), &mut out))
    });

    // The echo suppressor on one frame, ducking: the multiply always runs,
    // so this is its cost whatever the signals.
    let loud = [12_000i16; FRAME_SAMPLES];
    let mut ducker = Ducker::new(duck::Config::DEFAULT);
    let mut mic = [3_000i16; FRAME_SAMPLES];
    group.bench_function("duck", |b| {
        b.iter(|| ducker.process(black_box(&loud), black_box(&mut mic)))
    });

    // The no-loss path: decode, nothing else.
    let mut concealer = Concealer::<FRAME_SAMPLES>::new();
    group.bench_function("conceal_good", |b| {
        b.iter(|| {
            let playout = Playout::Frame {
                sequence: 0,
                payload: black_box(&bytes),
            };
            concealer.play(playout).map(|s| s[0])
        })
    });
    // A loss: one frame of repeated, faded audio, then audio again.
    let mut concealer = Concealer::<FRAME_SAMPLES>::new();
    group.bench_function("conceal_loss_then_good", |b| {
        b.iter(|| {
            concealer.play(Playout::Lost { sequence: 0 });
            let playout = Playout::Frame {
                sequence: 0,
                payload: black_box(&bytes),
            };
            concealer.play(playout).map(|s| s[0])
        })
    });
    group.finish();
}

/// The compound packet a participant sends every few seconds: SR with one
/// report block, plus SDES with a CNAME and a NAME.
fn rtcp_reports(c: &mut Criterion) {
    let mut group = c.benchmark_group("rtcp");
    let info = SenderInfo {
        ntp_timestamp: 0x1234_5678_9abc_def0,
        rtp_timestamp: 5,
        packet_count: 6,
        octet_count: 7,
    };
    let block = ReportBlock {
        ssrc: 9,
        jitter: 40,
        extended_highest: 70_000,
        ..ReportBlock::default()
    };
    let items: [(u8, &[u8]); 2] = [
        (rtcp::sdes::CNAME, b"kitchen@example"),
        (rtcp::sdes::NAME, b"Kitchen"),
    ];
    let mut buf = [0u8; 256];
    group.bench_function("write_sr_sdes", |b| {
        b.iter(|| {
            let mut writer = Writer::new(&mut buf);
            writer.sender_report(1, black_box(&info), &[block]).unwrap();
            writer.source_description(1, &items).unwrap();
            writer.len()
        })
    });
    let mut writer = Writer::new(&mut buf);
    writer.sender_report(1, &info, &[block]).unwrap();
    writer.source_description(1, &items).unwrap();
    let bytes = writer.finish().to_vec();
    group.bench_function("parse_and_read", |b| {
        b.iter(|| {
            let compound = Compound::parse(black_box(&bytes)).unwrap();
            compound.packets().count()
        })
    });
    group.finish();
}

const RED_PAYLOAD_TYPE: u8 = 97;
type RedEncoder = Encoder<1, FRAME_BYTES>;
const RED_PACKET_BYTES: usize = HEADER_LEN + RedEncoder::MAX_PAYLOAD;

/// Redundancy (RFC 2198) at depth 1: what it adds to each frame, both ends.
fn redundancy(c: &mut Criterion) {
    let mut group = c.benchmark_group("red");
    let payload = [0x5a; FRAME_BYTES];
    let mut encoder = RedEncoder::new(PAYLOAD_TYPE, TICKS);
    let mut out = [0u8; RedEncoder::MAX_PAYLOAD];
    let mut timestamp = 0u32;
    group.bench_function("encode", |b| {
        b.iter(|| {
            timestamp = timestamp.wrapping_add(TICKS);
            encoder
                .encode(timestamp, black_box(&payload), &mut out)
                .unwrap()
        })
    });
    group.bench_function("parse", |b| {
        b.iter(|| {
            let payload = Payload::parse(black_box(&out)).unwrap();
            payload.blocks().count() + payload.primary.len()
        })
    });
    group.finish();

    let mut group = c.benchmark_group("frame");
    group.throughput(Throughput::Bytes(FRAME_BYTES as u64));
    // As frame/send_receive_play, every packet carrying the frame before.
    let mut sender = Sender::new(1, RED_PAYLOAD_TYPE, TICKS, 0, 0);
    let mut encoder = RedEncoder::new(PAYLOAD_TYPE, TICKS);
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT)
        .with_redundancy(RED_PAYLOAD_TYPE, 1);
    let mut wire = [0u8; RED_PACKET_BYTES];
    let mut i = 0u32;
    group.bench_function("red_send_receive_play", |b| {
        b.iter(|| {
            let len = encoder
                .encode(
                    sender.timestamp(),
                    black_box(&payload),
                    &mut wire[HEADER_LEN..],
                )
                .unwrap();
            let len = sender.send_in_place(&mut wire, len).unwrap();
            let packet = Packet::parse(&wire[..len]).unwrap();
            i = i.wrapping_add(1);
            receiver.receive(&packet, i * TICKS);
            black_box(receiver.play());
        })
    });
    group.finish();
}

fn jitter_buffers(c: &mut Criterion) {
    jitter::<8>(c, 2);
    jitter::<16>(c, 2);
    jitter::<64>(c, 2);
}

/// One frame through one direction, as the server or a device runs it every
/// 20 ms: send, parse, receive, play.
fn frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame");
    group.throughput(Throughput::Bytes(FRAME_BYTES as u64));
    let payload = [0x5a; FRAME_BYTES];
    let mut wire = [0u8; PACKET_BYTES];

    // Receive side alone: only the sequence number and timestamp of a
    // prebuilt packet change between iterations.
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT);
    let mut bytes = intercom_packet(0);
    let mut i = 0u32;
    group.bench_function("receive_play", |b| {
        b.iter(|| {
            i = i.wrapping_add(1);
            bytes[2..4].copy_from_slice(&(i as u16).to_be_bytes());
            bytes[4..8].copy_from_slice(&(i * TICKS).to_be_bytes());
            let packet = Packet::parse(black_box(&bytes)).unwrap();
            receiver.receive(&packet, i * TICKS);
            black_box(receiver.play());
        })
    });

    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT);
    let mut i = 0u32;
    group.bench_function("send_receive_play", |b| {
        b.iter(|| {
            let len = sender.send(black_box(&payload), &mut wire).unwrap();
            let packet = Packet::parse(&wire[..len]).unwrap();
            i = i.wrapping_add(1);
            receiver.receive(&packet, i * TICKS);
            black_box(receiver.play());
        })
    });

    // The full receive path to playable samples: parse, receive, play,
    // decode (with concealment standing by).
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT);
    let mut concealer = Concealer::<FRAME_SAMPLES>::new();
    let mut bytes = intercom_packet(0);
    let mut i = 0u32;
    group.bench_function("receive_play_samples", |b| {
        b.iter(|| {
            i = i.wrapping_add(1);
            bytes[2..4].copy_from_slice(&(i as u16).to_be_bytes());
            bytes[4..8].copy_from_slice(&(i * TICKS).to_be_bytes());
            let packet = Packet::parse(black_box(&bytes)).unwrap();
            receiver.receive(&packet, i * TICKS);
            concealer.play(receiver.play()).map(|s| s[0])
        })
    });

    // As above, with the audio captured straight into the packet buffer.
    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT);
    let mut i = 0u32;
    group.bench_function("send_in_place_receive_play", |b| {
        b.iter(|| {
            let len = sender
                .send_in_place(black_box(&mut wire), FRAME_BYTES)
                .unwrap();
            let packet = Packet::parse(&wire[..len]).unwrap();
            i = i.wrapping_add(1);
            receiver.receive(&packet, i * TICKS);
            black_box(receiver.play());
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    packets,
    stats,
    sender,
    concealment,
    g722_codec,
    rtcp_reports,
    jitter_buffers,
    frame,
    redundancy
);
criterion_main!(benches);
