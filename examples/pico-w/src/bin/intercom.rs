//! An intercom client for the Raspberry Pi Pico W: an I2S microphone and an
//! I2S amplifier, talking RTP over Wi-Fi to a mixing server (SPQR, or anything
//! that speaks the `embedded_rtp::profile` format).
//!
//! The audio hardware keeps time. Every 20 ms the microphone's DMA buffer
//! fills, and that frame is sent; every 20 ms the speaker's DMA buffer
//! drains, and the next frame is played from the jitter buffer. Tasks:
//!
//! - `mic_task`: I2S in → L16 frame → the sender (in `main`).
//! - `main`: RTP out on each frame, and RTCP (RR or SR, plus SDES CNAME and
//!   NAME) every 3 s, which also keeps the device registered.
//! - `receive_task`: RTP in → jitter buffer; RTCP in → round-trip time.
//! - `speaker_task`: jitter buffer → concealment → I2S out.
//!
//! Push-to-talk by default: hold the button to talk, and the speaker is muted
//! meanwhile so it can't feed back into the microphone. See README.md for
//! wiring, configuration and the optional features.

#![no_std]
#![no_main]

use core::cell::RefCell;
use core::net::SocketAddrV4;
#[cfg(not(feature = "listen-only"))]
use core::sync::atomic::AtomicBool;
#[cfg(any(not(feature = "listen-only"), feature = "cpu-load"))]
use core::sync::atomic::Ordering;

use aligned::{Aligned, A4};
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use defmt::{info, unwrap, warn};
use embassy_executor::Spawner;
#[cfg(not(feature = "listen-only"))]
use embassy_futures::select::{select, Either};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, StackResources};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::dma;
#[cfg(not(feature = "listen-only"))]
use embassy_rp::gpio::{Input, Pull};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, PIO0, PIO1};
use embassy_rp::pio::{self, Pio};
use embassy_rp::pio_programs::i2s::{PioI2sOut, PioI2sOutProgram};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
#[cfg(not(feature = "listen-only"))]
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_rtp::jitter::Config;
use embedded_rtp::l16::Concealer;
use embedded_rtp::packet::{self, Packet};
#[cfg(not(feature = "listen-only"))]
use embedded_rtp::profile::PACKET_BYTES;
use embedded_rtp::profile::{CLOCK_RATE, FRAME_BYTES, FRAME_SAMPLES, PAYLOAD_TYPE};
use embedded_rtp::rtcp::{self, sdes, Compound, Writer};
use embedded_rtp::{Receiver, Sender};
#[cfg(not(feature = "listen-only"))]
use pico_w_intercom::{mic_samples, Mic, MicProgram};
use pico_w_intercom::{rtp_ticks, speaker_words};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => pio::InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>, dma::InterruptHandler<DMA_CH2>;
});

// Configuration, baked in at build time (README.md).
const WIFI_NETWORK: &str = env!("WIFI_NETWORK");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");
/// The server, as `address:port`, e.g. `192.168.1.10:1234`.
const SERVER: &str = env!("INTERCOM_SERVER");
/// The roster name, sent as SDES NAME (and CNAME).
const DEVICE_NAME: &str = match option_env!("DEVICE_NAME") {
    Some(name) => name,
    None => "Pico W",
};

const FRAME_TICKS: u32 = FRAME_SAMPLES as u32;
/// Jitter buffer slots: `Config::DEFAULT` reaches 8 frames (160 ms).
const JITTER_SLOTS: usize = 8;
/// Under the server's 5 s timeout, with room for a lost report.
const RTCP_INTERVAL: Duration = Duration::from_secs(3);
/// Frames between timing reports: 5 s.
const STATS_FRAMES: u32 = 250;

// Wi-Fi firmware and the Pico W's board configuration, 4-byte aligned as the
// driver needs.
static FIRMWARE: Aligned<A4, [u8; cyw43_firmware::CYW43_43439A0.len()]> =
    Aligned(*cyw43_firmware::CYW43_43439A0);
static NVRAM: Aligned<A4, [u8; include_bytes!("../../firmware/nvram_rp2040.bin").len()]> =
    Aligned(*include_bytes!("../../firmware/nvram_rp2040.bin"));

/// What the receive and speaker tasks share.
struct Playback {
    receiver: Receiver<JITTER_SLOTS, FRAME_BYTES>,
    concealer: Concealer<FRAME_SAMPLES>,
}

static PLAYBACK: Mutex<CriticalSectionRawMutex, RefCell<Playback>> =
    Mutex::new(RefCell::new(Playback {
        receiver: Receiver::new(PAYLOAD_TYPE, FRAME_TICKS, Config::DEFAULT),
        concealer: Concealer::new(),
    }));

