//! G.722 (ITU-T, 1988): 16 kHz audio in 64 kbit/s, a quarter of L16's
//! bandwidth.
//!
//! Sub-band ADPCM, all in 16-bit integer arithmetic: a quadrature mirror
//! filter splits the signal into a lower and a higher band, each sampled at
//! 8 kHz, and each band is coded by backward-adaptive ADPCM, 6 bits for the
//! lower and 2 for the higher. Every pair of input samples becomes one
//! byte: the higher band's code in its top two bits, the lower band's in
//! the rest. The decoder can also ignore the lower band's last one or two
//! bits ([`Mode`]), for 56 or 48 kbit/s.
//!
//! The arithmetic follows the Recommendation's fixed-point description and
//! is bit-exact with the ITU-T reference implementation (G.191's software
//! tool library), for 16-bit input and output as it scales them.
//!
//! In RTP (RFC 3551 §4.5.2), G.722 has static payload type 9, and its RTP
//! clock runs at 8000 Hz although it samples at 16 kHz: a 20 ms frame is
//! 320 samples, 160 bytes and 160 timestamp ticks.
//!
//! ```
//! use embedded_rtp::g722::{Decoder, Encoder};
//!
//! let mut encoder = Encoder::new();
//! let mut decoder = Decoder::new();
//! # let captured = [0i16; 320];
//! let mut payload = [0u8; 160];
//! assert_eq!(encoder.encode(&captured, &mut payload), 160);
//! let mut played = [0i16; 320];
//! assert_eq!(decoder.decode(&payload, &mut played), 320);
//! ```
//!
//! Both sides keep state from frame to frame, so each stream needs its own
//! encoder or decoder, fed every frame in order. When a frame is lost,
//! conceal it in samples ([`Concealer::play_decoded`]) and carry on decoding
//! the next one: the decoder's adaptation drifts from the encoder's for a
//! while, then catches up. With [redundancy](crate::red), lost frames come
//! back exactly and the decoder never drifts.
//!
//! [`Concealer::play_decoded`]: crate::l16::Concealer::play_decoded

/// The static RTP payload type (RFC 3551).
pub const PAYLOAD_TYPE: u8 = 9;
/// The RTP clock rate: 8000 Hz, although G.722 samples at 16 kHz (RFC 3551
/// §4.5.2 keeps the rate the original standard gave by mistake).
pub const CLOCK_RATE: u32 = 8_000;
/// The audio sample rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// The QMF's 24 taps, symmetric, in units of 2^-13.
const QMF: [i32; 24] = [
    3, -11, -11, 53, 12, -156, 32, 362, -210, -805, 951, 3876, 3876, 951, -805, -210, 362, 32,
    -156, 12, 53, -11, -11, 3,
];

/// The lower band quantizer's decision levels, against the scale factor.
const Q6: [i32; 31] = [
    0, 35, 72, 110, 150, 190, 233, 276, 323, 370, 422, 473, 530, 587, 650, 714, 786, 858, 940,
    1023, 1121, 1219, 1339, 1458, 1612, 1765, 1980, 2195, 2557, 2919, 0,
];
/// The lower band's 6-bit codes for each level, negative and positive.
const ILN: [u8; 31] = [
    0, 63, 62, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11,
    10, 9, 8, 7, 6, 5, 4,
];
const ILP: [u8; 31] = [
    0, 61, 60, 59, 58, 57, 56, 55, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 43, 42, 41, 40, 39,
    38, 37, 36, 35, 34, 33, 32,
];

/// Inverse quantizers for the lower band: 6, 5 and 4 bits.
const QM6: [i32; 64] = [
    -136, -136, -136, -136, -24808, -21904, -19008, -16704, -14984, -13512, -12280, -11192, -10232,
    -9360, -8576, -7856, -7192, -6576, -6000, -5456, -4944, -4464, -4008, -3576, -3168, -2776,
    -2400, -2032, -1688, -1360, -1040, -728, 24808, 21904, 19008, 16704, 14984, 13512, 12280,
    11192, 10232, 9360, 8576, 7856, 7192, 6576, 6000, 5456, 4944, 4464, 4008, 3576, 3168, 2776,
    2400, 2032, 1688, 1360, 1040, 728, 432, 136, -432, -136,
];
const QM5: [i32; 32] = [
    -280, -280, -23352, -17560, -14120, -11664, -9752, -8184, -6864, -5712, -4696, -3784, -2960,
    -2208, -1520, -880, 23352, 17560, 14120, 11664, 9752, 8184, 6864, 5712, 4696, 3784, 2960, 2208,
    1520, 880, 280, -280,
];
const QM4: [i32; 16] = [
    0, -20456, -12896, -8968, -6288, -4240, -2584, -1200, 20456, 12896, 8968, 6288, 4240, 2584,
    1200, 0,
];
/// Lower band scale factor adaptation: the 4-bit code's magnitude, and the
/// log step for each.
const RL42: [usize; 16] = [0, 7, 6, 5, 4, 3, 2, 1, 7, 6, 5, 4, 3, 2, 1, 0];
const WL: [i32; 8] = [-60, -30, 58, 172, 334, 538, 1198, 3042];

