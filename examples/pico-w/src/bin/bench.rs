//! On-device cost of each per-frame operation, in CPU cycles, for
//! embedded-rtp #5. No Wi-Fi or audio hardware needed: flash it and read
//! the defmt log.
//!
//! Each operation runs [`RUNS`] times on an intercom-sized frame (320
//! samples, 640 bytes) and reports the minimum, mean and maximum. The first
//! run warms the flash cache (XIP) and is not counted. Cycle counts include
//! about a dozen cycles of SysTick reads.
//!
//! Compare a build with `--features rom-intrinsics` (memcpy and division
//! from the RP2040 ROM) against one without.

#![no_std]
#![no_main]

use core::hint::black_box;

use defmt::{info, unwrap};
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::{Packet, HEADER_LEN};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::rtcp::{self, sdes, Compound, Writer};
use embedded_rtp::{Receiver, Sender};
use pico_w_intercom::{cycles, mic_samples, speaker_words, Frame};
use {defmt_rtt as _, panic_probe as _};

const RUNS: u32 = 200;
const TICKS: u32 = FRAME_SAMPLES as u32;

/// Minimum, mean and maximum cycles.
struct Stat {
    min: u32,
    max: u32,
    total: u64,
    runs: u32,
}

impl Stat {
    fn new() -> Self {
        Self {
            min: u32::MAX,
            max: 0,
            total: 0,
            runs: 0,
        }
    }

    fn add(&mut self, cycles: u32) {
        self.min = self.min.min(cycles);
        self.max = self.max.max(cycles);
        self.total += u64::from(cycles);
        self.runs += 1;
    }

    fn mean(&self) -> u32 {
        (self.total / u64::from(self.runs.max(1))) as u32
    }

    fn report(&self, name: &str, mhz: u32) {
        info!(
            "{=str}: {} cycles mean ({} µs); min {}, max {}",
            name,
            self.mean(),
            self.mean() / mhz,
            self.min,
            self.max
        );
    }
}

/// Time `f` [`RUNS`] times after one warm-up run.
fn bench(name: &str, mhz: u32, mut f: impl FnMut()) -> u32 {
    f();
    let mut stat = Stat::new();
    for _ in 0..RUNS {
        stat.add(cycles::measure(&mut f).0);
    }
    stat.report(name, mhz);
    stat.mean()
}

