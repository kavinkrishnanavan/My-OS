//! Shared syscall wrappers + a minimal heap allocator for MyOS ring-3
//! programs — factored out of `userland/hello` and `userland/counter`,
//! which started out as straight copies of each other. That duplication
//! is exactly what caused two real bugs (an `fs::write` filename-length
//! failure and a `writeln!` vs raw-`write_bytes` mismatch, both fixed
//! independently in each copy before this existed): a fix or a new
//! syscall wrapper written once here reaches every program that depends
//! on it instead of needing to be repeated and kept in sync by hand.
//!
//! This is *not* a libc — no POSIX semantics, no `errno`, no signals.
//! It's a thin, honest wrapper around exactly the syscalls
//! `kernel/src/interrupts.rs`'s `syscall_handler` implements, using the
//! same loose "`u64::MAX` means failure" convention the kernel side
//! already uses; see that file for the actual ABI documentation (syscall
//! numbers, `rdi`/`rsi`/`rdx` argument meanings per call).

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;

const SYS_WRITE_BYTE: u64 = 1;
const SYS_EXIT: u64 = 2;
const SYS_SBRK: u64 = 3;
const SYS_OPEN: u64 = 4;
const SYS_READ: u64 = 5;
const SYS_CLOSE: u64 = 6;
const SYS_WRITE: u64 = 7;
const SYS_SPAWN: u64 = 8;
const SYS_WAIT: u64 = 9;
const SYS_GETARG: u64 = 10;
const SYS_CONNECT: u64 = 11;
const SYS_CONNECT_STATUS: u64 = 12;
const SYS_RESOLVE: u64 = 13;
const SYS_RESOLVE_STATUS: u64 = 14;
const SYS_KILL: u64 = 15;
const SYS_UPTIME_MS: u64 = 16;
const SYS_MEMINFO: u64 = 17;
const SYS_PIPE: u64 = 18;
const SYS_READ_KEY: u64 = 19;
const SYS_RTC_NOW: u64 = 20;
const SYS_GETPID: u64 = 21;
const SYS_YIELD: u64 = 22;
const SYS_NOW_NS: u64 = 23;
const SYS_LSEEK: u64 = 24;
const SYS_DUP: u64 = 25;
const SYS_DUP2: u64 = 26;
const SYS_CLOCK_GETTIME: u64 = 27;
const SYS_MKDIR: u64 = 28;
const SYS_UNLINK: u64 = 29;
const SYS_RENAME: u64 = 30;
const SYS_STAT: u64 = 31;
const SYS_MMAP_ANON: u64 = 32;
const SYS_MUNMAP: u64 = 33;

/// The stdout convention `write_fd`'s callers (and `Writer`) use —
/// `interrupts.rs`'s `SYS_WRITE` handler special-cases this straight to
/// serial rather than treating it as a real file descriptor.
pub const STDOUT: u64 = 1;

/// `sys_open`'s `mode` argument: open for reading (the file must exist).
pub const O_READ: u64 = 0;
/// `sys_open`'s `mode` argument: open for writing, creating or
/// truncating — nothing reaches disk until `close`.
pub const O_WRITE: u64 = 1;

/// `read`/`write_fd` return this for a socket that is open but would
/// block right now. The kernel keeps network progress moving in its own
/// polling task, so userland just retries later.
pub const WOULD_BLOCK: u64 = u64::MAX - 1;

/// Writes one byte via the original, one-syscall-per-character demo
/// call (`SYS_WRITE_BYTE`) — kept around for the ring-3 demos that
/// predate `write_fd`/`SYS_WRITE` (`main.rs`'s hand-assembled payloads),
/// not meant for new code, which should prefer `write_fd`/`Writer`.
pub fn write_byte(b: u8) {
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_WRITE_BYTE,
            in("dil") b,
        );
    }
}

