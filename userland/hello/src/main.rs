//! A real, separately-compiled ring-3 program — the whole point of
//! `kernel/src/elf.rs`. Everything Milestones 2 and 3 ran was a
//! hand-assembled byte blob the kernel copied in itself; this is
//! ordinary Rust, cross-compiled to `x86_64-unknown-none` and linked
//! (see `link.ld`) as a plain static ELF64 executable, then loaded by
//! parsing that ELF's program headers — proof the loader handles
//! whatever a real toolchain actually produces, not just bytes shaped
//! to fit.
//!
//! Syscall wrappers/allocator come from `myos_userlib` (see
//! `userland/libmyos`) — factored out after this file and
//! `userland/counter`'s started out as straight copies of each other.
//!
//! Sequence: open+read+close `hello.txt` — the file `builder/src/main.rs`
//! seeds on the host (see `fs.rs`'s boot-time check, which reads the
//! same file kernel-side) — into a heap-allocated `Vec<u8>` and compare
//! it against the known expected bytes, proof this program's own
//! syscalls/`alloc` all actually work together, not just that the
//! syscall numbers exist. Finally, print a fixed count and actually
//! exit, proving `task::thread::exit_current` really removes this
//! thread from the scheduler instead of just hanging (every ring-3 demo
//! before this one looped forever).

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{read_whole, Writer};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const EXPECTED: &[u8] = b"Hello from the host filesystem!\n";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let ok = read_whole("hello.txt").is_some_and(|c| c == EXPECTED);
    // Empty when boot-spawned (main.rs::spawn_userland_demos passes no
    // arg); a real string when userland/spawner spawns this instead —
    // proof SYS_SPAWN's argument actually reaches the child, not just
    // that the syscall accepts one.
    let arg = myos_userlib::arg_string().unwrap_or_default();

    for i in 0..5 {
        let _ = writeln!(Writer, "hello[{i}]: file ok={ok} arg={arg:?}");
        for _ in 0..3_000_000 {
            core::hint::spin_loop();
        }
    }
    myos_userlib::exit(if ok { 0 } else { 1 });
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
