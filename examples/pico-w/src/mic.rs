//! I2S MEMS microphone input (INMP441, ICS-43434, SPH0645 and the like):
//! the RP2040 drives the bit clock and word select, and reads the left
//! slot.
//!
//! `embassy-rp`'s own `PioI2sIn` clocks 16 bits per slot and samples as the
//! clock falls. These microphones want 32-bit slots (64 bit clocks a frame),
//! change their data as the clock falls, and send a 24-bit sample at the top
//! of the left slot. So this program clocks 32-bit slots, samples on the
//! rising edge and reads only the left slot: every 32-bit word is one
//! sample, and a dropped word can never swap the channels. Tie the
//! microphone's L/R pin low.
//!
//! From a parallel implementation of this example (PR #16), where the
//! timing was checked in a cycle-by-cycle simulation: 64 bit clocks a frame,
//! every sample on a rising edge, exactly the left slot's 32 bits, word
//! select one bit early. Not yet run on hardware.

use embassy_rp::dma;
use embassy_rp::gpio::Pull;
use embassy_rp::interrupt;
use embassy_rp::pio::{
    Common, Config, Direction, FifoJoin, Instance, LoadedProgram, PioPin, ShiftConfig,
    ShiftDirection, StateMachine,
};
use embassy_rp::pio_programs::clock_divider::calculate_pio_clock_divider;
use embassy_rp::Peri;
use embedded_rtp::profile::CLOCK_RATE;

pub struct MicProgram<'d, PIO: Instance> {
    prg: LoadedProgram<'d, PIO>,
}

impl<'d, PIO: Instance> MicProgram<'d, PIO> {
    pub fn new(common: &mut Common<'d, PIO>) -> Self {
        // Side-set bit 0 is the bit clock, bit 1 word select. Each
        // instruction is half a bit period: clock low, then high. The
        // microphone changes its data as the clock falls, so the program
        // samples as it rises, half a period later. (Sampling on the falling
        // edge would read the previous bit and shift every word by one.)
        // Word select changes one bit before each slot, as I2S has it: low
        // from the last bit of the right slot, high from the last bit of the
        // left. Only the left slot is shifted in: 32 bits, one word.
        let prg = pio::pio_asm!(
            ".side_set 2",
            "    set x, 29          side 0b00", // left bit 0, low half
            "left:",
            "    in pins, 1         side 0b01", // sample left bits 0..=29
            "    jmp x-- left       side 0b00",
            "    in pins, 1         side 0b01", // sample left bit 30
            "    nop                side 0b10", // word select rises
            "    in pins, 1         side 0b11", // sample left bit 31: word done
            "    set x, 29          side 0b10",
            "right:",
            "    nop                side 0b11", // right bits 0..=29, ignored
            "    jmp x-- right      side 0b10",
            "    nop                side 0b11", // right bit 30
            "    nop                side 0b00", // word select falls
            "    nop                side 0b01", // right bit 31
        );
        Self {
            prg: common.load_program(&prg.program),
        }
    }
}

/// Bit clocks per frame: two 32-bit slots.
const BITS_PER_FRAME: u32 = 64;
/// PIO cycles per bit clock.
const PIO_CYCLES_PER_BIT: u32 = 2;

pub struct Mic<'d, P: Instance, const S: usize> {
    dma: dma::Channel<'d>,
    sm: StateMachine<'d, P, S>,
}

impl<'d, P: Instance, const S: usize> Mic<'d, P, S> {
    /// `ws` must be the pin after `sck` (side-set pins are consecutive).
    #[allow(clippy::too_many_arguments)]
    pub fn new<D: dma::ChannelInstance>(
        common: &mut Common<'d, P>,
        mut sm: StateMachine<'d, P, S>,
        dma: Peri<'d, D>,
        irq: impl interrupt::typelevel::Binding<D::Interrupt, dma::InterruptHandler<D>> + 'd,
        sd: Peri<'d, impl PioPin>,
        sck: Peri<'d, impl PioPin>,
        ws: Peri<'d, impl PioPin>,
        program: &MicProgram<'d, P>,
    ) -> Self {
        let mut sd = common.make_pio_pin(sd);
        // The microphone leaves the data line floating outside its slot.
        sd.set_pull(Pull::Down);
        let sck = common.make_pio_pin(sck);
        let ws = common.make_pio_pin(ws);

        let mut cfg = Config::default();
        cfg.use_program(&program.prg, &[&sck, &ws]);
        cfg.set_in_pins(&[&sd]);
        cfg.clock_divider =
            calculate_pio_clock_divider(CLOCK_RATE * BITS_PER_FRAME * PIO_CYCLES_PER_BIT);
        cfg.shift_in = ShiftConfig {
            threshold: 32,
            direction: ShiftDirection::Left,
            auto_fill: true,
        };
        // Eight words of FIFO: 500 µs to restart DMA between buffers.
        cfg.fifo_join = FifoJoin::RxOnly;
        sm.set_config(&cfg);
        sm.set_pin_dirs(Direction::In, &[&sd]);
        sm.set_pin_dirs(Direction::Out, &[&sck, &ws]);

        Self {
            dma: dma::Channel::new(dma, irq),
            sm,
        }
    }

    pub fn start(&mut self) {
        self.sm.set_enable(true);
    }

    /// Fill `words` with samples, one per word (24 bits at the top); the
    /// transfer starts now and the future completes when it is full.
    pub fn read<'b>(&'b mut self, words: &'b mut [u32]) -> dma::Transfer<'b> {
        self.sm.rx().dma_pull(&mut self.dma, words, false)
    }
}
