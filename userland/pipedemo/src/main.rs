//! Proves `SYS_PIPE` end-to-end: a *ring-3* program creating an in-kernel
//! pipe, writing a message into its write end, closing that end, and
//! reading the same bytes back out of the read end — all through the
//! already-existing `write_fd`/`read`/`close` wrappers, since a pipe fd
//! is meant to be indistinguishable from a file or socket fd once you
//! have one. Round-tripping bytes through `pipe()` this way is the
//! simplest possible proof that the kernel's pipe buffer, and its
//! fd-table wiring for two fds sharing one underlying object, both
//! actually work — not just that the syscall returns without faulting.
//!
//! The read side has to retry: a pipe read is non-blocking (returns
//! `WOULD_BLOCK` on an empty-but-open pipe, exactly like a socket read),
//! and nothing else in this single-threaded demo is going to wake it up
//! mid-syscall — so this spins the same bounded retry loop
//! `connect_blocking`/`resolve_blocking` already use elsewhere in
//! libmyos, rather than assuming the write is visible on the very first
//! read attempt.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{Writer, WOULD_BLOCK};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const MESSAGE: &[u8] = b"hello through a pipe";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let Some((read_fd, write_fd)) = myos_userlib::pipe() else {
        let _ = writeln!(Writer, "pipedemo: pipe() failed");
        myos_userlib::exit(1);
    };
    let _ = writeln!(Writer, "pipedemo: pipe() ok read_fd={read_fd} write_fd={write_fd}");

    let written = myos_userlib::write_fd(write_fd, MESSAGE);
    let _ = writeln!(Writer, "pipedemo: wrote {written} bytes to write_fd={write_fd}");
    myos_userlib::close(write_fd);

    let mut buf = [0u8; 64];
    let mut got: usize = 0;
    let mut hit_eof = false;
    for _ in 0..4000 {
        let n = myos_userlib::read(read_fd, &mut buf[got..]);
        match n {
            WOULD_BLOCK => {
                for _ in 0..200_000 {
                    core::hint::spin_loop();
                }
            }
            0 => {
                hit_eof = true;
                break;
            }
            u64::MAX => {
                let _ = writeln!(Writer, "pipedemo: read_fd={read_fd} read failed");
                break;
            }
            n => {
                got += n as usize;
                if got >= MESSAGE.len() {
                    break;
                }
            }
        }
    }
    myos_userlib::close(read_fd);

    let roundtripped = &buf[..got];
    if roundtripped == MESSAGE {
        if let Ok(s) = core::str::from_utf8(roundtripped) {
            let _ = writeln!(
                Writer,
                "pipedemo: pipe round-trip OK, got {got} bytes: {s}"
            );
        } else {
            let _ = writeln!(Writer, "pipedemo: pipe round-trip OK, got {got} bytes (non-utf8)");
        }
    } else {
        let _ = writeln!(
            Writer,
            "pipedemo: MISMATCH expected {} bytes got {got} bytes, hit_eof={hit_eof}",
            MESSAGE.len()
        );
    }

    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
