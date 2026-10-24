//! Clock drift correction (issue #10), by simulation.
//!
//! A sender produces 20 ms frames of audio on its clock, a network delays
//! them with jitter, and a receiver plays out on its own clock, up to
//! 200 ppm faster or slower, for a simulated hour. The simulation knows the
//! true clock difference and the true signal, so it checks:
//!
//! - drift alone causes no whole-frame drains and no underruns, and the
//!   averaged depth error stays within a frame; without the corrector the
//!   same runs do drain or underrun, so the test means something;
//! - the net samples dropped or repeated match the clocks' real difference;
//! - every sample comes out once and in order, apart from those corrections;
//! - a correction never makes a step larger than the signal's own steepest;
//! - with no drift, corrections stop and the audio passes through bit for
//!   bit.

mod common;

use common::Rng;
use embedded_rtp::drift::DriftCorrector;
use embedded_rtp::jitter::{Config, Counters};
use embedded_rtp::l16::{self, Concealer};
use embedded_rtp::packet::Packet;
use embedded_rtp::{Receiver, Sender};

const N: usize = 320;
const BYTES: usize = 2 * N;
const PT: u8 = 96;
/// An hour of 20 ms frames.
const HOUR: u64 = 180_000;

struct Run {
    /// Receiver clock against the sender's: +100 is 100 ppm slow (it plays
    /// less often, so audio piles up), -100 fast.
    ppm: f64,
    frames: u64,
    /// Network jitter: extra delay uniformly up to this many ticks.
    spread: f64,
    correct: bool,
    /// Talkspurts: the sender goes quiet for a second in every ten.
    talkspurts: bool,
    /// Sample `i` of the sent signal.
    signal: fn(u64) -> i16,
}

struct Result {
    out: Vec<i16>,
    counters: Counters,
    dropped: u32,
    repeated: u32,
    /// The corrector's averaged error each frame, once settled.
    worst_error: i32,
    /// Frame periods played, on the receiver's clock.
    periods: u64,
}

fn simulate(run: &Run) -> Result {
    let mut rng = Rng::new(7);
    let mut sender = Sender::new(0xd1f7, PT, N as u32, 500, 0);
    let mut receiver = Receiver::<16, BYTES>::new(PT, N as u32, Config::DEFAULT);
    let mut concealer = Concealer::<N>::new();
    let mut corrector = DriftCorrector::<N>::new();
    let period = N as f64 * (1.0 + run.ppm * 1e-6);
    let base_delay = 1_000.0;

    // Packets in arrival order: (arrival time on the sender's clock, bytes).
    let mut packets: Vec<(f64, Vec<u8>)> = (0..run.frames)
        .filter_map(|frame| {
            if run.talkspurts && frame % 500 >= 450 {
                sender.skip();
                return None;
            }
            let samples: Vec<i16> = (0..N as u64)
                .map(|i| (run.signal)(frame * N as u64 + i))
                .collect();
            let mut payload = [0u8; BYTES];
            l16::encode(&samples, &mut payload);
            let mut buf = vec![0u8; 12 + BYTES];
            let len = sender.send(&payload, &mut buf).unwrap();
            buf.truncate(len);
            let arrival = frame as f64 * N as f64
                + base_delay
                + rng.below(1_000) as f64 / 1_000.0 * run.spread;
            Some((arrival, buf))
        })
        .collect();
    packets.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut next = 0;
    let mut out = Vec::new();
    let mut worst_error = 0;
    let settle = 15_000; // five minutes
    let periods = (run.frames as f64 * N as f64 / period) as u64 - 10;
    for tick in 0..periods {
        let now = tick as f64 * period;
        while next < packets.len() && packets[next].0 <= now {
            let local_arrival = (packets[next].0 / (1.0 + run.ppm * 1e-6)) as u32;
            receiver.receive(&Packet::parse(&packets[next].1).unwrap(), local_arrival);
            next += 1;
        }
        let samples = if run.correct {
            corrector.play(&mut receiver, &mut concealer).copied()
        } else {
            concealer.play(receiver.play()).copied()
        };
        out.extend_from_slice(&samples.unwrap_or([0; N]));
        if tick > settle {
            worst_error = worst_error.max(corrector.error().abs());
        }
    }
    let corrections = corrector.corrections();
    Result {
        out,
        counters: receiver.counters(),
        dropped: corrections.dropped,
        repeated: corrections.repeated,
        worst_error,
        periods,
    }
}

fn tone(i: u64) -> i16 {
    (8_000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 16_000.0).sin()) as i16
}

/// A ramp: consecutive samples differ by one, so every dropped sample shows
/// as a step of two and every repeat as a step of zero.
fn ramp(i: u64) -> i16 {
    (i % 30_000) as i16
}

