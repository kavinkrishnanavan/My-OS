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
//! just a fixed-iteration busy loop pretending to be one; calls
//! `SYS_MEMINFO` and prints the kernel's live heap/frame counters; and
//! finally proves `SYS_GETPID`/`SYS_YIELD` by printing its own pid and
//! confirming it's unchanged after voluntarily yielding.

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

    let pid = myos_userlib::getpid();
    let _ = writeln!(Writer, "spawner: getpid()={pid}");
    myos_userlib::yield_now();
    let _ = writeln!(Writer, "spawner: yield_now() returned, still pid={}", myos_userlib::getpid());

    // Proves SYS_LSEEK/SYS_DUP/SYS_DUP2: open the same kernel.txt main.rs
    // already wrote+read-back at boot, seek to its exact midpoint, read
    // the second half, then seek back to the start via a *duplicated* fd
    // (dup2's target) and confirm it reads the same bytes from the top.
    let fd = myos_userlib::open("kernel.txt", myos_userlib::O_READ);
    if fd != u64::MAX {
        let end = myos_userlib::lseek(fd, 0, myos_userlib::SEEK_END);
        let mid = end / 2;
        let new_pos = myos_userlib::lseek(fd, mid as i64, myos_userlib::SEEK_SET);
        let mut second_half = [0u8; 64];
        let n = myos_userlib::read(fd, &mut second_half[..(end - mid) as usize]);

        let dup_fd = myos_userlib::dup(fd);
        let dup2_fd = 50; // an arbitrary unused fd number for dup2's explicit target
        let dup2_result = myos_userlib::dup2(fd, dup2_fd);
        let _ = myos_userlib::lseek(dup2_fd, 0, myos_userlib::SEEK_SET);
        let mut first_half = [0u8; 64];
        let n2 = myos_userlib::read(dup2_fd, &mut first_half[..mid as usize]);

        let _ = writeln!(
            Writer,
            "spawner: lseek end={end} mid_seek_to={new_pos} read_second_half={n}B dup_fd={dup_fd} dup2_result={dup2_result} read_via_dup2={n2}B fd_still_independent={}",
            myos_userlib::lseek(fd, 0, myos_userlib::SEEK_CUR) == end
        );

        myos_userlib::close(dup_fd);
        myos_userlib::close(dup2_fd);
        myos_userlib::close(fd);
    } else {
        let _ = writeln!(Writer, "spawner: lseek/dup demo skipped - kernel.txt not open-able");
    }

    // Proves SYS_CLOCK_GETTIME: sub-second-resolution time alongside the
    // whole-second SYS_RTC_NOW rtcdemo already proved.
    let ct = myos_userlib::clock_gettime();
    let _ = writeln!(
        Writer,
        "spawner: clock_gettime {:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:09}",
        ct.year, ct.month, ct.day, ct.hour, ct.minute, ct.second, ct.nanos
    );

    // Proves SYS_GETUID/GETPPID/GETCWD/CHDIR/UMASK/ISATTY.
    let mut cwd_buf = [0u8; 32];
    let cwd_len = myos_userlib::getcwd(&mut cwd_buf);
    let mut cwd_before_buf = [0u8; 32];
    cwd_before_buf[..cwd_len].copy_from_slice(&cwd_buf[..cwd_len]);
    let cwd_before = core::str::from_utf8(&cwd_before_buf[..cwd_len]).unwrap_or("?");
    let chdir_ok = myos_userlib::chdir("SUBDIR");
    let cwd_len2 = myos_userlib::getcwd(&mut cwd_buf);
    let cwd_after = core::str::from_utf8(&cwd_buf[..cwd_len2]).unwrap_or("?");
    let chdir_bogus = myos_userlib::chdir("NOPE_NOT_A_DIR");
    let old_umask = myos_userlib::umask(0o077);
    let _ = writeln!(
        Writer,
        "spawner: getuid={} getppid={} cwd_before={cwd_before:?} chdir(SUBDIR)={chdir_ok} cwd_after={cwd_after:?} chdir(bogus)={chdir_bogus} umask(0o077) old={old_umask:#o} isatty(1)={} isatty(99)={}",
        myos_userlib::getuid(),
        myos_userlib::getppid(),
        myos_userlib::isatty(1),
        myos_userlib::isatty(99)
    );

    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
