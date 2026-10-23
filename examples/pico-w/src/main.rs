//! An intercom device on a Raspberry Pi Pico W: joins the example
//! `mixing_server` over Wi-Fi, talks while the BOOTSEL button is held, and
//! plays the room through an I2S amplifier. The protocol side is the same
//! `Device` the desktop client and the tests use
//! (`examples/common/device.rs`).
//!
//! See `README.md` for wiring, configuration and flashing.

#![no_std]
#![no_main]

mod audio;
#[path = "../../common/device.rs"]
mod device;
mod mic;

use core::net::SocketAddrV4;

use cyw43::JoinOptions;
use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use defmt::{info, unwrap, warn};
use embassy_executor::{Executor, InterruptExecutor, Spawner};
use embassy_futures::select::{select, Either};
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpEndpoint, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::interrupt;
use embassy_rp::interrupt::{InterruptExt, Priority};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, DMA_CH2, PIO0, PIO1};
use embassy_rp::pio::{self, Pio};
use embassy_rp::{bind_interrupts, dma, peripherals, Peri};
use embassy_time::Instant;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use device::{Device, Frame, DATAGRAM_MAX};
use embedded_rtp::profile::FRAME_SAMPLES;

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => pio::InterruptHandler<PIO0>;
    PIO1_IRQ_0 => pio::InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>, dma::InterruptHandler<DMA_CH2>;
});

// Configuration, set when building (see README.md):
//     WIFI_SSID=... WIFI_PASSWORD=... SERVER=192.168.1.10:5004 \
//     DEVICE_ID=kitchen DEVICE_NAME=Kitchen DEVICE_KEK=<32 hex digits> \
//     cargo run --release
// Without DEVICE_KEK the device talks plain RTP, for a server run with
// `--insecure`.
const WIFI_SSID: &str = config(option_env!("WIFI_SSID"));
const WIFI_PASSWORD: &str = config(option_env!("WIFI_PASSWORD"));
const SERVER: &str = config(option_env!("SERVER"));
const DEVICE_ID: &str = config(option_env!("DEVICE_ID"));
const DEVICE_NAME: &str = config(option_env!("DEVICE_NAME"));
const DEVICE_KEK: &str = config(option_env!("DEVICE_KEK"));

const fn config(value: Option<&'static str>) -> &'static str {
    match value {
        Some(value) => value,
        None => "",
    }
}

/// Audio runs on a higher-priority executor, so a busy network task (SRTP
/// takes a while on a Cortex-M0+) can't make it miss a DMA deadline.
static AUDIO_EXECUTOR: InterruptExecutor = InterruptExecutor::new();

#[interrupt]
unsafe fn SWI_IRQ_1() {
    unsafe { AUDIO_EXECUTOR.on_interrupt() }
}

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = embassy_rp::init(Default::default());

    // Audio: I2S microphone and amplifier on PIO1, at high priority.
    let Pio {
        mut common,
        sm0,
        sm1,
        ..
    } = Pio::new(p.PIO1, Irqs);
    let i2s = audio::I2s::new(
        &mut common,
        sm0,
        sm1,
        p.DMA_CH1,
        p.DMA_CH2,
        Irqs,
        audio::Pins {
            mic_data: p.PIN_12,
            mic_bit_clock: p.PIN_10,
            mic_word_clock: p.PIN_11,
            amp_data: p.PIN_20,
            amp_bit_clock: p.PIN_18,
            amp_word_clock: p.PIN_19,
        },
    );
    interrupt::SWI_IRQ_1.set_priority(Priority::P2);
    let audio_spawner = AUDIO_EXECUTOR.start(interrupt::SWI_IRQ_1);
    audio_spawner.spawn(unwrap!(audio::run(i2s)));

    // Everything else in thread mode.
    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    let executor = EXECUTOR.init(Executor::new());
    executor.run(|spawner| {
        spawner.spawn(unwrap!(start(
            spawner,
            WifiBoard {
                pwr: p.PIN_23,
                dio: p.PIN_24,
                cs: p.PIN_25,
                clk: p.PIN_29,
                pio: p.PIO0,
                dma: p.DMA_CH0,
            },
            p.BOOTSEL,
        )));
    });
}

type WifiSpi = PioSpi<'static, PIO0, 0>;

#[embassy_executor::task]
async fn wifi_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, WifiSpi>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

/// The Pico W's wiring to its CYW43439 Wi-Fi chip.
struct WifiBoard {
    pwr: Peri<'static, peripherals::PIN_23>,
    dio: Peri<'static, peripherals::PIN_24>,
    cs: Peri<'static, peripherals::PIN_25>,
    clk: Peri<'static, peripherals::PIN_29>,
    pio: Peri<'static, PIO0>,
    dma: Peri<'static, DMA_CH0>,
}