/// Writes `bytes` to `fd` in one syscall — `fd == STDOUT` for serial
/// output, or a real file opened with `open(path, O_WRITE)`. Returns
/// how many bytes were accepted (`u64::MAX` on failure).
pub fn write_fd(fd: u64, bytes: &[u8]) -> u64 {
    let n: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_WRITE => n,
            in("rdi") fd,
            in("rsi") bytes.as_ptr() as u64,
            in("rdx") bytes.len() as u64,
        );
    }
    n
}

/// Lets `write!`/`writeln!` target stdout (`write_fd(STDOUT, ..)`)
/// directly, e.g. `writeln!(Writer, "count={n}")`.
pub struct Writer;

impl core::fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write_fd(STDOUT, s.as_bytes());
        Ok(())
    }
}

/// Grows (`increment > 0`) or just queries (`increment == 0`) this
/// process's heap by `increment` bytes, returning the break's value
/// *before* the call — the classic `sbrk` convention, so
/// `[return value, return value + increment)` is the newly available
/// region. Backing memory for `SbrkBumpAllocator` below; most programs
/// won't need to call this directly.
pub fn sbrk(increment: i64) -> *mut u8 {
    let old_break: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_SBRK => old_break,
            in("rdi") increment,
        );
    }
    old_break as *mut u8
}

/// `mmap_anon`'s `prot` argument: the mapped region is writable.
pub const PROT_WRITE: u64 = 1;
/// `mmap_anon`'s `prot` argument: the mapped region is executable.
pub const PROT_EXEC: u64 = 2;

/// Maps a fresh anonymous region of at least `bytes` bytes, with `prot`
/// (`PROT_WRITE`/`PROT_EXEC`, bitwise-or'd) controlling its permissions.
/// Returns the region's virtual address, or `None` on failure (out of
/// memory). This is a bump arena kernel-side, not a general-purpose VMA
/// map — see `munmap`'s doc comment for the LIFO-only unmap restriction
/// that implies.
pub fn mmap_anon(bytes: usize, prot: u64) -> Option<*mut u8> {
    let addr: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_MMAP_ANON => addr,
            in("rdi") bytes as u64,
            in("rsi") prot,
        );
    }
    if addr == u64::MAX {
        None
    } else {
        Some(addr as *mut u8)
    }
}

/// Unmaps a region previously returned by `mmap_anon`. `addr`/`bytes`
/// must be exactly that call's result and size — the kernel-side arena
/// is a bump allocator that can only reclaim its most recent allocation
/// (LIFO-only unmap, not general-purpose), so unmapping anything else
/// fails. Returns `true` on success.
pub fn munmap(addr: *mut u8, bytes: usize) -> bool {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_MUNMAP => result,
            in("rdi") addr as u64,
            in("rsi") bytes as u64,
        );
    }
    result == 0
}

/// Opens `path` (`O_READ` or `O_WRITE` — see their own doc comments),
/// returning a file descriptor (`u64::MAX` on failure).
pub fn open(path: &str, mode: u64) -> u64 {
    let fd: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_OPEN => fd,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") mode,
        );
    }
    fd
}

/// Creates a directory at `path`. Returns `0` success, `u64::MAX` failure.
pub fn mkdir(path: &str) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_MKDIR => result,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
        );
    }
    result
}

/// Removes the file at `path`. Returns `0` success, `u64::MAX` failure.
pub fn unlink(path: &str) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_UNLINK => result,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
        );
    }
    result
}

/// Renames/moves `old_path` to `new_path`. Returns `0` success, `u64::MAX`
/// failure.
pub fn rename(old_path: &str, new_path: &str) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_RENAME => result,
            in("rdi") old_path.as_ptr() as u64,
            in("rsi") old_path.len() as u64,
            in("rdx") new_path.as_ptr() as u64,
            in("r8") new_path.len() as u64,
        );
    }
    result
}