#[test]
fn an_hour_of_drift_causes_no_drains_or_underruns() {
    for ppm in [-200.0, -100.0, -30.0, 30.0, 100.0, 200.0] {
        let result = simulate(&Run {
            ppm,
            frames: HOUR,
            spread: 400.0,
            correct: true,
            talkspurts: false,
            signal: tone,
        });
        assert_eq!(result.counters.drained, 0, "{ppm} ppm drained a frame");
        assert_eq!(result.counters.underruns, 0, "{ppm} ppm ran dry");
        assert_eq!(result.counters.overflow, 0, "{ppm} ppm overflowed");
        assert!(
            result.worst_error < N as i32,
            "{ppm} ppm: depth error reached {} samples",
            result.worst_error
        );

        // The net correction is the clocks' real difference (the samples
        // the sender made in the time the receiver played, minus those
        // played), less whatever the audio waiting grew by from the first
        // frame to the last: up to 1.5 frames building up to the setpoint
        // and the standing error at the end, so within 3 frames.
        let played = result.periods as f64 * N as f64;
        let expected = played * ppm * 1e-6;
        let net = f64::from(result.dropped) - f64::from(result.repeated);
        assert!(
            (net - expected).abs() < 3.0 * N as f64,
            "{ppm} ppm: net correction {net}, clocks differ by {expected:.0}"
        );
        // And it doesn't hunt: few corrections go the wrong way.
        let wrong_way = result.dropped.min(result.repeated);
        assert!(
            wrong_way < 600,
            "{ppm} ppm: {wrong_way} corrections the wrong way"
        );
    }
}

#[test]
fn without_correction_the_same_drift_drains_or_underruns() {
    for ppm in [-200.0, 200.0] {
        let result = simulate(&Run {
            ppm,
            frames: HOUR,
            spread: 400.0,
            correct: false,
            talkspurts: false,
            signal: tone,
        });
        let c = result.counters;
        assert!(
            c.drained + c.underruns > 10,
            "{ppm} ppm uncorrected: {} drained, {} underruns",
            c.drained,
            c.underruns
        );
    }
}

#[test]
fn every_sample_comes_out_once_and_in_order_bar_the_corrections() {
    for ppm in [-200.0, 200.0] {
        let result = simulate(&Run {
            ppm,
            frames: 20_000,
            spread: 0.0,
            correct: true,
            talkspurts: false,
            signal: ramp,
        });
        // Skip the start (silence, then the concealer's fade-in).
        let start = result.out.iter().position(|&s| s > 100).unwrap() + 100;
        let (mut repeats, mut drops) = (0u32, 0u32);
        for pair in result.out[start..].windows(2) {
            match i32::from(pair[1]) - i32::from(pair[0]) {
                1 => {}
                0 => repeats += 1,
                2 => drops += 1,
                // The ramp wrapping round, possibly with a correction.
                step if step <= -29_997 => {}
                step => panic!("{ppm} ppm: a step of {step}"),
            }
        }
        assert!(drops + repeats > 0);
        assert!(drops <= result.dropped && repeats <= result.repeated);
        assert!(result.dropped - drops <= 2 && result.repeated - repeats <= 2);
    }
}

#[test]
fn corrections_never_step_further_than_the_signal() {
    for hz in [100.0, 440.0, 1_000.0, 3_000.0] {
        for ppm in [-200.0, 200.0] {
            let amplitude = 10_000.0;
            let signal = match hz as u32 {
                100 => |i: u64| {
                    (10_000.0 * (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 16_000.0).sin())
                        as i16
                },
                440 => |i: u64| {
                    (10_000.0 * (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 16_000.0).sin())
                        as i16
                },
                1_000 => |i: u64| {
                    (10_000.0 * (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / 16_000.0).sin())
                        as i16
                },
                _ => |i: u64| {
                    (10_000.0 * (2.0 * std::f64::consts::PI * 3_000.0 * i as f64 / 16_000.0).sin())
                        as i16
                },
            };
            let result = simulate(&Run {
                ppm,
                frames: 12_000,
                spread: 0.0,
                correct: true,
                talkspurts: false,
                signal,
            });
            assert!(result.dropped + result.repeated > 0);
            let start = result.out.iter().position(|&s| s != 0).unwrap() + 2 * N;
            let steepest = amplitude * 2.0 * std::f64::consts::PI * hz / 16_000.0;
            let worst = result.out[start..]
                .windows(2)
                .map(|w| (i32::from(w[1]) - i32::from(w[0])).abs())
                .max()
                .unwrap();
            assert!(
                f64::from(worst) <= steepest + 2.0,
                "{hz} Hz at {ppm} ppm: a step of {worst}, the signal's steepest is {steepest:.0}"
            );
        }
    }
}

