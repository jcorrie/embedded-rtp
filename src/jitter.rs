//! A jitter buffer for fixed-duration frames.
//!
//! Packets go in as they arrive, in any order, via [`JitterBuffer::push`].
//! Frames come out once per frame period, in sequence order, via
//! [`JitterBuffer::pop`]: the audio itself, [`Playout::Lost`] for a frame the
//! caller should conceal, or [`Playout::Silence`] while nothing is buffered.
//!
//! Frames are slotted by sequence number into a power-of-two ring, so there
//! is no allocation, no sorting and no searching beyond `SLOTS` steps.
//!
//! # Loss vs. silence
//!
//! A sender that suppresses silence stops sending but keeps its sequence
//! numbers contiguous (RFC 3551 §4.1). So when the next frame is missing:
//!
//! - if later frames are buffered, it was lost: [`Playout::Lost`];
//! - if nothing is buffered, the sender has gone quiet (or the network has
//!   stalled): [`Playout::Silence`], and the buffer prefills again before
//!   resuming.
//!
//! # Latency
//!
//! Playout starts once the target depth is buffered, or once the first frame
//! has waited that many frame periods, so a talkspurt shorter than the target
//! still plays promptly. The target adapts to
//! measured jitter ([`JitterBuffer::set_target_from_jitter`]). Latency is
//! bounded two ways: a frame more than `max_depth` ahead of the play point
//! moves the play point forward, and a buffer that stays well above target
//! for [`DRAIN_AFTER`] frames discards one frame to catch up.

/// How far above target the depth must be before draining starts.
pub const DRAIN_MARGIN: usize = 2;
/// Consecutive frame periods above target + [`DRAIN_MARGIN`] before one frame
/// is discarded. 25 frames of 20 ms is half a second.
pub const DRAIN_AFTER: u16 = 25;

/// Depth limits, in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Target depth until a jitter estimate is available.
    pub initial_depth: u8,
    /// The adaptive target never goes below this.
    pub min_depth: u8,
    /// Hard ceiling on depth, and so on added latency. At most `SLOTS`.
    pub max_depth: u8,
}

impl Config {
    /// Start at 2 frames, adapt between 1 and 8. For 20 ms frames: 40 ms to
    /// begin with, never more than 160 ms.
    pub const DEFAULT: Self = Self {
        initial_depth: 2,
        min_depth: 1,
        max_depth: 8,
    };
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What happened to a pushed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// Buffered for playout.
    Accepted,
    /// Behind the play point: its turn has passed. Dropped.
    Late,
    /// Already buffered. Dropped.
    Duplicate,
    /// Longer than `FRAME`. Dropped.
    Oversized,
}

/// What to play for one frame period.
///
/// A frame's payload is borrowed straight from the buffer's slot rather than
/// copied out: the borrow ends before the next push can reuse the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playout<'a> {
    /// Frame `sequence`, to be played now.
    Frame {
        /// The frame's RTP sequence number.
        sequence: u16,
        /// The frame's payload.
        payload: &'a [u8],
    },
    /// Frame `sequence` never arrived but later ones have: conceal it.
    Lost {
        /// The missing frame's RTP sequence number.
        sequence: u16,
    },
    /// Nothing to play: not started, prefilling, or the sender is quiet.
    Silence,
}

/// Running totals. Every sequence number the play point passes is counted
/// exactly once, in `played`, `concealed`, `overflow`, `drained` or
/// `skipped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// Frames returned as [`Playout::Frame`].
    pub played: u32,
    /// Frames returned as [`Playout::Lost`].
    pub concealed: u32,
    /// Buffered frames discarded because a frame arrived more than
    /// `max_depth` ahead.
    pub overflow: u32,
    /// Buffered frames discarded to bring latency back down to target.
    pub drained: u32,
    /// Sequence numbers passed over that were never buffered: missing frames
    /// jumped by an overflow or a drain, or the gap after an underrun.
    pub skipped: u32,
    /// Pushes rejected as [`Push::Late`].
    pub late: u32,
    /// Pushes rejected as [`Push::Duplicate`].
    pub duplicate: u32,
    /// Pushes rejected as [`Push::Oversized`].
    pub oversized: u32,
    /// Times playout ran dry and went back to prefilling.
    pub underruns: u32,
}