/// Result of `stat`: whether `path` exists, whether it's a directory, and
/// its size in bytes (`0` for a directory, or for a nonexistent path).
pub struct Stat {
    pub exists: bool,
    pub is_dir: bool,
    pub size: u64,
}

/// Looks up `path` without opening it. The kernel writes three `u64`s
/// (`[exists, is_dir, size]`) into a caller-owned buffer, the same
/// "write through a pointer" shape as `meminfo`/`get_arg`. Always
/// returns successfully — check the returned `Stat::exists` for whether
/// `path` actually existed, not a `u64::MAX` sentinel.
pub fn stat(path: &str) -> Stat {
    let mut buf = [0u64; 3];
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_STAT,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") buf.as_mut_ptr() as u64,
        );
    }
    Stat {
        exists: buf[0] != 0,
        is_dir: buf[1] != 0,
        size: buf[2],
    }
}

/// Reads up to `buf.len()` bytes from `fd` into `buf`, returning how
/// many actually landed (`0` at EOF, `u64::MAX` on failure). Only valid
/// for an `O_READ` `fd`.
pub fn read(fd: u64, buf: &mut [u8]) -> u64 {
    let n: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_READ => n,
            in("rdi") fd,
            in("rsi") buf.as_mut_ptr() as u64,
            in("rdx") buf.len() as u64,
        );
    }
    n
}

/// Closes `fd`. For an `O_WRITE` `fd`, this is the only point anything
/// actually reaches disk (see `kernel/src/task/thread.rs`'s
/// `sys_close`) — check the return value (`0` success, `u64::MAX`
/// failure) if that matters to the caller; a failure here (e.g. a
/// filename `embedded-sdmmc`'s 8.3 limit rejects) means nothing was
/// actually written, silently, unless the kernel's own serial log is
/// being watched.
pub fn close(fd: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_CLOSE => result,
            in("rdi") fd,
        );
    }
    result
}

pub const SEEK_SET: u64 = 0;
pub const SEEK_CUR: u64 = 1;
pub const SEEK_END: u64 = 2;

/// Moves an `O_READ` `fd`'s position (`whence` is one of the `SEEK_*`
/// constants). Returns the new absolute position, or `u64::MAX` for a
/// non-seekable fd.
pub fn lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    let pos: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_LSEEK => pos,
            in("rdi") fd,
            in("rsi") offset,
            in("rdx") whence,
        );
    }
    pos
}

/// Duplicates `fd` into a fresh fd number. Returns `u64::MAX` for a kind
/// with no sensible duplicate (see `kernel/src/task/thread.rs`'s
/// `sys_dup` doc comment for exactly which fd kinds support this).
pub fn dup(fd: u64) -> u64 {
    let new_fd: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_DUP => new_fd,
            in("rdi") fd,
        );
    }
    new_fd
}

/// Duplicates `fd` into the caller-chosen `new_fd`, closing whatever
/// `new_fd` previously held first. Returns `new_fd` on success,
/// `u64::MAX` on failure.
pub fn dup2(fd: u64, new_fd: u64) -> u64 {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_DUP2 => result,
            in("rdi") fd,
            in("rsi") new_fd,
        );
    }
    result
}

/// Loads and runs `path` (a file on the same filesystem `open`/`read`
/// reach) as a brand new, independent ring-3 process — the first syscall
/// that lets a program running *under* this OS launch another one,
/// rather than only `main.rs` deciding what runs at boot. Returns an
/// opaque non-`u64::MAX` handle on success (not a real PID — nothing
/// here does anything with it besides check success/failure, or pass to
/// `wait`); `u64::MAX` if `path` doesn't exist or isn't a valid ELF this
/// kernel's loader (`kernel/src/elf.rs`) accepts. Equivalent to
/// `spawn_with_arg(path, "")`.
pub fn spawn(path: &str) -> u64 {
    spawn_with_arg(path, "")
}

