//! The echo suppressor (issue #9), measured on synthetic signals rather
//! than read back from its own state:
//!
//! - Playback below the threshold leaves the microphone bit for bit
//!   unchanged; above it, the microphone comes out attenuated by the
//!   configured amount, against a floating-point decibel reference.
//! - Attack, hold and release take the configured number of samples,
//!   measured from the output of a constant microphone signal.
//! - Full-scale and extreme inputs never overflow, whatever the config.
//! - Processing in blocks of any size gives the same output as one block.

mod common;

use common::Rng;
use embedded_rtp::duck::{gain_from_db, Config, Ducker, UNITY};

/// A steady microphone signal: full scale negative, so each output sample
/// is exactly the gain applied to it, negated.
const DC: i16 = i16::MIN;

fn tone(len: usize, amplitude: f64, hz: f64) -> Vec<i16> {
    (0..len)
        .map(|i| (amplitude * (2.0 * std::f64::consts::PI * hz * i as f64 / 16_000.0).sin()) as i16)
        .collect()
}

/// The gain applied to each sample, in Q15, as seen in the output of a
/// steady microphone signal.
fn gains(ducker: &mut Ducker, played: &[i16]) -> Vec<i32> {
    let mut mic = vec![DC; played.len()];
    ducker.process(played, &mut mic);
    mic.iter().map(|&s| -i32::from(s)).collect()
}

fn db(ratio: f64) -> f64 {
    20.0 * ratio.log10()
}

#[test]
fn quiet_playback_leaves_the_microphone_untouched() {
    let config = Config::DEFAULT;
    let mut rng = Rng::new(1);
    let mut ducker = Ducker::new(config);
    for _ in 0..50 {
        let played: Vec<i16> = (0..320)
            .map(|_| {
                let level = rng.below(u32::from(config.threshold)) as i16;
                if rng.chance(500) {
                    -level
                } else {
                    level
                }
            })
            .collect();
        let mic: Vec<i16> = (0..320).map(|_| rng.next_u32() as i16).collect();
        let mut out = mic.clone();
        ducker.process(&played, &mut out);
        assert_eq!(out, mic);
        assert!(!ducker.is_ducking());
    }
}

#[test]
fn loud_playback_attenuates_by_the_configured_amount() {
    for attenuation in [6u8, 12, 24, 40] {
        let config = Config {
            ducked_gain: gain_from_db(attenuation),
            ..Config::DEFAULT
        };
        let mut ducker = Ducker::new(config);
        let played = tone(16_000, 8_000.0, 440.0);
        let mic = tone(16_000, 12_000.0, 1_000.0);
        let mut out = mic.clone();
        ducker.process(&played, &mut out);

        // After the attack, the microphone is down by `attenuation` dB.
        let settled = config.attack as usize;
        let power = |s: &[i16]| s.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
        let ratio = (power(&out[settled..]) / power(&mic[settled..])).sqrt();
        let measured = -db(ratio);
        assert!(
            (measured - f64::from(attenuation)).abs() < 0.1,
            "{attenuation} dB configured, {measured:.3} dB measured"
        );
    }
}

#[test]
fn gain_from_db_matches_decibels() {
    for attenuation in 0..=60u8 {
        let gain = f64::from(gain_from_db(attenuation)) / f64::from(UNITY);
        assert!(
            (-db(gain) - f64::from(attenuation)).abs() < 0.1,
            "{attenuation} dB gives {:.3} dB",
            -db(gain)
        );
    }
    assert_eq!(gain_from_db(0), UNITY);
}