#[derive(Clone, Copy)]
struct Slot<const FRAME: usize> {
    filled: bool,
    sequence: u16,
    len: u16,
    data: [u8; FRAME],
}

impl<const FRAME: usize> Slot<FRAME> {
    const EMPTY: Self = Self {
        filled: false,
        sequence: 0,
        len: 0,
        data: [0; FRAME],
    };
}

/// A jitter buffer of `SLOTS` frames of up to `FRAME` bytes each.
///
/// `SLOTS` must be a power of two, so slot indices stay continuous when the
/// sequence number wraps. Memory is roughly `SLOTS * (FRAME + 4)` bytes.
pub struct JitterBuffer<const SLOTS: usize, const FRAME: usize> {
    slots: [Slot<FRAME>; SLOTS],
    config: Config,
    /// Sequence number of the next frame to play. Meaningless until `started`.
    next: u16,
    started: bool,
    playing: bool,
    /// While prefilling after an underrun: the play point when it ran dry.
    /// Everything before it has been played or passed, so the play point
    /// may move back to meet a reordered frame, but never behind this.
    floor: Option<u16>,
    /// Frame periods spent prefilling with something buffered.
    waited: u8,
    buffered: usize,
    /// The furthest-ahead buffered frame; meaningful while `buffered > 0`.
    /// Every buffered frame lies in `next..=newest`, so depth is O(1).
    newest: u16,
    target: u8,
    /// Consecutive frame periods spent above target + `DRAIN_MARGIN`.
    excess: u16,
    counters: Counters,
}

impl<const SLOTS: usize, const FRAME: usize> JitterBuffer<SLOTS, FRAME> {
    const MASK: usize = SLOTS - 1;

    /// An empty buffer.
    ///
    /// # Panics
    ///
    /// If `SLOTS` is not a power of two (at most 256), `FRAME` does not fit a
    /// `u16`, or the config does not satisfy
    /// `1 <= min_depth <= initial_depth <= max_depth <= SLOTS`. In a `const`
    /// or `static` these are compile errors.
    #[must_use]
    pub const fn new(config: Config) -> Self {
        assert!(
            SLOTS.is_power_of_two() && SLOTS <= 256,
            "SLOTS must be a power of two, at most 256"
        );
        assert!(FRAME <= u16::MAX as usize, "FRAME must fit in a u16");
        assert!(
            1 <= config.min_depth
                && config.min_depth <= config.initial_depth
                && config.initial_depth <= config.max_depth
                && config.max_depth as usize <= SLOTS,
            "need 1 <= min_depth <= initial_depth <= max_depth <= SLOTS"
        );
        Self {
            slots: [Slot::EMPTY; SLOTS],
            config,
            next: 0,
            started: false,
            playing: false,
            floor: None,
            waited: 0,
            buffered: 0,
            newest: 0,
            target: config.initial_depth,
            excess: 0,
            counters: Counters {
                played: 0,
                concealed: 0,
                overflow: 0,
                drained: 0,
                skipped: 0,
                late: 0,
                duplicate: 0,
                oversized: 0,
                underruns: 0,
            },
        }
    }

    /// Forget everything buffered and start again, as for a new stream.
    /// Counters and the current target are kept.
    pub fn reset(&mut self) {
        for slot in &mut self.slots {
            slot.filled = false;
        }
        self.started = false;
        self.playing = false;
        self.floor = None;
        self.waited = 0;
        self.buffered = 0;
        self.excess = 0;
    }

    /// Count the sequence numbers between the floor and the play point as
    /// skipped, and raise the floor to the play point.
    fn settle_floor(&mut self) {
        if let Some(floor) = self.floor {
            self.counters.skipped += u32::from(self.next.wrapping_sub(floor));
            self.floor = Some(self.next);
        }
    }

    fn slot(&self, sequence: u16) -> &Slot<FRAME> {
        &self.slots[usize::from(sequence) & Self::MASK]
    }