/// `spawn`, plus one argument string the new process can read back via
/// `get_arg`/`arg_string` — there's no argv array, just this one string,
/// since nothing yet needs more than that.
pub fn spawn_with_arg(path: &str, arg: &str) -> u64 {
    let id: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_SPAWN => id,
            in("rdi") path.as_ptr() as u64,
            in("rsi") path.len() as u64,
            in("rdx") arg.as_ptr() as u64,
            in("r8") arg.len() as u64,
        );
    }
    id
}

/// Copies this process's own spawn-time argument string (set by whoever
/// `spawn_with_arg`'d it; empty if plain `spawn`, or not spawned via
/// `SYS_SPAWN` at all — e.g. `main.rs`'s own boot-time demos) into `buf`,
/// returning how many bytes actually landed.
pub fn get_arg(buf: &mut [u8]) -> usize {
    let n: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_GETARG => n,
            in("rdi") buf.as_mut_ptr() as u64,
            in("rsi") buf.len() as u64,
        );
    }
    n as usize
}

/// Starts a TCP connection to an IPv4 endpoint. Returns an fd immediately
/// (`u64::MAX` on failure); poll it with `connect_status` before reading
/// or writing.
pub fn connect(ip_be: u32, port: u16) -> u64 {
    let fd: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_CONNECT => fd,
            in("rdi") ip_be as u64,
            in("rsi") port as u64,
        );
    }
    fd
}

/// Returns `1` once a socket fd is established, `0` while the handshake
/// is still in flight, and `2` if it closed/failed or is not a socket.
pub fn connect_status(fd: u64) -> u64 {
    let status: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_CONNECT_STATUS => status,
            in("rdi") fd,
        );
    }
    status
}

/// `connect`, then spin-poll until the TCP handshake either establishes
/// or fails. This mirrors `wait`: blocking is userland retrying around a
/// non-blocking kernel primitive, which keeps the syscall path simple.
///
/// Bounded, not an infinite loop: an unreachable target (dead IP, no
/// route, a firewall that silently drops the SYN instead of answering
/// with a reset) never reaches `connect_status`'s `2` — smoltcp just
/// keeps retransmitting the SYN — so without a cap this spun forever the
/// first time this was tried against a since-dead hardcoded address (see
/// `userland/httpget`'s own history). A few thousand retries at ~a few
/// hundred thousand spin iterations apiece is on the order of a minute,
/// generous for a real handshake (usually one or two round trips) while
/// still eventually giving up on a genuinely dead target.
pub fn connect_blocking(ip_be: u32, port: u16) -> u64 {
    let fd = connect(ip_be, port);
    if fd == u64::MAX {
        return fd;
    }
    for _ in 0..4000 {
        match connect_status(fd) {
            1 => return fd,
            2 => {
                close(fd);
                return u64::MAX;
            }
            _ => {
                for _ in 0..200_000 {
                    core::hint::spin_loop();
                }
            }
        }
    }
    close(fd);
    u64::MAX
}

/// Starts an A-record DNS query for `host`. Returns an fd immediately
/// (`u64::MAX` on failure); poll it with `resolve_status`.
pub fn resolve(host: &str) -> u64 {
    let fd: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_RESOLVE => fd,
            in("rdi") host.as_ptr() as u64,
            in("rsi") host.len() as u64,
        );
    }
    fd
}

/// Polls a `resolve` fd. `WOULD_BLOCK` while still in flight (the fd
/// stays open — call again); on a terminal result, the fd is already
/// consumed kernel-side (see `sys_resolve_status`'s own doc comment) and
/// this returns either the resolved address, zero-extended into the low
/// 32 bits (safe to distinguish from either sentinel — see the same doc
/// comment for why), or `u64::MAX` on failure.
pub fn resolve_status(fd: u64) -> u64 {
    let status: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_RESOLVE_STATUS => status,
            in("rdi") fd,
        );
    }
    status
}

