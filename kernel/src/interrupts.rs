//! IDT + legacy 8259 PIC.
//!
//! The whole point of routing the NIC's IRQ through here (instead of
//! spinning on a "did a packet arrive yet?" register in a loop) is the
//! non-blocking architecture: the handler below does the absolute
//! minimum — ack the PIC and wake a task — and returns. The actual
//! packet processing happens later in `Executor::run`, cooperatively,
//! whenever the CPU is otherwise idle. Nothing in this kernel ever spins
//! waiting for the network; it registers interest and moves on.
//!
//! `extern "x86-interrupt"` (the calling convention the CPU's interrupt
//! mechanism actually needs — it can't use the normal C ABI, since an
//! interrupt lands mid-instruction with no call-site to set up a
//! return) is still nightly-only in this toolchain, so every handler
//! below is a small hand-written naked-function trampoline instead:
//! save every general-purpose register, call into ordinary safe Rust,
//! restore the registers, `iretq`. Stable naked functions
//! (`#[unsafe(naked)]` + `naked_asm!`) make this workable without nightly.

use crate::{gdt, keyboard, net, pci, rtc, serial_println, task::time};
use core::arch::naked_asm;
use lazy_static::lazy_static;
use pic8259::ChainedPics;
use spin::Mutex;
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::{PrivilegeLevel, VirtAddr};

/// The `int` vector ring-3 code uses to ask the kernel to do something on
/// its behalf — the only door back into ring 0 (see `syscall_trampoline`
/// and `task::thread`'s ring-3 support).
pub const SYSCALL_VECTOR: u8 = 0x80;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

pub static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

/// IRQ lines QEMU/real hardware commonly route a PCI NIC to. We register
/// the same handler on all of them and only unmask the one that matches
/// what the device's PCI config space actually reports at driver init —
/// see `net::rtl8139::init`.
const CANDIDATE_NIC_IRQS: [u8; 4] = [9, 10, 11, 12];

/// Saves rax,rcx,rdx,rbx,rsi,rdi,rbp,r8-r15 (15 registers, 120 bytes)
/// *and* the full SSE/x87/MMX register file (`fxsave`, 512 bytes),
/// points rdi at the CPU-pushed interrupt frame above all of that, calls
/// `$handler`, restores everything, and `iretq`s back to whatever was
/// interrupted. Used for vectors that push no error code and that must
/// resume the interrupted code (IRQs, breakpoint).
///
/// The `fxsave` block matters more than it looks: x86_64's baseline ABI
/// always has SSE2 available, so the compiler is free to use `xmm`
/// registers for perfectly ordinary code — and once real crypto (AES-GCM,
/// P-256 ECDH, SHA-2 for TLS) is in the picture, it *will*. Without
/// saving that state here, an interrupt landing mid-computation would
/// silently corrupt whatever the interrupted code had in `xmm0-15` the
/// moment this handler's own code (or the Rust runtime under it) touched
/// any of those registers — a heisenbug that only shows up once in a
/// while, under load, and never in a debugger single-stepping through it.
///
/// `fxsave`/`fxrstor` fault (#GP) unless their operand address is
/// 16-byte aligned, and — despite Intel's SDM saying the CPU aligns RSP
/// to 16 bytes before pushing the interrupt frame — empirically that
/// alignment did not hold here (this exact trampoline double-faulted at
/// `fxsave` before the fix below, with `CR4.OSFXSR` confirmed already
/// set, ruling out the more common #UD cause). Rather than pin down
/// exactly which assumption about hardware/QEMU behavior was wrong, this
/// aligns unconditionally and robustly: save the pre-alignment RSP in
/// `rbx`, force-align down to 16 with `and rsp, -16`, reserve the fxsave
/// area, and on the way out jump RSP directly back to the saved value —
/// correct no matter what alignment we actually entered with.
///
/// `rbx` specifically, not `rax`/`rcx`/`rdx`/etc: those are
/// caller-saved in the SysV ABI, so `$handler` (an ordinary Rust
/// `extern "C"` fn) is free to clobber them across the `call` below —
/// which is exactly what happened using `rax` here originally, silently
/// destroying the saved restore pointer and sending `iretq` off to a
/// near-null `rsp`. `rbx` (along with `rbp`, `r12-r15`) is callee-saved:
/// the ABI guarantees `$handler` preserves it.
macro_rules! trampoline_return {
    ($name:ident, $handler:path) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push rax", "push rcx", "push rdx", "push rbx",
                "push rsi", "push rdi", "push rbp",
                "push r8", "push r9", "push r10", "push r11",
                "push r12", "push r13", "push r14", "push r15",
                "mov rdi, rsp",
                "add rdi, 15*8",
                "mov rbx, rsp",  // remember where the GPR save area starts (callee-saved reg — see doc comment)
                "and rsp, -16",  // force-align for fxsave (and for `call`)
                "sub rsp, 512",  // fxsave area
                "fxsave [rsp]",
                "call {handler}",
                "fxrstor [rsp]",
                "mov rsp, rbx",  // jump straight back; undoes the align + sub in one move
                "pop r15", "pop r14", "pop r13", "pop r12", "pop r11",
                "pop r10", "pop r9", "pop r8",
                "pop rbp", "pop rdi", "pop rsi",
                "pop rbx", "pop rdx", "pop rcx", "pop rax",
                "iretq",
                handler = sym $handler,
            )
        }
    };
}

