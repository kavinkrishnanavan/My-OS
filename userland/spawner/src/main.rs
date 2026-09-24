//! Proves `SYS_SPAWN`/`SYS_WAIT`/`SYS_KILL`/`SYS_UPTIME_MS` end-to-end: a
//! *ring-3* program launching, waiting for, and forcibly terminating
//! other ring-3 programs, not just `main.rs::spawn_userland_demos`
//! deciding everything at boot. First spawns `hello.elf` (already on
//! disk — the same file `main.rs` itself boots) and waits for it
//! normally; then spawns `counter.elf` and kills it almost immediately,
//! proving `wait` unblocks with the killed sentinel (`u64::MAX`) instead
//! of hanging forever waiting for an exit that will now never come from
//! the victim itself; finally sleeps for a measured interval to prove
//! `sleep_ms`'s spin-wait against `uptime_ms` (PIT-tick-counted,
//! kernel-side) actually tracks real elapsed wall-clock time and isn't
//! just a fixed-iteration busy loop pretending to be one; finally calls
//! `SYS_MEMINFO` and prints the kernel's live heap/frame counters.

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
    let handle = myos_userlib::spawn_with_arg("hello.elf", "hi from spawner");
    let ok = handle != u64::MAX;
    let _ = writeln!(Writer, "spawner: spawn(hello.elf) ok={ok} handle={handle}");
    if ok {
        let status = myos_userlib::wait(handle);
        let _ = writeln!(
            Writer,
            "spawner: wait({handle}) returned - child exited status={status}"
        );
    }
    let victim = myos_userlib::spawn("counter.elf");
    let victim_ok = victim != u64::MAX;
    let _ = writeln!(Writer, "spawner: spawn(counter.elf) ok={victim_ok} handle={victim}");
    if victim_ok {
        let killed = myos_userlib::kill(victim);
        let status = myos_userlib::wait(victim);
        let _ = writeln!(
            Writer,
            "spawner: kill({victim}) ok={killed}, wait({victim}) returned status={status}"
        );
    }

    let before = myos_userlib::uptime_ms();
    myos_userlib::sleep_ms(500);
    let after = myos_userlib::uptime_ms();
    let _ = writeln!(Writer, "spawner: slept, uptime {before}ms -> {after}ms (elapsed {}ms)", after - before);

    let mem = myos_userlib::meminfo();
    let _ = writeln!(
        Writer,
        "spawner: meminfo heap_used={} heap_free={} frames_used={} frames_free={}",
        mem.heap_bytes_used, mem.heap_bytes_free, mem.frames_allocated_total, mem.frames_currently_free
    );

    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
