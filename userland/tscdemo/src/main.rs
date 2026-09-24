//! Proves `SYS_NOW_NS` end-to-end: a ring-3 program reading the kernel's
//! TSC-based nanosecond clock (`kernel/src/tsc.rs`, calibrated against the
//! PIT at boot) through `myos_userlib::now_ns()`. Two readings are taken
//! with a spin-wait delay in between, then checked for the two properties
//! a correct calibration must give: monotonicity (`t2 >= t1` — a real
//! clock never runs backwards) and a plausible delta (a few million spin
//! iterations should burn somewhere between a microsecond and a few
//! seconds of wall time, not zero and not something absurd) — a
//! calibration bug is far more likely to produce a wildly wrong number
//! than a merely-slightly-wrong one, so this checks for "wildly wrong"
//! rather than trusting the delta blindly.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::Writer;

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let t1 = myos_userlib::now_ns();

    for _ in 0..5_000_000 {
        core::hint::spin_loop();
    }

    let t2 = myos_userlib::now_ns();
    let delta_ns = t2.wrapping_sub(t1);

    let _ = writeln!(Writer, "tscdemo: t1={t1} t2={t2} delta_ns={delta_ns}");

    if t2 >= t1 {
        let _ = writeln!(Writer, "tscdemo: MONOTONIC OK");
    } else {
        let _ = writeln!(Writer, "tscdemo: NOT MONOTONIC (bug!)");
    }

    // Plausibility window: 1000ns (1us) to 120 seconds worth of ns. Wide
    // on the top end deliberately: this kernel's scheduler round-robins
    // this thread against however many other demo threads are still
    // ready at the same time (hello/counter/keydemo/etc, each printing
    // and spinning too), so 5,000,000 spin iterations' *wall-clock* time
    // — as opposed to CPU time actually spent running this loop — can
    // genuinely run into the low tens of seconds under real contention,
    // confirmed in practice (a clean boot measured ~12.5s here). A
    // calibration bug (wrong TSC frequency, off-by-a-thousand unit
    // mixup, etc.) is expected to blow way past even this, not land just
    // outside a tighter bound that doesn't account for scheduling.
    if (1_000..=120_000_000_000).contains(&delta_ns) {
        let _ = writeln!(Writer, "tscdemo: delta looks plausible");
    } else {
        let _ = writeln!(Writer, "tscdemo: delta looks implausible");
    }

    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
