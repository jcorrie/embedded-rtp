//! A steady-state frame loop for profilers: send (in place), parse, receive
//! and play one 640-byte frame per iteration through a 16-slot receiver,
//! with one frame in fifty lost.
//!
//! ```sh
//! cargo build --release --example profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out \
//!     target/release/examples/profile
//! callgrind_annotate --auto=yes callgrind.out
//! ```
//!
//! Instruction counts are deterministic, so unlike timings they attribute
//! cost to individual lines without noise.

use std::hint::black_box;

use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::Packet;
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::{Receiver, Sender};

const FRAMES: u32 = 100_000;

fn main() {
    let ticks = FRAME_SAMPLES as u32;
    let mut sender = Sender::new(1, PAYLOAD_TYPE, ticks, 0, 0);
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, ticks, Config::DEFAULT);
    // Audio is captured straight into the packet buffer, after the header.
    let mut wire = [0x5a; PACKET_BYTES];
    let mut played = 0u32;
    for i in 0..FRAMES {
        let len = sender
            .send_in_place(black_box(&mut wire), FRAME_BYTES)
            .unwrap();
        if i % 50 != 49 {
            let packet = Packet::parse(black_box(&wire[..len])).unwrap();
            receiver.receive(&packet, i * ticks);
        }
        if let Playout::Frame { payload, .. } = receiver.play() {
            black_box(payload);
            played += 1;
        }
    }
    println!("{played} of {FRAMES} frames played");
}
