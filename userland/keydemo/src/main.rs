//! Proves `SYS_READ_KEY` end-to-end: a ring-3 program polling the
//! kernel's keyboard buffer through `myos_userlib::read_key()` and
//! printing whatever comes back, the same non-blocking-retry-loop shape
//! `userland/httpget` uses against `WOULD_BLOCK` for `connect_blocking`/
//! `resolve_blocking` — this kernel has no blocking syscalls, so a
//! userland spin-and-retry loop bounded by an iteration count (rather
//! than a real wall-clock timeout) is the normal pattern here.
//!
//! There is no interactive keyboard during an automated headless QEMU
//! boot test, so seeing zero keys is the *expected* outcome of most
//! runs, not a failure — the point of this demo is only to prove the
//! syscall path itself (dispatch, buffer read, `Option<u8>` decode)
//! works without crashing. The "saw 0 keys" case is logged with an
//! unambiguous message for exactly that reason, so a human reading the
//! serial log later doesn't mistake it for a bug.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::Writer;

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const POLL_ITERATIONS: u32 = 200_000;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut seen: u32 = 0;

    for _ in 0..POLL_ITERATIONS {
        if let Some(byte) = myos_userlib::read_key() {
            seen += 1;
            if (0x20..=0x7e).contains(&byte) {
                let _ = writeln!(
                    Writer,
                    "keydemo: got key '{}' (0x{:02x})",
                    byte as char,
                    byte
                );
            } else {
                let _ = writeln!(Writer, "keydemo: got key 0x{:02x} (non-printable)", byte);
            }
        }
    }

    if seen == 0 {
        let _ = writeln!(
            Writer,
            "keydemo: saw 0 keys in this boot (no interactive input during automated boot) - this is expected, not a bug"
        );
    } else {
        let _ = writeln!(Writer, "keydemo: saw {seen} key(s) total in this boot");
    }

    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
