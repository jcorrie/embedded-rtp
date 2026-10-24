//! Clock drift correction, one sample at a time.
//!
//! A sender's and a receiver's audio clocks never run at exactly the same
//! rate: crystals are typically within 50 ppm, so two devices can differ by
//! 100 ppm. Left alone, a slow receiver's jitter buffer grows until it
//! drains a whole frame, and a fast one's runs dry. At 100 ppm either
//! happens every three or four minutes, and each is an audible click or gap.
//!
//! [`DriftCorrector`] sits after the [`Concealer`] and hands out one frame
//! of samples per frame period, as the concealer would, but it may pull
//! one frame from the receiver, two, or none. That lets it drop or repeat
//! single samples: a slow receiver drops one now and then to keep up, a
//! fast one repeats one to wait. One sample in thousands, at the flattest
//! point of the frame, is inaudible, where a dropped frame is not.
//!
//! ```
//! # use embedded_rtp::{drift::DriftCorrector, jitter::Config, l16::Concealer, Receiver};
//! let mut receiver = Receiver::<16, 640>::new(96, 320, Config::DEFAULT);
//! let mut concealer = Concealer::<320>::new();
//! let mut corrector = DriftCorrector::<320>::new();
//! // Every 20 ms, in place of `concealer.play(receiver.play())`:
//! if let Some(samples) = corrector.play(&mut receiver, &mut concealer) {
//!     // hand `samples` to the DAC
//! #   let _ = samples;
//! }
//! ```
//!
//! # How it works
//!
//! - **Measuring.** While the jitter buffer is playing, each frame period
//!   the audio waiting to be heard (the buffer's depth plus the samples
//!   left over from the last frame pulled) is compared with the buffer's
//!   target plus one and a half frames, so playout latency is about the
//!   target plus 1.5 frames: half a frame to a frame more than the buffer
//!   alone, which sits at its target to a frame over, but a frame clear of
//!   running dry. The difference is averaged over about
//!   [`AVERAGE_FRAMES`] periods, so network jitter doesn't register as
//!   drift.
//! - **Correcting.** The average is accumulated; each time the total passes
//!   a threshold, one sample is dropped (too much waiting) or repeated (too
//!   little). A standing error of `e` samples beyond a dead band of an
//!   eighth of a frame makes `e / GAIN_FRAMES` corrections a frame, at most
//!   one; 200 ppm needs one about every 16 frames, which an error of about
//!   half a frame sustains. Within the dead band nothing happens, so with
//!   no drift the corrections stop.
//! - **Where.** A dropped sample is the one whose neighbours are closest,
//!   a repeated one the one closest to its successor: the flattest point in
//!   the frame, often a waveform's peak or a stretch of silence. It picks
//!   the smallest step the frame allows; on a sine that is far smaller than
//!   the waveform's own steepest step, though a frame with no flat point
//!   (a steady ramp) still gets a step of two of its own.
//! - **Fallback.** The jitter buffer still drains whole frames and prefills
//!   after running dry, for large excursions such as Wi-Fi stalls; the
//!   corrector only keeps drift from causing them.
//!
//! Integer arithmetic only. A frame with no correction costs two copies; a
//! correction adds one scan of the frame.

use crate::l16::{Concealer, FADE};
use crate::Receiver;

/// Frame periods the depth error is averaged over, as a power of two.
pub const AVERAGE_FRAMES: u32 = 1 << AVERAGE_SHIFT;
const AVERAGE_SHIFT: u32 = 7;

/// Frame periods for the correction loop to work off an error: a standing
/// error of `GAIN_FRAMES` samples makes one correction a frame. Long against
/// [`AVERAGE_FRAMES`], so the loop doesn't overshoot.
pub const GAIN_FRAMES: i64 = 1_024;

/// Q8 fixed point for the averaged error.
const Q: u32 = 8;
/// An averaged error within this many eighths of a frame makes no
/// corrections, so with no drift they stop instead of dithering on
/// rounding.
const DEAD_BAND_EIGHTHS: usize = 1;
const THRESHOLD: i64 = GAIN_FRAMES << Q;

