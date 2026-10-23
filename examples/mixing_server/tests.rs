//! The intercom end to end over a simulated network: devices and the server
//! exchange datagrams in memory, tick by tick. Run with
//! `cargo test --features srtp --example mixing_server`.

use std::collections::HashMap;
use std::net::SocketAddr;

use super::intercom::*;
use embedded_rtp::profile::FRAME_SAMPLES;

fn seeded(seed: u64) -> Random {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    Box::new(move |buf: &mut [u8]| {
        for byte in buf {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            *byte = (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8;
        }
    })
}

fn kek(n: u8) -> Kek {
    [n; 16]
}

fn device_addr(n: usize) -> SocketAddr {
    format!("10.0.0.{}:40000", n + 10).parse().unwrap()
}

/// Frame `n` from device `who`: distinct for every pair, never zero, and
/// small enough that three summed don't clip.
fn frame(who: usize, n: usize) -> Frame {
    let mut frame = [0i16; FRAME_SAMPLES];
    for (k, sample) in frame.iter_mut().enumerate() {
        *sample = ((who + 1) * 1_000 + (n * 7 + k) % 997) as i16;
    }
    frame
}

fn sum(frames: &[Frame]) -> Frame {
    let mut out = [0i16; FRAME_SAMPLES];
    for frame in frames {
        for (o, s) in out.iter_mut().zip(frame) {
            *o += s;
        }
    }
    out
}

/// Devices borrow their id and name; tests make them `'static`.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

fn seed(n: u64) -> [u8; 10] {
    let mut bytes = [0u8; 10];
    seeded(n)(&mut bytes);
    bytes
}

struct Node {
    addr: SocketAddr,
    device: Device<'static>,
    /// Unplugged: stops ticking and receiving.
    unplugged: bool,
    heard: Vec<Option<Frame>>,
}

struct Sim {
    mixer: Mixer,
    nodes: Vec<Node>,
    now: u64,
    /// Every datagram sent to the server, with its source.
    up: Vec<(SocketAddr, Vec<u8>)>,
    /// Every datagram the server sent, with its destination.
    down: Vec<(SocketAddr, Vec<u8>)>,
}

impl Sim {
    /// Devices `a`, `b`, `c`, ... with KEKs 1, 2, 3, ... registered on a
    /// secure server.
    fn secure(count: usize) -> Self {
        let registry: HashMap<String, Kek> =
            (0..count).map(|i| (name(i), kek(i as u8 + 1))).collect();
        let mut sim = Self::empty(Mixer::new(registry, false, seeded(99)));
        for i in 0..count {
            sim.add(&name(i), Some(kek(i as u8 + 1)));
        }
        sim
    }

    fn empty(mixer: Mixer) -> Self {
        Self {
            mixer,
            nodes: Vec::new(),
            now: 1_000,
            up: Vec::new(),
            down: Vec::new(),
        }
    }

    fn add(&mut self, id: &str, kek: Option<Kek>) -> usize {
        let n = self.nodes.len();
        let display = id.to_uppercase();
        self.nodes.push(Node {
            addr: device_addr(n),
            device: Device::new(leak(id), leak(&display), kek, seed(n as u64 + 1)),
            unplugged: false,
            heard: Vec::new(),
        });
        n
    }

    /// One 20 ms tick everywhere. `talk(i, n)` gives what device `i`
    /// captures on tick `n` of the run.
    fn step(&mut self, captures: &[Option<Frame>]) {
        let mut to_server = Vec::new();
        for (i, node) in self.nodes.iter_mut().enumerate() {
            if node.unplugged {
                continue;
            }
            let capture = captures.get(i).copied().flatten();
            let addr = node.addr;
            let heard = node.device.tick(self.now, capture.as_ref(), &mut |d| {
                to_server.push((addr, d.to_vec()));
            });
            node.heard.push(heard);
        }
        let mut to_devices = Vec::new();
        for (from, datagram) in to_server {
            self.up.push((from, datagram.clone()));
            self.deliver(from, datagram, &mut to_devices);
        }
        self.mixer
            .tick(self.now, &mut |to, d| to_devices.push((to, d.to_vec())));
        for (to, mut datagram) in to_devices {
            self.down.push((to, datagram.clone()));
            if let Some(node) = self.nodes.iter_mut().find(|n| n.addr == to && !n.unplugged) {
                node.device.datagram(&mut datagram, self.now);
            }
        }
        self.now += TICK_MS;
    }

    fn deliver(
        &mut self,
        from: SocketAddr,
        mut datagram: Vec<u8>,
        to_devices: &mut Vec<(SocketAddr, Vec<u8>)>,
    ) {
        self.mixer
            .datagram(from, &mut datagram, self.now, &mut |to, d| {
                to_devices.push((to, d.to_vec()))
            });
    }

    /// Send a datagram to the server as if from `from`, between ticks.
    fn inject(&mut self, from: SocketAddr, datagram: Vec<u8>) {
        let mut to_devices = Vec::new();
        self.deliver(from, datagram, &mut to_devices);
        self.down.extend(to_devices);
    }

    fn idle(&mut self, ticks: usize) {
        for _ in 0..ticks {
            self.step(&[]);
        }
    }

    fn active(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .mixer
            .members()
            .into_iter()
            .filter(|m| m.active)
            .map(|m| m.device.unwrap_or(m.name))
            .collect();
        names.sort();
        names
    }

    fn heard_since(&self, node: usize, tick: usize) -> Vec<Option<Frame>> {
        self.nodes[node].heard[tick..].to_vec()
    }
}

fn name(i: usize) -> String {
    char::from(b'a' + i as u8).to_string()
}

/// `heard`, after its first few frames (the jitter buffers filling and
/// concealment fading in), must be exactly `expected(j)`, `expected(j + 1)`,
/// ... for some `j`, for at least `run` frames.
fn assert_hears(heard: &[Option<Frame>], expected: impl Fn(usize) -> Frame, run: usize) {
    let first = heard
        .iter()
        .position(Option::is_some)
        .expect("heard nothing");
    let start = first + 3;
    let at = heard[start].expect("a gap right after the start");
    let j = (0..400)
        .find(|&j| expected(j) == at)
        .expect("heard something that isn't the expected audio");
    for (offset, frame) in heard[start..start + run].iter().enumerate() {
        assert_eq!(
            frame.as_ref(),
            Some(&expected(j + offset)),
            "frame {offset} after the start"
        );
    }
}

/// Everyone joins over SRTP, and each hears exactly the others: A hears B,
/// B hears A, C (silent) hears A plus B. Nobody hears themselves.
#[test]
fn each_hears_exactly_the_others() {
    let mut sim = Sim::secure(3);
    sim.idle(10);
    assert_eq!(sim.active(), ["a", "b", "c"]);

    let start = sim.nodes[0].heard.len();
    for n in 0..300 {
        sim.step(&[Some(frame(0, n)), Some(frame(1, n)), None]);
    }
    assert_hears(&sim.heard_since(0, start), |j| frame(1, j), 250);
    assert_hears(&sim.heard_since(1, start), |j| frame(0, j), 250);
    assert_hears(
        &sim.heard_since(2, start),
        |j| sum(&[frame(0, j), frame(1, j)]),
        250,
    );
}

/// Nobody talking means no audio packets at all, only the reports.
#[test]
fn silence_sends_no_audio() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    let before = sim.down.len();
    sim.idle(500); // 10 s
    let sent = &sim.down[before..];
    // Two reports per participant in 10 s, nothing else.
    assert!(sent.len() <= 6, "{} datagrams", sent.len());
    assert!(sim
        .nodes
        .iter()
        .all(|n| n.heard[10..].iter().all(Option::is_none)));
}

