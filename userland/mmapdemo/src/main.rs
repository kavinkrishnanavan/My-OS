//! Proves `SYS_MMAP_ANON`/`SYS_MUNMAP` end-to-end: a *ring-3* program
//! mapping a fresh anonymous region, writing a recognizable byte pattern
//! through the raw returned pointer and reading it back to confirm the
//! mapping is actually backed by real, writable memory (not just a
//! non-null address that faults on touch), unmapping it, and then
//! mapping a second fresh region to prove the kernel-side bump arena's
//! cursor wasn't corrupted by the unmap — i.e. the arena can still hand
//! out usable memory afterward rather than being left in a broken state
//! by the LIFO-only reclaim.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{Writer, PROT_WRITE};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const REGION_SIZE: usize = 8192;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Step 1: map a fresh anonymous region
    let Some(addr) = myos_userlib::mmap_anon(REGION_SIZE, PROT_WRITE) else {
        let _ = writeln!(Writer, "mmapdemo: FAILED at step 1: mmap_anon({REGION_SIZE}) returned None");
        myos_userlib::exit(1);
    };
    let _ = writeln!(Writer, "mmapdemo: mmap_anon({REGION_SIZE}) ok addr={addr:p}");

    // Step 2: write a recognizable byte pattern through the raw pointer,
    // then read it back and confirm every byte matches.
    let mut mismatch_at: Option<usize> = None;
    unsafe {
        for i in 0..REGION_SIZE {
            addr.add(i).write((i % 256) as u8);
        }
        for i in 0..REGION_SIZE {
            let got = addr.add(i).read();
            let want = (i % 256) as u8;
            if got != want {
                mismatch_at = Some(i);
                break;
            }
        }
    }
    match mismatch_at {
        None => {
            let _ = writeln!(Writer, "mmapdemo: pattern write/read-back OK ({REGION_SIZE} bytes verified)");
        }
        Some(offset) => {
            let _ = writeln!(Writer, "mmapdemo: FAILED at step 2: MISMATCH at offset {offset}");
            myos_userlib::exit(1);
        }
    }

    // Step 3: unmap the region
    let unmap_ok = myos_userlib::munmap(addr, REGION_SIZE);
    let _ = writeln!(Writer, "mmapdemo: munmap(addr, {REGION_SIZE}) ok={unmap_ok}");
    if !unmap_ok {
        let _ = writeln!(Writer, "mmapdemo: FAILED at step 3: munmap");
        myos_userlib::exit(1);
    }

    // Step 4: map a fresh region again, proving the arena cursor still
    // works after the unmap.
    let second = myos_userlib::mmap_anon(REGION_SIZE, PROT_WRITE);
    let second_ok = second.is_some();
    let _ = writeln!(
        Writer,
        "mmapdemo: mmap_anon({REGION_SIZE}) after munmap ok={second_ok} addr={:?}",
        second
    );
    if !second_ok {
        let _ = writeln!(Writer, "mmapdemo: FAILED at step 4: mmap_anon after munmap");
        myos_userlib::exit(1);
    }

    let _ = writeln!(Writer, "mmapdemo: ALL OK");
    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
