//! What an intercom device does each 20 ms frame, one operation at a time,
//! for measuring on a microcontroller. `run` times each operation against a
//! counter the caller provides: CPU cycles on a Pico W
//! (`examples/pico-w/src/cycles.rs`), executed instructions under QEMU
//! (`examples/m0-cycles`). No allocator and no `std`.

#![allow(dead_code)]

use embedded_rtp::duck::{self, Ducker};
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::{self, Packet};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::srtp::{self, Inbound, MasterKey, Outbound, RTP_OVERHEAD};
use embedded_rtp::{Receiver, Sender};

use super::device::{Device, Frame, KEYS, KEYS_LEN};

/// Each operation is timed over this many runs and averaged.
pub const RUNS: u32 = 16;
const SRTP_BYTES: usize = PACKET_BYTES + RTP_OVERHEAD;
const TICKS: u32 = FRAME_SAMPLES as u32;

/// Everything the workload needs, in one place so it can live in a
/// `static` rather than on a small stack.
pub struct Workload {
    sender: Sender,
    outbound: Outbound,
    inbound: Inbound,
    receiver: Receiver<8, FRAME_BYTES>,
    concealer: Concealer<FRAME_SAMPLES>,
    ducker: Ducker,
    packet: [u8; SRTP_BYTES],
    /// Fresh SRTP packets: replays are rejected, so each run needs its own.
    protected: [[u8; SRTP_BYTES]; RUNS as usize],
    device: Device<'static>,
    /// What the server would send the device, protected under its keys.
    to_device: [[u8; SRTP_BYTES]; RUNS as usize],
    frame: Frame,
    wrong_key: Inbound,
}

fn key(seed: u8) -> MasterKey {
    let mut bytes = [0u8; 30];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = seed.wrapping_mul(29).wrapping_add(i as u8);
    }
    MasterKey::from_bytes(&bytes)
}

