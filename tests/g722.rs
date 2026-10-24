//! G.722 (ITU-T, 64 kbit/s sub-band ADPCM).
//!
//! The codec is checked bit for bit against ffmpeg's encoder and decoder,
//! from a captured fixture: ffmpeg's G.722 is itself bit-exact with the
//! ITU-T reference implementation, which `tests/interop.rs` checks along
//! with the reference's own test vectors in all three modes. The rest
//! checks properties the Recommendation implies: the embedded modes, the
//! codec delay and quality, and how decoding recovers from a lost frame.

mod common;

use common::{g722_test_input, Rng, G722_FIXTURE_CODES};
use embedded_rtp::g722::{Decoder, Encoder, Mode, CLOCK_RATE, PAYLOAD_TYPE, SAMPLE_RATE};
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::l16::Concealer;
use embedded_rtp::packet::Packet;
use embedded_rtp::profile::Codec;
use embedded_rtp::{Receiver, Sender};

/// Samples from the QMF analysis to the QMF synthesis.
const DELAY: usize = 22;

fn encode(samples: &[i16]) -> Vec<u8> {
    let mut codes = vec![0u8; samples.len() / 2];
    assert_eq!(Encoder::new().encode(samples, &mut codes), codes.len());
    codes
}

fn decode(codes: &[u8], mode: Mode) -> Vec<i16> {
    let mut samples = vec![0i16; codes.len() * 2];
    assert_eq!(
        Decoder::with_mode(mode).decode(codes, &mut samples),
        samples.len()
    );
    samples
}

fn fixture() -> (Vec<u8>, Vec<i16>) {
    let bytes = include_bytes!("fixtures/ffmpeg-g722.bin");
    let (codes, samples) = bytes.split_at(G722_FIXTURE_CODES);
    (codes.to_vec(), common::l16_samples(samples))
}

#[test]
fn encodes_exactly_as_ffmpeg() {
    let (codes, _) = fixture();
    let ours = encode(&g722_test_input());
    let first = ours.iter().zip(&codes).position(|(a, b)| a != b);
    assert_eq!(first, None, "first differing code");
}

#[test]
fn decodes_exactly_as_ffmpeg() {
    let (codes, samples) = fixture();
    let ours = decode(&codes, Mode::Kbps64);
    let first = ours.iter().zip(&samples).position(|(a, b)| a != b);
    assert_eq!(first, None, "first differing sample");
}

/// Encoding and decoding in pieces of any (even) size gives the same as in
/// one go: all the state carries over.
#[test]
fn pieces_make_no_difference() {
    let input = g722_test_input();
    let whole = encode(&input);
    let mut rng = Rng::new(1);
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let mut codes = Vec::new();
    let mut samples = Vec::new();
    let mut at = 0;
    while at < input.len() {
        let len = (2 * (1 + rng.below(200)) as usize).min(input.len() - at);
        let mut piece = vec![0u8; len / 2];
        encoder.encode(&input[at..at + len], &mut piece);
        let mut out = vec![0i16; len];
        decoder.decode(&piece, &mut out);
        codes.extend(piece);
        samples.extend(out);
        at += len;
    }
    assert_eq!(codes, whole);
    assert_eq!(samples, decode(&whole, Mode::Kbps64));
}

/// At 56 kbit/s the decoder ignores the lower band's last bit, at 48 its
/// last two: whatever is carried there makes no difference.
#[test]
fn lower_modes_ignore_the_data_bits() {
    let codes = encode(&g722_test_input());
    let mut rng = Rng::new(2);
    let scrambled: Vec<u8> = codes
        .iter()
        .map(|&c| c ^ (rng.next_u32() as u8 & 0x03))
        .collect();
    assert_eq!(
        decode(&codes, Mode::Kbps48),
        decode(&scrambled, Mode::Kbps48)
    );
    let one_bit: Vec<u8> = codes
        .iter()
        .zip(&scrambled)
        .map(|(&c, &s)| (c & !1) | (s & 1))
        .collect();
    assert_eq!(decode(&codes, Mode::Kbps56), decode(&one_bit, Mode::Kbps56));
    // And they do use the bits they keep.
    assert_ne!(decode(&codes, Mode::Kbps56), decode(&codes, Mode::Kbps48));
    assert_ne!(decode(&codes, Mode::Kbps64), decode(&codes, Mode::Kbps56));
}