/// Corrections made so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Corrections {
    /// Samples dropped, for a receiver clock running slow.
    pub dropped: u32,
    /// Samples repeated, for a receiver clock running fast.
    pub repeated: u32,
}

/// Turns frames from a [`Receiver`] and [`Concealer`] into samples, one
/// frame period at a time, dropping or repeating single samples to follow
/// the sender's clock. `N` is samples per frame. Memory is two frames of
/// samples.
pub struct DriftCorrector<const N: usize> {
    /// The last frame pulled.
    frame: [i16; N],
    /// Samples of `frame` already handed out; `N` when it is used up.
    used: usize,
    /// Whether `frame` was audio rather than silence.
    frame_audible: bool,
    out: [i16; N],
    /// Depth error averaged over `AVERAGE_FRAMES`, in samples, Q8.
    average: i64,
    /// The average accumulated, towards `THRESHOLD` either way.
    phase: i64,
    corrections: Corrections,
}

impl<const N: usize> Default for DriftCorrector<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// What one frame period's correction does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Correction {
    None,
    Drop,
    Repeat,
}

impl<const N: usize> DriftCorrector<N> {
    /// A corrector with nothing pulled yet.
    ///
    /// # Panics
    ///
    /// If `N` is outside what [`Concealer`] takes ([`FADE`]..=32768
    /// samples); in a `const` or `static` that is a compile error.
    #[must_use]
    pub const fn new() -> Self {
        assert!(
            N >= FADE && N <= 32_768,
            "frame must be FADE..=32768 samples, as for Concealer"
        );
        Self {
            frame: [0; N],
            used: N,
            frame_audible: false,
            out: [0; N],
            average: 0,
            phase: 0,
            corrections: Corrections {
                dropped: 0,
                repeated: 0,
            },
        }
    }

    /// Samples for one frame period, or `None` if they are all silence.
    /// Call it once per frame period in place of
    /// `concealer.play(receiver.play())`; it calls those zero, one or two
    /// times.
    pub fn play<const SLOTS: usize, const FRAME: usize>(
        &mut self,
        receiver: &mut Receiver<SLOTS, FRAME>,
        concealer: &mut Concealer<N>,
    ) -> Option<&[i16; N]> {
        let correction = self.measure(receiver);
        let mut audible = false;
        match correction {
            Correction::None => self.fill(0, N, receiver, concealer, &mut audible),
            Correction::Drop => {
                // N + 1 samples in, one out: the extra one lands in `last`.
                self.fill(0, N, receiver, concealer, &mut audible);
                let mut last = [0i16; 1];
                self.take(&mut last, receiver, concealer, &mut audible);
                self.drop_one(last[0]);
                self.corrections.dropped += 1;
            }
            Correction::Repeat => {
                self.fill(0, N - 1, receiver, concealer, &mut audible);
                self.repeat_one();
                self.corrections.repeated += 1;
            }
        }
        audible.then_some(&self.out)
    }

    /// Corrections made so far.
    #[must_use]
    pub fn corrections(&self) -> Corrections {
        self.corrections
    }

    /// The averaged gap between the audio waiting and the target, in
    /// samples: positive when the receiver is behind.
    #[must_use]
    pub fn error(&self) -> i32 {
        (self.average >> Q) as i32
    }