#[test]
fn without_drift_corrections_stop_and_audio_passes_through() {
    let result = simulate(&Run {
        ppm: 0.0,
        frames: 30_000,
        spread: 0.0,
        correct: true,
        talkspurts: false,
        signal: ramp,
    });
    // At most the half frame or so it takes to reach the setpoint.
    assert!(result.dropped + result.repeated <= N as u32);
    // The last half is the ramp exactly, delayed.
    let half = result.out.len() / 2;
    for pair in result.out[half..].windows(2) {
        let step = i32::from(pair[1]) - i32::from(pair[0]);
        assert!(step == 1 || step == -29_999, "a step of {step}");
    }
}

#[test]
fn never_worse_than_the_jitter_buffer_alone() {
    // From no jitter to jitter that fills the buffer to its ceiling: drift
    // or not, the corrector never adds drains, underruns, overflows or late
    // frames. (Near the ceiling it keeps out of the buffer's way.)
    for spread in [0.0, 400.0, 1_500.0] {
        for ppm in [-200.0, 0.0, 200.0] {
            let run = |correct| {
                let c = simulate(&Run {
                    ppm,
                    frames: 60_000,
                    spread,
                    correct,
                    talkspurts: false,
                    signal: tone,
                })
                .counters;
                c.drained + c.underruns + c.overflow + c.late
            };
            let (corrected, alone) = (run(true), run(false));
            assert!(
                corrected <= alone + 3,
                "spread {spread}, {ppm} ppm: {corrected} glitches corrected, {alone} without"
            );
        }
    }
}

#[test]
fn jitter_alone_hardly_makes_corrections() {
    // No drift, 25 ms of jitter, an hour: after building up to the setpoint
    // (up to 1.5 frames), jitter mustn't register as drift.
    let result = simulate(&Run {
        ppm: 0.0,
        frames: HOUR,
        spread: 400.0,
        correct: true,
        talkspurts: false,
        signal: tone,
    });
    let total = result.dropped + result.repeated;
    assert!(
        total < 2 * N as u32 + 200,
        "{} dropped and {} repeated with no drift",
        result.dropped,
        result.repeated
    );
}

#[test]
fn silences_between_talkspurts_do_not_wind_the_loop_up() {
    // The sender goes quiet for a second in every ten, so the buffer runs
    // dry and prefills each time. Silence is nothing to measure: the
    // corrector must neither count it as missing audio nor drain or
    // underrun beyond the silences themselves.
    for ppm in [-200.0, 0.0, 200.0] {
        let result = simulate(&Run {
            ppm,
            frames: 60_000,
            spread: 400.0,
            correct: true,
            talkspurts: true,
            signal: tone,
        });
        let spurts = 60_000 / 500;
        assert_eq!(result.counters.drained, 0, "{ppm} ppm");
        assert!(
            result.counters.underruns <= spurts,
            "{ppm} ppm: {:?}",
            result.counters
        );
        // Each talkspurt starts at the buffer's own level and builds up to
        // the setpoint, about 100 samples of repeats here, plus drift. A
        // corrector that measured the silences as missing audio would wind
        // up and repeat about half as much again.
        let played = result.periods as f64 * N as f64;
        let drift = (played * ppm * 1e-6).abs();
        let total = f64::from(result.dropped + result.repeated);
        let bound = drift + spurts as f64 * 0.4 * N as f64;
        assert!(
            total < bound,
            "{ppm} ppm: {} dropped, {} repeated; at most {bound:.0} expected",
            result.dropped,
            result.repeated
        );
    }
}

#[test]
fn no_headroom_means_no_corrections() {
    // A buffer held at exactly one frame (min = initial = max = 1) leaves
    // nothing to correct into: with no drift it must neither drop samples
    // nor run dry.
    let config = Config {
        initial_depth: 1,
        min_depth: 1,
        max_depth: 1,
    };
    let mut sender = Sender::new(1, PT, N as u32, 0, 0);
    let mut receiver = Receiver::<16, BYTES>::new(PT, N as u32, config);
    let mut concealer = Concealer::<N>::new();
    let mut corrector = DriftCorrector::<N>::new();
    let samples: Vec<i16> = (0..N as u64).map(tone).collect();
    let mut payload = [0u8; BYTES];
    l16::encode(&samples, &mut payload);
    for tick in 0..10_000u32 {
        let mut buf = [0u8; 12 + BYTES];
        let len = sender.send(&payload, &mut buf).unwrap();
        receiver.receive(&Packet::parse(&buf[..len]).unwrap(), tick * N as u32);
        corrector.play(&mut receiver, &mut concealer);
    }
    assert_eq!(receiver.counters().underruns, 0);
    let corrections = corrector.corrections();
    assert_eq!(
        corrections.dropped + corrections.repeated,
        0,
        "{corrections:?}"
    );
}
