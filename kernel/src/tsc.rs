//! Nanosecond-resolution monotonic clock via the CPU's `RDTSC` cycle
//! counter.
//!
//! `RDTSC` is free-running and cheap (no port I/O, just an instruction),
//! but it counts CPU cycles, not time, and the CPU never tells software
//! its own clock frequency. The PIT (`task::time`) ticks at a frequency
//! we set ourselves and trust exactly, so at boot we time a short window
//! against `task::time::uptime_ms()` and use that to derive TSC cycles
//! per millisecond. Everything after `init()` just reads `RDTSC` and
//! converts using that fixed ratio.

use core::arch::x86_64::_rdtsc;
use core::sync::atomic::{AtomicU64, Ordering};

const CALIBRATION_WINDOW_MS: u64 = 50;

// Single-core kernel, no interrupt handler touches these — `Relaxed` is
// just interior mutability for a `static`, not actual cross-thread
// synchronization (same posture as `task::time::TICKS`).
static TSC_AT_INIT: AtomicU64 = AtomicU64::new(0);
static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);

/// Calibrates the TSC frequency against the PIT and resets the epoch for
/// `now_ns()`. Must run after `task::time::init()` (the PIT must already
/// be ticking) and before any `now_ns()` call.
///
/// Busy-waits on `task::time::uptime_ms()` rather than reprogramming or
/// polling the PIT's own counter register directly: `uptime_ms()` is
/// already the kernel's trusted, IRQ-driven millisecond source, so
/// reusing it keeps this module from needing its own port-I/O access to
/// timer hardware.
pub fn init() {
    let start_ms = crate::task::time::uptime_ms();
    while crate::task::time::uptime_ms() == start_ms {
        core::hint::spin_loop();
    }

    let calib_start_ms = crate::task::time::uptime_ms();
    let tsc_start = unsafe { _rdtsc() };

    let target_ms = calib_start_ms + CALIBRATION_WINDOW_MS;
    while crate::task::time::uptime_ms() < target_ms {
        core::hint::spin_loop();
    }

    let tsc_end = unsafe { _rdtsc() };
    let elapsed_ms = crate::task::time::uptime_ms() - calib_start_ms;

    let tsc_per_ms = (tsc_end - tsc_start) / elapsed_ms.max(1);

    TSC_PER_MS.store(tsc_per_ms, Ordering::Relaxed);
    TSC_AT_INIT.store(unsafe { _rdtsc() }, Ordering::Relaxed);
}

/// Nanoseconds since `init()` was called. Returns 0 if called before
/// `init()` (`TSC_PER_MS` still 0) rather than dividing by zero.
///
/// `delta_cycles * 1_000_000` is computed before dividing by
/// `TSC_PER_MS`, to avoid losing precision to integer division — but
/// that means this overflows `u64` once `delta_cycles` exceeds about
/// 1.8*10^13, which on a ~3GHz CPU is roughly 100 minutes of uptime.
/// Past that point `now_ns()` wraps and silently produces garbage
/// rather than a panic. Acceptable for a hobby kernel's boot-time
/// demos; a real long-running deployment would need a periodic
/// re-basing scheme this module doesn't implement.
pub fn now_ns() -> u64 {
    let tsc_per_ms = TSC_PER_MS.load(Ordering::Relaxed);
    if tsc_per_ms == 0 {
        return 0;
    }

    let now = unsafe { _rdtsc() };
    let delta_cycles = now.wrapping_sub(TSC_AT_INIT.load(Ordering::Relaxed));
    delta_cycles * 1_000_000 / tsc_per_ms
}