/// For exceptions that never return (we panic and halt) and that push a
/// 64-bit error code below the interrupt frame (double fault, page
/// fault). The error code becomes the handler's second argument; no
/// register save/restore is needed since execution never resumes.
macro_rules! trampoline_diverging_errcode {
    ($name:ident, $handler:path) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "mov rsi, [rsp]",  // error code
                "lea rdi, [rsp + 8]", // interrupt frame starts above it
                "call {handler}",
                handler = sym $handler,
            )
        }
    };
}

/// Layout the CPU itself pushes on interrupt entry. We read it directly
/// instead of depending on x86_64's `InterruptStackFrame` (whose public
/// constructor is tied to the same nightly-only ABI machinery).
#[repr(C)]
#[derive(Debug)]
pub struct RawInterruptFrame {
    pub instruction_pointer: u64,
    pub code_segment: u64,
    pub cpu_flags: u64,
    pub stack_pointer: u64,
    pub stack_segment: u64,
}

/// The timer's trampoline can't use `trampoline_return!` like the others:
/// a plain IRQ handler always resumes the exact context it interrupted,
/// but the timer is also where preemptive scheduling happens (see
/// `task::thread`), which needs to resume a *different* thread's saved
/// context sometimes. The difference from `trampoline_return!` is small
/// but load-bearing:
///   - `rsi` additionally hands the handler `gpr_rsp` (where this GPR
///     save area starts) — the "identity" of the frame it's currently on.
///   - the handler's return value (`rax`) — not the fixed `rbx` the other
///     trampolines use — says which frame to resume, so it can be a
///     different thread's.
///   - restoring FPU/SSE state has to happen *after* picking the frame
///     to resume (`mov rsp, rax` first), from `rax`'s own fxsave area
///     (`align_down(rax, 16) - 512`), not the frame being switched away
///     from — otherwise a switch would restore the outgoing thread's own
///     FPU state right back into itself and never load the incoming
///     thread's, corrupting it silently on every switch.
#[unsafe(naked)]
extern "C" fn timer_trampoline() {
    naked_asm!(
        "push rax", "push rcx", "push rdx", "push rbx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "mov rdi, rsp",
        "add rdi, 15*8", // rdi = &RawInterruptFrame (1st arg)
        "mov rsi, rsp",  // rsi = this frame's gpr_rsp (2nd arg)
        "and rsp, -16",
        "sub rsp, 512",
        "fxsave [rsp]",
        "call {handler}", // rax = gpr_rsp to resume — same thread, or a switch
        "mov rcx, rax",
        "and rcx, -16",
        "sub rcx, 512",
        "fxrstor [rcx]",
        "mov rsp, rax",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11",
        "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi",
        "pop rbx", "pop rdx", "pop rcx", "pop rax",
        "iretq",
        handler = sym timer_interrupt_handler,
    )
}

trampoline_return!(nic_trampoline, nic_interrupt_handler);
trampoline_return!(keyboard_trampoline, keyboard_interrupt_handler);
trampoline_return!(breakpoint_trampoline, breakpoint_handler);
trampoline_diverging_errcode!(double_fault_trampoline, double_fault_handler);
trampoline_diverging_errcode!(page_fault_trampoline, page_fault_handler);
trampoline_diverging_errcode!(general_protection_fault_trampoline, general_protection_fault_handler);

