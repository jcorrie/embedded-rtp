//! L16 payloads (RFC 3551 §4.5.11) and packet loss concealment.
//!
//! L16 is signed 16-bit PCM in network byte order (big endian).
//!
//! [`Concealer`] turns what a [`Receiver`](crate::Receiver) plays into samples:
//!
//! ```
//! # use embedded_rtp::{jitter::Config, l16::Concealer, Receiver};
//! let mut receiver = Receiver::<16, 640>::new(96, 320, Config::DEFAULT);
//! let mut concealer = Concealer::<320>::new();
//! // Every 20 ms:
//! if let Some(samples) = concealer.play(receiver.play()) {
//!     // hand `samples` to the DAC or the mixer
//! #   let _ = samples;
//! }
//! ```
//!
//! # Concealment
//!
//! A lost frame is replaced by the last good frame, fading out: from full
//! to half amplitude over the first loss, from half to nothing over the
//! second, and silence from the third. When the stream runs dry
//! ([`Playout::Silence`]) straight after audio, the last frame fades out over
//! one frame period instead of stopping dead.
//!
//! Wherever the output would otherwise jump (into a concealed frame, between
//! concealed frames, and back into real audio), the first [`FADE`] samples
//! blend linearly from the last sample played. A clean stream is passed
//! through untouched.
//!
//! All integer arithmetic; gains are Q15 fixed point and need no division.

use crate::jitter::Playout;

/// Samples over which a join is blended, as a power of two: 2 ms at 16 kHz.
pub const FADE: usize = 32;
const FADE_SHIFT: u32 = FADE.trailing_zeros();

/// Unity gain in Q15.
const ONE: i32 = 1 << 15;
const HALF: i32 = ONE / 2;

/// Decode L16 bytes into samples. Returns the number of samples written:
/// the smaller of what `bytes` holds and what `out` has room for.
pub fn decode(bytes: &[u8], out: &mut [i16]) -> usize {
    let mut written = 0;
    for (sample, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *sample = i16::from_be_bytes([pair[0], pair[1]]);
        written += 1;
    }
    written
}

/// Encode samples as L16 bytes. Returns the number of bytes written: twice
/// the smaller of the sample count and the samples `out` has room for.
pub fn encode(samples: &[i16], out: &mut [u8]) -> usize {
    let mut written = 0;
    for (pair, sample) in out.chunks_exact_mut(2).zip(samples) {
        pair.copy_from_slice(&sample.to_be_bytes());
        written += 2;
    }
    written
}

/// Turns playout into samples, concealing lost frames. `N` is samples per
/// frame (320 for 20 ms at 16 kHz). Memory is two frames of samples.
pub struct Concealer<const N: usize> {
    /// The last good frame, decoded. Also what a good frame is returned in.
    frame: [i16; N],
    /// Concealed output.
    concealed: [i16; N],
    /// Whether `frame` holds audio to conceal from.
    have_frame: bool,
    /// Gain at the end of the last output frame, Q15.
    gain: i32,
    /// Consecutive lost frames.
    losses: u32,
    /// The last sample output, where the next join blends from.
    last_out: i16,
    /// Whether the next frame starts at a discontinuity.
    joining: bool,
}

impl<const N: usize> Default for Concealer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Concealer<N> {
    /// Per-sample gain step to fall by half of unity over one frame.
    const STEP_HALF: i32 = HALF / N as i32;
    /// Per-sample gain step to fall by all of unity over one frame.
    const STEP_ONE: i32 = ONE / N as i32;

    /// A concealer that has heard nothing yet.
    ///
    /// # Panics
    ///
    /// If `N` is shorter than [`FADE`] or longer than 32768 samples; in a
    /// `const` or `static` that is a compile error.
    #[must_use]
    pub const fn new() -> Self {
        assert!(
            N >= FADE && N <= 32_768,
            "frame must be FADE..=32768 samples"
        );
        Self {
            frame: [0; N],
            concealed: [0; N],
            have_frame: false,
            gain: ONE,
            losses: 0,
            last_out: 0,
            joining: true,
        }
    }

    /// Samples to play for one frame period, or `None` for silence.
    ///
    /// A [`Playout::Frame`] is decoded (a payload shorter than a frame is
    /// padded with silence, a longer one truncated). A [`Playout::Lost`] is
    /// concealed. [`Playout::Silence`] fades out what was playing, then is
    /// silent.
    pub fn play(&mut self, playout: Playout<'_>) -> Option<&[i16; N]> {
        match playout {
            Playout::Frame { payload, .. } => Some(self.good(payload)),
            Playout::Lost { .. } => {
                self.losses += 1;
                let target = if self.losses == 1 { HALF } else { 0 };
                self.fade_to(target)
            }
            Playout::Silence => {
                self.losses = 0;
                self.fade_to(0)
            }
        }
    }

    fn good(&mut self, payload: &[u8]) -> &[i16; N] {
        let written = decode(payload, &mut self.frame);
        if written < N {
            self.frame[written..].fill(0);
        }
        if self.joining {
            blend_from(self.last_out, &mut self.frame);
        }
        self.have_frame = true;
        self.gain = ONE;
        self.losses = 0;
        self.joining = false;
        self.last_out = self.frame[N - 1];
        &self.frame
    }

