# embedded-rtp: design

Status: **accepted**. Implementation in progress.

## Summary

A small RTP/RTCP crate with no dependencies by default, written for microcontrollers and
usable unchanged on a server. It covers:

- reading and writing packets;
- the statistics RFC 3550 defines (sequence tracking, loss, interarrival
  jitter);
- the RTCP basics;
- a jitter buffer;
- optionally, SRTP and SRTCP to authenticate and encrypt them.

It is `no_std`, needs no allocator and uses no floating point. Everything is
fixed size and can live in a `static`.

The first user is an intercom: Pico W devices and a server share this crate
for the UDP audio path, so both ends run the same code.

## Why a new crate

Nothing on crates.io (surveyed October 2026) is `no_std` *and* runs without
an allocator *and* includes a jitter buffer.

### Packet parsing

| Crate | Size | no_std | Notes |
|---|---|---|---|
| `rtp-rs` 0.6.0 | ~1.0k lines | ✗ | Its only `std` uses (`fmt`, `ops`, `cmp`, integer limits) all exist in `core`, so the port would be mechanical. Unmaintained since 2021. RTP only, no RTCP. |
| `rtp-types` 0.1.2 | ~1.8k lines | ✗ | Borrows packets instead of copying them; good API. Its `std` uses map to `core` apart from `thiserror` 1.x and a `Vec` writer. |
| `rtcp-types` 0.3.0 | ~8.2k lines | ✗ | Thorough RTCP, including all the WebRTC feedback messages. Much more than needed here. |
| `rtp-packet` / `rtcp-packet` | ~2.8k lines | ✓, needs an allocator | Copies CSRCs into a `Vec` on every parse. Very new, single author. |
| `discortp` 0.6.0 | — | ✓, needs an allocator | Built for Discord voice; uses `pnet` macros. |
| `rtp` / `rtc-rtp` (webrtc-rs), `str0m` | large | ✗ | Full WebRTC stacks. |

### Jitter buffers

| Crate | no_std | Notes |
|---|---|---|
| `neteq` 0.9.1 | ✗ | A copy of WebRTC's audio playout engine, ~8.4k lines. Heavy `f32` use; the Cortex-M0+ has no FPU. Default features pull in tokio, cpal and axum. |
| `jittr` 0.2.0 | ✗ | Built on `futures-timer`. Last release January 2023. |
| `rvoip-rtp-core`, `rtp-engine`, `rtp-core` | ✗ | Each is part of a full VoIP stack. |

### Why the gap exists

- Almost all RTP runs on a full OS (WebRTC, SIP, streaming), so authors have
  no reason to avoid `std`.
- Embedded RTP is usually written in C: Espressif's audio framework, Zephyr,
  lwIP, pjmedia.
- The fixed RTP header is 12 bytes, so embedded projects tend to write it
  inside their firmware instead of publishing a crate.
- A jitter buffer depends on frame size, clock rate, codec and latency budget.
  A general one grows into something like NetEQ; a specific one is ~200 lines
  nobody publishes.

Forking `rtp-rs` would save the ~150 lines of packet code. The statistics,
RTCP and jitter buffer would still have to be written, so we start fresh and
credit `rtp-rs` and `rtp-types` as API references.

## Goals

- `#![no_std]`, no `alloc`, `#![forbid(unsafe_code)]`, no dependencies. The
  one exception is the optional `srtp` feature, which uses audited RustCrypto
  crates rather than hand-written cryptography.
- No floating point anywhere. RFC 3550's jitter estimate is already defined
  in integer arithmetic.
- `const fn` constructors, so all state can sit in a `static` on embedded.
- Sans-IO: the crate never touches sockets or clocks. The caller passes in
  bytes and the current time, which keeps it runtime-agnostic (embassy, RTIC,
  tokio, bare loop) and deterministic under test.
- Reading a packet copies nothing; writing goes into a caller-provided
  `&mut [u8]`.
- Builds and passes tests for `thumbv6m-none-eabi` (RP2040) as well as the
  host.
- Small: about 700 lines of code plus tests.

## Non-goals

- ICE, DTLS, SDP negotiation, or anything else from WebRTC. SRTP keys come
  from the application (see `srtp` below), not from a handshake in the
  crate.
- RTCP feedback messages (NACK, PLI, TWCC, REMB).
- Opus or other floating-point codecs: too heavy for a Cortex-M0+.
- Multiple streams per session. The caller keeps one `Receiver` per remote
  stream.