impl Workload {
    pub fn new() -> Self {
        let frame: Frame = core::array::from_fn(|i| ((i as i32 * 97) % 8_000 - 4_000) as i16);
        let mut packet = [0u8; SRTP_BYTES];
        l16::encode(&frame, &mut packet[packet::HEADER_LEN..]);

        // Packets to unprotect, as another sender would make them.
        let mut sender = Sender::new(7, PAYLOAD_TYPE, TICKS, 100, 0);
        let mut outbound = Outbound::new(&key(1));
        let mut protected = [[0u8; SRTP_BYTES]; RUNS as usize];
        for p in &mut protected {
            p.copy_from_slice(&packet);
            let len = sender.send_in_place(p, FRAME_BYTES).unwrap();
            outbound.protect_rtp(p, len).unwrap();
        }

        // A device that has joined: hand it a KEYS reply.
        let kek = [9u8; 16];
        let (up, down) = (key(2), key(3));
        let mut device = Device::new("bench", "Bench", Some(kek), [1; 10]);
        let mut reply = [0u8; KEYS_LEN];
        reply[..4].copy_from_slice(KEYS);
        reply[4..44].copy_from_slice(&srtp::wrap(&kek, &up));
        reply[44..].copy_from_slice(&srtp::wrap(&kek, &down));
        device.datagram(&mut reply, 0);
        let mut server = Sender::new(8, PAYLOAD_TYPE, TICKS, 200, 0);
        let mut server_out = Outbound::new(&down);
        let mut to_device = [[0u8; SRTP_BYTES]; RUNS as usize];
        for p in &mut to_device {
            p.copy_from_slice(&packet);
            let len = server.send_in_place(p, FRAME_BYTES).unwrap();
            server_out.protect_rtp(p, len).unwrap();
        }

        Self {
            sender: Sender::new(1, PAYLOAD_TYPE, TICKS, 0, 0),
            outbound: Outbound::new(&key(4)),
            inbound: Inbound::new(&key(1)),
            receiver: Receiver::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT),
            concealer: Concealer::new(),
            ducker: Ducker::new(duck::Config::DEFAULT),
            packet,
            protected,
            device,
            to_device,
            frame,
            wrong_key: Inbound::new(&key(6)),
        }
    }

    /// Time every operation with `now` (a counter that wraps at `mask`) and
    /// pass each name and its average count per run to `report`.
    pub fn run(
        &mut self,
        mut now: impl FnMut() -> u32,
        mask: u32,
        counts_down: bool,
        mut report: impl FnMut(&str, u32),
    ) {
        let mut time = |name: &str, op: &mut dyn FnMut(u32)| {
            op(0); // warm up caches and branch state; not counted
            let start = now();
            for i in 1..RUNS {
                op(i);
            }
            let end = now();
            let elapsed = if counts_down {
                start.wrapping_sub(end)
            } else {
                end.wrapping_sub(start)
            } & mask;
            report(name, elapsed / (RUNS - 1));
        };

        let frame = self.frame;
        let packet = &mut self.packet;
        let sender = &mut self.sender;
        time("encode + header (send_in_place)", &mut |_| {
            l16::encode(&frame, &mut packet[packet::HEADER_LEN..]);
            sender.send_in_place(packet, FRAME_BYTES).unwrap();
        });

        let outbound = &mut self.outbound;
        time("srtp protect_rtp (652 B)", &mut |_| {
            outbound.protect_rtp(packet, PACKET_BYTES).unwrap();
        });

        // A packet under the wrong key: checking its tag (HMAC-SHA1) is all
        // that runs, so this is the authentication share of the cost.
        let forged = self.protected[0];
        let wrong = &mut self.wrong_key;
        time("srtp reject forged (HMAC only)", &mut |_| {
            let mut copy = forged;
            let _ = wrong.unprotect_rtp(&mut copy);
        });

        let protected = &mut self.protected;
        let inbound = &mut self.inbound;
        let receiver = &mut self.receiver;
        let mut lens = [0usize; RUNS as usize];
        time("srtp unprotect_rtp (662 B)", &mut |i| {
            lens[i as usize] = inbound.unprotect_rtp(&mut protected[i as usize]).unwrap();
        });

        let mut arrival = 0u32;
        time("parse + receive + play", &mut |i| {
            let len = lens[i as usize].max(packet::HEADER_LEN);
            if let Ok(p) = Packet::parse(&protected[i as usize][..len]) {
                arrival = arrival.wrapping_add(TICKS);
                receiver.receive(&p, arrival);
            }
            let _ = receiver.play();
        });

        let concealer = &mut self.concealer;
        let payload = &self.packet[packet::HEADER_LEN..PACKET_BYTES];
        time("concealer: good frame (decode)", &mut |_| {
            let _ = concealer.play(Playout::Frame {
                sequence: 0,
                payload,
            });
        });
        time("concealer: lost frame, then good", &mut |_| {
            let _ = concealer.play(Playout::Lost { sequence: 0 });
            let _ = concealer.play(Playout::Frame {
                sequence: 0,
                payload,
            });
        });

        // Full duplex only: the microphone against what the speaker plays.
        let ducker = &mut self.ducker;
        let loud = [12_000i16; FRAME_SAMPLES];
        time("echo suppressor (duck)", &mut |_| {
            let mut mic = frame;
            ducker.process(&loud, &mut mic);
            core::hint::black_box(mic);
        });

        let device = &mut self.device;
        let to_device = &mut self.to_device;
        let mut now_ms = 1_000u64;
        time("device: whole frame both ways, SRTP", &mut |i| {
            now_ms += 20;
            device.datagram(&mut to_device[i as usize], now_ms);
            let _ = device.tick(now_ms, Some(&frame), &mut |_| {});
        });

        time("srtp: new context (key derivation)", &mut |_| {
            core::hint::black_box(Outbound::new(&key(5)));
        });
    }
}