/// `resolve`, then spin-poll until it either resolves or fails —
/// `connect_blocking`'s counterpart for DNS. Bounded the same way and
/// for the same reason (see `connect_blocking`'s own doc comment): a
/// hostname with no A record, or one queried against an unreachable/
/// non-responding DNS server, otherwise never reaches a terminal state.
pub fn resolve_blocking(host: &str) -> Option<u32> {
    let fd = resolve(host);
    if fd == u64::MAX {
        return None;
    }
    for _ in 0..4000 {
        match resolve_status(fd) {
            WOULD_BLOCK => {
                for _ in 0..200_000 {
                    core::hint::spin_loop();
                }
            }
            u64::MAX => return None,
            ip => return Some(ip as u32),
        }
    }
    None
}

/// `get_arg` into a freshly allocated owned buffer — the common case for
/// a program that just wants its argument as a `&str` without picking a
/// fixed-size stack buffer itself. `None` if the bytes aren't valid UTF-8
/// (an empty arg — the common "not spawned with one" case — still
/// returns `Some("")`, not `None`).
pub fn arg_string() -> Option<alloc::string::String> {
    let mut buf = alloc::vec![0u8; 256];
    let n = get_arg(&mut buf);
    buf.truncate(n);
    alloc::string::String::from_utf8(buf).ok()
}

/// Blocks (via a retry loop, not a real blocking syscall — see
/// `kernel/src/task/thread.rs`'s `sys_wait` for why) until `handle` (a
/// `spawn` return value) has exited, returning whatever it passed to
/// `exit`. Each retry is a full syscall round-trip, which is what
/// actually lets the scheduler preempt this thread and make progress on
/// the child in between attempts; the pause between attempts just avoids
/// hammering the scheduler lock harder than this needs to.
pub fn wait(handle: u64) -> u64 {
    let mut status: u64 = 0;
    loop {
        let done: u64;
        unsafe {
            asm!(
                "int 0x80",
                inout("rax") SYS_WAIT => done,
                in("rdi") handle,
                in("rsi") &mut status as *mut u64 as u64,
            );
        }
        if done == 1 {
            return status;
        }
        for _ in 0..200_000 {
            core::hint::spin_loop();
        }
    }
}

/// Forcibly terminates another process (`handle`, a `spawn` return
/// value) — never your own (that returns `false`; use `exit` for that).
/// Returns `true` if it existed and was killed; `false` for an invalid,
/// already-gone, or self `handle`. Anything already (or later)
/// `wait`-ing on `handle` unblocks with status `u64::MAX` — `sys_kill`'s
/// own "this process didn't choose to exit, it was killed" sentinel.
pub fn kill(handle: u64) -> bool {
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_KILL => result,
            in("rdi") handle,
        );
    }
    result == 0
}

/// Milliseconds since boot (PIT-tick-counted — see `kernel/src/task/time.rs`).
pub fn uptime_ms() -> u64 {
    let ms: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_UPTIME_MS => ms,
        );
    }
    ms
}

/// Nanoseconds since boot-time TSC calibration (see `kernel/src/tsc.rs`) —
/// higher resolution than `uptime_ms()`, monotonic, not wall-clock.
pub fn now_ns() -> u64 {
    let ns: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_NOW_NS => ns,
        );
    }
    ns
}

/// This thread's own `ThreadId`, as a plain `u64` (the same value
/// `SYS_SPAWN`/`SYS_WAIT`/`SYS_KILL` hand around as a "pid" elsewhere).
pub fn getpid() -> u64 {
    let pid: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_GETPID => pid,
        );
    }
    pid
}

/// Gives up the rest of this thread's current timeslice voluntarily,
/// instead of waiting for the PIT to preempt it — a no-op if nothing else
/// is currently ready to run.
pub fn yield_now() {
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_YIELD => _,
        );
    }
}

