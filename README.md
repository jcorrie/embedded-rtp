# embedded-rtp

[![crates.io](https://img.shields.io/crates/v/embedded-rtp.svg)](https://crates.io/crates/embedded-rtp)
[![docs.rs](https://img.shields.io/docsrs/embedded-rtp)](https://docs.rs/embedded-rtp)
[![CI](https://github.com/jcorrie/embedded-rtp/actions/workflows/ci.yml/badge.svg)](https://github.com/jcorrie/embedded-rtp/actions/workflows/ci.yml)
[![MSRV 1.81](https://img.shields.io/badge/MSRV-1.81-blue.svg)](#installation)
[![no_std](https://img.shields.io/badge/no__std-yes-green.svg)](#features)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/embedded-rtp.svg)](#licence)

Real-time audio over RTP for microcontrollers and servers, from one crate.
It is `no_std`, needs no allocator, uses no floating point, has no
dependencies by default and forbids `unsafe`. Besides parsing and writing
packets, it includes the parts that make audio actually play: the RFC 3550
statistics (sequence validation, loss, interarrival jitter), an adaptive
reordering jitter buffer, click-free loss concealment for L16 audio, RTCP
reports with round-trip times, redundancy to recover lost frames exactly,
and optional SRTP to authenticate and encrypt it all.

Other Rust RTP crates either need `std`, need an allocator, or leave the
jitter buffer to you (see [why a new crate](docs/design.md#why-a-new-crate)).
This one builds for `thumbv6m-none-eabi` (RP2040 / Raspberry Pi Pico W,
Cortex-M0+, no FPU) and runs unchanged on a server, so both ends of a link
share the same implementation. Its first user is an
intercom: Pico W devices talking to a server over UDP.

## Contents

- [Features](#features)
- [Status](#status)
- [Installation](#installation)
- [Quick start](#quick-start)
- [RTCP](#rtcp)
- [SRTP](#srtp)
- [Redundancy](#redundancy)
- [Building an intercom](#building-an-intercom)
- [Example: std::net::UdpSocket](#example-stdnetudpsocket)
- [Pico W](#pico-w)
- [Key concepts](#key-concepts)
- [Wire format and interop](#wire-format-and-interop)
- [Memory](#memory)
- [Performance](#performance)
- [Testing and conformance](#testing-and-conformance)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [Releasing](#releasing)
- [Licence](#licence)

## Features

- **`no_std`, no `alloc`, no floats, no `unsafe`, no dependencies.** MSRV is
  Rust 1.81. The optional `srtp` feature adds audited RustCrypto crates,
  which are `no_std` and allocation-free too.
- **Sans-IO.** You pass in datagrams and the current time; the crate never
  touches sockets or clocks. It works the same under embassy, RTIC, tokio or
  a plain loop, and is deterministic under test.
- **Fixed-size state.** Every constructor is a `const fn`, so a `Receiver`,
  `Sender` or `Concealer` can live in a `static`.
- **Zero copy where it counts.** Parsing borrows from the datagram, playout
  borrows from the jitter buffer, and `Sender::send_in_place` writes only the
  header around audio already captured into the packet buffer.

| Module | What it does |
|---|---|
| [`packet`](src/packet.rs) | Parse and write RTP packets (RFC 3550 §5), with CSRCs, header extensions and padding. RTP/RTCP demultiplexing on one port (RFC 5761). |
| [`stats`](src/stats.rs) | RFC 3550 appendix A: sequence validation (A.1), loss (A.3), interarrival jitter (A.8), in integer arithmetic. |
| [`jitter`](src/jitter.rs) | A reordering, adaptive jitter buffer. Tells loss from silence, bounds latency, drains after delay spikes. |
| [`Receiver`](src/receiver.rs) / [`Sender`](src/sender.rs) | The above wired together for one incoming and one outgoing stream. This is what most code uses. |
| [`rtcp`](src/rtcp.rs) | RTCP (RFC 3550 §6): read and write compound packets with SR, RR, SDES and BYE, validated per appendix A.2. Round-trip times from report blocks. |
| [`l16`](src/l16.rs) | Big-endian PCM16 encode/decode (RFC 3551 §4.5.11) and `Concealer`, which turns playout into samples and fades over lost frames without clicks. |
| [`duck`](src/duck.rs) | An echo suppressor for full-duplex devices: `Ducker` turns the microphone down while the speaker plays, with attack, hold and release times. |
| [`red`](src/red.rs) | Redundant audio (RFC 2198): each packet also carries the frame or frames before it, so a lost frame is recovered exactly from the next packet. |
| [`drift`](src/drift.rs) | `DriftCorrector`, after the concealer: follows the sender's clock by dropping or repeating single samples at the flattest point of a frame, instead of the jitter buffer dropping whole frames or running dry. |
| [`srtp`](src/srtp.rs) | SRTP and SRTCP (RFC 3711), `AES_CM_128_HMAC_SHA1_80`, in place on the packet buffer, with replay protection and seamless rekeying. AES key wrap (RFC 5649) to deliver session keys without TLS. Behind the `srtp` feature. |
| [`profile`](src/profile.rs) | Constants for an intercom format: L16, 16 kHz mono, 20 ms frames, payload type 96. Nothing else depends on them. |

## Status

Version 0.1, in active development. The API may still change.

Done:

- packets, RFC 3550 statistics and RTP/RTCP demultiplexing (milestone 1, #1);
- jitter buffer, `Receiver` and `Sender` (milestone 2, #2);
- L16 helpers and click-free loss concealment (#7);
- RTCP: sender and receiver reports, SDES, BYE and round-trip times (#3);
- SRTP and SRTCP, interoperating with ffmpeg (#8);
- redundant audio (RFC 2198), interoperating with GStreamer (#11);
- an intercom: mixing server and desktop client examples, with a server
  guide (#4);
- a Pico W intercom device, built in CI, with per-frame costs counted on
  ARMv6-M under QEMU (#5). It still needs trying on hardware;
- an echo suppressor for full duplex, about 43 instructions a sample on a
  Cortex-M0+ (#9);
- clock drift corrected a sample at a time, instead of by whole frames
  (#10).

Not supported (yet):

- **No key exchange.** SRTP takes keys from you; how they reach both ends is
  up to the application (see [SRTP](#srtp)). End-to-end key agreement for
  direct calls is planned
  ([#15](https://github.com/jcorrie/embedded-rtp/issues/15)).
- **L16 only** for decoding and concealment. The payload is opaque to
  `Receiver` and `Sender`, so you can carry other formats, but G.722 support
  is still to come ([#12](https://github.com/jcorrie/embedded-rtp/issues/12)).
- **Echo suppression, not cancellation.** `duck` turns the microphone
  down while the speaker plays, so talking over each other works less well
  than in a phone call. An adaptive echo canceller is a possible later step
  ([#9](https://github.com/jcorrie/embedded-rtp/issues/9)).
- Frames must all have the same duration
  ([#13](https://github.com/jcorrie/embedded-rtp/issues/13)).
- Out of scope: ICE, DTLS, SDP negotiation, RTCP feedback (NACK, PLI and so
  on), floating-point codecs such as Opus, and multiple streams per
  `Receiver` (keep one `Receiver` per remote stream).

## Installation

From [crates.io](https://crates.io/crates/embedded-rtp):

```sh
cargo add embedded-rtp
cargo add embedded-rtp --features srtp   # with SRTP and SRTCP
```

or in `Cargo.toml`:

```toml
[dependencies]
embedded-rtp = "0.1"
# or, with SRTP:
embedded-rtp = { version = "0.1", features = ["srtp"] }
```

It needs Rust 1.81 or later, and builds for any target, `no_std` ones such
as `thumbv6m-none-eabi` included. API documentation is on
[docs.rs](https://docs.rs/embedded-rtp). While the version is 0.x, a minor
version (0.1 to 0.2) may break the API; see the
[releases](https://github.com/jcorrie/embedded-rtp/releases) for what
changed.

## Quick start

Every 20 ms an audio device does two things: send the frame it just captured,
and play one frame from the far end. In between, it feeds each datagram that
arrives to the `Receiver`. The example below runs both ends in memory, drops
one packet on the way, and shows what the receiver plays for it.

```rust
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::Packet;
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::{Received, Receiver, Sender};

const FRAME_TICKS: u32 = FRAME_SAMPLES as u32; // 320 ticks of the 16 kHz clock = 20 ms

// SSRC, starting sequence number and starting timestamp should be random
// (RFC 3550 §5.1); `core` has no RNG, so you supply them.
let mut sender = Sender::new(0x5eed_1234, PAYLOAD_TYPE, FRAME_TICKS, 4_000, 90_000);
// 8 slots of up to 640 bytes: about 5 KB, enough for 160 ms of buffering.
let mut receiver = Receiver::<8, FRAME_BYTES>::new(PAYLOAD_TYPE, FRAME_TICKS, Config::DEFAULT);
let mut concealer = Concealer::<FRAME_SAMPLES>::new();

let mut wire = [0u8; PACKET_BYTES];
let mut lost = 0;
for tick in 0..50u32 {
    // Send: encode the captured samples as L16 and packetize them.
    let captured = [1_000i16; FRAME_SAMPLES];
    let mut payload = [0u8; FRAME_BYTES];
    l16::encode(&captured, &mut payload);
    let len = sender.send(&payload, &mut wire).unwrap();

    // Network: packet 20 never arrives.
    if tick != 20 {
        let packet = Packet::parse(&wire[..len]).unwrap();
        // Arrival time in RTP clock ticks; only differences matter.
        let arrival = tick * FRAME_TICKS;
        assert_eq!(receiver.receive(&packet, arrival), Received::Accepted);
    }

    // Play: exactly once per frame period.
    let playout = receiver.play();
    if let Playout::Lost { .. } = playout {
        lost += 1;
    }
    if let Some(samples) = concealer.play(playout) {
        // Hand `samples` (&[i16; 320]) to the DAC, I2S or mixer.
        assert_eq!(samples.len(), FRAME_SAMPLES);
    }
}
assert_eq!(lost, 1);
assert_eq!(receiver.counters().concealed, 1);
```

### Receiving

`Receiver::receive(&packet, arrival)` checks the payload type, tracks the
SSRC, updates the statistics and buffers the frame. It returns a `Received`
saying what happened (`Accepted`, `Late`, `Duplicate`, `WrongPayloadType`,
...); dropped packets are counted, so you can usually ignore it.

`Receiver::play()` returns a `Playout<'_>` that borrows the frame from the
jitter buffer. `Concealer::play` decodes it into samples, conceals a `Lost`
frame from the last good one, and returns `None` once there is nothing to
play. Statistics are available at any time:

```rust
use embedded_rtp::jitter::Config;
use embedded_rtp::Receiver;

let mut receiver = Receiver::<8, 640>::new(96, 320, Config::DEFAULT);
// ... receive and play ...
let stats = receiver.stats();
let _jitter_ticks = stats.jitter(); // interarrival jitter, RTP clock ticks
let _depth = receiver.buffer().depth(); // frames of latency the buffer adds now
let _target = receiver.buffer().target(); // the depth it is aiming for
// RTCP report block figures; starts a new reporting interval.
assert!(receiver.report().is_none()); // nothing received yet
```

### Sending

`Sender::send(payload, &mut buf)` writes header and payload into `buf` and
returns the packet length. If you can capture audio straight into the packet
buffer (on a Pico, by DMA), `send_in_place` writes only the 12-byte header in
front of it and saves the copy. When there is nothing to send (silence
suppression), call `skip()` instead, once per silent frame period:

```rust
use embedded_rtp::l16;
use embedded_rtp::packet::{Packet, HEADER_LEN};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::Sender;

let mut sender = Sender::new(0x5eed_1234, PAYLOAD_TYPE, FRAME_SAMPLES as u32, 0, 0);
let mut buf = [0u8; PACKET_BYTES];

// Capture into the buffer after the header, then write only the header.
let samples = [0i16; FRAME_SAMPLES];
l16::encode(&samples, &mut buf[HEADER_LEN..]);
let len = sender.send_in_place(&mut buf, FRAME_BYTES).unwrap();
assert_eq!(len, PACKET_BYTES);

// Two silent frame periods: nothing is sent.
sender.skip();
sender.skip();

// The next packet keeps the sequence going, jumps the timestamp by the
// silence, and sets the marker bit to start a new talkspurt.
let len = sender.send_in_place(&mut buf, FRAME_BYTES).unwrap();
let header = Packet::parse(&buf[..len]).unwrap().header;
assert_eq!(header.sequence, 1);
assert_eq!(header.timestamp, 3 * FRAME_SAMPLES as u32);
assert!(header.marker);
```

## RTCP

RTCP travels on the same port as RTP (`packet::is_rtcp` tells them apart).
Every few seconds each side sends a compound packet: a sender report (SR) if
it is sending, otherwise a receiver report (RR), plus an SDES with its CNAME
and optionally a display NAME. Report blocks tell the other side how its
stream is arriving, and the round trip falls out of the SR timestamps a
receiver echoes back. A BYE (`Writer::goodbye`) says a source is leaving.

`Compound::parse` checks the whole datagram against RFC 3550 appendix A.2
first, so iterating over it cannot fail. `Writer` appends packets into your
buffer, each one all or nothing. Times are NTP short format (1/65536 s),
and each side only needs its own consistent clock; the clocks don't need to
agree.

```rust
use embedded_rtp::jitter::Config;
use embedded_rtp::packet::Packet;
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::rtcp::{self, sdes, Compound, Writer};
use embedded_rtp::{Receiver, Sender};

// NTP times from a local microsecond clock, in full and short form.
let ntp = rtcp::ntp_from_micros;
let short = |micros| rtcp::ntp_short(rtcp::ntp_from_micros(micros));

let mut sender = Sender::new(0x5eed_1234, PAYLOAD_TYPE, FRAME_SAMPLES as u32, 0, 0);
let mut receiver = Receiver::<8, FRAME_BYTES>::new(PAYLOAD_TYPE, FRAME_SAMPLES as u32, Config::DEFAULT);
let mut wire = [0u8; PACKET_BYTES];
for i in 0..50u32 {
    let len = sender.send(&[0; FRAME_BYTES], &mut wire).unwrap();
    receiver.receive(&Packet::parse(&wire[..len]).unwrap(), i * FRAME_SAMPLES as u32);
}

// Sender, at 10.0 s on its clock: a sender report plus its CNAME.
let mut buf = [0u8; 256];
let mut writer = Writer::new(&mut buf);
writer.sender_report(sender.ssrc(), &sender.sender_info(ntp(10_000_000)), &[]).unwrap();
writer.source_description(sender.ssrc(), &[(sdes::CNAME, b"kitchen@example")]).unwrap();
let sr = writer.finish().to_vec();

// Receiver, at 3.1 s on its own clock: note the SR...
for packet in Compound::parse(&sr).unwrap().packets() {
    if let rtcp::Packet::SenderReport { ssrc, info, .. } = packet {
        receiver.sender_report(ssrc, &info, short(3_100_000));
    }
}
// ...and at 3.6 s report back, echoing it.
let block = receiver.report_block(short(3_600_000)).unwrap();
let mut buf = [0u8; 256];
let mut writer = Writer::new(&mut buf);
writer.receiver_report(0x0bad_cafe, &[block]).unwrap();
writer
    .source_description(0x0bad_cafe, &[(sdes::CNAME, b"hall@example"), (sdes::NAME, b"Hallway")])
    .unwrap();
let rr = writer.finish().to_vec();

// Sender, at 10.7 s: 0.7 s since its SR, of which the receiver held it for
// 0.5 s, so the round trip is 0.2 s.
for packet in Compound::parse(&rr).unwrap().packets() {
    match packet {
        rtcp::Packet::ReceiverReport { reports, .. } => {
            for block in reports.filter(|b| b.ssrc == sender.ssrc()) {
                assert_eq!((block.extended_highest, block.cumulative_lost), (49, 0));
                let rtt = rtcp::round_trip(short(10_700_000), block.last_sr, block.delay_since_last_sr);
                assert!((13_106..=13_108).contains(&rtt.unwrap())); // 0.2 × 65536
            }
        }
        rtcp::Packet::SourceDescription(chunks) => {
            for chunk in chunks {
                assert_eq!(chunk.name(), Some(&b"Hallway"[..]));
            }
        }
        _ => {}
    }
}
```

## SRTP

With the `srtp` feature, `srtp::Outbound` protects what you send and
`srtp::Inbound` checks and decrypts what you receive, both in place on the
packet buffer. The suite is `AES_CM_128_HMAC_SHA1_80` (RFC 3711's default,
and what ffmpeg and SDES use): AES-128 in counter mode plus an 80-bit
HMAC-SHA1 tag on every packet. RTP grows by 10 bytes and RTCP by 14, so size
buffers with `srtp::RTP_OVERHEAD` and `srtp::RTCP_OVERHEAD`.

A packet reaches the jitter buffer only once its tag checks out. Replays and
packets more than 64 behind the newest are rejected (RFC 3711 §3.3.2), and a
rejected packet changes no state, so forged traffic can't disturb a stream.
Each context belongs to one stream: one SSRC, with its RTP and its RTCP.

```rust
# #[cfg(feature = "srtp")] {
use embedded_rtp::jitter::Config;
use embedded_rtp::packet::Packet;
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE};
use embedded_rtp::srtp::{self, Inbound, MasterKey, Outbound};
use embedded_rtp::{Receiver, Sender};

// A fresh key for this session, from a random number generator.
let key = MasterKey::from_bytes(&[0x2a; 30]);

let mut sender = Sender::new(0x5eed_1234, PAYLOAD_TYPE, FRAME_SAMPLES as u32, 0, 0);
let mut outbound = Outbound::new(&key);
let mut inbound = Inbound::new(&key);
let mut receiver = Receiver::<8, FRAME_BYTES>::new(PAYLOAD_TYPE, FRAME_SAMPLES as u32, Config::DEFAULT);

// Sending: write the packet, then protect it in place.
let mut wire = [0u8; PACKET_BYTES + srtp::RTP_OVERHEAD];
let len = sender.send(&[0; FRAME_BYTES], &mut wire).unwrap();
let len = outbound.protect_rtp(&mut wire, len).unwrap();

// Receiving: unprotect in place, then parse what's left.
let datagram = &mut wire[..len];
match inbound.unprotect_rtp(datagram) {
    Ok(len) => {
        receiver.receive(&Packet::parse(&datagram[..len]).unwrap(), 0);
    }
    Err(_) => { /* forged, replayed or under another key: drop it */ }
}
# }
```

### Keys

A `MasterKey` is the 16-byte key and 14-byte salt that SDP's
`a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:...` line carries in base64. The
crate doesn't care where it came from, but **use a fresh master key for every
session**. Counter mode derives each packet's keystream from the key, the
SSRC and the packet index. A device that keeps one key in flash and restarts
its sequence numbers after a reboot reuses keystream, which leaks the XOR of
two stretches of audio.

What works well for a fleet of devices and a server:

1. **A long-term secret per device**, used only to prove to the server who
   the device is. It never encrypts audio.
2. **Session keys from the server** when the device connects, or when a call
   starts for direct device-to-device calls. Either send them over an
   authenticated TLS connection, or, for devices without TLS, wrap them
   under a key derived from the device secret with `srtp::wrap` and
   `srtp::unwrap` (AES key wrap, RFC 5649).

A server that issues keys can decrypt the calls it keys, which a mixing
server needs to anyway. End-to-end keys that the server never sees are
[#15](https://github.com/jcorrie/embedded-rtp/issues/15).

`rekey` on both contexts switches to a new key mid-stream: the receiver keeps
accepting the old key until the first packet under the new one, so audio
isn't interrupted. Both ends must start a context together, because the
rollover counter starts at zero; that's the normal case with fresh keys per
session.

## Redundancy

Wi-Fi tends to lose single packets or short bursts, and each loss becomes a
concealed frame. With redundancy (RFC 2198), each packet also carries the
frame before it, or the two before it, so the receiver recovers a lost
frame exactly from the next packet. Redundant packets get their own payload
type, so a peer that doesn't use them is unaffected. The sender pairs a
`red::Encoder` with a `Sender` on that payload type; the receiver is set up
`with_redundancy`, and takes plain packets too:

```rust
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::packet::{Packet, HEADER_LEN};
use embedded_rtp::profile::{FRAME_BYTES, FRAME_SAMPLES, PAYLOAD_TYPE};
use embedded_rtp::red::Encoder;
use embedded_rtp::{Receiver, Sender};

const RED: u8 = 97; // a=rtpmap:97 red/16000/1
const TICKS: u32 = FRAME_SAMPLES as u32;
// Depth 1: each packet carries one earlier frame.
type RedEncoder = Encoder<1, FRAME_BYTES>;

let mut encoder = RedEncoder::new(PAYLOAD_TYPE, TICKS);
let mut sender = Sender::new(0x1234_5678, RED, TICKS, 0, 0);
// Wait one frame longer for the redundant copy to arrive.
let mut receiver: Receiver<8, FRAME_BYTES> =
    Receiver::new(PAYLOAD_TYPE, TICKS, Config::DEFAULT).with_redundancy(RED, 1);

let mut buf = [0u8; HEADER_LEN + RedEncoder::MAX_PAYLOAD];
for i in 0..4u8 {
    let frame = [i; FRAME_BYTES];
    let len = encoder.encode(sender.timestamp(), &frame, &mut buf[HEADER_LEN..])?;
    let len = sender.send_in_place(&mut buf, len)?;
    // The network loses the second packet.
    if i != 1 {
        receiver.receive(&Packet::parse(&buf[..len])?, u32::from(i) * TICKS);
    }
}
assert_eq!(receiver.counters().recovered, 1);
for i in 0..4u8 {
    assert!(matches!(receiver.play(), Playout::Frame { payload, .. } if payload[0] == i));
}
# Ok::<(), embedded_rtp::Error>(())
```

The costs, all configurable:

- **Bandwidth.** Depth 1 sends every frame twice: 1,297-byte packets, about
  520 kbit/s each way for the intercom profile instead of 260. Depth 2
  sends it three times, and recovers bursts of two.
- **Latency.** A lost frame can only be recovered if the next packet
  arrives before its turn to play, so the receiver waits `wait` frames
  longer (20 ms each) while redundant packets arrive. With a wait of 0, it
  recovers only what happens to arrive in time.
- **CPU.** Redundancy itself is cheap, about 10,000 ARMv6-M instructions a
  frame. SRTP is not: its cost grows with the packet, so depth 1 roughly
  doubles it, about another 280,000 instructions a frame both ways on a
  Pico. On a server it is about 0.04 µs a frame more without SRTP.

In a simulation with 5% independent loss, depth 1 cuts the frames lost to
the listener by about 95%; what it can't recover is a loss whose next
packet is lost too. With bursts of two, depth 2 recovers about 97%, all but
bursts that run into each other.

Frames are recovered by sequence number: a block `k` frames back on the
media clock is taken to be `k` packets back. `red::Encoder` only carries
frames for which that is true, so never across a silence. With a sender
that does carry frames across a silence, a recovered frame could land in
the wrong place.

## Building an intercom

The crate was written for an intercom: devices and a server, each device
hearing everyone but itself. The pieces fit together like this.

**Format.** Use `profile`: L16, 16 kHz mono, 20 ms frames (640 bytes),
payload type 96, RTP and RTCP on one UDP port.

**Device, every 20 ms tick:**

1. Capture 320 samples into the packet buffer after the 12-byte header,
   then `Sender::send_in_place` and `Outbound::protect_rtp`. Send.
   Nothing to say? Call `Sender::skip` and send nothing. On a full-duplex
   device, capture into an `i16` frame instead and run it through
   `Ducker::process` with what the speaker played, which turns down the
   echo the other end would hear; then `l16::encode` it into the packet
   buffer and carry on as above. (Push-to-talk devices, which mute the
   speaker while sending, don't need it.)
2. Play `corrector.play(&mut receiver, &mut concealer)`, or plain
   `concealer.play(receiver.play())` without the `DriftCorrector`. `None`
   means silence; keep the output clock running either way. The corrector
   follows the sender's clock a sample at a time, so the two crystals'
   drift never costs a dropped frame or a gap.

**Device, on each datagram:**

- `packet::is_rtcp`: `Inbound::unprotect_rtcp`, then `Compound::parse`.
- Otherwise: `Inbound::unprotect_rtp`, `Packet::parse`, then
  `Receiver::receive`. Drop anything that fails.

**Device, every few seconds:** an RR (or SR while talking) plus SDES
CNAME and NAME, through `Outbound::protect_rtcp`. It doubles as the
keep-alive. Send a BYE on shutdown.

**Server, per participant:** a `Receiver`, `Concealer` and `Inbound` for
what it sends; a `Sender` and `Outbound` for what it hears.

**Server, every 20 ms tick:**

1. Play every participant's concealer and sum all as `i32`.
2. For each participant, subtract its own samples ("mix minus"),
   clamp to `i16`, `l16::encode` into its packet and send. If the result
   is silent, `skip` instead.

**Keys.** The server makes fresh SRTP keys per participant when it
connects, and delivers them over TLS or with `srtp::wrap`. Never one fixed
key per device: see [SRTP](#srtp).

**Sizing.** About 6 KB of RAM per device for RTP (8-slot receiver), plus
about 3 KB for SRTP. 16 slots on the server.

**Lossy Wi-Fi.** [Redundancy](#redundancy) recovers lost frames exactly,
for twice the bandwidth and a frame more latency. The example server and
devices don't use it yet.

All of this, runnable: a mixing server and a desktop client, with SRTP and
a join handshake that delivers fresh keys. See
[docs/server.md](docs/server.md) to try it:

```sh
cargo run --release --features srtp --example mixing_server -- \
    --devices devices.txt
cargo run --release --features srtp --example intercom_client -- \
    --server 127.0.0.1:5004 --id kitchen --kek <hex KEK>
```

The [Pico W](#pico-w) device joins the same server.

## Example: std::net::UdpSocket

A complete sender and receiver over UDP on a server or desktop. The receiver
drains the socket without blocking, plays one frame per 20 ms tick, and
notes sender reports arriving on the shared port so its own reports can echo
them (sending those is as in [RTCP](#rtcp)). It is marked `no_run` because it opens sockets and
runs forever; it is still compiled as a doctest.

```rust,no_run
use std::io::ErrorKind;
use std::net::UdpSocket;
use std::thread;
use std::time::{Duration, Instant};

use embedded_rtp::jitter::Config;
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::{self, Packet};
use embedded_rtp::rtcp::{self, Compound};
use embedded_rtp::profile::{
    CLOCK_RATE, FRAME_BYTES, FRAME_SAMPLES, PACKET_BYTES, PAYLOAD_TYPE,
};
use embedded_rtp::{Receiver, Sender};

const FRAME: Duration = Duration::from_millis(20);
const FRAME_TICKS: u32 = FRAME_SAMPLES as u32;

/// Local time in RTP clock ticks. Truncating to u32 wraps, which is fine:
/// only differences matter.
fn rtp_now(start: Instant) -> u32 {
    (start.elapsed().as_micros() * u128::from(CLOCK_RATE) / 1_000_000) as u32
}

/// Local time in RTCP's NTP short format, on the same clock.
fn ntp_now(start: Instant) -> u32 {
    rtcp::ntp_short(rtcp::ntp_from_micros(start.elapsed().as_micros() as u64))
}

fn send(socket: UdpSocket, mut capture: impl FnMut(&mut [i16; FRAME_SAMPLES]) -> bool) {
    // Use random values here in real code.
    let mut sender = Sender::new(0x1234_5678, PAYLOAD_TYPE, FRAME_TICKS, 0, 0);
    let mut samples = [0i16; FRAME_SAMPLES];
    let mut buf = [0u8; PACKET_BYTES];
    let mut next = Instant::now();
    loop {
        if capture(&mut samples) {
            l16::encode(&samples, &mut buf[packet::HEADER_LEN..]);
            let len = sender.send_in_place(&mut buf, FRAME_BYTES).unwrap();
            socket.send(&buf[..len]).unwrap();
        } else {
            sender.skip(); // silence: send nothing, keep the clock running
        }
        next += FRAME;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

fn receive(socket: UdpSocket, mut output: impl FnMut(Option<&[i16; FRAME_SAMPLES]>)) {
    socket.set_nonblocking(true).unwrap();
    let mut receiver = Receiver::<16, FRAME_BYTES>::new(PAYLOAD_TYPE, FRAME_TICKS, Config::DEFAULT);
    let mut concealer = Concealer::<FRAME_SAMPLES>::new();
    let mut buf = [0u8; 1500];
    let start = Instant::now();
    let mut next = start;
    loop {
        // Take everything that has arrived since the last tick.
        loop {
            match socket.recv(&mut buf) {
                Ok(len) => {
                    let datagram = &buf[..len];
                    if packet::is_rtcp(datagram) {
                        // Remember sender reports for our next report block.
                        if let Ok(compound) = Compound::parse(datagram) {
                            for packet in compound.packets() {
                                if let rtcp::Packet::SenderReport { ssrc, info, .. } = packet {
                                    receiver.sender_report(ssrc, &info, ntp_now(start));
                                }
                            }
                        }
                        continue;
                    }
                    if let Ok(packet) = Packet::parse(datagram) {
                        receiver.receive(&packet, rtp_now(start));
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        // One frame per tick: audio, concealment, or None for silence.
        output(concealer.play(receiver.play()));

        next += FRAME;
        thread::sleep(next.saturating_duration_since(Instant::now()));
    }
}

fn main() -> std::io::Result<()> {
    let rx = UdpSocket::bind("127.0.0.1:5004")?;
    let tx = UdpSocket::bind("127.0.0.1:0")?;
    tx.connect("127.0.0.1:5004")?;

    // A 500 Hz square wave, talking for one second in every two.
    let mut frame = 0u32;
    thread::spawn(move || {
        send(tx, |samples| {
            frame += 1;
            for (i, s) in samples.iter_mut().enumerate() {
                *s = if (i / 16) % 2 == 0 { 4_000 } else { -4_000 };
            }
            frame % 100 < 50
        })
    });

    receive(rx, |samples| match samples {
        Some(samples) => println!("play {} samples", samples.len()),
        None => println!("silence"),
    });
    Ok(())
}
```

Waking up exactly every 20 ms is the caller's job. On a server, drive
`play()` from the audio device's callback or the mixer's tick rather than
`thread::sleep`, so playout follows the output clock.

## Pico W

[`examples/pico-w`](examples/pico-w) is a complete intercom device for a
Raspberry Pi Pico W, built on [embassy](https://embassy.dev). It joins the
mixing server over Wi-Fi with SRTP and talks while BOOTSEL is held. Audio
comes from an INMP441 I2S microphone and goes out to a MAX98357A amplifier.
Its protocol code is the same `Device` the desktop client and the tests
run. It builds in CI but has not yet been tried on hardware.

What a device does each frame costs about 280,000 ARMv6-M instructions,
both directions with SRTP, counted under QEMU in CI
([`examples/m0-cycles`](examples/m0-cycles)). That's roughly 15 to 18% of
a 125 MHz Cortex-M0+, almost all of it AES and SHA-1. Without SRTP it is
under 5,000. `cargo run --release --bin cycles` in `examples/pico-w`
measures real cycles on a board.

## Key concepts

**Sans-IO and the arrival clock.** The crate never reads a clock. Pass each
packet's local arrival time to `Receiver::receive` in **RTP clock ticks**
(for L16 the clock rate is the sample rate, so 16 ticks per millisecond at
16 kHz), from any origin. Only differences are used, and `u32` wraparound is
handled, so a truncated microsecond counter scaled to ticks is enough. The
arrival times feed the RFC 3550 jitter estimate, which sizes the buffer.

**Playout.** Call `Receiver::play()` exactly once per frame period. It returns:

- `Playout::Frame { sequence, payload }`: the next frame, borrowed from the
  buffer (not copied). The borrow ends before the next `receive`.
- `Playout::Lost { sequence }`: that frame never arrived but later ones did.
  Conceal it; `Concealer` repeats the last good frame, fading to half over the
  first loss and to silence over the second.
- `Playout::Silence`: nothing to play. The buffer is prefilling, or the
  sender has stopped sending. `Concealer` fades out whatever was playing.

The buffer tells loss from silence without timestamps: a gap with later
frames behind it is loss; an empty buffer is silence.

**Silence suppression.** A sender that sends nothing during silence calls
`Sender::skip()` once per silent frame period. The timestamp keeps advancing,
the sequence number does not, and the first packet of the next talkspurt
carries the marker bit, as RFC 3551 §4.1 asks. Receivers see an empty buffer,
play `Silence`, and prefill again when audio resumes; this is never counted
as loss.

**Jitter buffer `Config`.** Depths are in frames:

```rust
use embedded_rtp::jitter::Config;

// The default: start at 2 frames (40 ms at 20 ms per frame), adapt between
// 1 and 8 (20-160 ms).
assert_eq!(
    Config::DEFAULT,
    Config { initial_depth: 2, min_depth: 1, max_depth: 8 }
);
```

After 16 packets the target follows the measured jitter J:
`clamp(1 + ceil(3·J / frame), min_depth, max_depth)`. A packet more than
`max_depth` frames ahead pushes the play point forward (bounding latency),
and if the buffer stays more than two frames above target for about half a
second, one frame is dropped to bring latency back down. `max_depth` must not
exceed `SLOTS`, which must be a power of two; a bad configuration in a
`const` or `static` is a compile error. `receiver.counters()` reports frames
played, concealed, late, duplicated, drained and more.

## Wire format and interop

The crate is generic over payload type, clock rate and frame size. The
`profile` module holds the format the intercom uses, which standard
tools decode as is:

| | |
|---|---|
| Payload | L16: signed 16-bit PCM, **big endian**, mono |
| Clock / sample rate | 16,000 Hz |
| Packet time | 20 ms: 320 samples, 640 bytes, timestamp step 320 |
| Payload type | 96 (dynamic) |
| Packet size | 652 bytes (12-byte header, no CSRCs or extension) |
| RTCP | same port as RTP (RFC 5761) |
| Silence | no packets; the next has the marker bit set and a timestamp gap |

In SDP:

```text
m=audio 5004 RTP/AVP 96
a=rtpmap:96 L16/16000/1
a=ptime:20
```

With SRTP, the profile becomes `RTP/SAVP` and a crypto line carries the key
(base64 of the 30 bytes from `MasterKey::to_bytes`):

```text
m=audio 5004 RTP/SAVP 96
a=rtpmap:96 L16/16000/1
a=ptime:20
a=crypto:1 AES_CM_128_HMAC_SHA1_80 inline:<base64 key and salt>
```

With [redundancy](#redundancy), the redundant payload type is listed
first, with the format it carries:

```text
m=audio 5004 RTP/AVP 97 96
a=rtpmap:97 red/16000/1
a=fmtp:97 96/96
a=rtpmap:96 L16/16000/1
a=ptime:20
```

For example, to listen to a stream sent to port 5004, save a complete SDP
file (adding `v=0`, `o=`, `s=`, `c=IN IP4 127.0.0.1` and `t=0 0` lines) as
`stream.sdp` and run:

```sh
ffplay -protocol_whitelist file,udp,rtp -i stream.sdp
```

## Memory

Measured with `size_of` on a 64-bit host; the 32-bit RP2040 is the same or
slightly smaller.

| Item | Bytes |
|---|---|
| `Receiver<16, 640>` (server) | 10,488 |
| `Receiver<8, 640>` (Pico) | 5,320 |
| `Concealer<320>` | 1,292 (two frames of samples) |
| `SourceStats` | 40 |
| `Sender` | 24 |
| Packet buffer | 652 (662 with SRTP) |
| `red::Encoder<1, 640>` | 664 (one frame); packet buffer 1,297 |
| `srtp::Outbound` | 1,760 (1,040 on the RP2040) |
| `srtp::Inbound` | 3,536 (2,080 on the RP2040; half holds the old key during a rekey) |

A full-duplex Pico client (8-slot receiver, sender, one packet buffer) needs
about 6 KB of the RP2040's 264 KB, plus about 1.3 KB for a concealer. A jitter
buffer takes roughly `SLOTS × (FRAME + 4)` bytes. SRTP adds about 3 KB
for one stream each way.

## Performance

Host figures on an x86-64 VM; see
[docs/design.md#performance](docs/design.md#performance) for the breakdown,
the optimizations behind them and the latency budget.

| Operation | Time |
|---|---|
| One frame, one direction: send, parse, receive, play | ~43 ns (381 instructions) |
| Receive side only: parse, receive, play | ~41 ns |
| `l16::decode`, 640 bytes to 320 samples | 27 ns (15 ns with AVX2) |
| `Concealer::play`, no loss | ~30 ns (23 ns with AVX2) |
| A lost frame, then audio again | ~330 ns (124 ns with AVX2) |
| RTCP: write SR + report block + SDES / parse it | ~21 ns / ~42 ns |
| SRTP: protect a frame / protect and unprotect | ~0.67 µs / ~1.3 µs |
| SRTP: reject a forged frame | ~0.55 µs |
| One frame, one direction, with SRTP | ~1.3 µs |
| Redundancy, depth 1: encode / parse | ~35 ns / ~16 ns |

The hot path makes one payload copy (into the jitter buffer) and no division,
which matters on the Cortex-M0+: it has no divide instruction. Processing is
a negligible part of the delay a listener hears. That comes from
packetization (one 20 ms frame), the jitter buffer (`target` × 20 ms, 40 ms
to start) and the network.

On a server, build with `-C target-cpu=x86-64-v3` (AVX2) so concealment
vectorizes.

SRTP dominates once it's on. On a server that's mostly HMAC-SHA1, even
with the CPU's SHA instructions, and still only about 0.013% of a core per
stream. On a Cortex-M0+, both AES and SHA-1 run in software. A device frame,
both directions with SRTP, is about 280,000 instructions (see
[Pico W](#pico-w)), of which AES is about 80%.

```sh
# Criterion benchmarks
cargo bench --bench hot_path
cargo bench --bench hot_path -- --save-baseline before   # then, after a change:
cargo bench --bench hot_path -- --baseline before
cargo bench --features srtp --bench srtp

# Deterministic per-line instruction counts with Callgrind
cargo build --release --example profile
valgrind --tool=callgrind --callgrind-out-file=callgrind.out \
    target/release/examples/profile
callgrind_annotate --auto=yes callgrind.out
```

## Testing and conformance

The tests check the crate against something independent of it, not just its
own writer against its own reader, since a misreading shared by both would
pass:

- **RFC diagrams.** Packets are assembled byte by byte from the RFC 3550 §5.1
  header diagram in every combination of padding, extension and CSRCs, plus
  random ones. Every truncation and bit flip of valid packets, and 100,000
  random inputs, must never panic or return a slice outside the input.
  RTP/RTCP demultiplexing is checked for every payload type.
- **Redundancy.** Redundant payloads (RFC 2198) checked against payloads
  assembled from the RFC's diagram, every field at its extremes. In a
  simulated lossy network that knows which frames were lost, depth 1
  recovers over 90% of independent losses and depth 2 recovers bursts of
  two, with every frame played checked to be the right one.
- **Simulations with ground truth.** A simulated network drops, duplicates
  and reorders packets across several sequence number wraps; expected loss
  figures come from what it actually delivered. Jitter is checked against a
  floating-point evaluation of the RFC formula, within a proven bound. The
  jitter buffer runs with drifting clocks, delay spread, stalls and silence
  suppression, and every decision it makes is checked against what really
  arrived. The drift corrector runs for a simulated hour at up to 200 ppm:
  no drains or underruns, and its net correction matches the clocks' real
  difference.
- **Interop.** ffmpeg's packets parse to the exact audio it was given (also
  checked from a captured fixture in the default run); ffmpeg decodes our
  streams, with silences and wraparound, back to the exact audio; GStreamer's
  RTCP receiver reports about our stream agree with our statistics.
  With redundancy, GStreamer's `rtpreddec` recovers frames lost from our
  stream, and we recover every frame lost from its `rtpredenc` stream.
- **RTCP.** Packets are checked byte for byte against vectors written from
  the RFC 3550 §6.4–§6.6 diagrams, and each appendix A.2 validity rule is
  tested on its own. The round-trip arithmetic is checked against the RFC's
  worked example. ffmpeg's real RTCP parses, and its final sender report
  counts exactly the stream it sent. GStreamer accepts our sender reports:
  it echoes them in its receiver reports, giving a loopback round trip
  under 100 ms.
- **SRTP.** Packets protected byte for byte as libsrtp's own known-answer
  test does it, for RTP and SRTCP; the RFC 3711 appendix B keystream and key
  derivation vectors; key wrap against OpenSSL. ffmpeg decrypts our stream to
  the exact audio, and we decrypt ffmpeg's RTP and SRTCP, both across the
  sequence number wrap. Every single-bit change to a packet is rejected, as
  are wrong keys, replays and packets outside the window.
- **Concealment.** On a sine wave with random losses and dry spells at every
  phase, no sample step exceeds the designed bound.
- **Planted bugs.** Bugs planted by hand (wrong byte order, off-by-one
  limits, wrong jitter gain and so on) were caught by the suite.
- **Fuzzing.** `cargo fuzz` targets for the RTP, RTCP and redundant
  payload readers check that they never panic and keep every slice inside
  the input. Header fields, every SR, RR, SDES and BYE, and every
  redundant payload must also write back the same. They run in CI on pull requests touching
  the readers, and weekly.

```sh
cargo test --all-features                                               # everything except interop
cargo test --all-features --test interop -- --ignored --test-threads=1  # needs ffmpeg and GStreamer
cargo build --release --target thumbv6m-none-eabi        # the embedded build
cd fuzz && cargo +nightly fuzz run rtcp_compound          # or rtp_packet, red_payload; needs cargo-fuzz
```

The interop tests are Unix only. On Debian or Ubuntu they need
`ffmpeg gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good`.
The code blocks in this README are compiled as doctests. See
[docs/design.md#testing](docs/design.md#testing) for the full list.

## Roadmap

From the [milestones](docs/design.md#milestones) in the design document:

| Issue | | Priority |
|---|---|---|
| [#12](https://github.com/jcorrie/embedded-rtp/issues/12) | G.722 codec | Low until bandwidth matters |
| [#13](https://github.com/jcorrie/embedded-rtp/issues/13) | Packets of varying duration | Low |
| [#14](https://github.com/jcorrie/embedded-rtp/issues/14) | Talker information (CSRC, levels) | Low |
| [#15](https://github.com/jcorrie/embedded-rtp/issues/15) | End-to-end key agreement for direct calls | Low until needed |
| [#9](https://github.com/jcorrie/embedded-rtp/issues/9) | Adaptive echo cancellation (suppression is done) | Low until needed |

[docs/design.md](docs/design.md) explains the design: why existing crates
don't fit, the algorithms, the memory budget, server integration and
the decisions taken.

## Contributing

Issues and pull requests are welcome. CI runs, and a pull request needs to
pass:

```sh
cargo fmt --check
RUSTFLAGS="-D warnings" cargo clippy --all-targets               # and with --all-features
cargo test                                                       # and with --all-features, on stable and 1.81 (MSRV)
cargo build --release --target thumbv6m-none-eabi --all-features  # no std, no allocator
cargo test --all-features --test interop -- --ignored --test-threads=1
```

Keep the library free of dependencies (optional, audited ones aside),
allocation, floating point and `unsafe`, and keep constructors `const` (SRTP
contexts are the exception: they derive keys). New behaviour should be tested
against an independent reference (an RFC, a simulation that knows the right
answer, or another implementation) rather than by round-tripping through the
crate's own code.

## Releasing

Merging a version bump releases it. In a pull request, bump `version` in
`Cargo.toml` (0.x: a minor bump for breaking changes, a patch bump
otherwise) and update the lock files with `cargo update -p embedded-rtp` in
the root, `fuzz/` and `examples/pico-w/`. When it merges,
`.github/workflows/release.yml` sees a version on `main` with no tag yet,
runs the whole of CI, publishes to crates.io, then tags the commit
(`v0.2.0`) and makes a GitHub release with notes from the merged pull
requests. A version with a pre-release part (`0.2.0-rc.1`) makes a
pre-release.

If a release fails part way, rerun it from the Actions tab (or with "Run
workflow" on the release workflow): a version already on crates.io isn't
published twice, so the rerun goes on to the tag and release.

crates.io publishes through Trusted Publishing, so no token is stored. Its
settings for the crate name this repository, the workflow `release.yml` and
the environment `release`.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT licence](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
licence, shall be dual licensed as above, without any additional terms or
conditions.
