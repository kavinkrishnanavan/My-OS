//! Proves `SYS_MKDIR`/`SYS_STAT`/`SYS_RENAME`/`SYS_UNLINK` end-to-end: a
//! *ring-3* program creating a directory, writing a file into it via the
//! already-existing `open`/`write_fd`/`close` wrappers, `stat`-ing that
//! file and the directory itself to confirm the kernel's metadata (size,
//! `is_dir`) matches what was actually written, renaming the file and
//! confirming the old path disappears while the new one carries the same
//! size forward, and finally unlinking it and confirming it's gone. Each
//! step prints its own ok/fail line so a serial log reader can see
//! exactly which filesystem operation broke, rather than just a final
//! pass/fail with no way to tell which syscall is at fault.

#![no_std]
#![no_main]

extern crate alloc;

use core::fmt::Write as _;
use core::panic::PanicInfo;
use myos_userlib::{Writer, O_WRITE};

#[global_allocator]
static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;

const MESSAGE: &[u8] = b"hello from fsdemo";

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Step 1: mkdir
    let mkdir_ok = myos_userlib::mkdir("FSDEMO") == 0;
    let _ = writeln!(Writer, "fsdemo: mkdir(FSDEMO) ok={mkdir_ok}");
    if !mkdir_ok {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 1: mkdir(FSDEMO)");
        myos_userlib::exit(1);
    }

    // Step 2: write a known message into FSDEMO/A.TXT
    let fd = myos_userlib::open("FSDEMO/A.TXT", O_WRITE);
    if fd == u64::MAX {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 2: open(FSDEMO/A.TXT, O_WRITE)");
        myos_userlib::exit(1);
    }
    let written = myos_userlib::write_fd(fd, MESSAGE);
    myos_userlib::close(fd);
    let write_ok = written == MESSAGE.len() as u64;
    let _ = writeln!(
        Writer,
        "fsdemo: wrote {written} bytes to FSDEMO/A.TXT (expected {}) ok={write_ok}",
        MESSAGE.len()
    );
    if !write_ok {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 2: write_fd size mismatch");
        myos_userlib::exit(1);
    }

    // Step 3: stat FSDEMO/A.TXT and confirm size matches
    let st = myos_userlib::stat("FSDEMO/A.TXT");
    let _ = writeln!(
        Writer,
        "fsdemo: stat(FSDEMO/A.TXT) exists={} is_dir={} size={}",
        st.exists, st.is_dir, st.size
    );
    if !st.exists || st.is_dir || st.size != MESSAGE.len() as u64 {
        let _ = writeln!(
            Writer,
            "fsdemo: FAILED at step 3: stat(FSDEMO/A.TXT) MISMATCH (expected exists=true is_dir=false size={})",
            MESSAGE.len()
        );
        myos_userlib::exit(1);
    }
    let _ = writeln!(Writer, "fsdemo: stat(FSDEMO/A.TXT) size OK");

    // Step 4: stat FSDEMO and confirm is_dir
    let dir_st = myos_userlib::stat("FSDEMO");
    let _ = writeln!(
        Writer,
        "fsdemo: stat(FSDEMO) exists={} is_dir={} size={}",
        dir_st.exists, dir_st.is_dir, dir_st.size
    );
    if !dir_st.exists || !dir_st.is_dir {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 4: stat(FSDEMO) MISMATCH (expected exists=true is_dir=true)");
        myos_userlib::exit(1);
    }
    let _ = writeln!(Writer, "fsdemo: stat(FSDEMO) is_dir OK");

    // Step 5: stat a nonexistent path
    let nope_st = myos_userlib::stat("FSDEMO/NOPE.TXT");
    let _ = writeln!(
        Writer,
        "fsdemo: stat(FSDEMO/NOPE.TXT) exists={} is_dir={} size={}",
        nope_st.exists, nope_st.is_dir, nope_st.size
    );
    if nope_st.exists {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 5: stat(FSDEMO/NOPE.TXT) MISMATCH (expected exists=false)");
        myos_userlib::exit(1);
    }
    let _ = writeln!(Writer, "fsdemo: stat(FSDEMO/NOPE.TXT) exists=false OK");

    // Step 6: rename FSDEMO/A.TXT -> FSDEMO/B.TXT
    let rename_ok = myos_userlib::rename("FSDEMO/A.TXT", "FSDEMO/B.TXT") == 0;
    let _ = writeln!(Writer, "fsdemo: rename(FSDEMO/A.TXT -> FSDEMO/B.TXT) ok={rename_ok}");
    if !rename_ok {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 6: rename");
        myos_userlib::exit(1);
    }
    let old_st = myos_userlib::stat("FSDEMO/A.TXT");
    let new_st = myos_userlib::stat("FSDEMO/B.TXT");
    let _ = writeln!(
        Writer,
        "fsdemo: post-rename stat(FSDEMO/A.TXT) exists={} | stat(FSDEMO/B.TXT) exists={} is_dir={} size={}",
        old_st.exists, new_st.exists, new_st.is_dir, new_st.size
    );
    if old_st.exists || !new_st.exists || new_st.is_dir || new_st.size != MESSAGE.len() as u64 {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 6: rename post-check MISMATCH");
        myos_userlib::exit(1);
    }
    let _ = writeln!(Writer, "fsdemo: rename post-check OK (old gone, new present with matching size)");

    // Step 7: unlink FSDEMO/B.TXT
    let unlink_ok = myos_userlib::unlink("FSDEMO/B.TXT") == 0;
    let _ = writeln!(Writer, "fsdemo: unlink(FSDEMO/B.TXT) ok={unlink_ok}");
    if !unlink_ok {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 7: unlink");
        myos_userlib::exit(1);
    }
    let gone_st = myos_userlib::stat("FSDEMO/B.TXT");
    let _ = writeln!(Writer, "fsdemo: post-unlink stat(FSDEMO/B.TXT) exists={}", gone_st.exists);
    if gone_st.exists {
        let _ = writeln!(Writer, "fsdemo: FAILED at step 7: unlink post-check MISMATCH (still exists)");
        myos_userlib::exit(1);
    }
    let _ = writeln!(Writer, "fsdemo: unlink post-check OK");

    let _ = writeln!(Writer, "fsdemo: ALL OK");
    myos_userlib::exit(0);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
