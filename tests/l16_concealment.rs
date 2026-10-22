//! L16 and loss concealment conformance (issue #7).
//!
//! - Byte order matches an independent implementation, ffmpeg's `pcm_s16be`.
//! - Concealment never produces a step (click) larger than the bound its
//!   design implies, wherever losses fall in a waveform's phase. Naive
//!   concealment (silence for a lost frame) is shown to break that bound,
//!   so the bound is meaningful.
//! - Losses fade to silence by the third; full-scale audio never overflows.
//! - A clean stream passes through untouched.

mod common;

use common::{l16_bytes, test_signal, Rng};
use embedded_rtp::jitter::{Config, Playout};
use embedded_rtp::l16::{self, Concealer, FADE};
use embedded_rtp::packet::Packet;
use embedded_rtp::{Receiver, Sender};

const N: usize = 320;

#[test]
fn byte_order_matches_ffmpeg() {
    let packets = common::split_fixture(include_bytes!("fixtures/ffmpeg-l16-16000.bin"));
    let mut offset = 0;
    for bytes in packets {
        let payload = Packet::parse(bytes).unwrap().payload;
        let expected = test_signal(payload.len() / 2, offset);

        let mut decoded = vec![0i16; expected.len()];
        assert_eq!(l16::decode(payload, &mut decoded), expected.len());
        assert_eq!(decoded, expected, "decoding ffmpeg's bytes");

        let mut encoded = vec![0u8; payload.len()];
        assert_eq!(l16::encode(&expected, &mut encoded), payload.len());
        assert_eq!(encoded, payload, "encoding matches ffmpeg byte for byte");
        offset += expected.len();
    }
}

/// What a listener hears for a schedule of frame periods: `Some(i)` plays
/// frame `i` of `signal`, `None` loses it, and `Err(())` is the stream
/// running dry.
fn listen(signal: &[i16], schedule: &[Result<Option<usize>, ()>]) -> Vec<i16> {
    let mut concealer = Concealer::<N>::new();
    let mut heard = Vec::new();
    for period in schedule {
        let bytes;
        let playout = match period {
            Ok(Some(i)) => {
                bytes = l16_bytes(&signal[i * N..(i + 1) * N]);
                Playout::Frame {
                    sequence: *i as u16,
                    payload: &bytes,
                }
            }
            Ok(None) => Playout::Lost { sequence: 0 },
            Err(()) => Playout::Silence,
        };
        match concealer.play(playout) {
            Some(samples) => heard.extend_from_slice(samples),
            None => heard.extend(std::iter::repeat(0).take(N)),
        }
    }
    heard
}

fn max_step(samples: &[i16]) -> i32 {
    samples
        .windows(2)
        .map(|w| (i32::from(w[1]) - i32::from(w[0])).abs())
        .max()
        .unwrap_or(0)
}

fn sine(frames: usize, amplitude: f64, hz: f64) -> Vec<i16> {
    (0..frames * N)
        .map(|k| {
            (amplitude * (2.0 * std::f64::consts::PI * hz * k as f64 / 16_000.0).sin()).round()
                as i16
        })
        .collect()
}