    fn slot_mut(&mut self, sequence: u16) -> &mut Slot<FRAME> {
        &mut self.slots[usize::from(sequence) & Self::MASK]
    }

    /// Whether frame `sequence` is buffered.
    fn holds(&self, sequence: u16) -> bool {
        let slot = self.slot(sequence);
        slot.filled && slot.sequence == sequence
    }

    /// Move the play point past `next`, discarding that frame if buffered.
    /// A discarded frame is counted with `discarded`; a missing one as
    /// skipped.
    fn advance(&mut self, discarded: fn(&mut Counters)) {
        let sequence = self.next;
        if self.holds(sequence) {
            self.slot_mut(sequence).filled = false;
            self.buffered -= 1;
            discarded(&mut self.counters);
        } else {
            self.counters.skipped += 1;
        }
        self.next = sequence.wrapping_add(1);
    }

    /// Buffer a frame.
    pub fn push(&mut self, sequence: u16, payload: &[u8]) -> Push {
        if payload.len() > FRAME {
            self.counters.oversized += 1;
            return Push::Oversized;
        }
        let max_depth = usize::from(self.config.max_depth);
        if !self.started {
            self.started = true;
            self.next = sequence;
        } else if !self.playing {
            // Prefilling: nothing from this run has played yet, so the play
            // point follows the earliest frame to arrive - as long as it is
            // not behind what was already played, and the window still fits.
            let behind_floor = self
                .floor
                .is_some_and(|floor| (sequence.wrapping_sub(floor) as i16) < 0);
            if behind_floor {
                self.counters.late += 1;
                return Push::Late;
            }
            let back = self.next.wrapping_sub(sequence) as i16;
            if self.buffered == 0 || (back > 0 && self.depth() + back as usize <= max_depth) {
                self.next = sequence;
            }
        }

        let offset = sequence.wrapping_sub(self.next) as i16;
        if offset < 0 {
            self.counters.late += 1;
            return Push::Late;
        }

        let ahead = offset as usize;
        if ahead >= max_depth {
            // Too far ahead: move the play point so this frame lands at the
            // back of the allowed window.
            self.settle_floor();
            let jump = ahead + 1 - max_depth;
            if jump >= SLOTS {
                // Everything buffered is behind the new play point.
                self.counters.overflow += self.buffered as u32;
                self.counters.skipped += (jump - self.buffered) as u32;
                for slot in &mut self.slots {
                    slot.filled = false;
                }
                self.buffered = 0;
                self.next = self.next.wrapping_add(jump as u16);
            } else {
                for _ in 0..jump {
                    self.advance(|c| c.overflow += 1);
                }
            }
            if self.floor.is_some() {
                self.floor = Some(self.next);
            }
        }

        if self.holds(sequence) {
            self.counters.duplicate += 1;
            return Push::Duplicate;
        }
        let slot = self.slot_mut(sequence);
        slot.filled = true;
        slot.sequence = sequence;
        slot.len = payload.len() as u16;
        slot.data[..payload.len()].copy_from_slice(payload);
        if self.buffered == 0 || (sequence.wrapping_sub(self.newest) as i16) > 0 {
            self.newest = sequence;
        }
        self.buffered += 1;
        self.check_depth();
        Push::Accepted
    }

    /// The next frame period's output. Call exactly once per frame period.
    pub fn pop(&mut self) -> Playout<'_> {
        if !self.started {
            return Playout::Silence;
        }
        if !self.playing {
            if self.buffered == 0 {
                self.waited = 0;
                return Playout::Silence;
            }
            // Start once the target depth is buffered, or once what is
            // buffered has waited as long as the target - otherwise a
            // talkspurt shorter than the target would never play.
            if self.depth() < usize::from(self.target) && self.waited < self.target {
                self.waited += 1;
                return Playout::Silence;
            }
            self.playing = true;
            self.waited = 0;
            self.excess = 0;
            self.settle_floor();
            self.floor = None;
        }
        if self.buffered == 0 {
            self.playing = false;
            self.floor = Some(self.next);
            self.counters.underruns += 1;
            return Playout::Silence;
        }

