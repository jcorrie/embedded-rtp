//! An intercom device on a desktop: joins a `mixing_server`, sends a test
//! tone, and records what it hears. See `examples/common/intercom.rs`.
//!
//! ```sh
//! cargo run --release --features srtp --example intercom_client -- \
//!     --server 192.168.1.10:5004 --id kitchen --kek <32 hex digits> \
//!     --name Kitchen --tone 440 --out heard.raw
//! ffplay -f s16be -ar 16000 -ch_layout mono heard.raw
//! ```
//!
//! Without `--kek` it talks plain RTP, for a server run with `--insecure`.
//! `--tone 0` listens only. It talks for one second in every two, so you
//! can hear the silences come through.

#[path = "../common/intercom.rs"]
mod intercom;

use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use embedded_rtp::profile::{CLOCK_RATE, FRAME_SAMPLES};
use intercom::{Device, Frame, DATAGRAM_MAX, TICK_MS};

const USAGE: &str = "usage: intercom_client --server ADDR --id ID [--kek HEX] [--name NAME] \
                     [--tone HZ] [--out FILE] [--seconds N]";

fn main() {
    let mut server: Option<SocketAddr> = None;
    let mut id = None;
    let mut kek = None;
    let mut name = None;
    let mut tone = 440u32;
    let mut out = None;
    let mut seconds = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().expect(USAGE);
        match arg.as_str() {
            "--server" => server = Some(value.parse().expect("--server: address and port")),
            "--id" => id = Some(value),
            "--kek" => kek = Some(intercom::parse_kek(&value).expect("--kek: 32 hex digits")),
            "--name" => name = Some(value),
            "--tone" => tone = value.parse().expect("--tone: hertz"),
            "--out" => out = Some(std::fs::File::create(&value).expect("--out")),
            "--seconds" => seconds = Some(value.parse::<u64>().expect("--seconds")),
            _ => panic!("{USAGE}"),
        }
    }
    let server = server.expect(USAGE);
    let id = id.expect(USAGE);
    let name = name.unwrap_or_else(|| id.clone());

    let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect(server).unwrap();
    let random: intercom::Random =
        Box::new(|buf: &mut [u8]| getrandom::getrandom(buf).expect("OS random numbers"));
    let mut device = Device::new(&id, &name, kek, random);

    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;
    let tick = Duration::from_millis(TICK_MS);
    let mut next_tick = start;
    let mut frames = 0u64;
    let mut peak = 0i16;
    let mut buf = [0u8; DATAGRAM_MAX];
    let mut joined = false;

    loop {
        let now = Instant::now();
        if now >= next_tick {
            if seconds.is_some_and(|s| frames * TICK_MS >= s * 1_000) {
                device.leave(&mut |d| drop(socket.send(d)));
                return;
            }
            let talking = tone > 0 && (frames / 50) % 2 == 0;
            let capture = talking.then(|| sine(tone, frames));
            let heard = device.tick(now_ms(), capture.as_ref(), &mut |d| drop(socket.send(d)));
            if device.is_joined() != joined {
                joined = device.is_joined();
                eprintln!("{}", if joined { "joined" } else { "joining..." });
            }
            let heard = heard.unwrap_or([0; FRAME_SAMPLES]);
            peak = peak.max(heard.iter().map(|s| s.saturating_abs()).max().unwrap_or(0));
            if let Some(out) = &mut out {
                let bytes: Vec<u8> = heard.iter().flat_map(|s| s.to_be_bytes()).collect();
                out.write_all(&bytes).unwrap();
            }
            frames += 1;
            if frames % 50 == 0 {
                eprintln!("hearing peak {peak}");
                peak = 0;
            }
            next_tick += tick;
            continue;
        }
        socket.set_read_timeout(Some(next_tick - now)).unwrap();
        match socket.recv(&mut buf) {
            Ok(len) => device.datagram(&mut buf[..len], now_ms()),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => {} // server not up yet
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) => panic!("receive: {e}"),
        }
    }
}

/// Frame `n` of a sine wave at `hz`, at a quarter of full scale.
fn sine(hz: u32, n: u64) -> Frame {
    let mut frame = [0i16; FRAME_SAMPLES];
    for (i, sample) in frame.iter_mut().enumerate() {
        let t = (n * FRAME_SAMPLES as u64 + i as u64) as f64 / f64::from(CLOCK_RATE);
        *sample = ((t * f64::from(hz) * std::f64::consts::TAU).sin() * 8_000.0) as i16;
    }
    frame
}
