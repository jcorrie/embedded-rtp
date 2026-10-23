# Intercom on a Raspberry Pi Pico W

A full-duplex intercom device: it joins the example `mixing_server`
(`docs/server.md`) over Wi-Fi with SRTP, sends the microphone while the
BOOTSEL button is held, and plays the room through an amplifier. The
protocol side is `examples/common/device.rs`, the same code the desktop
client and the simulation tests run.

Not yet tried on hardware. It builds in CI, and the protocol code is
tested on the host. Reports welcome.

## Parts

- Raspberry Pi Pico W
- INMP441 I2S microphone
- MAX98357A I2S amplifier and a small speaker
- A debug probe (another Pico running debugprobe works), for flashing and
  logs

## Wiring

| Signal | Pico W | INMP441 | MAX98357A |
|---|---|---|---|
| Bit clock | GP10 | SCK | |
| Word clock | GP11 | WS | |
| Data | GP12 | SD | |
| Bit clock | GP18 | | BCLK |
| Word clock | GP19 | | LRC |
| Data | GP20 | | DIN |
| 3.3 V | 3V3 | VDD | VIN |
| Ground | GND | GND, L/R | GND |

Both buses run at 16 kHz with 16-bit stereo slots. The microphone's L/R
pin goes to ground, so it sends on the left channel. The amplifier gets the
same sample in both channels.

## Configure and flash

Settings are compiled in. Add the device to the server's `devices.txt`
with the same KEK:

```sh
# Install once: rustup target add thumbv6m-none-eabi; cargo install probe-rs-tools
cd examples/pico-w
WIFI_SSID=home WIFI_PASSWORD=secret \
SERVER=192.168.1.10:5004 \
DEVICE_ID=kitchen DEVICE_NAME=Kitchen \
DEVICE_KEK=000102030405060708090a0b0c0d0e0f \
cargo run --release
```

The log (over the probe) shows the Wi-Fi join, the address it got and
`joined the room`. Hold BOOTSEL to talk.

- Without `DEVICE_KEK`, it talks plain RTP to a server run with
  `--insecure`.
- `--features listen-only` builds a speaker-only device: no microphone, and
  BOOTSEL is ignored.

## How it runs

- **Audio** runs on a higher-priority executor (`SWI_IRQ_1`). Each 20 ms
  frame, microphone and amplifier DMA run double-buffered on PIO1. A
  transfer must restart within the FIFO's 0.5 ms; at a higher priority, the
  network task's SRTP work can't delay it.
- **The network task** ticks the `Device` once per captured frame, so
  playout follows the audio clock. It sends what was captured (or nothing,
  when BOOTSEL isn't held) and hands the next frame to play back.
- **Wi-Fi** power saving is off; it would add tens of milliseconds to every
  packet.
- **Half duplex by button** is the echo baseline from issue #9. Holding
  BOOTSEL briefly stalls flash reads; the audio buffers cover it.

## Cost per frame

The same workload runs on a Pico (`cargo run --release --bin cycles`, real
cycles) and under QEMU in CI (`examples/m0-cycles`, ARMv6-M instructions).
QEMU's counts for a whole device frame, both directions with SRTP:

| Operation | Instructions |
|---|---|
| Encode and add the header | 1,040 |
| SRTP protect, 652 bytes | 137,080 |
| SRTP check only (a forged packet) | 29,000 |
| SRTP unprotect, 662 bytes | 137,440 |
| Parse, receive, play | 1,160 |
| Concealment: good frame / lost frame | 2,440 / 7,480 |
| **Whole frame, both ways** | **279,480** |
| New SRTP context (key derivation) | 80,320 |

A Cortex-M0+ takes 1 to 2 cycles per instruction, so a frame is roughly
360,000 to 450,000 cycles: 3 to 3.6 ms of every 20 ms at 125 MHz, about 15
to 18% of the CPU. Software AES-128 is about 108,000 instructions of each
direction's 137,000, and the HMAC the rest. Flash is about 375 KB (231 KB
of it Wi-Fi firmware); static RAM is about 52 KB.

## Files

- `src/main.rs`: configuration, Wi-Fi, the network task
- `src/audio.rs`: I2S microphone and amplifier
- `src/cycles.rs`: the on-device cost measurement
- `firmware/`: the CYW43439's NVRAM settings (see its README for the
  licence)
