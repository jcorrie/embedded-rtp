# Test fixtures

Packets produced by independent RTP implementations, so the default
`cargo test` run checks the crate against something other than its own
writer, even on machines without the tools installed.

## `ffmpeg-l16-16000.bin`

The first three RTP packets ffmpeg's RTP muxer sent for one second of
`common::test_signal` audio, as L16/16000/1 on payload type 96 (ffmpeg 6.1.1).
Each packet is prefixed with its length as a big-endian `u16`.

To regenerate (needs ffmpeg):

```sh
UPDATE_FIXTURES=1 cargo test --test interop ffmpeg_sends_l16_we_parse -- --ignored
```

ffmpeg picks a random SSRC, starting sequence number and starting timestamp,
so the file changes on every regeneration. The tests only depend on the
relationships between packets and on the decoded audio.

## `ffmpeg-rtcp.bin`

The two RTCP datagrams ffmpeg's RTP muxer (6.1.1) sent while streaming 1.5 s
of a 440 Hz sine as L16/16000/1, with `-cname embedded-rtp-test -rtpflags
send_bye`:

1. at the start: SR (0 packets, 0 octets) + SDES CNAME;
2. at the end: SR (47 packets, 48,000 octets) + SDES CNAME + BYE.

Same length-prefixed format. To regenerate (needs ffmpeg):

```sh
UPDATE_FIXTURES=1 cargo test --test interop ffmpeg_rtcp -- --ignored
```

## `ffmpeg-g722.bin`

ffmpeg's G.722 encoding (6.1.1, `-c:a g722`) of `common::g722_test_input`:
8000 code bytes, then ffmpeg's decoding of those codes as 16,000 big-endian
samples. ffmpeg's encoder and decoder are bit-exact with the ITU-T reference
on this input; they depart from it only at extremes the input avoids (see
`itu_reference_g722_on_hard_signals` in `tests/interop.rs`).

To regenerate (needs ffmpeg; the file only changes if ffmpeg does):

```sh
UPDATE_FIXTURES=1 cargo test --test interop ffmpeg_g722 -- --ignored
```
