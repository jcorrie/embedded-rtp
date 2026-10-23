# Pico W intercom

A full-duplex-capable intercom client for the Raspberry Pi Pico W, built on
`embedded-rtp` and [embassy](https://embassy.dev): an I2S microphone and an
I2S amplifier, talking RTP over Wi-Fi to a mixing server. It speaks the
crate's intercom profile (L16, 16 kHz mono, 20 ms frames, payload type 96),
so it works with SPQR's audio room, or anything else that speaks it.

Plus `bench`, which measures what each per-frame operation costs on the
RP2040 (embedded-rtp #5).

**Status:** builds in CI for `thumbv6m-none-eabi`, in every feature
combination. **It has not yet run on hardware.** The I2S microphone timing
was checked in a cycle-by-cycle simulation of its PIO program, but expect a
first bring-up to need adjusting.

## How it works

The audio hardware keeps time, not a software timer:

- Every 20 ms the microphone's DMA buffer fills. That frame is converted to
  L16 and sent as one RTP packet.
- Every 20 ms the speaker's DMA buffer drains. The next frame comes from the
  jitter buffer (`Receiver`), through loss concealment (`Concealer`), into
  the other buffer.
- Packets from the server go into the jitter buffer as they arrive.
- Every 3 s it sends RTCP: a receiver report about the server's stream (a
  sender report once it has sent audio) and SDES with its name. That is
  also the keep-alive for the server's 5 s timeout. The server's sender
  reports give the round-trip time, which is logged.

Push-to-talk by default: hold the button to talk. While you talk, the
speaker plays silence so it can't feed back into the microphone. That is the
half-duplex baseline from embedded-rtp #9; `always-on` sends all the time.

## Hardware

- Raspberry Pi Pico W.
- An I2S MEMS microphone with 24-bit samples in 32-bit slots: INMP441,
  ICS-43434, SPH0645 or similar. Tie its L/R (or SEL) pin to ground, so it
  sends in the left slot.
- An I2S amplifier that accepts 16-bit samples at 32 bit clocks per frame:
  MAX98357A, and a small speaker.
- A push button.
- A debug probe for flashing and logs: another Pico running
  [debugprobe](https://github.com/raspberrypi/debugprobe) works.

| Pico W pin | Connects to |
|---|---|
| GP10 | Microphone SCK (bit clock) |
| GP11 | Microphone WS (word select) |
| GP12 | Microphone SD (data) |
| GP18 | Amplifier BCLK |
| GP19 | Amplifier LRC |
| GP20 | Amplifier DIN |
| GP15 | Push-to-talk button, other side to ground |
| 3V3, GND | Microphone power; microphone L/R to GND |
| VSYS (5 V), GND | Amplifier power |

Word select must be the pin after the bit clock, on both buses: PIO
side-set pins are consecutive. Wi-Fi uses GP23, GP24, GP25 and GP29
internally.

## Configure and run

Settings are baked in at build time:

| Variable | |
|---|---|
| `WIFI_NETWORK` | Wi-Fi network name (WPA2) |
| `WIFI_PASSWORD` | Its password |
| `INTERCOM_SERVER` | The server, `address:port`, e.g. `192.168.1.10:1234` |
| `DEVICE_NAME` | Optional: the roster name. Default `Pico W` |

```sh
cd examples/pico-w
export WIFI_NETWORK=home WIFI_PASSWORD=... INTERCOM_SERVER=192.168.1.10:1234 DEVICE_NAME=Kitchen
cargo run --release --bin intercom       # flash through the probe and stream the log
```

`cargo run` uses [probe-rs](https://probe.rs). Logs are defmt over RTT, so
they need the probe. Without one, `elf2uf2-rs` converts the ELF for
drag-and-drop flashing in BOOTSEL mode, but you get no logs.

Cargo features:

| Feature | |
|---|---|
| `listen-only` | No microphone: receive and play, with RTCP as the keep-alive |
| `always-on` | Send the microphone all the time (full duplex, no echo handling) |
| `cpu-load` | Log CPU load every 5 s (keeps the CPU from sleeping; for measuring) |
| `rom-intrinsics` | Use the RP2040 ROM's `memcpy` and division routines |
| `srtp` | On by default; only `bench` uses it (see below) |

## Measuring (embedded-rtp #5)

`bench` needs no Wi-Fi, microphone or amplifier:

```sh
cargo run --release --bin bench
cargo run --release --bin bench --features rom-intrinsics   # compare
```

It logs the cycles for each per-frame operation (sending, receiving and
playing, concealment, RTCP, SRTP), and the primitives they are made of
(`memcpy`, 32- and 64-bit division), with the total per frame.

On the intercom itself:

- Each task logs its processing time per frame every 5 s (`mic`, `send`,
  `receive`, `play`).
- `--features cpu-load` logs the whole CPU's load, Wi-Fi and network stack
  included, by counting idle-loop iterations against a one-second baseline
  taken at boot.
- The log shows the round trip, loss and jitter the server reports.

Please record the numbers in `docs/design.md`, next to the host figures.

## Memory

| | |
|---|---|
| Flash | 348 KB, of which 226 KB is the Wi-Fi firmware |
| Static RAM | 38 KB of 264 KB: the jitter buffer and concealer (6.6 KB), the network stack and socket buffers, and the tasks' audio buffers |

## Not here (yet)

- **SRTP.** The crate's `srtp` module says to use fresh keys for every
  session, never a fixed key per device: a device that restarts its sequence
  numbers under the same key reuses keystream. That needs a way for the
  server to hand out session keys (for example `srtp::wrap` under a device
  secret), which doesn't exist yet. Until then the intercom speaks plain
  RTP, and `bench` measures what SRTP would cost.
- **BYE.** A Pico has no orderly shutdown to send one from; the server times
  the device out after 5 s.
- **Zero-copy capture.** The microphone's 32-bit slots have to be converted
  to 16-bit big-endian samples anyway, so a frame is converted into the
  packet rather than captured straight into it.

## Licences

The example is MIT or Apache-2.0, like the crate.
`firmware/nvram_rp2040.bin` (the Pico W's board configuration) and the Wi-Fi
firmware from the `cyw43-firmware` crate are under the
[Infineon Permissive Binary License](firmware/LICENSE-permissive-binary-license-1.0.txt).