#[test]
fn attack_hold_and_release_take_the_configured_time() {
    let config = Config {
        threshold: 1_000,
        ducked_gain: gain_from_db(20),
        attack: 40,
        hold: 800,
        release: 1_200,
    };
    let ducked = i32::from(config.ducked_gain);
    let one = i32::from(UNITY);
    // Silence, then a tone, then silence.
    let start = 2_000;
    let mut played = vec![0i16; start];
    played.extend(tone(4_000, 6_000.0, 300.0));
    played.extend(vec![0i16; 6_000]);
    let first_loud = played
        .iter()
        .position(|s| s.unsigned_abs() >= 1_000)
        .unwrap();
    let last_loud = played
        .iter()
        .rposition(|s| s.unsigned_abs() >= 1_000)
        .unwrap();

    let gain = gains(&mut Ducker::new(config), &played);
    // Untouched before the speaker plays.
    assert!(gain[..first_loud].iter().all(|&g| g == one));
    // Fully down within `attack` samples of the first loud one, not before.
    let attack = config.attack as usize;
    assert!(gain[first_loud + attack - 2] > ducked);
    assert!(gain[first_loud + attack - 1..=last_loud]
        .iter()
        .all(|&g| g == ducked));
    // Held for `hold` samples after the last loud one.
    let hold_end = last_loud + config.hold as usize;
    assert!(gain[last_loud..=hold_end].iter().all(|&g| g == ducked));
    assert!(gain[hold_end + 1] > ducked);
    // Back to unity within `release` samples (the step is rounded up, so
    // a little sooner), rising throughout.
    let release_end = hold_end + config.release as usize;
    assert!(gain[hold_end..=release_end]
        .windows(2)
        .all(|w| w[1] >= w[0]));
    assert!(gain[hold_end + config.release as usize * 98 / 100] < one);
    assert!(gain[release_end..].iter().all(|&g| g == one));
    // About half way up at half the release.
    let midpoint = gain[hold_end + config.release as usize / 2];
    let half = (one + ducked) / 2;
    assert!((midpoint - half).abs() < one / 100, "{midpoint} vs {half}");
}

#[test]
fn zero_times_switch_at_once() {
    let config = Config {
        attack: 0,
        hold: 0,
        release: 0,
        ..Config::DEFAULT
    };
    let ducked = i32::from(config.ducked_gain);
    let mut played = vec![0i16; 100];
    played[40] = 5_000;
    played[41] = 5_000;
    let gain = gains(&mut Ducker::new(config), &played);
    let one = i32::from(UNITY);
    assert!(gain[..40].iter().all(|&g| g == one));
    assert_eq!(&gain[40..42], &[ducked, ducked]);
    assert!(gain[42..].iter().all(|&g| g == one));
}

#[test]
fn speech_gaps_inside_the_hold_stay_down() {
    // Syllables 150 ms apart, with a 100 ms hold: gaps under the hold stay
    // ducked, so the microphone doesn't pump between syllables.
    let config = Config::DEFAULT;
    let mut played = Vec::new();
    for _ in 0..5 {
        played.extend(tone(800, 6_000.0, 200.0)); // 50 ms of sound
        played.extend(vec![0i16; 1_200]); // 75 ms gap
    }
    let gain = gains(&mut Ducker::new(config), &played);
    let ducked = i32::from(config.ducked_gain);
    let first_loud = played
        .iter()
        .position(|s| s.unsigned_abs() >= config.threshold)
        .unwrap();
    assert!(gain[first_loud + config.attack as usize..9_000]
        .iter()
        .all(|&g| g == ducked));
}

#[test]
fn extremes_never_overflow() {
    let mut rng = Rng::new(9);
    let edges = [i16::MIN, i16::MIN + 1, -1, 0, 1, i16::MAX - 1, i16::MAX];
    for ducked_gain in [0, 1, gain_from_db(24), UNITY - 1, UNITY] {
        for (attack, hold, release) in [
            (0, 0, 0),
            (1, 1, 1),
            (32, 1_600, 1_600),
            (u32::MAX, u32::MAX, u32::MAX),
        ] {
            let config = Config {
                threshold: rng.next_u32() as u16,
                ducked_gain,
                attack,
                hold,
                release,
            };
            let mut ducker = Ducker::new(config);
            for _ in 0..20 {
                let pick = |rng: &mut Rng| {
                    if rng.chance(300) {
                        edges[rng.below(edges.len() as u32) as usize]
                    } else {
                        rng.next_u32() as i16
                    }
                };
                let played: Vec<i16> = (0..320).map(|_| pick(&mut rng)).collect();
                let mic: Vec<i16> = (0..320).map(|_| pick(&mut rng)).collect();
                let mut out = mic.clone();
                ducker.process(&played, &mut out);
                for (&before, &after) in mic.iter().zip(&out) {
                    // Never louder, never flipped in sign.
                    assert!(after.unsigned_abs() <= before.unsigned_abs());
                    assert!(after == 0 || (after < 0) == (before < 0));
                }
                assert!(ducker.gain() >= ducked_gain && ducker.gain() <= UNITY);
            }
        }
    }
}

