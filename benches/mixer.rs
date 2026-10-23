//! What the example mixing server costs per 20 ms tick with N participants,
//! all talking over SRTP: receiving their N packets, then mixing and
//! sending N mixes. Socket calls are left out, so this is the server's own
//! work. Divide by 20 ms for the share of one core.
//!
//! ```sh
//! cargo bench --features srtp --bench mixer
//! ```

#[path = "../examples/common/intercom.rs"]
mod intercom;

use std::collections::HashMap;
use std::hint::black_box;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use embedded_rtp::profile::FRAME_SAMPLES;
use intercom::{Device, Frame, Kek, Mixer, Random, TICK_MS};

fn seeded(seed: u64) -> Random {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    Box::new(move |buf: &mut [u8]| {
        for byte in buf {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            *byte = (x >> 32) as u8;
        }
    })
}

struct Room {
    mixer: Mixer,
    devices: Vec<(SocketAddr, Device)>,
    now: u64,
    outbox: Vec<(SocketAddr, Vec<u8>)>,
}

impl Room {
    fn new(count: usize) -> Self {
        let registry: HashMap<String, Kek> = (0..count)
            .map(|i| (format!("d{i}"), [i as u8; 16]))
            .collect();
        let devices = (0..count)
            .map(|i| {
                let addr: SocketAddr = format!("10.0.{}.{}:4000", i / 200, i % 200 + 1)
                    .parse()
                    .unwrap();
                let device =
                    Device::new(&format!("d{i}"), "d", Some([i as u8; 16]), seeded(i as u64));
                (addr, device)
            })
            .collect();
        let mut room = Self {
            mixer: Mixer::new(registry, false, seeded(1)),
            devices,
            now: 0,
            outbox: Vec::new(),
        };
        for _ in 0..50 {
            room.round(None); // join and fill the jitter buffers
        }
        room
    }

    /// Every device captures and sends one frame; returns what reached the
    /// server, ready to deliver.
    fn capture(&mut self, frame: Option<&Frame>) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut up = Vec::new();
        for (addr, device) in &mut self.devices {
            device.tick(self.now, frame, &mut |d| up.push((*addr, d.to_vec())));
        }
        up
    }

    /// The server's side of one tick: take every packet, mix, send.
    fn serve(&mut self, up: Vec<(SocketAddr, Vec<u8>)>) {
        let outbox = &mut self.outbox;
        outbox.clear();
        for (from, mut datagram) in up {
            self.mixer
                .datagram(from, &mut datagram, self.now, &mut |to, d| {
                    outbox.push((to, d.to_vec()))
                });
        }
        // Keep the reports (every 5 s) for the devices, so they stay in the
        // room; the audio only needs to be produced.
        self.mixer.tick(self.now, &mut |to, d| {
            if embedded_rtp::packet::is_rtcp(d) {
                outbox.push((to, d.to_vec()));
            }
            black_box((to, d));
        });
    }

    /// Hand the server's replies to the devices.
    fn reply(&mut self) {
        let replies = std::mem::take(&mut self.outbox);
        for (to, mut datagram) in replies {
            if let Some((_, device)) = self.devices.iter_mut().find(|(a, _)| *a == to) {
                device.datagram(&mut datagram, self.now);
            }
        }
    }

    fn round(&mut self, frame: Option<&Frame>) {
        let up = self.capture(frame);
        self.serve(up);
        self.reply();
        self.now += TICK_MS;
    }
}

fn mixer(c: &mut Criterion) {
    sanity();
    let mut group = c.benchmark_group("mixer_tick");
    let frame: Frame = core::array::from_fn(|i| (i as i16 - 160) * 50);
    for count in [2, 8, 32] {
        let mut room = Room::new(count);
        for _ in 0..20 {
            room.round(Some(&frame));
        }
        group.bench_function(BenchmarkId::new("talking", count), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let up = room.capture(Some(&frame)); // devices: not timed
                    let start = Instant::now();
                    room.serve(up);
                    total += start.elapsed();
                    room.reply();
                    room.now += TICK_MS;
                }
                total
            })
        });
    }
    group.finish();
}

/// Guard against measuring an empty room: after a long run, every device
/// must still be in it and every mix must still be sent.
fn sanity() {
    let frame: Frame = [100; FRAME_SAMPLES];
    let mut room = Room::new(4);
    for _ in 0..5_000 {
        room.round(Some(&frame)); // 100 s, well past every timeout
    }
    assert_eq!(room.mixer.members().iter().filter(|m| m.active).count(), 4);
    let up = room.capture(Some(&frame));
    assert_eq!(up.len(), 4, "every device still sending audio");
}

criterion_group!(benches, mixer);
criterion_main!(benches);