Once out of scope, now done or planned:
- SRTP and SRTCP behind an optional feature using audited RustCrypto crates
  (#8, done);
- end-to-end key agreement for direct calls (#15);
- integer codecs, G.722 (#12);
- sample-level clock-drift correction instead of dropping whole frames (#10);
- RFC 8285 header extensions, for talker levels (#14).

## Modules

```
embedded-rtp
├── packet     RTP header: read and write, RTP/RTCP demux on one port
├── rtcp       SR, RR, SDES, BYE: read and write compound packets
├── stats      RFC 3550 A.1 / A.3 / A.8: sequence, loss, jitter
├── jitter     jitter buffer
├── receiver   stats + jitter buffer + SSRC handling
├── sender     sequence/timestamp/marker bookkeeping
├── l16        PCM16 encode/decode, simple loss concealment
├── duck       echo suppressor: microphone down while the speaker plays
└── srtp       SRTP/SRTCP and key wrap (optional `srtp` feature)
```

### `packet`

```rust
pub struct Header { marker: bool, payload_type: u8, sequence: u16, timestamp: u32, ssrc: u32 }
pub struct Packet<'a> { header: Header, extension: Option<Extension<'a>>, payload: &'a [u8], /* csrc bytes */ }

impl<'a> Packet<'a> {
    pub fn parse(buf: &'a [u8]) -> Result<Self, Error>;   // checks version, CSRC count, extension and padding bounds
    pub fn csrcs(&self) -> impl Iterator<Item = u32> + 'a;
}
pub fn write(header: &Header, payload: &[u8], buf: &mut [u8]) -> Result<usize, Error>;
pub fn is_rtcp(buf: &[u8]) -> bool;                        // RFC 5761: second byte in 192..=223
```

`Error` is a small `Copy` enum (`Truncated`, `BadVersion`, `BadPadding`,
`BufferTooSmall`, `TooMany`) and implements `core::error::Error`.

### `stats`

RFC 3550 appendix A, transcribed directly:

- **A.1, sequence validation.** A new source must arrive in order twice
  before it is trusted. Jumps up to `MAX_DROPOUT` (3000) count as forward
  progress. Reordering up to `MAX_MISORDER` (100) is accepted. A larger jump
  counts as a sender restart only if the next packet confirms it.
- **A.3, loss.** Extended highest sequence number, cumulative loss, and loss
  fraction per report interval.
- **A.8, interarrival jitter.** `J += (|D| - J) / 16`, stored scaled by 16 so
  it stays an integer.

```rust
pub enum SeqStatus { Valid, Probation, Restarted, Invalid }

pub struct SourceStats { /* ... */ }
impl SourceStats {
    pub const fn new() -> Self;
    pub fn update(&mut self, sequence: u16) -> SeqStatus;
    pub fn record_arrival(&mut self, rtp_timestamp: u32, arrival: u32); // arrival in RTP clock units
    pub fn jitter(&self) -> u32;                                         // RTP clock units
    pub fn report(&mut self) -> Report;   // loss fraction, cumulative loss, extended highest, jitter; starts a new interval
}
```

`report()` returns plain numbers rather than an RTCP type, so `stats` doesn't
depend on `rtcp`. The RTCP module builds its report block from a `Report`.

### `jitter`

A ring of `SLOTS` frame slots indexed by `sequence % SLOTS`, where `SLOTS`
must be a power of two so the index stays continuous when the sequence number
wraps. Each slot stores its sequence number, so a stale slot is never mistaken
for a fresh one.

```rust
pub struct Config { initial_depth: u8, min_depth: u8, max_depth: u8 }

pub enum Push    { Accepted, Late, Duplicate, Oversized }
pub enum Playout<'a> { Frame { sequence: u16, payload: &'a [u8] }, Lost { sequence: u16 }, Silence }   // payload borrowed, not copied

pub struct JitterBuffer<const SLOTS: usize, const FRAME: usize> { /* ... */ }
impl<const SLOTS: usize, const FRAME: usize> JitterBuffer<SLOTS, FRAME> {
    pub const fn new(config: Config) -> Self;
    pub fn push(&mut self, sequence: u16, payload: &[u8]) -> Push;
    pub fn pop(&mut self) -> Playout<'_>;   // call once per frame period
    pub fn set_target_from_jitter(&mut self, jitter: u32, frame_ticks: u32);
    pub fn counters(&self) -> Counters;   // played, concealed, overflow, drained, skipped, late, duplicate, oversized, underruns
    pub fn depth(&self) -> usize;
    pub fn reset(&mut self);
}
```

How it behaves:

- **Prefill.** Playout starts once frames up to the target depth are buffered
  ahead of the play point, or once the first frame has waited `target` frame
  periods. Without that timeout, a talkspurt shorter than the target would
  sit in the buffer until the next one arrived; the simulation test found
  this.
- **Reordered talkspurt starts.** While prefilling, nothing from the new run
  has played, so an earlier frame that arrives second pulls the play point
  back to itself. The play point stays within `max_depth`, and never moves
  behind the point where the previous run ended.
- **Loss vs. silence.** If the next frame is missing but later ones have
  arrived, that frame is lost: return `Lost` and move on. If nothing is
  buffered at all, the sender has gone quiet: return `Silence` and prefill
  again. Silence suppression and loss are told apart without timestamps.
- **Late frames** (behind the play point) are dropped and counted.
- **Accounting.** Every sequence number the play point passes is counted
  exactly once: `played`, `concealed`, `overflow`, `drained` or `skipped`.
- **Overflow.** A packet more than `max_depth` frames ahead moves the play
  point forward, dropping the oldest frames. This bounds latency after a
  burst.
- **Adaptive depth.** `target = clamp(1 + ceil(3·J / frame), min, max)`,
  where J is the A.8 jitter. For 20 ms frames at 5–15 ms of Wi-Fi jitter,
  that gives 2–4 frames (40–80 ms).
- **Draining.** If the buffer stays more than two frames above target for
  about half a second, one frame is dropped. This recovers latency after a
  delay spike during continuous speech, which never refills on its own.

### `receiver` and `sender`

What a client actually uses.

```rust
pub struct Receiver<const SLOTS: usize, const FRAME: usize> { /* stats + buffer + current SSRC */ }
impl Receiver {
    pub const fn new(payload_type: u8, frame_ticks: u32, config: jitter::Config) -> Self;
    pub fn receive(&mut self, packet: &Packet, arrival: u32) -> Received; // checks PT; resets on new SSRC or Restarted
    pub fn play(&mut self) -> Playout<'_>;
    pub fn report(&mut self) -> Option<stats::Report>;
    pub fn sender_report(&mut self, ssrc: u32, info: &rtcp::SenderInfo, now: u32); // remember the last SR
    pub fn report_block(&mut self, now: u32) -> Option<rtcp::ReportBlock>;        // with LSR and DLSR filled in
}

pub struct Sender { /* ssrc, pt, sequence, timestamp, counts */ }
impl Sender {
    pub const fn new(ssrc: u32, payload_type: u8, frame_ticks: u32, seq0: u16, ts0: u32) -> Self;
    pub fn send(&mut self, payload: &[u8], buf: &mut [u8]) -> Result<usize, Error>;
    pub fn send_in_place(&mut self, buf: &mut [u8], payload_len: usize) -> Result<usize, Error>; // payload already at buf[12..]
    pub fn skip(&mut self);                         // silent frame: advance timestamp, mark next packet
    pub fn packet_count(&self) -> u32;              // for sender reports (milestone 3)
    pub fn octet_count(&self) -> u32;
}
```

The caller supplies the random SSRC and starting values (RFC 3550 §5.1), since
there is no RNG in `core`. Packets from probation-stage sources are still
buffered: the version, payload type and length checks already reject garbage,
and dropping the first packet of every talkspurt start would be audible.

### `rtcp`

Compound packets per RFC 3550 §6:

```rust
let compound = rtcp::Compound::parse(datagram)?;   // all of A.2 checked up front
for packet in compound.packets() {                 // so iteration cannot fail
    match packet {
        rtcp::Packet::SenderReport { ssrc, info, reports } => { /* ... */ }
        rtcp::Packet::ReceiverReport { ssrc, reports } => { /* ... */ }
        rtcp::Packet::SourceDescription(chunks) => { /* chunk.cname(), chunk.name() */ }
        rtcp::Packet::Goodbye { sources, reason } => { /* ... */ }
        rtcp::Packet::Other { packet_type, count, body } => { /* APP, feedback */ }
    }
}

let mut writer = rtcp::Writer::new(&mut buf);       // each append is all or nothing
writer.sender_report(ssrc, &sender.sender_info(ntp), &[block])?;
writer.source_description(ssrc, &[(rtcp::sdes::CNAME, b"kitchen@example")])?;
writer.goodbye(&[ssrc], None)?;
let bytes = writer.finish();
```

- **Reading:** `Compound::parse` applies RFC 3550 appendix A.2 to the whole
  datagram: every packet is version 2, the first is SR or RR, only the last
  may be padded, and the lengths add up exactly. It also checks each
  packet's counts against its length and walks every SDES chunk. Nothing is
  copied: report blocks, chunks, items and BYE sources are iterators over the
  datagram.
- **Writing:** `Writer` appends SR, RR, SDES (one chunk, terminated by zeros
  to a word boundary) and BYE (with optional reason). Over 31 items is
  `Error::TooMany`; an item over 255 bytes is `Error::TooLong`.
- **Round trips:** `Receiver::sender_report(ssrc, &info, now)` records the
  last SR, and `Receiver::report_block(now)` echoes it as LSR, with the time
  held as DLSR. The sender computes `rtcp::round_trip(arrival, lsr, dlsr)`.
  Times are NTP short format (1/65536 s) from `ntp_from_micros`, and each
  side only needs a consistent clock of its own.

For the intercom, these RTCP messages replace ad-hoc control traffic:

- **Receiver reports** carry loss and jitter in each direction.
- **SDES NAME** gives a device's roster name.
- **BYE** ends a session immediately instead of waiting for the 5 s timeout.
- **Periodic RR** works as the keep-alive for listen-only devices.

### `l16`

- Big-endian PCM16 encode/decode (RFC 3551 §4.5.11), checked byte for byte
  against ffmpeg's `pcm_s16be`.
- `Concealer<N>` turns `Playout` into samples: `concealer.play(receiver.play())`.
  - **Good frames** are decoded straight from the borrowed payload into the
    concealer's buffer. That one conversion doubles as the "last good frame"
    memory, so the no-loss path costs no more than a decode.
  - **Lost frames** repeat the last good frame with the gain ramping down:
    full → half over the first loss, half → nothing over the second, silent
    from the third.
  - **Running dry** (`Silence`) after audio fades the last frame out over one
    frame period instead of cutting off.
  - **Joins** (into concealment, between concealed frames, back into audio,
    and at stream start) blend linearly over `FADE` = 32 samples (2 ms) from
    the last sample played, so there is no step.
  - Q15 gains, no division; the gain is in closed form per sample so the loop
    vectorizes.
  - Memory: two frames of samples, 1,280 bytes at `N` = 320.
  - Tested: on a sine wave with random runs of losses and dry spells at every
    phase, no sample step exceeds the bound the design implies, and naive
    concealment demonstrably breaks that bound.

### `duck`

An echo suppressor for full-duplex devices (#9). A device's speaker comes
back through its microphone, and the server's mix-minus can't remove it:
to the server it is new audio.

- `Ducker::process(played, mic)` runs once a frame, before sending. Per
  sample: if the speaker's sample is at or above `threshold`, restart a
  hold counter. While playing or holding, the microphone gain falls towards
  `ducked_gain`; otherwise it rises back to unity. Both move linearly, over
  `attack` and `release` samples.
- `hold` covers the delay from handing a frame to the speaker to hearing it
  in the microphone (audio buffers and the air) and the room's echo tail.
  With a 100 ms hold, gaps between syllables don't let the gain pump.
- Q15 gain, one multiply a sample, no division: the steps are worked out
  once in the `const` constructor. The gain is held with 16 more bits than
  it is applied with, so a fade's step can be tiny and a long attack or
  release (up to a day) takes as long as configured, within 0.05% for a
  minute's fade. Unity gain passes samples through bit
  for bit. `gain_from_db` turns decibels into a Q15 gain at compile time.
- `Config::DEFAULT`, for 16 kHz: above about -30 dBFS, 24 dB down within
  2 ms, back over 100 ms after 100 ms of quiet.
- Cost: about 13,600 ARMv6-M instructions a frame, 43 a sample: at 1.3 to
  1.6 cycles an instruction, 0.7 to 0.9% of a 125 MHz M0+. State is a few
  words.
- A suppressor, not a canceller: while the far end talks, the near end is
  turned down too. An adaptive (NLMS) canceller would allow real double
  talk, but costs a multiply-accumulate per filter tap per sample: 64 taps
  (4 ms of echo path) would be about 20 times the suppressor. Only worth it
  if suppression proves not enough.
- Tested on synthetic signals: below the threshold the microphone is
  untouched, above it attenuated by the configured decibels (against a
  floating-point reference, within 0.1 dB); attack, hold and release times
  measured from the output; no overflow at any extreme; block size makes no
  difference.

### `srtp`

SRTP and SRTCP (RFC 3711) with the default suite, `AES_CM_128_HMAC_SHA1_80`,
behind the `srtp` feature.

- **Crypto from audited crates.** `aes`, `ctr`, `hmac`, `sha1` and `aes-kw`
  from RustCrypto, all `no_std` without an allocator. They stay on the
  releases that support Rust 1.81 (aes 0.8, ctr 0.9, hmac 0.12, sha1 0.10,
  aes-kw 0.2); newer ones need a newer compiler.
- **Two contexts per stream direction.** `Outbound` protects one sender's RTP
  and RTCP; `Inbound` checks and decrypts one remote stream's. Each binds to
  the first SSRC it sees and rejects others, so one key can't silently serve
  two streams with confused state.
- **In place.** `protect_rtp` encrypts the payload (everything after the
  header, CSRCs and extension) where it lies and appends the 10-byte tag.
  `unprotect_rtp` returns the plain packet's length for `Packet::parse`.
  Session keys are derived once per context; the AES key schedule and the
  keyed HMAC state are kept, so a packet costs one counter-mode pass and one
  HMAC.
- **Order of checks when receiving.** Estimate the packet index (RFC 3711
  appendix A), check the replay window, verify the tag in constant time,
  and only then update state and decrypt. A packet that fails any step
  changes nothing.
- **Replay window** of 64 packets (§3.3.2's minimum), separately for RTP and
  SRTCP.
- **No index reuse on send.** `Outbound`'s packet index only moves forward: a
  sequence number that isn't after the last one is taken as the next
  rollover. Repeating a sequence number therefore never reuses keystream; the
  receiver sees a replay.
- **SRTCP** always encrypts. Authentication-only SRTCP (E flag clear) is
  rejected rather than accepted untested; nothing in this crate's world
  sends it. The first SRTCP index is 0, as §3.4 says (libsrtp starts at 1;
  receivers accept either).
- **Rekeying.** `rekey` on `Inbound` keeps the previous keys until a packet
  authenticates under the new ones, so the sender can switch whenever it
  likes without a gap. No MKI: neither SDES as ffmpeg uses it nor our
  devices need one.
- **Keying model.** The crate takes a master key and salt and leaves delivery
  to the application, but its documentation is firm on one point: a fresh
  master key per session. A fixed key per device plus a reboot that restarts
  sequence numbers reuses counter-mode keystream, leaking the XOR of two
  audio streams (§9.1). The recommended layering: a long-term device secret
  only authenticates the device; the server issues random session keys on
  connect (or per call for direct calls), over TLS or wrapped with
  `srtp::wrap` (AES key wrap with padding, RFC 5649) under a key derived from
  the device secret. A key-issuing server can decrypt what it keys; #15 is
  for when it must not.
- **Cost.** About 0.67 µs to protect and 0.6 µs to unprotect a 652-byte
  packet on the host, mostly HMAC-SHA1 even with SHA instructions. On the
  RP2040 both run in software: about 137,000 ARMv6-M instructions per
  packet each way, 108,000 of them AES (see "On a Cortex-M0+" below). That
  is roughly 15% of a 125 MHz core for a full-duplex device. If that proves
  too much, an authentication-only mode (null cipher, about 29,000
  instructions) is the fallback.
- Tested against libsrtp's known-answer packets (RTP and SRTCP), the
  RFC 3711 appendix B vectors and OpenSSL's key wrap, and against ffmpeg in
  both directions across the sequence wrap. Every bit flip, wrong key,
  replay and out-of-window packet is rejected.

## Memory

Measured with `size_of` on a 64-bit host. The 32-bit RP2040 is the same or
slightly smaller.

| Item | Bytes |
|---|---|
| `Receiver<16, 640>` (server) | 10,472 |
| `Receiver<8, 640>` (Pico) | 5,304 |
| `Concealer<320>` | 1,292 |
| `JitterBuffer<16, 640>` | 10,400 |
| `JitterBuffer<8, 640>` | 5,232 |
| `SourceStats` | 40 |
| `Sender` | 24 |
| Packet buffer | 652 (12 + 640), 662 with SRTP |
| `srtp::Outbound` | 1,760 (1,040 on the RP2040) |
| `srtp::Inbound` | 3,536 (2,080 on the RP2040) |

The RP2040 has 264 KB of SRAM. A full-duplex client with an 8-slot receiver,
a sender and one packet buffer uses about 6 KB, and about 9 KB with SRTP.
Half of `Inbound` holds the previous keys during a rekey.

## Performance

Benchmarks: `cargo bench --bench hot_path` (Criterion; compare runs with
`-- --save-baseline a` and `-- --baseline a`). For a line-by-line instruction
breakdown, run `examples/profile.rs` under Callgrind. Its header gives the
commands.

### Where the time goes

Host figures (x86-64, 2.1 GHz Xeon VM). Instruction counts come from
Callgrind and are deterministic. Timings are Criterion medians, and repeat
runs on this shared VM vary by about ±10%.

| One frame, one direction | Before | After |
|---|---|---|
| send → parse → receive → play | 71.6 ns, 673 instructions | ~43 ns, 381 instructions |
| receive side only (parse → receive → play) | — | ~41 ns |

What changed, in order of effect:

1. **Zero-copy playout.** `pop` and `play` return the payload borrowed from
   its slot, instead of copying 640 bytes into a caller buffer. The borrow
   ends before the next push can reuse the slot.
2. **O(1) `depth()`.** The buffer tracks its newest frame, instead of scanning
   every slot up to three times per frame. The scan cost 4.8 ns at 8 slots and
   45 ns at 64; `depth()` now costs 0.7 ns at any size. Debug builds check the
   tracked value against the scan after every push and pop, so the whole test
   suite verifies it.
3. **`Sender::send_in_place`.** Audio captured straight into the packet buffer
   (on the Pico, by DMA) needs only its header written.
4. **No division on the hot path.** The target depth is found by addition,
   bounded by `max_depth`, instead of `ceil(3J / frame)`. On the Cortex-M0+,
   which has no divide instruction, that removes a software division call per
   packet.

What remains per frame (381 instructions):

| | Instructions | Note |
|---|---|---|
| Copy into the jitter buffer (640 B) | 87 | Needed: the caller reuses its receive buffer. Removing it would mean receiving straight into a slot, and the network stack copies into the caller's buffer anyway. |
| Jitter buffer logic | 77 | |
| `Packet::parse` | 56 | Mostly bounds checks. |
| Statistics, receiver, sender | ~51 | |
| The profiling loop itself | ~21 | |

Turning a frame into samples (`l16`, #7):

| | Baseline x86-64 | `-C target-cpu=x86-64-v3` |
|---|---|---|
| `decode` (640 B → 320 samples) | 27 ns | 15 ns |
| `Concealer::play`, no loss | ~30 ns: a decode | 23 ns |
| A lost frame, then audio again | ~330 ns | 124 ns |

Baseline x86-64 is SSE2 only, which has no 32-bit lane multiply, so the
concealment loop can't vectorize. With AVX2 (`x86-64-v3`, which every x86
server CPU since 2013 has) it does, so build servers with that flag.
The closed-form gain matters there: the per-sample loop it replaced took
269 ns even with AVX2. Losses are rare, so this is per lost frame, not per
frame.

RTCP runs every few seconds, not per frame: writing an SR with one report
block plus a two-item SDES takes ~21 ns, and parsing and reading it ~42 ns.

On the RP2040 the receive path compiles to a single `memcpy` call and no
division calls.

### On a Cortex-M0+

What a Pico W device does each frame, in ARMv6-M instructions. Counted
under QEMU in CI (`examples/m0-cycles`): the code is compiled for the
Cortex-M0+ and run on a Cortex-M3 board, which executes it unchanged, with
each instruction taking 1 ns of virtual time so that SysTick counts
instructions.

| Operation | Instructions |
|---|---|
| L16 encode and RTP header (`send_in_place`) | 1,040 |
| Parse, receive, play | 1,160 |
| Concealment: good frame / lost frame then good | 2,440 / 7,480 |
| Echo suppressor (`duck`), full duplex only | 13,600 |
| SRTP protect, 652 bytes | 137,080 |
| SRTP unprotect, 662 bytes | 137,440 |
| SRTP tag check only (a forged packet) | 29,000 |
| Whole device frame, both directions, SRTP | 279,480 |
| New SRTP context (key derivation) | 80,320 |

A Cortex-M0+ takes one cycle for most instructions and two for loads,
stores and taken branches, so cycles are roughly 1.3 to 1.6 times these
counts. The whole frame is then 3 to 3.6 ms of every 20 ms at 125 MHz,
about 15 to 18% of the CPU. Without SRTP it would be under 5,000
instructions. CI fails if the whole frame passes 400,000 instructions.
`examples/pico-w`'s `cycles` binary runs the same workload on a board and
reports real cycles, including flash cache misses. Those numbers still
need recording here.

### Latency budget

Processing is not where the delay is. At about 40 ns a frame on the host, and
probably tens of microseconds on the Pico, it is a vanishing fraction of the
20 ms between frames. The delay a listener hears comes from:

| Source | Delay | Controlled by |
|---|---|---|
| Packetization: a frame must be captured before it is sent | 20 ms | Frame size (`FRAME_SAMPLES`) |
| Jitter buffer | `target` × 20 ms: 20–160 ms, 40 ms to start | `Config`, and measured jitter |
| Network | LAN: < 1 ms. Wi-Fi: a few ms, with spikes | — |
| Server mix, if any | Up to one 20 ms tick | The mixer's tick |

To cut latency, the levers are shorter frames (10 ms halves packetization
delay, and doubles the packet rate) and a smaller `max_depth`, which trades
concealment for delay. Not nanoseconds.

## Intercom profile

| | |
|---|---|
| Payload | L16, **big endian**, mono |
| Clock / sample rate | 16 000 Hz |
| Packet time | 20 ms: 320 samples, 640 bytes, timestamp step 320 |
| Payload type | 96 (dynamic) |
| SDP | `a=rtpmap:96 L16/16000/1`, `a=ptime:20` |
| Transport | UDP, RTP and RTCP on the same port (RFC 5761) |
| Silence | no packets; next packet has the marker bit set and a timestamp gap |

These live in a `profile` module of constants, not in the core types. The
crate itself is generic over frame size and clock rate.

### Using it in a server

A server that mixes several devices ("mix minus": each hears everyone but
itself) uses the same types as a device:

- **Inbound.** One `Receiver<16, 640>` plus one `Concealer<320>` per
  participant. On each 20 ms mixer tick, `concealer.play(receiver.play())`
  gives that participant's samples, or `None` for silence.
- **Outbound.** One `Sender` per participant. A silent tick calls `skip()`, so
  the next packet carries the marker bit and the timestamp gap.
- **RTCP.** Every few seconds, an SR with a report block about the
  participant's stream, plus SDES CNAME. Incoming SDES NAME gives a display
  name, and BYE removes a participant at once.

Issue #4 tracks a worked example.

## Testing

Conformance tests check the crate against something independent of it: the
RFC's definitions, a simulation that knows the true answer, or another
implementation. Round-tripping our writer through our reader doesn't count,
because a misreading shared by both halves would pass. See issue #1.

- **Wire format** (`tests/rfc3550_wire.rs`):
  - packets assembled byte by byte from the RFC 3550 §5.1 diagram, in every
    combination of padding, extension and CSRC count, plus 20,000 random
    ones; the parser must report exactly what went in;
  - the writer must produce the diagram's bytes;
  - every truncation and single-bit flip of valid packets, and 100,000 random
    inputs, must never panic and never return a slice outside the input;
  - RTP/RTCP demultiplexing checked for every payload type, with and without
    the marker bit (RFC 5761 §4);
  - packets captured from ffmpeg (`tests/fixtures/`) must parse to the exact
    audio ffmpeg was given.
- **Statistics** (`tests/rfc3550_stats.rs`):
  - a simulated network drops, duplicates and reorders packets, and the
    expected cumulative loss, loss fraction per interval and extended highest
    sequence number come from what it actually delivered (§6.4.1). Runs
    cover up to 4.5 sequence number wraps;
  - `MAX_DROPOUT`, `MAX_MISORDER`, probation, restart confirmation and the
    24-bit clamp tested at their exact edges;
  - jitter checked against a floating-point evaluation of the §6.4.1 formula.
    The integer method of A.8 provably stays within (−1.5, +0.5] ticks of it,
    and the test asserts that bound.
- **Interop** (`tests/interop.rs`, `#[ignore]`d; a CI job installs the
  tools):
  - ffmpeg → us: ffmpeg's RTP muxer sends L16/16000/1. Our parser recovers
    the exact samples, with contiguous sequence numbers and timestamps that
    step by the sample count. ffmpeg's SDP matches our profile word for word;
  - us → ffmpeg: our packets, described by an SDP file, decode back to the
    exact audio, across sequence and timestamp wraparound, with lost and
    reordered frames;
  - GStreamer's RTCP receiver reports about our stream agree with
    `SourceStats` on extended highest sequence number exactly, and on
    cumulative loss up to one documented GStreamer deviation: its
    `rtpsource.c` counts packets held during probation as received. We follow
    the RFC's reference code.
- **Jitter buffer** (`tests/jitter_buffer.rs`): a simulation in which the
  sender and receiver run on separate clocks (with drift), and the network
  adds delay spread, loss, duplication, stalls and silence suppression. The
  simulation knows what arrived and when, so it checks every decision the
  buffer makes:
  - a played frame carries exactly its own bytes, and plays once;
  - a frame is reported `Lost` only if it really had not arrived;
  - playout is in sequence order, with every passed sequence number counted;
  - accepted frames are conserved;
  - depth never exceeds `max_depth`.

  Per scenario:
  - clean network: nothing concealed or skipped;
  - silence suppression: never reported as loss;
  - slow receiver clock: drains instead of growing;
  - fast receiver clock: underruns without concealment;
  - the target adapts to jitter.
- **Sender and receiver** (`tests/sender_receiver.rs`): sender bytes checked
  against the header diagram.
  - RFC 3551 §4.1 silence: timestamps advance, sequence numbers don't, and
    the next packet carries the marker bit;
  - sequence numbers and timestamps wrap correctly;
  - packet and octet counts per §6.4.1;
  - receiver payload type filter, new SSRC, confirmed restart, and
    probation packets still played;
  - sender to receiver is bit-exact through silences.
- **Interop for milestone 2:**
  - ffmpeg decodes a `Sender` stream with silences into exactly the right
    audio, silences filled to the sample (`aresample=async=1`), across
    sequence and timestamp wraparound;
  - an ffmpeg stream plays through `Receiver` unchanged.
- **RTCP** (`tests/rfc3550_rtcp.rs`):
  - SR, empty RR, SDES and BYE checked byte for byte against hand-written
    vectors from the §6.4–§6.6 diagrams, including an SDES chunk whose items
    end exactly on a word boundary (it still needs a word of zeros);
  - every A.2 validity rule tested on its own;
  - the 24-bit signed cumulative loss, including clamping;
  - round-trip arithmetic against the RFC's own worked example (figure 2:
    6.125 s);
  - ffmpeg's real RTCP (fixture): SR + SDES + BYE parse, the final SR counts
    exactly the packets and octets of the 1.5 s stream, and its NTP and RTP
    timestamps advance together;
  - mangled and random input never panics.

  Interop:
  - ffmpeg's live RTCP matches the RTP stream it sent;
  - GStreamer's RTCP passes our A.2 checks and carries a CNAME;
  - GStreamer echoes our SR in its next receiver report, and the round trip
    worked out from it is under 100 ms on loopback.

  Eight bugs planted in the RTCP code were all caught. Swapping the packet
  and octet counts was also caught by the ffmpeg fixture alone.
- **Planted bugs.** Milestone 2: 12 of 13 bugs planted by hand were caught.
  The one survivor (dropping the slot's sequence check in `pop`) can never
  change the outcome, because of the ring invariant, and the check stays as a
  guard. Milestone 1: bugs planted by hand were all caught: little-endian
  sequence numbers, padding not removed, too narrow an RTCP range, the
  misorder limit off by one, duplicates not counted, the wrong jitter gain, no
  24-bit clamp, the wrong fraction scaling.
- **Embedded build.** CI runs `cargo build --target thumbv6m-none-eabi`.
- **Fuzzing (#6).** `cargo fuzz` targets in `fuzz/` for `Packet::parse`
  and `rtcp::Compound::parse`: neither may panic or hand out a slice outside
  its input. A parsed RTP header, written back bare, must keep the input's
  V, M, PT, sequence, timestamp and SSRC. Every SR, RR, SDES and BYE
  accepted, written again with `Writer`, must parse to the same contents;
  other packet types (APP, feedback) are only checked for bounds, as
  `Writer` can't write them. CI runs each for a minute on pull requests
  that touch the readers, and for 20 minutes weekly. The first runs found
  one bug: a BYE with a zero reason length followed by non-zero bytes was
  accepted, its "padding" never checked.

## Milestones

1. `packet` + `stats` + tests; thumbv6m CI. **Done** (#1).
2. `jitter` + `receiver` + `sender` + simulation test. **Done** (#2).
3. Loss concealment and L16 helpers (#7), then RTCP (#3). **Done.**
   SRTP and SRTCP (#8). **Done.**
4. Server integration guide and mixing-server example, including server
   performance (#4). **Done**: see `docs/server.md`.
5. Pico W example (embassy-net) under `examples/`, with on-device
   measurements (#5). **Built**: `examples/pico-w`, with instruction counts
   from QEMU. Still to do on hardware: a first run, and real cycle counts
   from its `cycles` binary.

Further work, by priority:

| Issue | | Priority |
|---|---|---|
| #10 | Clock drift by samples, not frames | Medium |
| #11 | Redundant audio (RFC 2198) | Medium |
| #12 | G.722 codec | Low until bandwidth matters |
| #13 | Packets of varying duration | Low |
| #14 | Talker information (CSRC, levels) | Low |
| #15 | End-to-end key agreement for direct calls | Low until needed |
| #9 | Adaptive echo cancellation (suppression is done) | Low until needed |

## Decisions

| Question | Decision |
|---|---|
| RTP payload byte order | Big-endian (standard L16), so standard RTP tools interoperate. |
| RTCP | In v1: SR, RR, SDES and BYE. Optional for devices; the server works without it. |
| Codec | Raw 16-bit PCM (L16) only. The payload stays opaque, so a codec can be added later without API changes. |
| Licence | Dual MIT OR Apache-2.0. |
| Distribution | Git dependency; not published to crates.io. |
| Security | SRTP with `AES_CM_128_HMAC_SHA1_80`, optional feature. Fresh session keys from the application; the crate never invents or exchanges keys. |