    /// Update the measurement and decide this period's correction.
    fn measure<const SLOTS: usize, const FRAME: usize>(
        &mut self,
        receiver: &Receiver<SLOTS, FRAME>,
    ) -> Correction {
        let buffer = receiver.buffer();
        if !buffer.is_playing() {
            // Prefilling or silent: nothing to measure, and the buffer
            // restarts at its target anyway.
            return Correction::None;
        }
        let waiting = (buffer.depth() * N + (N - self.used)) as i64;
        // Target + 1.5 frames. The samples left over don't guard against
        // running dry, only frames in the buffer do, and the depth is only
        // known to a frame (when the next one arrives within the period is
        // not). So the buffer settles at target + 1 frames before each pop
        // with about half a frame left over: a frame clear of running dry
        // even as an arrival slips, and clear of draining (above target +
        // DRAIN_MARGIN).
        // Near the ceiling, the margin would only make the buffer overflow:
        // stay 1.5 frames under `max_depth` and leave the rest to it. Never
        // below the target itself, though: with no headroom at all (target
        // = max_depth = 1) a setpoint of zero would drop samples, and run
        // the buffer dry, with no drift at all.
        let target = usize::from(buffer.target()) * N;
        let ceiling = usize::from(buffer.config().max_depth) * N;
        let setpoint = (target + 3 * N / 2)
            .min(ceiling.saturating_sub(3 * N / 2))
            .max(target) as i64;
        let error = (waiting - setpoint) << Q;
        self.average += (error - self.average) >> AVERAGE_SHIFT;
        let dead_band = ((N * DEAD_BAND_EIGHTHS / 8) as i64) << Q;
        let push = if self.average > dead_band {
            self.average - dead_band
        } else if self.average < -dead_band {
            self.average + dead_band
        } else {
            0
        };
        self.phase = (self.phase + push).clamp(-2 * THRESHOLD, 2 * THRESHOLD);
        if self.phase >= THRESHOLD {
            self.phase -= THRESHOLD;
            Correction::Drop
        } else if self.phase <= -THRESHOLD {
            self.phase += THRESHOLD;
            Correction::Repeat
        } else {
            Correction::None
        }
    }

    /// Fill `out[start..start + len]` with the next samples, pulling frames
    /// as needed.
    fn fill<const SLOTS: usize, const FRAME: usize>(
        &mut self,
        start: usize,
        len: usize,
        receiver: &mut Receiver<SLOTS, FRAME>,
        concealer: &mut Concealer<N>,
        audible: &mut bool,
    ) {
        let mut at = start;
        let end = start + len;
        while at < end {
            if self.used == N {
                self.pull(receiver, concealer);
            }
            let n = (end - at).min(N - self.used);
            self.out[at..at + n].copy_from_slice(&self.frame[self.used..self.used + n]);
            self.used += n;
            at += n;
            *audible |= self.frame_audible;
        }
    }

    /// Take the next samples into `dst`, outside `out`.
    fn take<const SLOTS: usize, const FRAME: usize>(
        &mut self,
        dst: &mut [i16],
        receiver: &mut Receiver<SLOTS, FRAME>,
        concealer: &mut Concealer<N>,
        audible: &mut bool,
    ) {
        for sample in dst {
            if self.used == N {
                self.pull(receiver, concealer);
            }
            *sample = self.frame[self.used];
            self.used += 1;
            *audible |= self.frame_audible;
        }
    }

    fn pull<const SLOTS: usize, const FRAME: usize>(
        &mut self,
        receiver: &mut Receiver<SLOTS, FRAME>,
        concealer: &mut Concealer<N>,
    ) {
        match concealer.play(receiver.play()) {
            Some(samples) => {
                self.frame.copy_from_slice(samples);
                self.frame_audible = true;
            }
            None => {
                self.frame.fill(0);
                self.frame_audible = false;
            }
        }
        self.used = 0;
    }

    /// `out` holds the first N of N + 1 samples and `last` the final one:
    /// remove the sample whose neighbours are closest, so the join is as
    /// small a step as the frame allows.
    fn drop_one(&mut self, last: i16) {
        let at = |i: usize| {
            if i == N {
                i32::from(last)
            } else {
                i32::from(self.out[i])
            }
        };
        let mut best = 1;
        let mut best_step = i32::MAX;
        for i in 1..N {
            let step = (at(i + 1) - at(i - 1)).abs();
            if step < best_step {
                best = i;
                best_step = step;
            }
        }
        self.out.copy_within(best + 1..N, best);
        self.out[N - 1] = last;
    }

    /// `out` holds N - 1 samples: repeat the one closest to its successor,
    /// so the frame is stretched where it is flattest.
    fn repeat_one(&mut self) {
        let mut best = 0;
        let mut best_step = i32::MAX;
        for i in 0..N - 2 {
            let step = (i32::from(self.out[i + 1]) - i32::from(self.out[i])).abs();
            if step < best_step {
                best = i;
                best_step = step;
            }
        }
        self.out.copy_within(best..N - 1, best + 1);
    }
}