#[embassy_executor::task]
async fn start(spawner: Spawner, wifi: WifiBoard, bootsel: Peri<'static, peripherals::BOOTSEL>) {
    let server: SocketAddrV4 = match SERVER.parse() {
        Ok(server) => server,
        Err(_) => defmt::panic!("set SERVER=<ip>:<port> when building, e.g. 192.168.1.10:5004"),
    };
    if WIFI_SSID.is_empty() || DEVICE_ID.is_empty() {
        defmt::panic!("set WIFI_SSID, WIFI_PASSWORD and DEVICE_ID when building");
    }
    let kek = if DEVICE_KEK.is_empty() {
        warn!("no DEVICE_KEK: plain RTP, for a server run with --insecure");
        None
    } else {
        Some(
            device::parse_kek(DEVICE_KEK)
                .unwrap_or_else(|| defmt::panic!("DEVICE_KEK: 32 hex digits")),
        )
    };

    // Wi-Fi.
    static FIRMWARE: &cyw43::Aligned<cyw43::A4, [u8]> =
        &cyw43::Aligned(*cyw43_firmware::CYW43_43439A0);
    static CLM: &cyw43::Aligned<cyw43::A4, [u8]> =
        &cyw43::Aligned(*cyw43_firmware::CYW43_43439A0_CLM);
    let nvram = cyw43::aligned_bytes!("../firmware/nvram_rp2040.bin");
    let pwr = Output::new(wifi.pwr, Level::Low);
    let cs = Output::new(wifi.cs, Level::High);
    let mut pio = Pio::new(wifi.pio, Irqs);
    let spi = PioSpi::new(
        &mut pio.common,
        pio.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio.irq0,
        cs,
        wifi.dio,
        wifi.clk,
        dma::Channel::new(wifi.dma, Irqs),
    );
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) =
        cyw43::new(STATE.init(cyw43::State::new()), pwr, spi, FIRMWARE, nvram).await;
    spawner.spawn(unwrap!(wifi_task(runner)));
    control.init(CLM).await;
    // Power saving adds latency to every packet; audio needs it off.
    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    let mut rng = RoscRng;
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        embassy_net::Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        rng.next_u64(),
    );
    spawner.spawn(unwrap!(net_task(runner)));

    info!("joining Wi-Fi {}", WIFI_SSID);
    while control
        .join(WIFI_SSID, JoinOptions::new(WIFI_PASSWORD.as_bytes()))
        .await
        .is_err()
    {
        warn!("Wi-Fi join failed, retrying");
    }
    stack.wait_config_up().await;
    info!("on the network as {}", stack.config_v4().map(|c| c.address));

    // One UDP socket for everything: RTP, RTCP and joining.
    static RX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 8]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
    let mut socket = UdpSocket::new(
        stack,
        RX_META.init([PacketMetadata::EMPTY; 8]),
        RX_BUF.init([0; 4096]),
        TX_META.init([PacketMetadata::EMPTY; 8]),
        TX_BUF.init([0; 4096]),
    );
    unwrap!(socket.bind(0));

    let mut seed = [0u8; 10];
    rng.fill_bytes(&mut seed);
    static DEVICE: StaticCell<Device<'static>> = StaticCell::new();
    let device = DEVICE.init(Device::new(DEVICE_ID, DEVICE_NAME, kek, seed));
    let server_endpoint = IpEndpoint::from(server);
    info!(
        "talking to {} as {}; hold BOOTSEL to talk",
        SERVER, DEVICE_ID
    );

    run(device, &mut socket, server_endpoint, bootsel).await
}

/// Up to three datagrams a tick: audio, a report, or a join.
struct Outbox {
    slots: [([u8; 700], usize); 3],
    used: usize,
}

impl Outbox {
    fn push(&mut self, datagram: &[u8]) {
        if let Some((slot, len)) = self.slots.get_mut(self.used) {
            if datagram.len() <= slot.len() {
                slot[..datagram.len()].copy_from_slice(datagram);
                *len = datagram.len();
                self.used += 1;
            }
        }
    }
}

async fn run(
    device: &mut Device<'static>,
    socket: &mut UdpSocket<'_>,
    server: IpEndpoint,
    mut bootsel: Peri<'static, peripherals::BOOTSEL>,
) -> ! {
    static OUTBOX: StaticCell<Outbox> = StaticCell::new();
    let outbox = OUTBOX.init(Outbox {
        slots: [([0; 700], 0); 3],
        used: 0,
    });
    let mut buf = [0u8; DATAGRAM_MAX];
    let mut joined = false;
    loop {
        match select(audio::CAPTURED.receive(), socket.recv_from(&mut buf)).await {
            // The audio clock ticks every 20 ms: send what was captured, play
            // what has arrived.
            Either::First(captured) => {
                let now = Instant::now().as_millis();
                let talking = !cfg!(feature = "listen-only")
                    && embassy_rp::bootsel::is_bootsel_pressed(bootsel.reborrow());
                let capture: Option<&Frame> = if talking { Some(&captured) } else { None };
                outbox.used = 0;
                let heard = device.tick(now, capture, &mut |d| outbox.push(d));
                audio::PLAYBACK.signal(heard.unwrap_or([0; FRAME_SAMPLES]));
                for (datagram, len) in &outbox.slots[..outbox.used] {
                    if socket.send_to(&datagram[..*len], server).await.is_err() {
                        warn!("send failed");
                    }
                }
                if device.is_joined() != joined {
                    joined = device.is_joined();
                    info!(
                        "{}",
                        if joined {
                            "joined the room"
                        } else {
                            "joining..."
                        }
                    );
                }
            }
            Either::Second(Ok((len, meta))) if meta.endpoint == server => {
                device.datagram(&mut buf[..len], Instant::now().as_millis());
            }
            Either::Second(_) => {}
        }
    }
}