/// Blocks the calling thread for at least `ms` milliseconds — a spin-wait
/// against `uptime_ms`, not a real scheduler-level sleep (this kernel has
/// no such primitive; see `sys_wait`'s doc comment for the same
/// "no way to suspend a thread mid-syscall" reason). Still real waiting,
/// not busy work that starves everyone else: this call returns to
/// ordinary user code between checks, exactly like `wait`/
/// `connect_blocking`'s own retry loops, so the scheduler keeps
/// preempting this thread normally and other threads make progress in
/// the meantime.
pub fn sleep_ms(ms: u64) {
    let deadline = uptime_ms() + ms;
    while uptime_ms() < deadline {
        for _ in 0..50_000 {
            core::hint::spin_loop();
        }
    }
}

/// Returns the next buffered keypress as an ASCII byte, or `None` if
/// nothing is currently buffered — non-blocking, always returns
/// immediately (there's no way to block in a syscall handler in this
/// kernel, so a caller wanting to wait for a keypress must retry in a
/// loop itself, the same "userland does the spinning" shape as
/// `wait`/`connect_blocking`).
pub fn read_key() -> Option<u8> {
    let key: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_READ_KEY => key,
        );
    }
    if key == u64::MAX {
        None
    } else {
        Some(key as u8)
    }
}

/// Wall-clock time read from the kernel's CMOS RTC driver, as of the
/// moment of the call.
pub struct RtcTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Reads the current wall-clock time from the kernel's CMOS RTC driver.
/// The kernel writes six `u32`s into a caller-owned buffer in one syscall
/// (the same "write through a pointer" shape as `meminfo`/`get_arg`)
/// rather than packing them into `rax`. Always succeeds (no `u64::MAX`
/// sentinel for this one).
pub fn rtc_now() -> RtcTime {
    let mut buf = [0u32; 6];
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_RTC_NOW,
            in("rdi") buf.as_mut_ptr() as u64,
        );
    }
    RtcTime {
        year: buf[0],
        month: buf[1],
        day: buf[2],
        hour: buf[3],
        minute: buf[4],
        second: buf[5],
    }
}

/// Wall-clock time with sub-second resolution: the CMOS RTC's six fields
/// plus `nanos` (0..1_000_000_000) filled in from the TSC clock — see
/// `kernel/src/task/thread.rs`'s `sys_clock_gettime` doc comment for the
/// precision caveat (the two clocks are read back to back, not fused
/// atomically).
pub struct ClockTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanos: u32,
}

/// Reads `ClockTime` in one syscall. Always succeeds.
pub fn clock_gettime() -> ClockTime {
    let mut buf = [0u32; 7];
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_CLOCK_GETTIME,
            in("rdi") buf.as_mut_ptr() as u64,
        );
    }
    ClockTime {
        year: buf[0],
        month: buf[1],
        day: buf[2],
        hour: buf[3],
        minute: buf[4],
        second: buf[5],
        nanos: buf[6],
    }
}

/// Snapshot of kernel-side heap and physical-frame accounting, as of the
/// moment of the call — see `MemInfo`'s field docs for what each count
/// means. Always succeeds (no `u64::MAX` sentinel for this one).
pub struct MemInfo {
    pub heap_bytes_used: u64,
    pub heap_bytes_free: u64,
    pub frames_allocated_total: u64,
    pub frames_currently_free: u64,
}

/// Reads the kernel's live heap/frame-allocator counters into a
/// `MemInfo`. The kernel writes all four `u64`s into a caller-owned
/// buffer in one syscall (the same "write through a pointer" shape as
/// `get_arg`), rather than returning them packed into `rax`, since there's
/// more state here than one register can hold.
pub fn meminfo() -> MemInfo {
    let mut buf = [0u64; 4];
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_MEMINFO,
            in("rdi") buf.as_mut_ptr() as u64,
        );
    }
    MemInfo {
        heap_bytes_used: buf[0],
        heap_bytes_free: buf[1],
        frames_allocated_total: buf[2],
        frames_currently_free: buf[3],
    }
}