/// A test tone, so concealment and conversion work on real-looking audio.
fn tone() -> Frame {
    let mut frame = [0i16; FRAME_SAMPLES];
    for (i, sample) in frame.iter_mut().enumerate() {
        // A triangle wave: no floating point needed.
        let phase = (i % 64) as i16;
        *sample = (if phase < 32 { phase } else { 64 - phase } - 16) * 1000;
    }
    frame
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let _p = embassy_rp::init(Default::default());
    let core = unwrap!(cortex_m::Peripherals::take());
    cycles::start(core.SYST);
    let mhz = embassy_rp::clocks::clk_sys_freq() / 1_000_000;
    info!(
        "bench: clk_sys {} MHz, {} runs each, rom-intrinsics {}",
        mhz,
        RUNS,
        cfg!(feature = "rom-intrinsics")
    );

    let samples = tone();
    let mut payload = [0u8; FRAME_BYTES];
    l16::encode(&samples, &mut payload);

    // Primitives: what the hot path is made of.
    let mut copy = [0u8; FRAME_BYTES];
    bench("memcpy 640 bytes", mhz, || {
        copy.copy_from_slice(black_box(&payload));
    });
    let (mut a, b) = (black_box(1_000_000_007u32), black_box(97u32));
    bench("u32 division", mhz, || {
        a = black_box(a / b + 1_000_000_000);
    });
    let (mut a, b) = (black_box(1_000_000_000_007u64), black_box(97u64));
    bench("u64 division", mhz, || {
        a = black_box(a / b + 1_000_000_000_000);
    });

    // Send path, per frame.
    let mut words = [0u32; FRAME_SAMPLES];
    let mut converted = [0i16; FRAME_SAMPLES];
    for (word, sample) in words.iter_mut().zip(&samples) {
        *word = (*sample as u32) << 16;
    }
    let mic = bench("send: mic words to samples", mhz, || {
        mic_samples(black_box(&words), &mut converted);
    });
    let mut encoded = [0u8; FRAME_BYTES];
    let encode = bench("send: l16::encode", mhz, || {
        l16::encode(black_box(&converted), &mut encoded);
    });
    let mut sender = Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut packet = [0u8; PACKET_BYTES];
    packet[HEADER_LEN..].copy_from_slice(&payload);
    let header = bench("send: Sender::send_in_place", mhz, || {
        black_box(unwrap!(sender.send_in_place(&mut packet, FRAME_BYTES).ok()));
    });

    // Receive path, per frame: parse and buffer a packet, then play a frame.
    let mut wire = Sender::new(2, PAYLOAD_TYPE, TICKS, 0, 0);
    let mut receiver = Receiver::<8, FRAME_BYTES>::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT);
    let mut concealer = Concealer::<FRAME_SAMPLES>::new();
    let mut out = [0u32; FRAME_SAMPLES];
    let (mut receive, mut play) = (Stat::new(), Stat::new());
    for i in 0..=RUNS {
        let len = unwrap!(wire.send(&payload, &mut packet).ok());
        let (c, ()) = cycles::measure(|| {
            let parsed = unwrap!(Packet::parse(&packet[..len]).ok());
            black_box(receiver.receive(&parsed, i * TICKS));
        });
        let (p, ()) = cycles::measure(|| {
            if let Some(samples) = concealer.play(receiver.play()) {
                speaker_words(samples, &mut out);
            }
        });
        // The first frames prefill the jitter buffer; count steady state.
        if i > 4 {
            receive.add(c);
            play.add(p);
        }
    }
    receive.report("receive: parse + Receiver::receive", mhz);
    play.report("receive: play + conceal (good frame) + speaker words", mhz);

    let mut lost = Stat::new();
    for _ in 0..RUNS {
        let (c, _) = cycles::measure(|| {
            black_box(concealer.play(Playout::Lost { sequence: 0 }));
        });
        lost.add(c);
        // A good frame between losses, so each loss starts from audio.
        concealer.play(Playout::Frame {
            sequence: 0,
            payload: &payload,
        });
    }
    lost.report("receive: conceal a lost frame", mhz);

    // RTCP, every few seconds.
    let mut report = [0u8; 128];
    let mut report_len = 0;
    let ntp = rtcp::ntp_from_micros(10_000_000);
    bench("rtcp: write SR + block + SDES", mhz, || {
        let block = receiver.report_block(rtcp::ntp_short(ntp));
        let mut writer = Writer::new(&mut report);
        unwrap!(writer
            .sender_report(1, &sender.sender_info(ntp), block.as_slice())
            .ok());
        unwrap!(writer
            .source_description(1, &[(sdes::CNAME, b"kitchen"), (sdes::NAME, b"kitchen")])
            .ok());
        report_len = writer.len();
    });
    bench("rtcp: parse and read", mhz, || {
        let compound = unwrap!(Compound::parse(black_box(&report[..report_len])).ok());
        black_box(compound.packets().count());
    });

    let send = mic + encode + header;
    let receive_play = receive.mean() + play.mean();
    info!(
        "per frame: send {} cycles ({} µs), receive + play {} cycles ({} µs); \
         {} µs of every 20 ms",
        send,
        send / mhz,
        receive_play,
        receive_play / mhz,
        (send + receive_play) / mhz
    );

    #[cfg(feature = "srtp")]
    srtp(mhz, &payload);

    info!("bench: done");
    loop {
        cortex_m::asm::wfi();
    }
}

/// SRTP on each frame, both ways, and SRTCP on a report.
#[cfg(feature = "srtp")]
fn srtp(mhz: u32, payload: &[u8; FRAME_BYTES]) {
    use embedded_rtp::srtp::{Inbound, MasterKey, Outbound, RTCP_OVERHEAD, RTP_OVERHEAD};

    let master = MasterKey::from_bytes(&[7; 30]);
    let mut outbound = Outbound::new(&master);
    let mut inbound = Inbound::new(&master);
    let mut sender = Sender::new(3, PAYLOAD_TYPE, TICKS, 0, 0);
    let (mut protect, mut unprotect) = (Stat::new(), Stat::new());
    for _ in 0..=RUNS {
        let mut packet = [0u8; PACKET_BYTES + RTP_OVERHEAD];
        let len = unwrap!(sender.send(payload, &mut packet).ok());
        let (c, len) = cycles::measure(|| unwrap!(outbound.protect_rtp(&mut packet, len).ok()));
        protect.add(c);
        let (u, _) = cycles::measure(|| unwrap!(inbound.unprotect_rtp(&mut packet[..len]).ok()));
        unprotect.add(u);
    }
    protect.report("srtp: protect one frame", mhz);
    unprotect.report("srtp: unprotect one frame", mhz);

    let mut report = [0u8; 72 + RTCP_OVERHEAD];
    report[0] = 0x80;
    report[1] = 201;
    bench("srtcp: protect a 72-byte report", mhz, || {
        let mut buf = report;
        black_box(unwrap!(outbound.protect_rtcp(&mut buf, 72).ok()));
    });
    let (keys, _) = cycles::measure(|| black_box(Inbound::new(black_box(&master))));
    info!("srtp: deriving a context's keys took {} cycles", keys);
    info!(
        "srtp per frame: +{} µs to send, +{} µs to receive",
        protect.mean() / mhz,
        unprotect.mean() / mhz
    );
}