/// The embedded property: the predictors adapt on 4 bits in every mode, so
/// a decoder that switches mode stays in step. Once the QMF's 24 samples
/// have passed, it is back to exactly what a 64 kbit/s decoder plays.
#[test]
fn switching_mode_keeps_the_decoder_in_step() {
    let codes = encode(&g722_test_input());
    let reference = decode(&codes, Mode::Kbps64);
    let mut decoder = Decoder::new();
    let mut out = vec![0i16; reference.len()];
    for (i, chunk) in codes.chunks(500).enumerate() {
        decoder.set_mode([Mode::Kbps48, Mode::Kbps64, Mode::Kbps56][i % 3]);
        decoder.decode(chunk, &mut out[i * 1000..]);
    }
    // Chunks 1, 4, 7... were decoded at 64 kbit/s.
    for chunk in (1..16).step_by(3) {
        let settled = chunk * 1000 + 24..(chunk + 1) * 1000;
        assert_eq!(out[settled.clone()], reference[settled], "chunk {chunk}");
    }
}

fn sine(len: usize, freq: u32, amplitude: f64) -> Vec<i16> {
    (0..len)
        .map(|i| {
            let phase = 2.0 * std::f64::consts::PI * f64::from(freq) * i as f64;
            (amplitude * (phase / f64::from(SAMPLE_RATE)).sin()).round() as i16
        })
        .collect()
}

/// Signal to error ratio in dB of `output` against `input` delayed by
/// `delay`, after the codec has adapted.
fn snr(input: &[i16], output: &[i16], delay: usize) -> f64 {
    let range = 4000..input.len() - delay;
    let signal: f64 = range.clone().map(|i| f64::from(input[i]).powi(2)).sum();
    let error: f64 = range
        .map(|i| (f64::from(output[i + delay]) - f64::from(input[i])).powi(2))
        .sum();
    10.0 * (signal / error).log10()
}

/// The codec delays the audio by 22 samples, 1.4 ms: the two 24-tap QMFs.
/// Three sines that don't share a short period, so no other delay fits.
#[test]
fn delay_is_the_qmf_pair() {
    let input: Vec<i16> = [(230, 4_000.0), (1000, 4_000.0), (1370, 4_000.0)]
        .iter()
        .map(|&(freq, amplitude)| sine(16_000, freq, amplitude))
        .fold(vec![0i16; 16_000], |sum, part| {
            sum.iter().zip(&part).map(|(a, b)| a + b).collect()
        });
    let output = decode(&encode(&input), Mode::Kbps64);
    let best = (0..40).max_by(|&a, &b| snr(&input, &output, a).total_cmp(&snr(&input, &output, b)));
    assert_eq!(best, Some(DELAY));
}

/// Quality on sine waves across the band. The lower band (to 4 kHz) gets 6
/// bits a sample, the higher only 2, which shows. Measured with the
/// reference-exact codec; the thresholds leave a few dB of room.
#[test]
fn quality_across_the_band() {
    for (freq, amplitude, at_least) in [
        (100, 10_000.0, 55.0),
        (300, 3_000.0, 50.0),
        (1000, 30_000.0, 42.0),
        (1000, 10_000.0, 42.0),
        (1000, 300.0, 30.0),
        (3000, 10_000.0, 35.0),
        (5000, 10_000.0, 18.0),
        (7000, 10_000.0, 18.0),
    ] {
        let input = sine(16_000, freq, amplitude);
        let output = decode(&encode(&input), Mode::Kbps64);
        let snr = snr(&input, &output, DELAY);
        assert!(snr > at_least, "{freq} Hz at {amplitude}: {snr:.1} dB");
    }
    // Fewer lower band bits, lower quality in the lower band.
    let input = sine(16_000, 1000, 10_000.0);
    let codes = encode(&input);
    let [q64, q56, q48] = [Mode::Kbps64, Mode::Kbps56, Mode::Kbps48]
        .map(|mode| snr(&input, &decode(&codes, mode), DELAY));
    assert!(
        q64 > q56 + 3.0 && q56 > q48 + 3.0,
        "{q64:.1} {q56:.1} {q48:.1}"
    );
}

