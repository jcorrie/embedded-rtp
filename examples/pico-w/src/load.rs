//! CPU load, for the `cpu-load` feature: a task at the back of the thread
//! executor counts how often it gets to run. Compared with a count taken
//! before Wi-Fi starts, that is the share of the core the rest of the
//! firmware leaves idle. The idle task never lets the core sleep, so this is
//! only for measuring.

use portable_atomic::{AtomicU32, Ordering};

use defmt::info;
use embassy_time::{Duration, Ticker, Timer};

static IDLE: AtomicU32 = AtomicU32::new(0);

/// How often the load is logged.
const PERIOD_SECS: u32 = 5;

#[embassy_executor::task]
pub async fn idle() -> ! {
    loop {
        IDLE.fetch_add(1, Ordering::Relaxed);
        embassy_futures::yield_now().await;
    }
}

/// Idle-task iterations in a second with only the audio running: what 0%
/// load looks like. Spawn `idle` first.
pub async fn baseline() -> u32 {
    let start = IDLE.load(Ordering::Relaxed);
    Timer::after_secs(1).await;
    let baseline = IDLE.load(Ordering::Relaxed).wrapping_sub(start);
    info!("cpu: {} idle iterations a second at rest", baseline);
    baseline
}

/// Log the load every few seconds, against `baseline`.
#[embassy_executor::task]
pub async fn report(baseline: u32) -> ! {
    let mut ticker = Ticker::every(Duration::from_secs(u64::from(PERIOD_SECS)));
    let mut last = IDLE.load(Ordering::Relaxed);
    loop {
        ticker.next().await;
        let count = IDLE.load(Ordering::Relaxed);
        let per_second = count.wrapping_sub(last) / PERIOD_SECS;
        last = count;
        // In tenths of a percent.
        let idle = (u64::from(per_second) * 1000 / u64::from(baseline.max(1))).min(1000);
        let busy = 1000 - idle;
        info!("cpu: {}.{}% busy", busy / 10, busy % 10);
    }
}