/// Encoded microphone frames, from `mic_task` to the sender.
#[cfg(not(feature = "listen-only"))]
static MIC_FRAMES: Channel<CriticalSectionRawMutex, [u8; FRAME_BYTES], 2> = Channel::new();

/// True while push-to-talk is held: the speaker plays silence so the
/// microphone doesn't pick it up.
#[cfg(not(feature = "listen-only"))]
static MUTED: AtomicBool = AtomicBool::new(false);

/// Processing time per frame, reported every [`STATS_FRAMES`].
struct Timing {
    name: &'static str,
    frames: u32,
    total_us: u64,
    max_us: u64,
}

impl Timing {
    const fn new(name: &'static str) -> Self {
        Self {
            name,
            frames: 0,
            total_us: 0,
            max_us: 0,
        }
    }

    fn record(&mut self, started: Instant) {
        let us = started.elapsed().as_micros();
        self.frames += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        if self.frames == STATS_FRAMES {
            info!(
                "{}: {} µs per frame on average, {} µs at most",
                self.name,
                self.total_us / u64::from(self.frames),
                self.max_us
            );
            *self = Self::new(self.name);
        }
    }
}

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

/// Capture 20 ms at a time, double buffered: while DMA fills one buffer, the
/// other is converted and handed to the sender.
#[cfg(not(feature = "listen-only"))]
#[embassy_executor::task]
async fn mic_task(mut mic: Mic<'static, PIO1, 0>) -> ! {
    let mut buffers = [[0u32; FRAME_SAMPLES]; 2];
    let mut samples = [0i16; FRAME_SAMPLES];
    let mut payload = [0u8; FRAME_BYTES];
    let mut timing = Timing::new("mic");
    let mut filled = false;
    let mut flip = false;
    mic.start();
    loop {
        let (recording, recorded) = halves(&mut buffers, flip);
        let transfer = mic.read(recording);
        if filled {
            let started = Instant::now();
            mic_samples(recorded, &mut samples);
            embedded_rtp::l16::encode(&samples, &mut payload);
            // A sender that has fallen behind loses this frame rather than
            // delaying the next.
            let _ = MIC_FRAMES.try_send(payload);
            timing.record(started);
        }
        transfer.await;
        flip = !flip;
        filled = true;
    }
}

/// Play 20 ms at a time, double buffered: while DMA plays one buffer, the
/// next frame is taken from the jitter buffer into the other.
#[embassy_executor::task]
async fn speaker_task(mut speaker: PioI2sOut<'static, PIO1, 1>) -> ! {
    let mut buffers = [[0u32; FRAME_SAMPLES]; 2];
    let mut timing = Timing::new("play");
    let mut flip = false;
    speaker.start();
    loop {
        let (playing, next) = halves(&mut buffers, flip);
        let transfer = speaker.write(playing);
        let started = Instant::now();
        PLAYBACK.lock(|playback| {
            let Playback {
                receiver,
                concealer,
            } = &mut *playback.borrow_mut();
            // Always take the frame, so the jitter buffer keeps time.
            match concealer.play(receiver.play()) {
                Some(samples) if !muted() => speaker_words(samples, next),
                _ => next.fill(0),
            }
        });
        timing.record(started);
        transfer.await;
        flip = !flip;
    }
}

/// The two halves of a double buffer: the one DMA is using, and the other.
fn halves<T>(buffers: &mut [T; 2], flip: bool) -> (&mut T, &mut T) {
    let [a, b] = buffers;
    if flip {
        (b, a)
    } else {
        (a, b)
    }
}

fn muted() -> bool {
    #[cfg(not(feature = "listen-only"))]
    return MUTED.load(Ordering::Relaxed);
    #[cfg(feature = "listen-only")]
    false
}