/// Silence decodes to idle noise of the smallest quantizer steps, and
/// full-scale input decodes without overflow, close to the input.
#[test]
fn silence_and_full_scale() {
    assert!(decode(&encode(&[0; 3200]), Mode::Kbps64)
        .iter()
        .all(|&s| s.abs() <= 3));
    let input = sine(16_000, 440, 32_767.0);
    let output = decode(&encode(&input), Mode::Kbps64);
    assert!(snr(&input, &output, DELAY) > 40.0);
}

/// A decoder that misses a 20 ms frame has adapted differently from the
/// encoder; it catches up over the next frames, to within 30 dB of a
/// decoder that never missed one by 100 ms after, and exactly some time
/// after that.
#[test]
fn decoding_recovers_from_a_lost_frame() {
    let mut input = Vec::new();
    for k in 0..100 {
        input.extend(sine(320, 300 + 37 * k, 8_000.0));
    }
    let codes = encode(&input);
    let mut clean = Decoder::new();
    let mut lossy = Decoder::new();
    let mut a = [0i16; 320];
    let mut b = [0i16; 320];
    let mut snrs = Vec::new();
    for (frame, chunk) in codes.chunks(160).enumerate() {
        clean.decode(chunk, &mut a);
        if frame == 20 {
            continue;
        }
        lossy.decode(chunk, &mut b);
        if frame > 20 {
            let signal: f64 = a.iter().map(|&s| f64::from(s).powi(2)).sum();
            let error: f64 = a
                .iter()
                .zip(&b)
                .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
                .sum();
            snrs.push(if error == 0.0 {
                f64::INFINITY
            } else {
                10.0 * (signal / error).log10()
            });
        }
    }
    assert!(
        snrs[0] < 20.0,
        "the frame after a loss is off: {:.1} dB",
        snrs[0]
    );
    assert!(snrs[4] > 30.0, "within 30 dB by 100 ms: {:?}", &snrs[..6]);
    assert_eq!(snrs.last(), Some(&f64::INFINITY), "back in step");
}

/// G.722 through `Sender`, `Receiver` and the concealer, with RFC 3551's
/// 8 kHz clock: 160 ticks and 160 bytes a frame. A lost frame is concealed
/// in samples and decoding carries on.
#[test]
fn through_sender_and_receiver() {
    let codec = Codec::G722;
    assert_eq!(
        (codec.payload_type(), codec.clock_rate()),
        (PAYLOAD_TYPE, CLOCK_RATE)
    );
    let ticks = codec.frame_ticks();
    let mut sender = Sender::new(0xc0de, codec.payload_type(), ticks, 10, 20);
    let mut receiver: Receiver<8, 160> =
        Receiver::new(codec.payload_type(), ticks, Config::DEFAULT);
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let mut reference = Decoder::new();
    let mut concealer = Concealer::<320>::new();

    let input = g722_test_input();
    let mut played = Vec::new();
    let mut expected = Vec::new();
    for (i, frame) in input.chunks(320).enumerate() {
        let mut payload = [0u8; 160];
        encoder.encode(frame, &mut payload);
        let mut packet = [0u8; 12 + 160];
        let len = sender.send(&payload, &mut packet).unwrap();
        assert_eq!(len, codec.packet_bytes());
        let parsed = Packet::parse(&packet[..len]).unwrap();
        assert_eq!(parsed.header.timestamp, 20 + i as u32 * 160);
        if i != 10 {
            receiver.receive(&parsed, i as u32 * ticks);
            let mut out = [0i16; 320];
            reference.decode(&payload, &mut out);
            expected.push(Some(out));
        } else {
            expected.push(None);
        }
        if i >= 1 {
            let playout = receiver.play();
            let lost = matches!(playout, Playout::Lost { .. });
            let samples =
                concealer.play_decoded(playout, |payload, out| decoder.decode(payload, out));
            played.push((lost, samples.copied()));
        }
    }
    // Frame 10 was concealed, and the rest decoded as the reference did,
    // after the concealer's fade back in.
    assert!(played[10].0);
    for (i, (lost, samples)) in played.iter().enumerate().filter(|(i, _)| *i != 10) {
        assert!(!lost);
        let samples = samples.unwrap();
        let expected = expected[i].unwrap();
        let from = if i == 0 || i == 11 { 32 } else { 0 };
        assert_eq!(samples[from..], expected[from..], "frame {i}");
    }
}