/// A stranger who joins with someone else's id gets a reply it can't unwrap,
/// never any audio, and doesn't disturb the real device.
#[test]
fn a_stranger_with_someone_elses_id_hears_nothing() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    let stranger = sim.add("a", Some(kek(77)));
    let start = sim.nodes[0].heard.len();
    for n in 0..300 {
        sim.step(&[Some(frame(0, n)), Some(frame(1, n)), None]);
    }
    let to_stranger: Vec<&Vec<u8>> = sim
        .down
        .iter()
        .filter(|(to, _)| *to == sim.nodes[stranger].addr)
        .map(|(_, d)| d)
        .collect();
    assert!(!to_stranger.is_empty());
    assert!(to_stranger.iter().all(|d| d.starts_with(b"KEYS")));
    assert!(!sim.nodes[stranger].device.is_joined());
    assert!(sim.nodes[stranger].heard.iter().all(Option::is_none));
    assert_eq!(sim.active(), ["a", "b"]);
    assert_hears(&sim.heard_since(1, start), |j| frame(0, j), 250);
}

#[test]
fn unknown_devices_get_no_reply() {
    let mut sim = Sim::secure(1);
    let stranger = sim.add("mallory", Some(kek(9)));
    sim.idle(100);
    assert!(sim
        .down
        .iter()
        .all(|(to, _)| *to != sim.nodes[stranger].addr));
    assert_eq!(sim.active(), ["a"]);
}

/// Someone else's packets, replayed from another address or forged, admit
/// nobody. A replayed `JOIN` gets only a `KEYS` reply, which needs the
/// device's KEK to read.
#[test]
fn replayed_and_forged_packets_admit_nobody() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    for n in 0..20 {
        sim.step(&[Some(frame(0, n)), None]);
    }
    let elsewhere: SocketAddr = "10.0.0.99:1".parse().unwrap();
    let captured: Vec<Vec<u8>> = sim.up.iter().map(|(_, d)| d.clone()).collect();
    let before = sim.down.len();
    for datagram in &captured {
        sim.inject(elsewhere, datagram.clone());
        let mut forged = datagram.clone();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        sim.inject(sim.nodes[0].addr, forged);
    }
    assert!(sim.down[before..]
        .iter()
        .all(|(to, d)| *to != elsewhere || d.starts_with(b"KEYS")));
    assert_eq!(sim.active(), ["a", "b"]);
}