/// RTP from the server into the jitter buffer; the server's RTCP for the
/// round trip. Anything not from the server is ignored.
#[embassy_executor::task]
async fn receive_task(socket: &'static UdpSocket<'static>, server: IpEndpoint, ours: u32) -> ! {
    let mut buf = [0u8; 1500];
    let mut timing = Timing::new("receive");
    loop {
        let Ok((len, meta)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        if meta.endpoint != server {
            continue;
        }
        let started = Instant::now();
        let now = started.as_micros();
        let datagram = &buf[..len];
        if packet::is_rtcp(datagram) {
            let Ok(compound) = Compound::parse(datagram) else {
                continue;
            };
            let now = rtcp::ntp_short(rtcp::ntp_from_micros(now));
            for packet in compound.packets() {
                if let rtcp::Packet::SenderReport {
                    ssrc,
                    info,
                    reports,
                } = packet
                {
                    PLAYBACK.lock(|p| p.borrow_mut().receiver.sender_report(ssrc, &info, now));
                    for block in reports.filter(|block| block.ssrc == ours) {
                        if let Some(rtt) =
                            rtcp::round_trip(now, block.last_sr, block.delay_since_last_sr)
                        {
                            // 1/65536 s units.
                            info!(
                                "server: round trip {} ms, {}/256 lost, jitter {} ticks",
                                (u64::from(rtt) * 1000) >> 16,
                                block.fraction_lost,
                                block.jitter
                            );
                        }
                    }
                }
            }
        } else if let Ok(packet) = Packet::parse(datagram) {
            PLAYBACK.lock(|p| {
                p.borrow_mut().receiver.receive(&packet, rtp_ticks(now));
            });
            timing.record(started);
        }
    }
}

/// Our RTCP: an SR once we have sent audio (else an RR), with a report block
/// about the server's stream, and our name.
fn write_report(sender: &Sender, buf: &mut [u8]) -> usize {
    let ntp = rtcp::ntp_from_micros(Instant::now().as_micros());
    let block = PLAYBACK.lock(|p| p.borrow_mut().receiver.report_block(rtcp::ntp_short(ntp)));
    let ssrc = sender.ssrc();
    let name = DEVICE_NAME.as_bytes();
    let mut writer = Writer::new(buf);
    let written = if sender.packet_count() > 0 {
        writer.sender_report(ssrc, &sender.sender_info(ntp), block.as_slice())
    } else {
        writer.receiver_report(ssrc, block.as_slice())
    }
    .and_then(|()| writer.source_description(ssrc, &[(sdes::CNAME, name), (sdes::NAME, name)]));
    unwrap!(written.ok(), "the report fits its buffer");
    writer.len()
}

/// Counts idle-loop iterations, for the `cpu-load` feature.
#[cfg(feature = "cpu-load")]
static IDLE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[cfg(feature = "cpu-load")]
#[embassy_executor::task]
async fn idle_task() -> ! {
    let mut count = 0u32;
    loop {
        count = count.wrapping_add(1);
        IDLE.store(count, Ordering::Relaxed);
        embassy_futures::yield_now().await;
    }
}

/// CPU load from the idle count, against `baseline` iterations a second
/// with nothing else running.
#[cfg(feature = "cpu-load")]
#[embassy_executor::task]
async fn load_task(baseline: u32) -> ! {
    let mut ticker = Ticker::every(Duration::from_secs(5));
    let mut last = IDLE.load(Ordering::Relaxed);
    loop {
        ticker.next().await;
        let count = IDLE.load(Ordering::Relaxed);
        let per_second = count.wrapping_sub(last) / 5;
        last = count;
        let idle = (u64::from(per_second) * 1000 / u64::from(baseline.max(1))).min(1000);
        info!("cpu: {}.{}% busy", (1000 - idle) / 10, (1000 - idle) % 10);
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;
    let server: SocketAddrV4 = unwrap!(SERVER.parse().ok(), "INTERCOM_SERVER must be address:port");
    let server = IpEndpoint::new(IpAddress::Ipv4(*server.ip()), server.port());

    #[cfg(feature = "cpu-load")]
    let baseline = {
        // Idle iterations a second with nothing else running.
        spawner.spawn(unwrap!(idle_task()));
        let start = IDLE.load(Ordering::Relaxed);
        Timer::after_secs(1).await;
        let baseline = IDLE.load(Ordering::Relaxed).wrapping_sub(start);
        info!("cpu: {} idle iterations a second at rest", baseline);
        baseline
    };

    // Wi-Fi: the CYW43 on PIO0 and DMA channel 0, on the Pico W's fixed pins.
    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio0 = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio0.common,
        pio0.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio0.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (device, mut control, runner) =
        cyw43::new(STATE.init(cyw43::State::new()), pwr, spi, &FIRMWARE, &NVRAM).await;
    spawner.spawn(unwrap!(wifi_task(runner)));
    control.init(cyw43_firmware::CYW43_43439A0_CLM).await;
    // Power saving holds received packets for up to a beacon interval: far
    // too long for audio.
    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        rng.next_u64(),
    );
    spawner.spawn(unwrap!(net_task(runner)));

    while control
        .join(
            WIFI_NETWORK,
            cyw43::JoinOptions::new(WIFI_PASSWORD.as_bytes()),
        )
        .await
        .is_err()
    {
        warn!("joining {} failed; retrying", WIFI_NETWORK);
        Timer::after_secs(1).await;
    }
    stack.wait_config_up().await;
    if let Some(config) = stack.config_v4() {
        info!("on {} as {}", WIFI_NETWORK, config.address);
    }

    // One socket for both directions: the server sends the mix back to the
    // address and port we send from.
    static RX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; 2048]> = StaticCell::new();
    let mut socket = UdpSocket::new(
        stack,
        RX_META.init([PacketMetadata::EMPTY; 8]),
        RX_BUF.init([0; 4096]),
        TX_META.init([PacketMetadata::EMPTY; 8]),
        TX_BUF.init([0; 2048]),
    );
    let port = 49_152 + (rng.next_u32() % 16_384) as u16;
    unwrap!(socket.bind(port).ok());
    static SOCKET: StaticCell<UdpSocket<'static>> = StaticCell::new();
    let socket: &'static UdpSocket<'static> = SOCKET.init(socket);

    // RFC 3550 §5.1: a random SSRC, sequence number and timestamp, new on
    // every boot.
    #[cfg_attr(feature = "listen-only", allow(unused_mut))]
    let mut sender = Sender::new(
        rng.next_u32(),
        PAYLOAD_TYPE,
        FRAME_TICKS,
        rng.next_u32() as u16,
        rng.next_u32(),
    );
    spawner.spawn(unwrap!(receive_task(socket, server, sender.ssrc())));

    // Audio on PIO1: the amplifier on state machine 1 and DMA channel 2, the
    // microphone on state machine 0 and DMA channel 1.
    let mut pio1 = Pio::new(p.PIO1, Irqs);
    let speaker_program = PioI2sOutProgram::new(&mut pio1.common);
    let speaker = PioI2sOut::new(
        &mut pio1.common,
        pio1.sm1,
        p.DMA_CH2,
        Irqs,
        p.PIN_20, // DIN
        p.PIN_18, // BCLK
        p.PIN_19, // LRC
        CLOCK_RATE,
        16,
        &speaker_program,
    );
    spawner.spawn(unwrap!(speaker_task(speaker)));

    #[cfg(not(feature = "listen-only"))]
    let button = {
        let mic_program = MicProgram::new(&mut pio1.common);
        let mic = Mic::new(
            &mut pio1.common,
            pio1.sm0,
            p.DMA_CH1,
            Irqs,
            p.PIN_12, // SD
            p.PIN_10, // SCK
            p.PIN_11, // WS
            &mic_program,
        );
        spawner.spawn(unwrap!(mic_task(mic)));
        // Push-to-talk, to ground.
        Input::new(p.PIN_15, Pull::Up)
    };
    #[cfg(feature = "listen-only")]
    let _ = p.DMA_CH1;

    #[cfg(feature = "cpu-load")]
    spawner.spawn(unwrap!(load_task(baseline)));

    info!("intercom up: {} -> {}", DEVICE_NAME, SERVER);
    let mut report = [0u8; 128];
    let mut rtcp_ticker = Ticker::every(RTCP_INTERVAL);

    // Listening only: RTCP is all we send, and keeps us registered.
    #[cfg(feature = "listen-only")]
    loop {
        rtcp_ticker.next().await;
        send_report(socket, server, &sender, &mut report).await;
    }

    #[cfg(not(feature = "listen-only"))]
    {
        let mut packet = [0u8; PACKET_BYTES];
        let mut timing = Timing::new("send");
        loop {
            match select(MIC_FRAMES.receive(), rtcp_ticker.next()).await {
                Either::First(payload) => {
                    let started = Instant::now();
                    let talking = cfg!(feature = "always-on") || button.is_low();
                    MUTED.store(talking && !cfg!(feature = "always-on"), Ordering::Relaxed);
                    if talking {
                        packet[packet::HEADER_LEN..].copy_from_slice(&payload);
                        let len = unwrap!(sender.send_in_place(&mut packet, FRAME_BYTES).ok());
                        if socket.send_to(&packet[..len], server).await.is_err() {
                            warn!("send failed");
                        }
                    } else {
                        // Timestamps keep counting through silence; the next
                        // packet starts a talkspurt.
                        sender.skip();
                    }
                    timing.record(started);
                }
                Either::Second(()) => send_report(socket, server, &sender, &mut report).await,
            }
        }
    }
}

async fn send_report(socket: &UdpSocket<'_>, server: IpEndpoint, sender: &Sender, buf: &mut [u8]) {
    let len = write_report(sender, buf);
    if socket.send_to(&buf[..len], server).await.is_err() {
        warn!("rtcp send failed");
    }
}
