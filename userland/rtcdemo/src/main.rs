//! Proves `SYS_RTC_NOW` end-to-end: a ring-3 program reading the CMOS
//! real-time clock through `myos_userlib::rtc_now()` and printing the
//! six fields it returns. Unlike `userland/keydemo`'s syscall, this one
//! always succeeds (no buffered-vs-empty distinction), so the only real
//! correctness signal available from an automated boot log — there's no
//! way to compare against true wall-clock time from inside the demo
//! itself — is a plausibility check on the returned fields: year roughly
//! 2000-2100, month 1-12, day 1-31, hour under 24, minute/second under
//! 60. A failure there means the CMOS read or its BCD-to-binary decode
//! is wrong, not that the demo itself crashed.

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
    let t = myos_userlib::rtc_now();

    let _ = writeln!(
        Writer,
        "rtcdemo: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    );

    let mut problems = alloc::vec::Vec::new();
    if !(2000..=2100).contains(&t.year) {
        problems.push("year");
    }
    if !(1..=12).contains(&t.month) {
        problems.push("month");
    }
    if !(1..=31).contains(&t.day) {
        problems.push("day");
    }
    if t.hour >= 24 {
        problems.push("hour");
    }
    if t.minute >= 60 {
        problems.push("minute");
    }
    if t.second >= 60 {
        problems.push("second");
    }

    if problems.is_empty() {
        let _ = writeln!(Writer, "rtcdemo: values look sane");
    } else {
        let _ = writeln!(Writer, "rtcdemo: SUSPICIOUS value(s): {problems:?}");
    }

    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