/// The block `push rax` through `push r15` leaves on the stack, read back
/// low-address-first (where `rsp` ends up pointing). Each `push`
/// decrements `rsp`, so the *last* register pushed (`r15`) lands at the
/// *lowest* address — hence `r15` first here and `rax` (pushed first)
/// last, the reverse of the push order in the trampoline below.
#[repr(C)]
struct SavedGprs {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rbx: u64,
    rdx: u64,
    rcx: u64,
    rax: u64,
}

/// The `int 0x80` gate. Structurally identical to `timer_trampoline`
/// (same rationale: `rax`'s return value picks which frame to resume,
/// fxsave/fxrstor ordering follows the same "restore only after picking
/// the frame" rule — see that trampoline's doc comment for why), not
/// `trampoline_return!`, because a syscall *can* be a scheduling point
/// now: `SYS_EXIT` (see `syscall_handler`) needs to resume some *other*
/// thread entirely, since the calling one no longer exists. It also
/// additionally hands the handler a pointer to the saved GPRs, not just
/// the CPU-pushed frame, since the syscall number and argument travel in
/// `rax`/`rdi`, which plain IRQ handlers never need to see.
#[unsafe(naked)]
extern "C" fn syscall_trampoline() {
    naked_asm!(
        "push rax", "push rcx", "push rdx", "push rbx",
        "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11",
        "push r12", "push r13", "push r14", "push r15",
        "mov rdi, rsp",
        "add rdi, 15*8", // rdi = &RawInterruptFrame (1st arg)
        "mov rsi, rsp",  // rsi = &SavedGprs (2nd arg)
        "and rsp, -16",
        "sub rsp, 512",
        "fxsave [rsp]",
        "call {handler}", // rax = gpr_rsp to resume — same thread, or a switch on exit
        "mov rcx, rax",
        "and rcx, -16",
        "sub rcx, 512",
        "fxrstor [rcx]",
        "mov rsp, rax",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11",
        "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi",
        "pop rbx", "pop rdx", "pop rcx", "pop rax",
        "iretq",
        handler = sym syscall_handler,
    )
}

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        unsafe {
            idt.breakpoint
                .set_handler_addr(fn_addr(breakpoint_trampoline));
            idt.page_fault
                .set_handler_addr(fn_addr(page_fault_trampoline));
            idt.double_fault
                .set_handler_addr(fn_addr(double_fault_trampoline))
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
            idt.general_protection_fault
                .set_handler_addr(fn_addr(general_protection_fault_trampoline));
            for irq in CANDIDATE_NIC_IRQS {
                idt[PIC_1_OFFSET + irq].set_handler_addr(fn_addr(nic_trampoline));
            }
            idt[PIC_1_OFFSET].set_handler_addr(fn_addr(timer_trampoline));
            // IRQ1: the legacy PS/2 keyboard controller — a fixed line on
            // real hardware and every PC emulator (unlike the NIC, which
            // needs a PCI scan to know which of several possible lines
            // it's on), so this is unmasked unconditionally in `init()`
            // rather than waiting for a driver's own init call.
            idt[PIC_1_OFFSET + 1].set_handler_addr(fn_addr(keyboard_trampoline));
            // DPL 3: without this, ring-3 code executing `int 0x80` gets
            // an immediate #GP instead of reaching our handler — IDT
            // gates default to DPL 0, i.e. "only the kernel may invoke
            // this via `int`" (hardware/software IRQs above aren't
            // affected; they're never triggered by an `int` instruction).
            idt[SYSCALL_VECTOR]
                .set_handler_addr(fn_addr(syscall_trampoline))
                .set_privilege_level(PrivilegeLevel::Ring3);
        }
        idt
    };
}

fn fn_addr(f: extern "C" fn()) -> VirtAddr {
    VirtAddr::new(f as usize as u64)
}

pub fn init() {
    IDT.load();
    unsafe { PICS.lock().initialize() };
    // Mask everything to start; individual drivers unmask the lines they
    // actually own once they've registered their handler.
    unsafe { PICS.lock().write_masks(0xFF, 0xFF) };
    x86_64::instructions::interrupts::enable();
    unmask_irq(1); // keyboard — always present, unlike the NIC's PCI-scanned line
}

