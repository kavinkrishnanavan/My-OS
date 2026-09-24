//! A second, genuinely different real program — not another copy of
//! `userland/hello`, loaded and run *concurrently* with it via its own
//! `spawn_elf` call and its own isolated address space (see
//! `main.rs::spawn_userland_demos`). The point is proving the kernel
//! hosts distinct real workloads at once, not just multiple instances of
//! one hand-written demo the way Milestone 3's A/B test did.
//!
//! Syscall wrappers/allocator come from `myos_userlib` (see
//! `userland/libmyos`) — factored out after this file and
//! `userland/hello`'s started out as straight copies of each other.
//!
//! Reads `kernel.txt` — the file the *kernel itself* writes at boot (see
//! `main.rs::verify_filesystem`), not something the host seeded. Also
//! writes `ctrout.txt` and reads it straight back — the first file *this
//! kernel* has ever had written to it by ring-3 code, not just by the
//! kernel itself (`fs::write`, for `kernel.txt`) or the host (`builder`,
//! for `hello.txt`). (8.3-short-filename-compliant on purpose — an
//! earlier, longer name here silently failed to persist at close time,
//! caught only once `sys_close`'s error became visible over serial.)

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{read_whole, write_whole, Writer};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const EXPECTED: &[u8] = b"written by the kernel itself\n";
const OWN_OUTPUT: &[u8] = b"written by counter.elf itself\n";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let read_ok = read_whole("kernel.txt").is_some_and(|c| c == EXPECTED);

    let write_ok = write_whole("ctrout.txt", OWN_OUTPUT)
        && read_whole("ctrout.txt").is_some_and(|c| c == OWN_OUTPUT);

    for i in 0..5 {
        let _ = writeln!(
            Writer,
            "counter[{i}]: kernel.txt ok={read_ok} ctrout.txt ok={write_ok}"
        );
        for _ in 0..3_000_000 {
            core::hint::spin_loop();
        }
    }
    myos_userlib::exit(if read_ok && write_ok { 0 } else { 1 });
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