    /// Repeat the last good frame with the gain ramping to `target`, or
    /// return `None` if there is nothing (left) to play.
    fn fade_to(&mut self, target: i32) -> Option<&[i16; N]> {
        self.joining = true;
        if !self.have_frame || self.gain == 0 {
            self.gain = 0;
            self.last_out = 0;
            return None;
        }
        let step = if self.gain - target >= ONE {
            Self::STEP_ONE
        } else {
            Self::STEP_HALF
        };
        // Gain in closed form per sample, so no sample depends on the one
        // before and the loop can vectorize.
        let start = self.gain;
        for (i, (out, &sample)) in self.concealed.iter_mut().zip(&self.frame).enumerate() {
            let gain = (start - step * (i as i32 + 1)).max(target);
            *out = ((i32::from(sample) * gain) >> 15) as i16;
        }
        // The steps are rounded down, so land exactly on the target.
        self.gain = target;
        blend_from(self.last_out, &mut self.concealed);
        self.last_out = self.concealed[N - 1];
        Some(&self.concealed)
    }
}

/// Blend the first [`FADE`] samples of `frame` linearly from `from`, so a
/// join with what played before has no step.
fn blend_from(from: i16, frame: &mut [i16]) {
    let from = i32::from(from);
    for (i, sample) in frame.iter_mut().take(FADE).enumerate() {
        let target = i32::from(*sample);
        *sample = (from + (((target - from) * i as i32) >> FADE_SHIFT)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_bytes(value: i16) -> [u8; 128] {
        let mut bytes = [0u8; 128];
        encode(&[value; 64], &mut bytes);
        bytes
    }

    fn good(bytes: &[u8]) -> Playout<'_> {
        Playout::Frame {
            sequence: 0,
            payload: bytes,
        }
    }

    const LOST: Playout<'static> = Playout::Lost { sequence: 0 };

    #[test]
    fn round_trip_and_byte_order() {
        let samples = [0x0102, -2, i16::MIN, i16::MAX];
        let mut bytes = [0u8; 8];
        assert_eq!(encode(&samples, &mut bytes), 8);
        assert_eq!(bytes, [0x01, 0x02, 0xff, 0xfe, 0x80, 0x00, 0x7f, 0xff]);
        let mut back = [0i16; 4];
        assert_eq!(decode(&bytes, &mut back), 4);
        assert_eq!(back, samples);
    }

    #[test]
    fn lengths_are_the_smaller_side() {
        let mut out = [0i16; 2];
        assert_eq!(decode(&[0; 7], &mut out), 2);
        assert_eq!(decode(&[0; 3], &mut out), 1);
        let mut bytes = [0u8; 3];
        assert_eq!(encode(&[1, 2, 3], &mut bytes), 2);
    }

    #[test]
    fn clean_stream_passes_through_after_the_first_fade_in() {
        let mut concealer = Concealer::<64>::new();
        let first = *concealer.play(good(&frame_bytes(1000))).unwrap();
        assert_eq!(first[0], 0, "the stream fades in from silence");
        assert_eq!(first[FADE..], [1000; 64 - FADE]);
        for _ in 0..3 {
            assert_eq!(concealer.play(good(&frame_bytes(1000))), Some(&[1000; 64]));
        }
    }

    #[test]
    fn losses_fade_to_silence_by_the_third() {
        let mut concealer = Concealer::<64>::new();
        concealer.play(good(&frame_bytes(16_000)));
        let first = *concealer.play(LOST).unwrap();
        assert!(
            (7_900..=8_200).contains(&first[63]),
            "half by the end of the first loss: {}",
            first[63]
        );
        let second = *concealer.play(LOST).unwrap();
        assert!(
            second[63].abs() <= 300,
            "near silence by the end of the second: {}",
            second[63]
        );
        assert_eq!(concealer.play(LOST), None, "silent from the third");
        assert_eq!(concealer.play(LOST), None);
    }

    #[test]
    fn silence_after_audio_fades_out_once() {
        let mut concealer = Concealer::<64>::new();
        concealer.play(good(&frame_bytes(-12_000)));
        let tail = *concealer.play(Playout::Silence).unwrap();
        assert_eq!(tail[0], -12_000, "starts where the audio left off");
        assert!(tail[63].abs() <= 400, "ends near silence: {}", tail[63]);
        assert_eq!(concealer.play(Playout::Silence), None);
        // The next talkspurt fades in.
        let next = *concealer.play(good(&frame_bytes(5_000))).unwrap();
        assert!(next[0].abs() <= 400);
        assert_eq!(next[FADE..], [5_000; 64 - FADE]);
    }

    #[test]
    fn nothing_to_conceal_before_the_first_frame() {
        let mut concealer = Concealer::<64>::new();
        assert_eq!(concealer.play(LOST), None);
        assert_eq!(concealer.play(Playout::Silence), None);
    }

    #[test]
    fn short_payloads_are_padded_long_ones_truncated() {
        let mut concealer = Concealer::<64>::new();
        concealer.play(good(&frame_bytes(1)));
        let short = *concealer.play(good(&frame_bytes(7)[..10])).unwrap();
        assert_eq!(short[..5], [7; 5]);
        assert_eq!(short[5..], [0; 59]);
        let long = [0x11u8; 300];
        assert_eq!(concealer.play(good(&long)).unwrap()[63], 0x1111);
    }

    #[test]
    fn usable_in_a_static() {
        static CONCEALER: Concealer<320> = Concealer::new();
        assert!(!CONCEALER.have_frame);
    }
}