/// Must run with interrupts disabled for its duration: it holds the
/// `PICS` lock while interrupts are (briefly) enabled elsewhere, and the
/// very IRQ line being unmasked here can fire before this function
/// returns. Without `without_interrupts`, that IRQ's handler would try
/// to re-lock `PICS` for its EOI while this function still holds it —
/// a single-CPU self-deadlock, and since interrupt gates hardware-disable
/// interrupts for the ISR's duration, nothing would ever break it out.
pub fn unmask_irq(irq: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut pics = PICS.lock();
        unsafe {
            let masks = pics.read_masks();
            let mut new_masks = masks;
            if irq < 8 {
                new_masks[0] = masks[0] & !(1 << irq);
            } else {
                new_masks[1] = masks[1] & !(1 << (irq - 8));
            }
            pics.write_masks(new_masks[0], new_masks[1]);
        }
    });
}

extern "C" fn breakpoint_handler(frame: *const RawInterruptFrame) {
    serial_println!("EXCEPTION: BREAKPOINT\n{:#x?}", unsafe { &*frame });
}

extern "C" fn double_fault_handler(frame: *const RawInterruptFrame, error_code: u64) -> ! {
    panic!(
        "EXCEPTION: DOUBLE FAULT (error_code={:#x})\n{:#x?}",
        error_code,
        unsafe { &*frame }
    );
}

extern "C" fn page_fault_handler(frame: *const RawInterruptFrame, error_code: u64) -> ! {
    let addr = x86_64::registers::control::Cr2::read();
    panic!(
        "EXCEPTION: PAGE FAULT at {:?} (error_code={:#x})\n{:#x?}",
        addr,
        error_code,
        unsafe { &*frame }
    );
}

/// Fires whenever anything (most relevantly: a ring-3 thread) attempts a
/// privileged operation the CPU itself refuses — this is the enforcement
/// this whole milestone exists to prove is real, not just "code happens
/// to run in ring 3". Unhandled until now: before this existed, any such
/// fault (e.g. bringing up a buggy ring-3 payload) hit an empty IDT entry
/// instead of a clear diagnostic.
extern "C" fn general_protection_fault_handler(frame: *const RawInterruptFrame, error_code: u64) -> ! {
    panic!(
        "EXCEPTION: GENERAL PROTECTION FAULT (error_code={:#x})\n{:#x?}",
        error_code,
        unsafe { &*frame }
    );
}

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
const SYS_GETUID: u64 = 34;
const SYS_GETGID: u64 = 35;
const SYS_GETEUID: u64 = 36;
const SYS_GETEGID: u64 = 37;
const SYS_GETPPID: u64 = 38;
const SYS_GETCWD: u64 = 39;
const SYS_CHDIR: u64 = 40;
const SYS_UMASK: u64 = 41;
const SYS_ISATTY: u64 = 42;
const SYS_USOCK_CREATE: u64 = 43;
const SYS_USOCK_BIND_LISTEN: u64 = 44;
const SYS_USOCK_ACCEPT: u64 = 45;
const SYS_USOCK_CONNECT: u64 = 46;
const SYS_USOCK_READ: u64 = 47;
const SYS_USOCK_WRITE: u64 = 48;

/// Upper bound on one `SYS_WRITE` call — it reads directly out of user
/// memory with no length-vs-actual-mapping validation (see the handler's
/// own note), so this is a coarse guard against a buggy/hostile caller
/// claiming an absurd length, not a real safety mechanism.
const SYS_WRITE_MAX_LEN: u64 = 1 << 20;

