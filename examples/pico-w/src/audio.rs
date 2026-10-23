//! I2S audio: a microphone in and an amplifier out, one 20 ms frame at a
//! time, each double-buffered over DMA so there is a whole frame period to
//! hand frames to and from the network task.
//!
//! Wiring (3.3 V parts):
//!
//! | Signal | Pico W | INMP441 microphone | MAX98357A amplifier |
//! |---|---|---|---|
//! | Bit clock | GP10 / GP18 | SCK | BCLK |
//! | Word clock | GP11 / GP19 | WS | LRC |
//! | Data | GP12 / GP20 | SD | DIN |
//!
//! The microphone's L/R pin goes to ground (left channel). Each bus runs at
//! 16 kHz with 16-bit stereo slots; the amplifier gets the same sample in
//! both channels.

use embassy_futures::join::join;
use embassy_rp::dma;
use embassy_rp::pio::{Common, Instance, PioPin, StateMachine};
use embassy_rp::pio_programs::i2s::{PioI2sIn, PioI2sInProgram, PioI2sOut, PioI2sOutProgram};
use embassy_rp::{interrupt, Peri};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embedded_rtp::profile::{CLOCK_RATE, FRAME_SAMPLES};
use static_cell::StaticCell;

use crate::device::Frame;

/// Frames captured from the microphone, for the network task. Two deep: if
/// the network task falls behind, the newest frames win.
pub static CAPTURED: Channel<CriticalSectionRawMutex, Frame, 2> = Channel::new();
/// The next frame to play, from the network task. If none has arrived when
/// the amplifier needs it, it plays silence.
pub static PLAYBACK: Signal<CriticalSectionRawMutex, Frame> = Signal::new();

/// Turn the microphone's 24-bit samples (in the top of each slot) up a
/// little: 16 bits of it is quiet at speaking distance.
const MIC_GAIN_SHIFT: u32 = 2;

pub struct Pins<'d, A: PioPin, B: PioPin, C: PioPin, D: PioPin, E: PioPin, F: PioPin> {
    pub mic_data: Peri<'d, A>,
    pub mic_bit_clock: Peri<'d, B>,
    pub mic_word_clock: Peri<'d, C>,
    pub amp_data: Peri<'d, D>,
    pub amp_bit_clock: Peri<'d, E>,
    pub amp_word_clock: Peri<'d, F>,
}

pub struct I2s<P: Instance + 'static> {
    mic: PioI2sIn<'static, P, 0>,
    amp: PioI2sOut<'static, P, 1>,
}

impl<P: Instance> I2s<P> {
    #[allow(clippy::too_many_arguments)]
    pub fn new<D0: dma::ChannelInstance, D1: dma::ChannelInstance>(
        common: &mut Common<'static, P>,
        sm0: StateMachine<'static, P, 0>,
        sm1: StateMachine<'static, P, 1>,
        dma_in: Peri<'static, D0>,
        dma_out: Peri<'static, D1>,
        irqs: impl interrupt::typelevel::Binding<D0::Interrupt, dma::InterruptHandler<D0>>
            + interrupt::typelevel::Binding<D1::Interrupt, dma::InterruptHandler<D1>>
            + 'static,
        pins: Pins<
            'static,
            impl PioPin,
            impl PioPin,
            impl PioPin,
            impl PioPin,
            impl PioPin,
            impl PioPin,
        >,
    ) -> Self {
        let in_program = PioI2sInProgram::new(common);
        let out_program = PioI2sOutProgram::new(common);
        let mic = PioI2sIn::new(
            common,
            sm0,
            dma_in,
            irqs,
            true, // pull the data line down when the microphone isn't driving it
            pins.mic_data,
            pins.mic_bit_clock,
            pins.mic_word_clock,
            CLOCK_RATE,
            16,
            2,
            &in_program,
        );
        let amp = PioI2sOut::new(
            common,
            sm1,
            dma_out,
            irqs,
            pins.amp_data,
            pins.amp_bit_clock,
            pins.amp_word_clock,
            CLOCK_RATE,
            16,
            &out_program,
        );
        Self { mic, amp }
    }
}

/// One `u32` per sample period: left in the top half, right in the bottom.
type Words = [u32; FRAME_SAMPLES];

#[embassy_executor::task]
pub async fn run(mut i2s: I2s<embassy_rp::peripherals::PIO1>) -> ! {
    static BUFFERS: StaticCell<[Words; 4]> = StaticCell::new();
    let [mic_a, mic_b, amp_a, amp_b] = BUFFERS.init([[0; FRAME_SAMPLES]; 4]);
    let (mut mic_front, mut mic_back) = (mic_a, mic_b);
    let (mut amp_front, mut amp_back) = (amp_a, amp_b);
    i2s.mic.start();
    i2s.amp.start();
    loop {
        // Start the next frame in each direction first; the DMA runs while
        // the last frame is handed over below.
        let transfers = join(i2s.mic.read(mic_front), i2s.amp.write(amp_front));

        let mut captured = [0i16; FRAME_SAMPLES];
        for (sample, word) in captured.iter_mut().zip(mic_back.iter()) {
            let left = (*word >> 16) as i16;
            *sample = left.saturating_mul(1 << MIC_GAIN_SHIFT);
        }
        if CAPTURED.try_send(captured).is_err() {
            // The network task is behind: drop the oldest frame.
            let _ = CAPTURED.try_receive();
            let _ = CAPTURED.try_send(captured);
        }
        let playback = PLAYBACK.try_take().unwrap_or([0; FRAME_SAMPLES]);
        for (word, sample) in amp_back.iter_mut().zip(playback) {
            *word = u32::from(sample as u16) * 0x1_0001;
        }

        transfers.await;
        core::mem::swap(&mut mic_front, &mut mic_back);
        core::mem::swap(&mut amp_front, &mut amp_back);
    }
}
