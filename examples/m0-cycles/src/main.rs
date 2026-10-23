//! Counts the instructions an intercom device executes per 20 ms frame,
//! compiled for the Cortex-M0+ (ARMv6-M) and run under QEMU:
//!
//! ```sh
//! cd examples/m0-cycles && cargo run --release
//! ```
//!
//! QEMU has no RP2040, so this runs on its `mps2-an385` board, a
//! Cortex-M3, which executes ARMv6-M code unchanged: the instruction counts
//! are those of the same code on a Pico. QEMU's `-icount shift=0` makes each
//! instruction take 1 ns of virtual time, so SysTick, calibrated against a
//! loop of known length, counts instructions. A Cortex-M0+ takes one or two
//! cycles per instruction (loads, stores and taken branches take two), so
//! cycles are roughly 1.3 to 1.6 times these counts; `examples/pico-w`'s
//! `cycles` binary measures real cycles on hardware.

#![no_std]
#![no_main]

#[path = "../../common/device.rs"]
mod device;
#[path = "../../common/workload.rs"]
mod workload;

use cortex_m::peripheral::syst::SystClkSource;
use cortex_m::peripheral::SYST;
use cortex_m_rt::entry;
use cortex_m_semihosting::{debug, hprintln};
use panic_semihosting as _;

const MASK: u32 = 0x00ff_ffff;

/// CI fails if a whole device frame, both directions with SRTP, takes more
/// instructions than this: about a quarter of a 20 ms frame at 125 MHz.
const FRAME_BUDGET: u32 = 400_000;

/// Instructions per SysTick count, from a loop of exactly two instructions
/// per iteration.
fn calibrate() -> u32 {
    const ITERATIONS: u32 = 1_000_000;
    let start = SYST::get_current();
    unsafe {
        core::arch::asm!(
            "2:",
            "subs {n}, #1",
            "bne 2b",
            n = inout(reg) ITERATIONS => _,
            options(nomem, nostack),
        );
    }
    let ticks = start.wrapping_sub(SYST::get_current()) & MASK;
    (2 * ITERATIONS + ticks / 2) / ticks
}

#[entry]
fn main() -> ! {
    let mut p = cortex_m::Peripherals::take().unwrap();
    p.SYST.set_clock_source(SystClkSource::Core);
    p.SYST.set_reload(MASK);
    p.SYST.clear_current();
    p.SYST.enable_counter();

    let per_tick = calibrate();
    hprintln!(
        "ARMv6-M instructions per operation (SysTick x {}):",
        per_tick
    );
    let mut workload = workload::Workload::new();
    let mut frame = 0;
    workload.run(SYST::get_current, MASK, true, |name, ticks| {
        let instructions = ticks * per_tick;
        hprintln!("{:>9}  {}", instructions, name);
        if name.starts_with("device: whole frame") {
            frame = instructions;
        }
    });
    if frame == 0 || frame > FRAME_BUDGET {
        hprintln!(
            "a whole frame takes {} instructions; the budget is {}",
            frame,
            FRAME_BUDGET
        );
        debug::exit(debug::EXIT_FAILURE);
    }
    debug::exit(debug::EXIT_SUCCESS);
    loop {
        cortex_m::asm::wfi();
    }
}