#[test]
fn no_clicks_at_any_loss_phase() {
    const AMPLITUDE: f64 = 12_000.0;
    // 437 Hz does not divide the frame, so losses land at every phase.
    let signal = sine(400, AMPLITUDE, 437.0);
    let natural = max_step(&signal);
    // A blend moves at most 2A over FADE samples on top of the waveform's own
    // slope; the gain ramp adds at most A / N per sample.
    let bound = natural + 2 * AMPLITUDE as i32 / FADE as i32 + AMPLITUDE as i32 / N as i32 + 2;

    let mut rng = Rng::new(7);
    for _ in 0..50 {
        let mut schedule = Vec::new();
        let mut frame = 0;
        while frame < 400 {
            match rng.below(20) {
                // A run of 1-4 losses, then audio resumes.
                0 => {
                    for _ in 0..=rng.below(4) {
                        schedule.push(Ok(None));
                        frame += 1;
                    }
                }
                // The stream runs dry for a while.
                1 => {
                    for _ in 0..=rng.below(10) {
                        schedule.push(Err(()));
                    }
                }
                _ => {
                    schedule.push(Ok(Some(frame)));
                    frame += 1;
                }
            }
        }
        let heard = listen(&signal, &schedule);
        let step = max_step(&heard);
        assert!(
            step <= bound,
            "step {step} exceeds bound {bound} (natural {natural})"
        );
    }

    // Without concealment, a lost frame is a drop to silence and back.
    let naive: Vec<i16> = signal
        .chunks(N)
        .enumerate()
        .flat_map(|(i, frame)| {
            if i % 7 == 3 {
                vec![0; N]
            } else {
                frame.to_vec()
            }
        })
        .collect();
    assert!(
        max_step(&naive) > bound,
        "the bound would not catch a naive gap"
    );
}

#[test]
fn silent_by_the_third_loss() {
    let signal = sine(10, 20_000.0, 300.0);
    let schedule = [
        Ok(Some(0)),
        Ok(Some(1)),
        Ok(None),
        Ok(None),
        Ok(None),
        Ok(None),
    ];
    let heard = listen(&signal, &schedule);
    let frames: Vec<&[i16]> = heard.chunks(N).collect();
    let peak = |f: &[i16]| f.iter().map(|s| i32::from(*s).abs()).max().unwrap();
    assert!(
        peak(frames[2]) > 10_000,
        "the first loss still carries the sound"
    );
    // The second loss ramps from half amplitude down to nothing.
    assert!(
        peak(frames[3]) <= 10_000 + 100,
        "at most half amplitude on the second loss"
    );
    assert!(
        frames[3][N - 1].abs() <= 100,
        "and ends at silence: {}",
        frames[3][N - 1]
    );
    assert_eq!(peak(frames[4]), 0, "silent from the third");
    assert_eq!(peak(frames[5]), 0);
}

#[test]
fn full_scale_audio_never_overflows() {
    // A full-scale square wave, inverting every frame: every blend spans the
    // whole i16 range. Debug builds panic on any arithmetic overflow.
    let signal: Vec<i16> = (0..40 * N)
        .map(|k| if (k / N) % 2 == 0 { i16::MAX } else { i16::MIN })
        .collect();
    let mut schedule = Vec::new();
    for i in 0..40 {
        schedule.push(if i % 3 == 2 { Ok(None) } else { Ok(Some(i)) });
        if i % 11 == 10 {
            schedule.push(Err(()));
        }
    }
    let heard = listen(&signal, &schedule);
    assert_eq!(heard.len(), schedule.len() * N);
}

#[test]
fn clean_stream_through_sender_and_receiver_is_untouched() {
    let input = test_signal(50 * N, 0);
    let mut sender = Sender::new(1, 96, N as u32, 0, 0);
    let mut receiver = Receiver::<16, 640>::new(96, N as u32, Config::DEFAULT);
    let mut concealer = Concealer::<N>::new();
    let mut heard = Vec::new();
    let mut take = |receiver: &mut Receiver<16, 640>, heard: &mut Vec<i16>| {
        if let Some(samples) = concealer.play(receiver.play()) {
            heard.extend_from_slice(samples);
        }
    };
    for (i, frame) in input.chunks(N).enumerate() {
        let mut wire = [0u8; 652];
        let len = sender.send(&l16_bytes(frame), &mut wire).unwrap();
        receiver.receive(&Packet::parse(&wire[..len]).unwrap(), (i * N) as u32);
        take(&mut receiver, &mut heard);
    }
    for _ in 0..4 {
        take(&mut receiver, &mut heard);
    }
    // The tail is the fade-out when the stream runs dry.
    assert!(heard.len() >= input.len());
    assert_eq!(
        heard[FADE..input.len()],
        input[FADE..],
        "untouched after the opening fade-in"
    );
}