/// A BYE leaves at once; a device that goes quiet is dropped after the
/// timeout; a listen-only device stays, kept alive by its reports.
#[test]
fn leaving_timing_out_and_staying() {
    let mut sim = Sim::secure(3);
    sim.idle(10);

    let addr = sim.nodes[1].addr;
    let mut bye = Vec::new();
    sim.nodes[1].device.leave(&mut |d| bye.push(d.to_vec()));
    sim.nodes[1].unplugged = true;
    for datagram in bye {
        sim.inject(addr, datagram);
    }
    assert_eq!(sim.active(), ["a", "c"]);

    sim.nodes[2].unplugged = true; // no BYE: power cut
    sim.idle((TIMEOUT_MS / TICK_MS) as usize - 50);
    assert_eq!(sim.active(), ["a", "c"]);
    sim.idle(100);
    assert_eq!(sim.active(), ["a"]);

    // `a` never talked: listening alone keeps it in the room.
    sim.idle(3_000); // a minute
    assert_eq!(sim.active(), ["a"]);
}

/// After the server restarts, devices notice the silence, join again with
/// fresh keys, and audio flows again.
#[test]
fn devices_rejoin_a_restarted_server() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    let registry = (0..2).map(|i| (name(i), kek(i as u8 + 1))).collect();
    sim.mixer = Mixer::new(registry, false, seeded(123));
    sim.idle((TIMEOUT_MS / TICK_MS) as usize + 100);
    assert_eq!(sim.active(), ["a", "b"]);

    let start = sim.nodes[1].heard.len();
    for n in 0..200 {
        sim.step(&[Some(frame(0, n)), None]);
    }
    assert_hears(&sim.heard_since(1, start), |j| frame(0, j), 150);
}

/// A device that joins again (a reboot, a new port) replaces its old session
/// rather than appearing twice.
#[test]
fn rejoining_replaces_the_old_session() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    sim.nodes[0].unplugged = true;
    let again = sim.add("a", Some(kek(1)));
    sim.idle(10);
    assert_eq!(sim.active(), ["a", "b"]);
    let members = sim.mixer.members();
    let a = members
        .iter()
        .find(|m| m.device.as_deref() == Some("a"))
        .unwrap();
    assert_eq!(a.addr, sim.nodes[again].addr);
}

/// Names come from the devices' SDES.
#[test]
fn names_come_from_sdes() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    let mut names: Vec<String> = sim.mixer.members().into_iter().map(|m| m.name).collect();
    names.sort();
    assert_eq!(names, ["A", "B"]);
}

/// In insecure mode anyone sending plain RTP joins and is mixed; a secure
/// server ignores them.
#[test]
fn insecure_mode() {
    let mut sim = Sim::empty(Mixer::new(HashMap::new(), true, seeded(5)));
    sim.add("x", None);
    sim.add("y", None);
    let start = sim.nodes[0].heard.len();
    for n in 0..200 {
        sim.step(&[Some(frame(0, n)), Some(frame(1, n))]);
    }
    assert_eq!(sim.active().len(), 2);
    assert_hears(&sim.heard_since(0, start), |j| frame(1, j), 150);
    assert_hears(&sim.heard_since(1, start), |j| frame(0, j), 150);

    let mut sim = Sim::secure(1);
    sim.add("x", None);
    for n in 0..100 {
        sim.step(&[None, Some(frame(1, n))]);
    }
    assert_eq!(sim.active(), ["a"]);
}

#[test]
fn devices_file() {
    let devices = parse_devices(
        "# id  KEK\nkitchen 000102030405060708090a0b0c0d0e0f\n\nhall ffffffffffffffffffffffffffffffff # upstairs\n",
    )
    .unwrap();
    assert_eq!(devices["kitchen"][15], 0x0f);
    assert_eq!(devices["hall"], [0xff; 16]);
    assert!(parse_devices("kitchen 0011").is_err());
    assert!(parse_devices("kitchen").is_err());
}

/// A `KEYS` reply delivered again (a slow duplicate, or replayed by someone
/// on the path) changes nothing: the device keeps its keys, its replay
/// window and its jitter buffer, so the audio doesn't skip.
#[test]
fn a_repeated_keys_reply_changes_nothing() {
    let mut sim = Sim::secure(2);
    sim.idle(10);
    let b = sim.nodes[1].addr;
    let keys = sim
        .down
        .iter()
        .find(|(to, d)| *to == b && d.starts_with(b"KEYS"))
        .map(|(_, d)| d.clone())
        .unwrap();
    let start = sim.nodes[1].heard.len();
    for n in 0..300 {
        if n == 100 {
            sim.nodes[1].device.datagram(&mut keys.clone(), sim.now);
        }
        sim.step(&[Some(frame(0, n)), None]);
    }
    assert_hears(&sim.heard_since(1, start), |j| frame(0, j), 280);
}
