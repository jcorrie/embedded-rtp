# Running an intercom server

How to run the example mixing server, how devices join it, and what keeps a
server like it fast. The code is in `examples/`:

| File | What it is |
|---|---|
| `common/intercom.rs` | The room: `Mixer` and `Device`, sans-IO |
| `mixing_server/` | The server: a UDP loop around `Mixer`, plus its tests |
| `intercom_client/` | A desktop device: a UDP loop around `Device` |
| `../benches/mixer.rs` | What a tick costs with N participants |

## Try it

```sh
# One line per device: an id and a 16-byte key-encryption key in hex.
cat > devices.txt <<'END'
kitchen 000102030405060708090a0b0c0d0e0f
hall    101112131415161718191a1b1c1d1e1f
END

cargo run --release --features srtp --example mixing_server -- \
    --devices devices.txt --listen 0.0.0.0:5004

# In two more terminals:
cargo run --release --features srtp --example intercom_client -- \
    --server 127.0.0.1:5004 --id kitchen \
    --kek 000102030405060708090a0b0c0d0e0f --tone 440
cargo run --release --features srtp --example intercom_client -- \
    --server 127.0.0.1:5004 --id hall --name Hallway \
    --kek 101112131415161718191a1b1c1d1e1f --tone 0 --out heard.raw

ffplay -f s16be -ar 16000 -ch_layout mono heard.raw
```

The hallway hears the kitchen's tone, one second on, one off. The kitchen
hears nothing: nobody else is talking, and nobody hears themselves. Every
10 s the server logs who is in the room and how each jitter buffer is
doing.

Generate real KEKs with a random source, for example
`openssl rand -hex 16`, and keep `devices.txt` private.

A Raspberry Pi Pico W can join the same room: see
[`examples/pico-w`](../examples/pico-w/README.md).

## The room

- **Mix minus.** Each 20 ms tick, the server plays every participant's
  jitter buffer (with concealment), sums them, and sends each participant
  the sum minus its own audio, clamped to 16 bits.
- **Silence is free.** If nobody else is talking, a participant gets no
  packet that tick.
- **Keep-alive.** Both sides send RTCP every 5 s. The server drops a
  participant it hasn't heard from in 15 s. A device that hasn't heard the
  server in 15 s joins again. A BYE leaves at once.
- **Names** come from each device's RTCP SDES NAME.

## Joining

This is the example's own signalling, not part of RTP. It follows the
crate's keying advice: fresh SRTP keys per session, delivered under a
per-device secret.

1. The device sends `JOIN <id>` (resent every second until answered).
2. The server makes two random master keys: one for what the device sends,
   one for what it hears. It replies `KEYS`, then both keys wrapped under
   the device's KEK (`srtp::wrap`, 80 bytes).
3. The device unwraps them and sends RTCP straight away.
4. The first packet that authenticates makes the participant active. Only
   then is it mixed and sent audio.

So a stranger using someone else's id gets a reply it can't read, and no
audio. A device that joins again (a reboot, a new port) gets new keys and
replaces its old session. `JOIN` and `KEYS` start with bytes that can't be
RTP or RTCP, so everything shares one port.

`--insecure` also admits anyone who sends plain RTP or RTCP, unencrypted.
It exists for testing with tools like ffmpeg, on a trusted network only.

## Keeping it fast

The crate's part is small: about 1.7 µs per participant per tick with
SRTP, almost all of it the SHA-1 and AES of `unprotect` and `protect`. The
whole server tick, without the socket calls, measures (`cargo bench
--features srtp --bench mixer`, everyone talking):

| Participants | Per 20 ms tick | Share of a core |
|---|---|---|
| 2 | 3.2 µs | 0.02% |
| 8 | 13 µs | 0.07% |
| 32 | 56 µs | 0.3% |

What usually costs more is around it:

- **Allocation.** The example allocates nothing per audio packet or tick:
  participants are boxed once on joining, and mixing and sending use fixed
  buffers.
- **Socket calls.** One `send_to` per participant per tick. Past a few
  hundred participants, batch them with `sendmmsg` (Linux, for example via
  the `socket2` crate).
- **Tick timing.** A late tick adds straight to what everyone hears. The
  example wakes on each datagram or tick, whichever is first, rather than
  sleeping blind, and logs its worst lateness. On a busy or virtual host,
  give the process real-time priority or a dedicated core.
- **CPU target.** `RUSTFLAGS="-C target-cpu=x86-64-v3"` (AVX2) speeds up
  L16 decoding and concealment where the host supports it.

## Tests

`cargo test --features srtp --example mixing_server` runs the room over a
simulated network:

- each participant hears exactly the others, never itself;
- silence sends no audio;
- strangers, replays and forgeries get nowhere;
- BYE, timeouts and rejoins;
- names from SDES;
- insecure mode.

`tests/interop.rs` runs the server over real UDP with ffmpeg as a
participant. ffmpeg's audio reaches a listener exactly, and ffmpeg decodes
the mix to exactly what was said.