/// The higher band: decision level, codes, inverse quantizer and
/// adaptation.
const Q2: i32 = 564;
const IHN: [u8; 3] = [0, 1, 0];
const IHP: [u8; 3] = [0, 3, 2];
const QM2: [i32; 4] = [-7408, -1616, 7408, 1616];
const RH2: [usize; 4] = [2, 1, 2, 1];
const WH: [i32; 3] = [0, -214, 798];

/// Log to linear for the scale factors: `2^(i/32)` in units of 2^-11.
const ILB: [i32; 32] = [
    2048, 2093, 2139, 2186, 2233, 2282, 2332, 2383, 2435, 2489, 2543, 2599, 2656, 2714, 2774, 2834,
    2896, 2960, 3025, 3091, 3158, 3228, 3298, 3371, 3444, 3520, 3597, 3676, 3756, 3838, 3922, 4008,
];

/// Saturate to 16 bits, as every adder in the Recommendation does.
fn sat(x: i32) -> i32 {
    x.clamp(i16::MIN.into(), i16::MAX.into())
}

/// The Recommendation's multiply: 16 by 16 bits, keeping bits 15 to 30,
/// saturated. For 16-bit operands only -32768 * -32768 overflows.
fn mul(a: i32, b: i32) -> i32 {
    ((a * b) >> 15).min(i16::MAX.into())
}

/// The multiply by a positive constant below 32768, which can't overflow.
fn mulc(a: i32, c: i32) -> i32 {
    (a * c) >> 15
}

/// A sum the Recommendation saturates, but which the operands' ranges keep
/// within 16 bits anyway. Debug builds (so every test) check that.
fn fits(x: i32) -> i32 {
    debug_assert_eq!(x, sat(x), "out of 16-bit range");
    x
}

/// Limit a sub-band signal to 15 bits.
const fn limit(x: i32) -> i32 {
    if x > 16_383 {
        16_383
    } else if x < -16_384 {
        -16_384
    } else {
        x
    }
}

/// The magnitude of a difference, one's complement style.
const fn magnitude(x: i32) -> i32 {
    if x >= 0 {
        x
    } else {
        !x
    }
}

/// One band's adaptive predictor and quantizer scale.
#[derive(Debug, Clone, Copy)]
struct Band {
    /// Signal estimate, and its zero section's part.
    s: i32,
    sz: i32,
    /// The last two reconstructed signals, doubled (saturated), as the pole
    /// section uses them; and the last two partial reconstructions.
    r: [i32; 2],
    p: [i32; 2],
    /// Pole coefficients a1, a2 (index 0 unused).
    a: [i32; 3],
    /// The last six quantized differences, doubled (saturated), as the zero
    /// section uses them: doubling keeps the sign, which is all their
    /// adaptation looks at. And the zero coefficients b1..b6. Index 0 of
    /// both is unused.
    d: [i32; 7],
    b: [i32; 7],
    /// Log and linear quantizer scale factors.
    nb: i32,
    det: i32,
}

impl Band {
    const fn new(det: i32) -> Self {
        Self {
            s: 0,
            sz: 0,
            r: [0; 2],
            p: [0; 2],
            a: [0; 3],
            d: [0; 7],
            b: [0; 7],
            nb: 0,
            det,
        }
    }

    /// Adapt the log scale factor by `step`, within `0..=max`, and convert
    /// it to the linear one. `shift` is 8 for the lower band, 10 for the
    /// higher.
    fn scale(&mut self, step: i32, max: i32, shift: i32) {
        // nb <= 22528, so this stays under 25400.
        self.nb = fits(mulc(self.nb, 32_512) + step).clamp(0, max);
        let wd1 = ILB[((self.nb >> 6) & 31) as usize];
        let wd2 = shift - (self.nb >> 11);
        let wd3 = if wd2 < 0 { wd1 << -wd2 } else { wd1 >> wd2 };
        self.det = wd3 << 2;
    }