        if self.depth() > usize::from(self.target) + DRAIN_MARGIN {
            self.excess += 1;
            if self.excess >= DRAIN_AFTER {
                self.excess = 0;
                self.advance(|c| c.drained += 1);
            }
        } else {
            self.excess = 0;
        }

        let sequence = self.next;
        self.next = sequence.wrapping_add(1);
        if self.holds(sequence) {
            let index = usize::from(sequence) & Self::MASK;
            self.slots[index].filled = false;
            self.buffered -= 1;
            self.counters.played += 1;
            self.check_depth();
            let slot = &self.slots[index];
            Playout::Frame {
                sequence,
                payload: &slot.data[..usize::from(slot.len)],
            }
        } else {
            self.counters.concealed += 1;
            self.check_depth();
            Playout::Lost { sequence }
        }
    }

    /// Frames from the play point to the newest buffered frame, inclusive,
    /// gaps included: the latency the buffer currently adds, in frames.
    #[must_use]
    pub fn depth(&self) -> usize {
        if self.buffered == 0 {
            0
        } else {
            usize::from(self.newest.wrapping_sub(self.next)) + 1
        }
    }

    /// `depth` by brute force: scan for the furthest-ahead buffered frame.
    fn depth_by_scan(&self) -> usize {
        (0..SLOTS)
            .rev()
            .find(|&ahead| self.holds(self.next.wrapping_add(ahead as u16)))
            .map_or(0, |ahead| ahead + 1)
    }

    /// In debug builds (so in every test), check the O(1) depth against the
    /// scan it replaced.
    fn check_depth(&self) {
        debug_assert_eq!(
            self.depth(),
            self.depth_by_scan(),
            "tracked depth disagrees with scan"
        );
    }

    /// Frames currently buffered.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffered
    }

    /// Whether frames are being played out (as opposed to prefilling).
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Current target depth, in frames.
    #[must_use]
    pub fn target(&self) -> u8 {
        self.target
    }

    /// Set the target depth, clamped to the configured limits.
    pub fn set_target(&mut self, frames: u8) {
        self.target = frames.clamp(self.config.min_depth, self.config.max_depth);
    }

    /// Set the target from an interarrival jitter estimate (RFC 3550 §6.4.1,
    /// in RTP clock ticks): `1 + ceil(3 * jitter / frame_ticks)` frames,
    /// clamped. Three times the mean deviation covers nearly all arrivals.
    ///
    /// Runs per packet, so it avoids division: the Cortex-M0+ has no divide
    /// instruction. Counting frames up to `max_depth` takes at most that many
    /// additions.
    pub fn set_target_from_jitter(&mut self, jitter: u32, frame_ticks: u32) {
        let needed = jitter.saturating_mul(3);
        let mut frames = 1u8;
        let mut covered = 0u32;
        while covered < needed && frames < self.config.max_depth {
            covered = covered.saturating_add(frame_ticks);
            frames += 1;
        }
        self.set_target(frames);
    }

    /// The configured limits.
    #[must_use]
    pub fn config(&self) -> Config {
        self.config
    }

    /// Running totals.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.counters
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Buffer = JitterBuffer<8, 4>;

    fn frame(sequence: u16) -> [u8; 4] {
        let [a, b] = sequence.to_be_bytes();
        [a, b, !a, !b]
    }

    /// `Playout` without the borrow, for comparing.
    #[derive(Debug, PartialEq, Eq)]
    enum Got {
        Frame(u16),
        Lost(u16),
        Silence,
    }

    fn pop(buffer: &mut Buffer) -> Got {
        match buffer.pop() {
            Playout::Frame { sequence, payload } => {
                assert_eq!(payload, frame(sequence), "payload belongs to its frame");
                Got::Frame(sequence)
            }
            Playout::Lost { sequence } => Got::Lost(sequence),
            Playout::Silence => Got::Silence,
        }
    }

    #[test]
    fn prefills_then_plays_in_order() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        assert_eq!(pop(&mut buffer), Got::Silence);
        assert_eq!(buffer.push(10, &frame(10)), Push::Accepted);
        assert_eq!(
            pop(&mut buffer),
            Got::Silence,
            "one frame is under the target of two"
        );
        assert_eq!(buffer.push(11, &frame(11)), Push::Accepted);
        assert_eq!(pop(&mut buffer), Got::Frame(10));
        assert_eq!(buffer.push(12, &frame(12)), Push::Accepted);
        assert_eq!(pop(&mut buffer), Got::Frame(11));
        assert_eq!(pop(&mut buffer), Got::Frame(12));
    }

    #[test]
    fn reorders() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.push(2, &frame(2));
        buffer.push(1, &frame(1));
        buffer.push(0, &frame(0));
        for sequence in 0..3 {
            assert_eq!(pop(&mut buffer), Got::Frame(sequence));
        }
    }

    #[test]
    fn talkspurt_shorter_than_the_target_still_plays() {
        let mut buffer = Buffer::new(Config {
            initial_depth: 3,
            min_depth: 1,
            max_depth: 8,
        });
        buffer.push(0, &frame(0));
        assert_eq!(pop(&mut buffer), Got::Silence);
        assert_eq!(pop(&mut buffer), Got::Silence);
        assert_eq!(pop(&mut buffer), Got::Silence);
        assert_eq!(pop(&mut buffer), Got::Frame(0));
    }

    #[test]
    fn missing_frame_with_later_ones_buffered_is_lost() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.push(0, &frame(0));
        buffer.push(2, &frame(2));
        assert_eq!(pop(&mut buffer), Got::Frame(0));
        assert_eq!(pop(&mut buffer), Got::Lost(1));
        assert_eq!(pop(&mut buffer), Got::Frame(2));
        // A frame that turns up after its turn is late.
        assert_eq!(buffer.push(1, &frame(1)), Push::Late);
    }

    #[test]
    fn empty_buffer_is_silence_then_refill() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.push(0, &frame(0));
        buffer.push(1, &frame(1));
        pop(&mut buffer);
        pop(&mut buffer);
        assert_eq!(pop(&mut buffer), Got::Silence);
        assert_eq!(buffer.counters().underruns, 1);
        assert!(!buffer.is_playing());
        // The sender resumes; sequence numbers carried on.
        buffer.push(2, &frame(2));
        assert_eq!(pop(&mut buffer), Got::Silence, "prefilling again");
        buffer.push(3, &frame(3));
        assert_eq!(pop(&mut buffer), Got::Frame(2));
        assert_eq!(buffer.counters().concealed, 0);
    }

    #[test]
    fn reordered_start_of_a_talkspurt_is_kept() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.push(0, &frame(0));
        buffer.push(1, &frame(1));
        pop(&mut buffer);
        pop(&mut buffer);
        assert_eq!(pop(&mut buffer), Got::Silence); // underrun, floor = 2
                                                    // The next talkspurt's first two frames arrive swapped.
        assert_eq!(buffer.push(3, &frame(3)), Push::Accepted);
        assert_eq!(buffer.push(2, &frame(2)), Push::Accepted);
        // Anything before the floor has had its turn.
        assert_eq!(buffer.push(1, &frame(1)), Push::Late);
        assert_eq!(pop(&mut buffer), Got::Frame(2));
        assert_eq!(pop(&mut buffer), Got::Frame(3));
        assert_eq!(buffer.counters().skipped, 0);
    }

    #[test]
    fn gap_while_dry_is_counted_once_playout_resumes() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.push(0, &frame(0));
        buffer.push(1, &frame(1));
        pop(&mut buffer);
        pop(&mut buffer);
        pop(&mut buffer); // underrun, floor = 2
        buffer.push(7, &frame(7));
        buffer.push(6, &frame(6)); // pulls the play point back to 6
        assert_eq!(pop(&mut buffer), Got::Frame(6));
        // 2..=5 never arrived and were passed over.
        assert_eq!(buffer.counters().skipped, 4);
    }

    #[test]
    fn duplicates_and_oversize_are_rejected() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        assert_eq!(buffer.push(5, &frame(5)), Push::Accepted);
        assert_eq!(buffer.push(5, &frame(5)), Push::Duplicate);
        assert_eq!(buffer.push(6, &[0; 5]), Push::Oversized);
        let counters = buffer.counters();
        assert_eq!((counters.duplicate, counters.oversized), (1, 1));
    }

    #[test]
    fn far_ahead_frame_moves_the_play_point() {
        let mut buffer = Buffer::new(Config {
            initial_depth: 2,
            min_depth: 1,
            max_depth: 4,
        });
        buffer.push(0, &frame(0));
        buffer.push(1, &frame(1));
        assert_eq!(buffer.push(5, &frame(5)), Push::Accepted);
        // Window is now 2..=5: frames 0 and 1 were discarded.
        assert_eq!(buffer.depth(), 4);
        let counters = buffer.counters();
        assert_eq!((counters.overflow, counters.skipped), (2, 0));
        assert_eq!(pop(&mut buffer), Got::Lost(2));
    }

    #[test]
    fn huge_jump_clears_the_buffer() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.push(0, &frame(0));
        buffer.push(1, &frame(1));
        assert_eq!(buffer.push(1000, &frame(1000)), Push::Accepted);
        assert_eq!(buffer.buffered(), 1);
        assert_eq!(buffer.depth(), usize::from(Config::DEFAULT.max_depth));
        let counters = buffer.counters();
        assert_eq!(counters.overflow, 2);
        assert_eq!(counters.overflow + counters.skipped, 1000 + 1 - 8);
    }

    #[test]
    fn sustained_excess_is_drained() {
        let mut buffer = Buffer::new(Config {
            initial_depth: 1,
            min_depth: 1,
            max_depth: 8,
        });
        // Six frames banked against a target of one.
        for sequence in 0..6 {
            buffer.push(sequence, &frame(sequence));
        }
        let mut next = 6;
        let mut played = 0;
        while buffer.counters().drained == 0 {
            assert!(matches!(pop(&mut buffer), Got::Frame(_)));
            buffer.push(next, &frame(next));
            next += 1;
            played += 1;
        }
        assert_eq!(played, usize::from(DRAIN_AFTER));
    }

    #[test]
    fn wraps_around() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        let mut sequence = 65_530u16;
        buffer.push(sequence, &frame(sequence));
        for _ in 0..20 {
            buffer.push(sequence.wrapping_add(1), &frame(sequence.wrapping_add(1)));
            assert_eq!(pop(&mut buffer), Got::Frame(sequence));
            sequence = sequence.wrapping_add(1);
        }
    }

    #[test]
    fn target_without_division_matches_the_formula() {
        for max_depth in [1u8, 2, 8, 16] {
            let config = Config {
                initial_depth: 1,
                min_depth: 1,
                max_depth: max_depth.min(8),
            };
            for frame_ticks in [1u32, 160, 320, 333, 960] {
                for jitter in (0..5_000).chain([u32::MAX / 3, u32::MAX]) {
                    let mut buffer = Buffer::new(config);
                    buffer.set_target_from_jitter(jitter, frame_ticks);
                    let formula = 1 + (3 * u64::from(jitter)).div_ceil(u64::from(frame_ticks));
                    let expected = formula.clamp(1, u64::from(config.max_depth)) as u8;
                    assert_eq!(
                        buffer.target(),
                        expected,
                        "jitter {jitter}, frame {frame_ticks}"
                    );
                }
            }
        }
    }

    #[test]
    fn target_from_jitter() {
        let mut buffer = Buffer::new(Config::DEFAULT);
        buffer.set_target_from_jitter(0, 320);
        assert_eq!(buffer.target(), 1);
        buffer.set_target_from_jitter(107, 320); // 3J = 321: just over a frame
        assert_eq!(buffer.target(), 3);
        buffer.set_target_from_jitter(1_000_000, 320);
        assert_eq!(buffer.target(), 8);
    }

    #[test]
    fn usable_in_a_static() {
        static BUFFER: JitterBuffer<16, 640> = JitterBuffer::new(Config::DEFAULT);
        assert_eq!(BUFFER.buffered(), 0);
    }
}