/// The kernel side of the syscalls ring-3 code can make (see
/// `task::thread`'s user-mode payloads): `rax` = syscall number, `rdi` =
/// argument, matching the classic x86_64 Linux syscall convention (not
/// load-bearing here, just a familiar one to reuse rather than invent).
/// Returns the `gpr_rsp` `syscall_trampoline` should resume from — the
/// calling thread's own (`gprs as u64` — the `SavedGprs` pointer *is*
/// its `gpr_rsp`) for every syscall except `SYS_EXIT`, which hands back
/// some *other* ready thread's instead, since this one is gone.
///
/// `gprs` is `*mut`, not `*const`: it's a pointer into the *same* stack
/// memory `syscall_trampoline`'s `pop`s read back into the real
/// registers on the way out, so writing `g.rax` here is how a syscall
/// hands a return value back (`SYS_SBRK` does this; the classic
/// x86_64-Linux convention this loosely follows, reused for familiarity).
extern "C" fn syscall_handler(_frame: *const RawInterruptFrame, gprs: *mut SavedGprs) -> u64 {
    let gprs_addr = gprs as u64;
    let g = unsafe { &mut *gprs };
    match g.rax {
        SYS_WRITE_BYTE => {
            if let Some(c) = char::from_u32(g.rdi as u8 as u32) {
                crate::serial_print!("{c}");
            }
            gprs_addr
        }
        // `rdi` = exit status, stored for whatever `SYS_WAIT`s on this thread.
        SYS_EXIT => crate::task::thread::exit_current(g.rdi),
        SYS_SBRK => {
            g.rax = crate::task::thread::sbrk(g.rdi as i64);
            gprs_addr
        }
        // rdi/rsi/rdx as arg1/arg2/arg3 — the classic x86_64-Linux
        // syscall convention, same reasoning as `rax`/`rdi` above.
        // SYS_OPEN's `rdx` = mode: 0 read, 1 write (create/truncate).
        SYS_OPEN => {
            g.rax = crate::task::thread::sys_open(g.rdi, g.rsi, g.rdx);
            gprs_addr
        }
        SYS_READ => {
            g.rax = crate::task::thread::sys_read(g.rdi, g.rsi, g.rdx);
            gprs_addr
        }
        SYS_CLOSE => {
            g.rax = crate::task::thread::sys_close(g.rdi);
            gprs_addr
        }
        // `rdi` = fd, `rsi` = ptr, `rdx` = len. `fd == 1` is the stdout
        // convention (every demo payload's `Writer` uses this) and goes
        // straight to serial, handled here rather than in task::thread
        // since it doesn't touch any per-thread state; anything else is
        // a real `sys_open(..., mode: 1)` file, handled there. Neither
        // path validates that `[ptr, ptr+len)` is actually mapped: a
        // caller that lies here takes down the whole kernel with a page
        // fault, not just itself — the same known, documented gap every
        // other direct-user-pointer syscall here already has, not
        // something newly introduced by this one.
        SYS_WRITE => {
            let len = g.rdx.min(SYS_WRITE_MAX_LEN) as usize;
            if g.rdi == 1 {
                let bytes = unsafe { core::slice::from_raw_parts(g.rsi as *const u8, len) };
                for &b in bytes {
                    if let Some(c) = char::from_u32(b as u32) {
                        crate::serial_print!("{c}");
                    }
                }
                g.rax = len as u64;
            } else {
                g.rax = crate::task::thread::sys_write_fd(g.rdi, g.rsi, g.rdx);
            }
            gprs_addr
        }
        // `rdi`/`rsi` = ptr/len of a path on the same FAT filesystem
        // `SYS_OPEN` reads from, `rdx`/`r8` = ptr/len of an optional
        // argument string (0/0 for none) the new process can read back
        // via `SYS_GETARG` — this is what lets a real ring-3 program
        // (not just `main.rs`'s own boot-time calls) create another
        // process: the first time this kernel's *userland*, not just the
        // kernel itself, decides what runs next.
        SYS_SPAWN => {
            g.rax = crate::task::thread::sys_spawn(g.rdi, g.rsi, g.rdx, g.r8);
            gprs_addr
        }
        // `rdi` = the `ThreadId` `SYS_SPAWN` returned, `rsi` = an
        // optional `*mut u64` for the exit status (0 to skip). Non-blocking
        // poll (`1` done, `0` still running) — see `sys_wait`'s own doc
        // comment for why this kernel can't offer a real blocking wait.
        SYS_WAIT => {
            g.rax = crate::task::thread::sys_wait(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi`/`rsi` = buf ptr/len — copies this thread's own spawn-time
        // argument string in, returning how many bytes actually landed.
        SYS_GETARG => {
            g.rax = crate::task::thread::sys_getarg(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi` = IPv4 address (big-endian-packed u32, e.g. `1.1.1.1` =
        // `(1<<24)|(1<<16)|(1<<8)|1`), `rsi` = port. Returns a real fd
        // immediately (connection may still be handshaking) — see
        // `sys_connect`'s own doc comment.
        SYS_CONNECT => {
            g.rax = crate::task::thread::sys_connect(g.rdi as u32, g.rsi);
            gprs_addr
        }
        // `rdi` = the fd `SYS_CONNECT` returned. `1` established, `0`
        // still connecting, `2` closed/failed/not-a-socket-fd.
        SYS_CONNECT_STATUS => {
            g.rax = crate::task::thread::sys_connect_status(g.rdi);
            gprs_addr
        }
        // `rdi`/`rsi` = hostname ptr/len — starts an A-record DNS query,
        // returning a fd immediately (not once resolved).
        SYS_RESOLVE => {
            g.rax = crate::task::thread::sys_resolve(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi` = the fd `SYS_RESOLVE` returned. Terminal result consumes
        // the fd — see `sys_resolve_status`'s own doc comment for the
        // WOULD_BLOCK/FD_ERROR/resolved-address return convention.
        SYS_RESOLVE_STATUS => {
            g.rax = crate::task::thread::sys_resolve_status(g.rdi);
            gprs_addr
        }
        // `rdi` = the `ThreadId` to terminate (a `SYS_SPAWN` return
        // value) — must not be the caller's own. `0` success, `FD_ERROR`
        // (bad/self/already-gone pid).
        SYS_KILL => {
            g.rax = crate::task::thread::sys_kill(g.rdi);
            gprs_addr
        }
        // No args. Returns milliseconds since boot (PIT-tick-counted —
        // see `task::time`), for userland timing/`sleep_ms`.
        SYS_UPTIME_MS => {
            g.rax = crate::task::time::uptime_ms();
            gprs_addr
        }
        // `rdi` = ptr to a caller-owned buffer of 4 consecutive `u64`s
        // (32 bytes) in the caller's own address space — safe to write
        // directly for the same reason `SYS_GETARG` already relies on:
        // the syscall handler runs with the caller's `CR3` still active.
        // Layout, in order: heap bytes used, heap bytes free, total
        // frames ever handed out, frames currently free for reuse.
        // Always returns 0 — no failure mode, same as every other
        // syscall here that trusts a raw user pointer.
        SYS_MEMINFO => {
            let (heap_used, heap_free) = crate::allocator::heap_stats();
            let (frames_total, frames_free) = crate::memory::frame_stats();
            let out = g.rdi as *mut u64;
            unsafe {
                out.write(heap_used);
                out.add(1).write(heap_free);
                out.add(2).write(frames_total);
                out.add(3).write(frames_free);
            }
            g.rax = 0;
            gprs_addr
        }
        // `rdi` = ptr to a caller-owned `[u64; 2]` buffer. Writes
        // `[read_fd, write_fd]` on success and returns `0`; this syscall
        // has no failure mode (see `sys_pipe`'s own doc comment).
        SYS_PIPE => {
            g.rax = crate::task::thread::sys_pipe(g.rdi);
            gprs_addr
        }
        // No args. Returns the next buffered keypress as an ASCII byte in
        // `rax`, or `u64::MAX` if none is currently buffered — see
        // `keyboard::pop_key`'s own doc comment for why this can never
        // block waiting for a real keypress.
        SYS_READ_KEY => {
            g.rax = match keyboard::pop_key() {
                Some(byte) => byte as u64,
                None => u64::MAX,
            };
            gprs_addr
        }
        // `rdi` = ptr to a caller-owned buffer of 6 consecutive `u32`s (24
        // bytes): `[year, month, day, hour, minute, second]`. Always
        // succeeds (`rax` = 0) — same direct-user-pointer trust as
        // `SYS_MEMINFO`/`SYS_GETARG`.
        SYS_RTC_NOW => {
            let t = rtc::now();
            let out = g.rdi as *mut u32;
            unsafe {
                out.write(t.year);
                out.add(1).write(t.month as u32);
                out.add(2).write(t.day as u32);
                out.add(3).write(t.hour as u32);
                out.add(4).write(t.minute as u32);
                out.add(5).write(t.second as u32);
            }
            g.rax = 0;
            gprs_addr
        }
        // No args. Returns this thread's own `ThreadId` — always succeeds.
        SYS_GETPID => {
            g.rax = crate::task::thread::sys_getpid();
            gprs_addr
        }
        // No args. Forces an immediate scheduler switch instead of
        // waiting out the rest of this thread's timeslice — a no-op if
        // nothing else is ready. Returns the `gpr_rsp` to resume from,
        // same convention as `SYS_EXIT`/`SYS_WAIT`: not necessarily this
        // thread's own frame if a switch actually happened.
        SYS_YIELD => crate::task::thread::sys_yield(gprs_addr),
        // No args. Nanoseconds since boot-time TSC calibration
        // (`tsc::init`, called from `main.rs` right after the PIT/thread
        // scheduler come up) — always succeeds.
        SYS_NOW_NS => {
            g.rax = crate::tsc::now_ns();
            gprs_addr
        }
        // `rdi` = fd, `rsi` = offset (as i64, reinterpreted from the u64
        // register), `rdx` = whence (0 start, 1 current, 2 end). Returns
        // the new absolute position, or `FD_ERROR` for a non-seekable fd.
        SYS_LSEEK => {
            g.rax = crate::task::thread::sys_lseek(g.rdi, g.rsi as i64, g.rdx);
            gprs_addr
        }
        // `rdi` = fd to duplicate. Returns the new fd, or `u64::MAX`.
        SYS_DUP => {
            g.rax = crate::task::thread::sys_dup(g.rdi);
            gprs_addr
        }
        // `rdi` = fd to duplicate, `rsi` = the destination fd number.
        SYS_DUP2 => {
            g.rax = crate::task::thread::sys_dup2(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi` = ptr to a caller-owned buffer of 7 consecutive `u32`s (28
        // bytes): `[year, month, day, hour, minute, second, nanos]`.
        // Always succeeds — see `sys_clock_gettime`'s own doc comment for
        // the RTC+TSC fusion and its precision caveat.
        SYS_CLOCK_GETTIME => {
            g.rax = crate::task::thread::sys_clock_gettime(g.rdi);
            gprs_addr
        }
        // `rdi`/`rsi` = path ptr/len. `0` success, `u64::MAX` failure.
        SYS_MKDIR => {
            g.rax = match str_arg(g.rdi, g.rsi) {
                Some(path) => match crate::fs::mkdir(path) {
                    Ok(()) => 0,
                    Err(_) => u64::MAX,
                },
                None => u64::MAX,
            };
            gprs_addr
        }
        SYS_UNLINK => {
            g.rax = match str_arg(g.rdi, g.rsi) {
                Some(path) => match crate::fs::unlink(path) {
                    Ok(()) => 0,
                    Err(_) => u64::MAX,
                },
                None => u64::MAX,
            };
            gprs_addr
        }
        // `rdi`/`rsi` = old path ptr/len, `rdx`/`r8` = new path ptr/len.
        SYS_RENAME => {
            g.rax = match (str_arg(g.rdi, g.rsi), str_arg(g.rdx, g.r8)) {
                (Some(old_path), Some(new_path)) => match crate::fs::rename(old_path, new_path) {
                    Ok(()) => 0,
                    Err(_) => u64::MAX,
                },
                _ => u64::MAX,
            };
            gprs_addr
        }
        // `rdi`/`rsi` = path ptr/len, `rdx` = ptr to a caller-owned
        // buffer of 3 consecutive `u64`s: `[exists, is_dir, size]`.
        // Always returns 0 — check `buf[0]`, not `rax`, for whether the
        // path existed (matches `myos_userlib::stat`'s own contract).
        SYS_STAT => {
            let stat = match str_arg(g.rdi, g.rsi) {
                Some(path) => crate::fs::stat(path),
                None => crate::fs::Stat { exists: false, is_dir: false, size: 0 },
            };
            let out = g.rdx as *mut u64;
            unsafe {
                out.write(stat.exists as u64);
                out.add(1).write(stat.is_dir as u64);
                out.add(2).write(stat.size);
            }
            g.rax = 0;
            gprs_addr
        }
        // `rdi` = size in bytes, `rsi` = prot flags (bit0 writable, bit1
        // executable). Returns the mapped address, or `FD_ERROR`.
        SYS_MMAP_ANON => {
            g.rax = crate::task::thread::sys_mmap_anon(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi` = address, `rsi` = size in bytes — must exactly match the
        // most recent `SYS_MMAP_ANON` call (LIFO-only arena).
        SYS_MUNMAP => {
            g.rax = crate::task::thread::sys_munmap(g.rdi, g.rsi);
            gprs_addr
        }
        // No args. This kernel is single-user — see `sys_getuid`'s own
        // doc comment — so all four of these honestly return `0`.
        SYS_GETUID | SYS_GETGID | SYS_GETEUID | SYS_GETEGID => {
            g.rax = crate::task::thread::sys_getuid();
            gprs_addr
        }
        // No args. Returns the spawning thread's id, or `u64::MAX` for a
        // thread with no parent (a non-isolated kernel-thread demo).
        SYS_GETPPID => {
            g.rax = crate::task::thread::sys_getppid();
            gprs_addr
        }
        // `rdi`/`rsi` = buf ptr/len. Returns bytes written.
        SYS_GETCWD => {
            g.rax = crate::task::thread::sys_getcwd(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi`/`rsi` = path ptr/len. `0` success (target must actually
        // be a directory), `u64::MAX` failure.
        SYS_CHDIR => {
            g.rax = crate::task::thread::sys_chdir(g.rdi, g.rsi);
            gprs_addr
        }
        // `rdi` = new mask. Returns the previous mask.
        SYS_UMASK => {
            g.rax = crate::task::thread::sys_umask(g.rdi);
            gprs_addr
        }
        // `rdi` = fd. `1` iff `fd == 1` (the stdout convention).
        SYS_ISATTY => {
            g.rax = crate::task::thread::sys_isatty(g.rdi);
            gprs_addr
        }
        // No args. Returns a fresh unix-socket fd — always succeeds.
        SYS_USOCK_CREATE => {
            g.rax = crate::task::thread::sys_usock_create();
            gprs_addr
        }
        // `rdi` = fd, `rsi`/`rdx` = name ptr/len, `r8` = backlog.
        SYS_USOCK_BIND_LISTEN => {
            g.rax = crate::task::thread::sys_usock_bind_listen(g.rdi, g.rsi, g.rdx, g.r8);
            gprs_addr
        }
        // `rdi` = listening fd. Returns a new fd for the accepted
        // connection, `WOULD_BLOCK`, or `FD_ERROR`.
        SYS_USOCK_ACCEPT => {
            g.rax = crate::task::thread::sys_usock_accept(g.rdi);
            gprs_addr
        }
        // `rdi` = fd, `rsi`/`rdx` = name ptr/len.
        SYS_USOCK_CONNECT => {
            g.rax = crate::task::thread::sys_usock_connect(g.rdi, g.rsi, g.rdx);
            gprs_addr
        }
        SYS_USOCK_READ => {
            g.rax = crate::task::thread::sys_usock_read(g.rdi, g.rsi, g.rdx);
            gprs_addr
        }
        SYS_USOCK_WRITE => {
            g.rax = crate::task::thread::sys_usock_write(g.rdi, g.rsi, g.rdx);
            gprs_addr
        }
        other => {
            serial_println!("syscall: unknown number {other}");
            gprs_addr
        }
    }
}

extern "C" fn timer_interrupt_handler(_frame: *const RawInterruptFrame, gpr_rsp: u64) -> u64 {
    time::on_tick();
    unsafe {
        PICS.lock().notify_end_of_interrupt(PIC_1_OFFSET);
    }
    crate::task::thread::on_timer_tick(gpr_rsp)
}

extern "C" fn nic_interrupt_handler(_frame: *const RawInterruptFrame) {
    net::rtl8139::handle_interrupt();
    unsafe {
        // All of CANDIDATE_NIC_IRQS live on the secondary PIC (vectors
        // 41-44, i.e. IRQ 9-12), which needs EOI sent to *both* PICs —
        // the secondary to clear its own in-service bit, the primary
        // because that's how it's cascaded in. `notify_end_of_interrupt`
        // decides which PICs to notify purely from which one's range the
        // passed vector falls in, not the exact original vector, so any
        // vector in that range (we don't track which of the 4 actually
        // fired) EOIs correctly. Using PIC_1_OFFSET here — as opposed to
        // this NIC-range vector — would only EOI the primary PIC, and
        // the secondary's stuck in-service bit would silently block this
        // exact IRQ line from ever firing again after the first packet.
        PICS.lock()
            .notify_end_of_interrupt(PIC_1_OFFSET + CANDIDATE_NIC_IRQS[0]);
    }
}

/// Reads `len` bytes at `ptr` (user memory, valid to dereference directly
/// here for the same reason `task::thread::sys_open`'s own path-reading
/// does — the syscall handler runs with the caller's `CR3` still active)
/// as UTF-8. Shared by the filesystem syscalls below that take a path
/// argument directly in `interrupts.rs` rather than through `task::thread`.
fn str_arg<'a>(ptr: u64, len: u64) -> Option<&'a str> {
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    core::str::from_utf8(bytes).ok()
}

extern "C" fn keyboard_interrupt_handler(_frame: *const RawInterruptFrame) {
    keyboard::on_irq();
    unsafe {
        PICS.lock().notify_end_of_interrupt(PIC_1_OFFSET + 1);
    }
}

pub fn log_pci_bus() {
    for dev in pci::scan() {
        serial_println!(
            "pci: {:02x}:{:02x}.{} vendor={:04x} device={:04x} class={:02x}.{:02x} irq_line={}",
            dev.bus,
            dev.slot,
            dev.function,
            dev.vendor_id,
            dev.device_id,
            dev.class,
            dev.subclass,
            dev.interrupt_line
        );
    }
}
