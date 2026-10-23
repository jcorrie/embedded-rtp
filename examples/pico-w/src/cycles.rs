//! Measures, in CPU cycles on the Pico W itself, what an intercom device
//! does each 20 ms frame: the same workload `examples/m0-cycles` counts
//! instructions for under QEMU. Needs only the board and a debug probe:
//!
//! ```sh
//! cargo run --release --bin cycles
//! ```
//!
//! SysTick runs at the core clock, so its counts are cycles, including flash
//! cache misses and the RP2040's ROM routines (`intrinsics`).

#![no_std]
#![no_main]

#[path = "../../common/device.rs"]
mod device;
#[path = "../../common/workload.rs"]
mod workload;

use cortex_m::peripheral::syst::SystClkSource;
use cortex_m::peripheral::SYST;
use defmt::info;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

const MASK: u32 = 0x00ff_ffff;

#[cortex_m_rt::entry]
fn main() -> ! {
    let _p = embassy_rp::init(Default::default());
    let hz = embassy_rp::clocks::clk_sys_freq();
    let mut core = cortex_m::Peripherals::take().unwrap();
    core.SYST.set_clock_source(SystClkSource::Core);
    core.SYST.set_reload(MASK);
    core.SYST.clear_current();
    core.SYST.enable_counter();

    static WORKLOAD: StaticCell<workload::Workload> = StaticCell::new();
    let workload = WORKLOAD.init_with(workload::Workload::new);
    let per_us = hz / 1_000_000;
    info!("cycles per operation at {} MHz:", per_us);
    workload.run(SYST::get_current, MASK, true, |name, cycles| {
        info!("{} cycles, {} us: {}", cycles, cycles / per_us, name);
    });
    info!("a 20 ms frame is {} cycles", hz / 50);
    loop {
        cortex_m::asm::wfi();
    }
}