/// Creates an in-kernel pipe: a pair of fds (`read_fd`, `write_fd`)
/// connected to opposite ends of the same one-directional byte buffer,
/// following this kernel's existing fd-allocation convention (plain
/// `u64`s starting at `FIRST_REAL_FD`). Neither fd needs `open` — both
/// are already valid arguments to `read`/`write_fd`/`close`, exactly as
/// if they'd come from a file or socket. Returns `None` on failure (the
/// caller's buffer is left untouched kernel-side in that case, so there's
/// nothing meaningful to read out of it).
///
/// Non-blocking, same as sockets: `read` on an empty-but-still-open pipe,
/// or `write_fd` on a full one, returns `WOULD_BLOCK` rather than
/// stalling the syscall — retry later (see `connect_blocking`'s doc
/// comment for the general shape of that retry loop).
pub fn pipe() -> Option<(u64, u64)> {
    let mut fds: [u64; 2] = [0; 2];
    let result: u64;
    unsafe {
        asm!(
            "int 0x80",
            inout("rax") SYS_PIPE => result,
            in("rdi") fds.as_mut_ptr() as u64,
        );
    }
    if result == 0 {
        Some((fds[0], fds[1]))
    } else {
        None
    }
}

/// Terminates this thread — never returns. Whatever's ready next (some
/// other thread, or this kernel's idle loop) resumes in its place.
/// `status` is an arbitrary caller-defined code (`wait`, if anything
/// waits on this process, gets it back) — this kernel doesn't interpret
/// it, so any convention (0 = success or otherwise) is up to the caller.
pub fn exit(status: u64) -> ! {
    unsafe {
        asm!(
            "int 0x80",
            in("rax") SYS_EXIT,
            in("rdi") status,
            options(noreturn),
        );
    }
}

/// Opens `path` for reading, reads it to the end, and closes it —
/// `None` if the open failed. The common case every program that just
/// wants a whole file's bytes was hand-rolling before this existed.
pub fn read_whole(path: &str) -> Option<Vec<u8>> {
    let fd = open(path, O_READ);
    if fd == u64::MAX {
        return None;
    }
    let mut contents = Vec::new();
    let mut chunk = [0u8; 64];
    loop {
        let n = read(fd, &mut chunk);
        if n == 0 || n == u64::MAX {
            break;
        }
        contents.extend_from_slice(&chunk[..n as usize]);
    }
    close(fd);
    Some(contents)
}

/// Opens `path` for writing, writes `data` in one call, and closes it —
/// `true` only if every step (including the close-time `fs::write`)
/// succeeded. The write-side counterpart to `read_whole`.
pub fn write_whole(path: &str, data: &[u8]) -> bool {
    let fd = open(path, O_WRITE);
    if fd == u64::MAX {
        return false;
    }
    let wrote = write_fd(fd, data) == data.len() as u64;
    let closed = close(fd) == 0;
    wrote && closed
}

/// A bump allocator backed by `sbrk`: peeks the current break, rounds up
/// for the requested alignment, then reserves exactly that much. Never
/// reclaims anything (`dealloc` is a no-op) — same "leak by design"
/// tradeoff the kernel's own frame allocator and `sbrk`
/// (`task::thread::sbrk`) already make; reasonable for a program that
/// runs once and exits, not something meant to survive long-running
/// reuse. Install it with:
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: myos_userlib::SbrkBumpAllocator = myos_userlib::SbrkBumpAllocator;
/// ```
pub struct SbrkBumpAllocator;

unsafe impl GlobalAlloc for SbrkBumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let current = sbrk(0) as usize; // peek, doesn't move the break
        let aligned = (current + layout.align() - 1) & !(layout.align() - 1);
        let reserve = (aligned - current) + layout.size();
        let base = sbrk(reserve as i64) as usize;
        (base + (aligned - current)) as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
