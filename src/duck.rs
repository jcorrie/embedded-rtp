//! An echo suppressor for full-duplex devices: while the speaker is playing,
//! turn the microphone down.
//!
//! A device with a speaker and a microphone sends what it plays back into
//! the room, and the other end hears itself a moment later. The server's
//! mix-minus can't help: the echo is new audio as far as it knows.
//! [`Ducker`] works at the device. Whenever the signal going to the speaker
//! is loud enough to come back, it attenuates the microphone by a fixed
//! amount. It fades down quickly ([`Config::attack`]), stays down for a
//! while after the speaker goes quiet ([`Config::hold`]) to cover the room's
//! echo tail and the device's own buffering, then fades back up
//! ([`Config::release`]) so speech isn't chopped.
//!
//! ```
//! # use embedded_rtp::duck::{Config, Ducker};
//! let mut ducker = Ducker::new(Config::DEFAULT);
//! # let played = [0i16; 320];
//! # let mut captured = [0i16; 320];
//! // Every 20 ms, before sending the microphone:
//! ducker.process(&played, &mut captured);
//! ```
//!
//! This is a suppressor, not a canceller: while the far end talks, the near
//! end is quieter too, so talking over each other works less well than in
//! a phone call. It costs a few integer operations per sample, which a
//! Cortex-M0+ can spare; an adaptive canceller (#9) might not fit.
//!
//! All integer arithmetic; gains are Q15 fixed point and need no division
//! per sample.

/// Unity gain in Q15.
pub const UNITY: u16 = 1 << 15;
const ONE: i32 = UNITY as i32;

/// `10^(-1/20)` in Q15: one decibel down.
const ONE_DB_DOWN: u32 = 29_205;

/// A Q15 gain `db` decibels below unity, for [`Config::ducked_gain`]: 6 is
/// about half, 20 a tenth, 40 a hundredth. Exact to within 0.1 dB up to
/// 60 dB.
#[must_use]
pub const fn gain_from_db(db: u8) -> u16 {
    let mut gain = UNITY as u32;
    let mut i = 0;
    while i < db {
        gain = (gain * ONE_DB_DOWN + (1 << 14)) >> 15;
        i += 1;
    }
    gain as u16
}

/// How a [`Ducker`] behaves. Times are in samples: at 16 kHz, 16 a
/// millisecond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Playback level, as a sample's magnitude, at or above which the
    /// microphone is turned down. Below it, playback is too quiet to come
    /// back as an audible echo.
    pub threshold: u16,
    /// Microphone gain while turned down, Q15, at most [`UNITY`]. See
    /// [`gain_from_db`].
    pub ducked_gain: u16,
    /// Samples to fade from unity down to `ducked_gain`. Zero switches at
    /// once.
    pub attack: u32,
    /// Samples the microphone stays down after the last playback sample at
    /// or above `threshold`. It has to cover the delay from handing a frame
    /// to the speaker to hearing it in the microphone (the device's audio
    /// buffers and the air) plus the room's echo tail.
    pub hold: u32,
    /// Samples to fade from `ducked_gain` back up to unity after the hold.
    /// Zero switches at once.
    pub release: u32,
}

impl Config {
    /// For the intercom profile, 16 kHz: playback above about -30 dBFS
    /// turns the microphone down by 24 dB within 2 ms, and it comes back
    /// over 100 ms once the speaker has been quiet for 100 ms.
    pub const DEFAULT: Self = Self {
        threshold: 1_024,
        ducked_gain: gain_from_db(24),
        attack: 32,
        hold: 1_600,
        release: 1_600,
    };
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Turns a microphone down while a speaker plays. Memory is a few words.
#[derive(Debug, Clone)]
pub struct Ducker {
    threshold: u16,
    ducked: i32,
    hold: u32,
    attack_step: i32,
    release_step: i32,
    /// Current gain, Q15.
    gain: i32,
    /// Samples of hold left.
    holding: u32,
}

impl Default for Ducker {
    fn default() -> Self {
        Self::new(Config::DEFAULT)
    }
}

impl Ducker {
    /// A ducker at unity gain.
    ///
    /// # Panics
    ///
    /// If `config.ducked_gain` is above [`UNITY`]; in a `const` or `static`
    /// that is a compile error.
    #[must_use]
    pub const fn new(config: Config) -> Self {
        assert!(config.ducked_gain <= UNITY, "ducked_gain is at most UNITY");
        let ducked = config.ducked_gain as i32;
        Self {
            threshold: config.threshold,
            ducked,
            hold: config.hold,
            attack_step: step(ONE - ducked, config.attack),
            release_step: step(ONE - ducked, config.release),
            gain: ONE,
            holding: 0,
        }
    }

    /// Process one block: `played` is what goes to the speaker, `mic` what
    /// the microphone captured over the same period, turned down in place
    /// as needed. At unity gain a sample passes through unchanged. If
    /// `played` is shorter than `mic`, the rest counts as silence.
    pub fn process(&mut self, played: &[i16], mic: &mut [i16]) {
        let mut played = played.iter();
        for sample in mic {
            let level = played.next().map_or(0, |p| p.unsigned_abs());
            // Down while playing and for `hold` samples after, else back up.
            let down = if level >= self.threshold {
                self.holding = self.hold;
                true
            } else if self.holding > 0 {
                self.holding -= 1;
                true
            } else {
                false
            };
            if down {
                self.gain = (self.gain - self.attack_step).max(self.ducked);
            } else {
                self.gain = (self.gain + self.release_step).min(ONE);
            }
            // |sample| * gain <= 2^15 * 2^15: no overflow, and the result
            // fits an i16 since gain <= unity.
            *sample = ((i32::from(*sample) * self.gain) >> 15) as i16;
        }
    }

    /// The current microphone gain, Q15.
    #[must_use]
    pub fn gain(&self) -> u16 {
        self.gain as u16
    }

    /// Whether the microphone is turned down at all.
    #[must_use]
    pub fn is_ducking(&self) -> bool {
        self.gain < ONE
    }
}

/// The per-sample step to cover `span` in `samples` samples, rounded up so
/// the fade never takes longer.
const fn step(span: i32, samples: u32) -> i32 {
    if samples == 0 || span == 0 {
        return ONE;
    }
    let samples = if samples > ONE as u32 {
        ONE
    } else {
        samples as i32
    };
    let step = (span + samples - 1) / samples;
    if step < 1 {
        1
    } else {
        step
    }
}