    /// Take the quantized difference `d` for this sample: reconstruct,
    /// adapt the predictor and predict the next sample.
    fn update(&mut self, d: i32) {
        // Reconstructed signal, and the partial one the poles adapt on.
        let r = sat(self.s + d);
        let p = sat(self.sz + d);
        let sign = |x: i32| x < 0;
        let (sg0, sg1, sg2) = (sign(p), sign(self.p[0]), sign(self.p[1]));

        // Second pole.
        let wd1 = sat(self.a[1] << 2);
        let wd2 = if sg0 == sg1 { sat(-wd1) } else { wd1 };
        let wd3 = (wd2 >> 7) + if sg0 == sg2 { 128 } else { -128 };
        let a2 = fits(wd3 + mulc(self.a[2], 32_512)).clamp(-12_288, 12_288);

        // First pole, kept inside the stability triangle: |a1| was at most
        // 15360 + 12288 last time, so none of this overflows.
        let wd1 = if sg0 == sg1 { 192 } else { -192 };
        let limit1 = fits(15_360 - a2);
        let a1 = fits(wd1 + mulc(self.a[1], 32_640)).clamp(-limit1, limit1);

        // Zeros, in one pass from the oldest tap, the order the saturating
        // sum needs: adapt b_i on the difference i samples back, shift the
        // delay line, and add b_i's term to the next zero prediction.
        let step = if d == 0 { 0 } else { 128 };
        let mut sz = 0;
        for i in (1..7).rev() {
            let wd2 = if sign(self.d[i]) == sign(d) {
                step
            } else {
                -step
            };
            // At most 32639 + 128 either way.
            self.b[i] = fits(wd2 + mulc(self.b[i], 32_640));
            self.d[i] = if i == 1 { sat(d << 1) } else { self.d[i - 1] };
            sz = sat(sz + mul(self.b[i], self.d[i]));
        }
        self.sz = sz;

        // Poles, and the prediction.
        self.r = [sat(r << 1), self.r[0]];
        self.p = [p, self.p[0]];
        self.a = [0, a1, a2];
        let sp = sat(mul(a1, self.r[0]) + mul(a2, self.r[1]));
        self.s = sat(sp + sz);
    }
}

/// The quadrature mirror filter's last 24 inputs, in a ring written twice
/// over, so the 24 from `at` are always in one piece, newest first, without
/// shifting them along every sample.
#[derive(Debug, Clone, Copy)]
struct Qmf {
    ring: [i32; 48],
    at: usize,
}

impl Qmf {
    const fn new() -> Self {
        Self {
            ring: [0; 48],
            at: 0,
        }
    }

    /// Push two samples, the older first, and return the even and odd
    /// halves of the filter.
    fn push(&mut self, older: i32, newer: i32) -> (i32, i32) {
        self.at = if self.at == 0 { 22 } else { self.at - 2 };
        let at = self.at;
        self.ring[at] = newer;
        self.ring[at + 1] = older;
        self.ring[at + 24] = newer;
        self.ring[at + 25] = older;
        let x = &self.ring[at..at + 24];
        let mut even = 0;
        let mut odd = 0;
        for i in 0..12 {
            even += QMF[2 * i] * x[2 * i];
            odd += QMF[2 * i + 1] * x[2 * i + 1];
        }
        (even, odd)
    }
}

/// Encodes 16 kHz samples into G.722 at 64 kbit/s. Memory is about 400
/// bytes.
#[derive(Debug, Clone)]
pub struct Encoder {
    qmf: Qmf,
    low: Band,
    high: Band,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    /// An encoder in its initial state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            qmf: Qmf::new(),
            low: Band::new(32),
            high: Band::new(8),
        }
    }

    /// Encode `samples` into `out`, one byte per two samples, and return
    /// the number of bytes written: the smaller of half the samples and
    /// `out`'s length. An odd last sample is left out.
    pub fn encode(&mut self, samples: &[i16], out: &mut [u8]) -> usize {
        let mut written = 0;
        for (code, pair) in out.iter_mut().zip(samples.chunks_exact(2)) {
            *code = self.encode_pair(i32::from(pair[0]), i32::from(pair[1]));
            written += 1;
        }
        written
    }

    fn encode_pair(&mut self, older: i32, newer: i32) -> u8 {
        let (even, odd) = self.qmf.push(older, newer);
        let xl = limit((even + odd) >> 14);
        let xh = limit((even - odd) >> 14);

        // Lower band: 6-bit quantizer, 4-bit feedback.
        let low = &mut self.low;
        let el = sat(xl - low.s);
        // The first level whose decision threshold is above the error, or
        // 30. The scale factor tracks the signal, so this usually stops
        // within a few levels; a binary search was slower.
        let wd = magnitude(el);
        let mut level = 1;
        while level < 30 && wd >= (Q6[level] * low.det) >> 12 {
            level += 1;
        }
        let il = if el < 0 { ILN[level] } else { ILP[level] };
        let il4 = usize::from(il >> 2);
        let dlt = mul(low.det, QM4[il4]);
        low.scale(WL[RL42[il4]], 18_432, 8);
        low.update(dlt);

        // Higher band: 2-bit quantizer.
        let high = &mut self.high;
        let eh = sat(xh - high.s);
        let level = if magnitude(eh) >= (Q2 * high.det) >> 12 {
            2
        } else {
            1
        };
        let ih = if eh < 0 { IHN[level] } else { IHP[level] };
        let dh = mul(high.det, QM2[usize::from(ih)]);
        high.scale(WH[RH2[usize::from(ih)]], 22_528, 10);
        high.update(dh);

        (ih << 6) | il
    }
}