#[test]
fn block_size_does_not_matter() {
    let mut rng = Rng::new(3);
    let mut played = tone(8_000, 7_000.0, 250.0);
    played.extend(vec![0i16; 8_000]);
    played.extend(tone(8_000, 3_000.0, 600.0));
    let mic: Vec<i16> = (0..played.len()).map(|_| rng.next_u32() as i16).collect();

    let mut whole = mic.clone();
    Ducker::new(Config::DEFAULT).process(&played, &mut whole);

    let mut blocks = mic.clone();
    let mut ducker = Ducker::new(Config::DEFAULT);
    let mut at = 0;
    while at < blocks.len() {
        let len = (1 + rng.below(700) as usize).min(blocks.len() - at);
        ducker.process(&played[at..at + len], &mut blocks[at..at + len]);
        at += len;
    }
    assert_eq!(blocks, whole);
}

#[test]
fn short_playback_counts_as_silence() {
    let config = Config {
        attack: 0,
        hold: 10,
        release: 20,
        ..Config::DEFAULT
    };
    let (one, ducked) = (i32::from(UNITY), i32::from(config.ducked_gain));
    // Duck it, then give 5 samples of loud playback for 100 of microphone:
    // the other 95 count as silence, so it holds and then releases over
    // them, exactly as for 95 samples of real silence.
    let mut ducker = Ducker::new(config);
    gains(&mut ducker, &[20_000; 10]);
    let mut mic = vec![DC; 100];
    let mut reference = mic.clone();
    let mut twin = ducker.clone();
    ducker.process(&[20_000; 5], &mut mic);
    let mut padded = vec![0i16; 100];
    padded[..5].fill(20_000);
    twin.process(&padded, &mut reference);
    assert_eq!(mic, reference);
    let gain: Vec<i32> = mic.iter().map(|&s| -i32::from(s)).collect();
    // Loud to sample 4, held for 10 more, releasing from sample 15.
    assert!(gain[..15].iter().all(|&g| g == ducked));
    assert!(gain[15] > ducked);
    assert!(gain[35..].iter().all(|&g| g == one));
    // And none at all: the same as silence.
    let mut none = vec![DC; 50];
    Ducker::new(config).process(&[], &mut none);
    assert!(none.iter().all(|&s| s == DC));
}

#[test]
fn zero_threshold_never_ducks_on_silence() {
    let config = Config {
        threshold: 0,
        attack: 0,
        hold: 0,
        release: 0,
        ..Config::DEFAULT
    };
    let (one, ducked) = (i32::from(UNITY), i32::from(config.ducked_gain));
    let gain = gains(&mut Ducker::new(config), &[0, 0, 1, -1, 0]);
    assert_eq!(gain, [one, one, ducked, ducked, one]);
}

#[test]
fn long_fades_take_as_long_as_asked() {
    // A 24 dB fade over a minute (960,000 samples) at 16 kHz: halfway at
    // half the time, and down within the configured time, not much sooner.
    let attack = 960_000;
    let config = Config {
        attack,
        hold: u32::MAX,
        ..Config::DEFAULT
    };
    let (one, ducked) = (i32::from(UNITY), i32::from(config.ducked_gain));
    let played = vec![20_000i16; attack as usize + 1_000];
    let gain = gains(&mut Ducker::new(config), &played);
    let half = (one + ducked) / 2;
    let midpoint = gain[attack as usize / 2];
    assert!(
        (midpoint - half).abs() <= 2,
        "{midpoint} at half time, {half} expected"
    );
    let reached = gain.iter().position(|&g| g == ducked).unwrap();
    assert!(
        reached <= attack as usize && reached >= attack as usize * 999 / 1_000,
        "down after {reached} samples, {attack} configured"
    );
}

#[test]
fn builds_in_a_static() {
    static DUCKER: std::sync::Mutex<Ducker> = std::sync::Mutex::new(Ducker::new(Config::DEFAULT));
    let mut mic = [DC; 4];
    DUCKER.lock().unwrap().process(&[20_000; 4], &mut mic);
    assert!(mic[3] > DC);
}

#[test]
fn the_threshold_itself_counts_as_loud() {
    let config = Config {
        attack: 0,
        hold: 0,
        release: 0,
        ..Config::DEFAULT
    };
    let below = config.threshold as i16 - 1;
    let at = config.threshold as i16;
    let gain = gains(&mut Ducker::new(config), &[below, -below, at, -at, below]);
    let (one, ducked) = (i32::from(UNITY), i32::from(config.ducked_gain));
    assert_eq!(gain, [one, one, ducked, ducked, one]);
}
