//! A mixing server for an intercom: every participant hears everyone else,
//! never themselves, over SRTP. See `examples/common/intercom.rs` for how
//! the room works and how devices join.
//!
//! ```sh
//! # devices.txt: one `<id> <32 hex digit KEK>` per line
//! cargo run --release --features srtp --example mixing_server -- \
//!     --devices devices.txt --listen 0.0.0.0:5004
//! ```
//!
//! `--insecure` also admits plain RTP from anyone, unauthenticated and in the
//! clear: for testing with tools like ffmpeg on a trusted network only.
//!
//! The loop wakes for each datagram and for each 20 ms tick, allocates
//! nothing per tick, and every 10 s logs the room and how late ticks ran.

#[path = "../common/intercom.rs"]
mod intercom;

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use intercom::{Mixer, DATAGRAM_MAX, TICK_MS};

const USAGE: &str = "usage: mixing_server --devices FILE [--listen ADDR] [--insecure]";

fn main() {
    let mut devices_path = None;
    let mut listen: SocketAddr = "0.0.0.0:5004".parse().unwrap();
    let mut insecure = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--devices" => devices_path = args.next(),
            "--listen" => listen = args.next().and_then(|a| a.parse().ok()).expect(USAGE),
            "--insecure" => insecure = true,
            _ => panic!("{USAGE}"),
        }
    }
    let devices = match devices_path {
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            intercom::parse_devices(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
        }
        None if insecure => Default::default(),
        None => panic!("{USAGE}"),
    };

    let socket = UdpSocket::bind(listen).unwrap_or_else(|e| panic!("bind {listen}: {e}"));
    eprintln!(
        "mixing server on {listen}: {} devices{}",
        devices.len(),
        if insecure {
            ", INSECURE: plain RTP admitted"
        } else {
            ""
        }
    );
    let random: intercom::Random =
        Box::new(|buf: &mut [u8]| getrandom::getrandom(buf).expect("OS random numbers"));
    let mut mixer = Mixer::new(devices, insecure, random);

    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;
    let tick = Duration::from_millis(TICK_MS);
    let mut next_tick = start + tick;
    let mut late = Duration::ZERO;
    let mut log_at = start + Duration::from_secs(10);
    let mut buf = [0u8; DATAGRAM_MAX];
    let mut send = |to: SocketAddr, datagram: &[u8]| {
        // A full socket buffer or an unreachable peer just loses a packet.
        let _ = socket.send_to(datagram, to);
    };

    loop {
        let now = Instant::now();
        if now >= next_tick {
            late = late.max(now - next_tick);
            mixer.tick(now_ms(), &mut send);
            next_tick += tick;
            if Instant::now() > next_tick + 5 * tick {
                next_tick = Instant::now() + tick; // stalled: don't race to catch up
            }
            if now >= log_at {
                log_at += Duration::from_secs(10);
                let members = mixer.members();
                let active = members.iter().filter(|m| m.active).count();
                eprintln!("{active} in the room, ticks up to {late:?} late");
                for m in members {
                    let state = if m.active { "" } else { " (joining)" };
                    let c = m.counters;
                    eprintln!(
                        "  {} at {}{state}: {} played, {} concealed, {} dropped late, \
                         {} dropped to cut delay",
                        m.name,
                        m.addr,
                        c.played,
                        c.concealed,
                        c.late,
                        c.overflow + c.drained,
                    );
                }
                late = Duration::ZERO;
            }
            continue;
        }

        // Sleep until the next datagram or the next tick, whichever is first.
        socket.set_read_timeout(Some(next_tick - now)).unwrap();
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => mixer.datagram(from, &mut buf[..len], now_ms(), &mut send),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            // Windows reports an earlier send's ICMP error on the next receive.
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
            Err(e) => panic!("receive: {e}"),
        }
    }
}

#[cfg(test)]
mod tests;