/// How many of the lower band's 6 bits the decoder uses: all of them at
/// 64 kbit/s, or 5 or 4, leaving the rest of each byte for data at 56 or
/// 48 kbit/s. Whatever the mode, the encoder sends 64 kbit/s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// 64 kbit/s: 6 bits of lower band. What RTP uses.
    #[default]
    Kbps64,
    /// 56 kbit/s: 5 bits of lower band, 1 bit of data.
    Kbps56,
    /// 48 kbit/s: 4 bits of lower band, 2 bits of data.
    Kbps48,
}

/// Decodes G.722 into 16 kHz samples. Memory is about 400 bytes.
#[derive(Debug, Clone)]
pub struct Decoder {
    qmf: Qmf,
    low: Band,
    high: Band,
    mode: Mode,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A 64 kbit/s decoder in its initial state.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_mode(Mode::Kbps64)
    }

    /// A decoder in its initial state, using `mode`.
    #[must_use]
    pub const fn with_mode(mode: Mode) -> Self {
        Self {
            qmf: Qmf::new(),
            low: Band::new(32),
            high: Band::new(8),
            mode,
        }
    }

    /// Decode `codes` into `out`, two samples per byte, and return the
    /// number of samples written: the smaller of twice the bytes and
    /// `out`'s length, rounded down to a whole byte's worth.
    pub fn decode(&mut self, codes: &[u8], out: &mut [i16]) -> usize {
        let mut written = 0;
        for (pair, &code) in out.chunks_exact_mut(2).zip(codes) {
            let (first, second) = self.decode_byte(code);
            pair[0] = first;
            pair[1] = second;
            written += 2;
        }
        written
    }

    fn decode_byte(&mut self, code: u8) -> (i16, i16) {
        // Lower band: output from as many bits as the mode uses, feedback
        // from 4, as in the encoder.
        let il = usize::from(code & 0x3f);
        let low = &mut self.low;
        let wd = match self.mode {
            Mode::Kbps64 => QM6[il],
            Mode::Kbps56 => QM5[il >> 1],
            Mode::Kbps48 => QM4[il >> 2],
        };
        let rl = limit(low.s + mul(low.det, wd));
        let dlt = mul(low.det, QM4[il >> 2]);
        low.scale(WL[RL42[il >> 2]], 18_432, 8);
        low.update(dlt);

        // Higher band.
        let ih = usize::from(code >> 6);
        let high = &mut self.high;
        let dh = mul(high.det, QM2[ih]);
        let rh = limit(high.s + dh);
        high.scale(WH[RH2[ih]], 22_528, 10);
        high.update(dh);

        // Recombine the bands at 16 kHz.
        let (even, odd) = self.qmf.push(sat(rl + rh), sat(rl - rh));
        (sat(even >> 11) as i16, sat(odd >> 11) as i16)
    }

    /// The mode in use.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Change the mode from the next byte on, keeping the state.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths_are_the_smaller_side() {
        let mut encoder = Encoder::new();
        let mut out = [0u8; 2];
        assert_eq!(encoder.encode(&[0; 7], &mut out), 2);
        assert_eq!(encoder.encode(&[0; 3], &mut out), 1);
        let mut decoder = Decoder::new();
        let mut samples = [0i16; 5];
        assert_eq!(decoder.decode(&[0; 4], &mut samples), 4);
        assert_eq!(decoder.decode(&[0; 1], &mut samples), 2);
    }

    #[test]
    fn scale_factors_start_at_their_minimum() {
        // A log scale factor of zero gives the initial linear ones.
        let mut low = Band::new(0);
        low.scale(0, 18_432, 8);
        assert_eq!(low.det, 32);
        let mut high = Band::new(0);
        high.scale(0, 22_528, 10);
        assert_eq!(high.det, 8);
    }

    #[test]
    fn usable_in_a_static() {
        static ENCODER: Encoder = Encoder::new();
        static DECODER: Decoder = Decoder::new();
        assert_eq!(ENCODER.low.det, 32);
        assert_eq!(DECODER.mode(), Mode::Kbps64);
    }
}
