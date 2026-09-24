//! Preemptive kernel threads, layered on top of the existing PIT-driven
//! interrupt trampoline rather than a second, parallel save/restore path.
//!
//! `interrupts.rs`'s timer trampoline already saves every GPR plus full
//! FPU/SSE state onto the interrupted stack before calling into Rust, and
//! restores + `iretq`s afterward. A context switch is just *pointing that
//! restore at a different, previously-saved frame* — see
//! `on_timer_tick` and the timer trampoline in `interrupts.rs` for the two
//! halves of that mechanism.
//!
//! This is additive, not a replacement for the async `Executor`/`Task`
//! model in `task/executor.rs`: the original boot context keeps running
//! that executor exactly as before (it's just "thread 0" here), and new
//! kernel threads are a separate layer future blocking subsystems (disk
//! I/O, etc.) can use without stalling the whole machine.

use crate::interrupts::RawInterruptFrame;
use crate::{gdt, memory};
use alloc::alloc::{alloc_zeroed, Layout};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

const STACK_SIZE: usize = 64 * 1024;
const GPR_COUNT: usize = 15;
const GPR_AREA_SIZE: usize = GPR_COUNT * 8;
const FRAME_SIZE: usize = core::mem::size_of::<RawInterruptFrame>();
const FXSAVE_SIZE: usize = 512;
/// Total size of one saved-context block: `[fxsave 512B][GPRs 120B][frame
/// 40B]`, contiguous and 16-byte aligned — the same layout `spawn`'s
/// kernel-stack frame already uses, reused as a small standalone buffer
/// for ring-3 threads (see `spawn_user`'s doc comment for why).
const FRAME_TOTAL_SIZE: usize = FXSAVE_SIZE + GPR_AREA_SIZE + FRAME_SIZE;

/// Where a ring-3 thread's code/stack land. Only ever one of each right
/// now (this milestone proves ring-3 execution works at all, not a real
/// process model — see the Milestone 2 plan), so fixed addresses are
/// fine; a real loader would pick these per-process.
const USER_CODE_ADDR: u64 = 0x5555_5555_0000;
const USER_STACK_ADDR: u64 = 0x5555_5556_0000;
const USER_STACK_SIZE: u64 = 4096;
/// Deliberately *different* addresses than the ones above, not just a
/// styling choice: `new_address_space` clones the shared table's PML4
/// *entries*, which point at shared, unduplicated lower-level tables.
/// Overriding an address the shared table already maps (like
/// `USER_CODE_ADDR`) would mean fixing up those shared tables in place —
/// a real copy-on-write scheme, out of scope here. Using an address the
/// shared table has *never* mapped means `map_page_in` walks into
/// "not present" PML4/PDPT/PD entries and allocates entirely fresh,
/// private sub-tables for it instead — automatically isolated, no
/// shared structure ever touched, which is the whole reason this
/// milestone's isolated demo uses these instead of the constants above.
const ISOLATED_USER_CODE_ADDR: u64 = 0x6666_6666_0000;
const ISOLATED_USER_STACK_ADDR: u64 = 0x6666_6667_0000;
/// Generous upper bound on the demo user payload's compiled size (see
/// `spawn_user`) — copied bytes past the payload's actual end are never
/// executed (it's an infinite loop) and still land on readable kernel
/// `.text`, so over-copying is harmless, just wasteful if set too high.
pub const USER_PAYLOAD_COPY_LEN: usize = 128;

/// First fd `sys_open` hands out — `0`/`1`/`2` are reserved (stdin
/// unused so far, `1` is the stdout convention `interrupts.rs`'s
/// `SYS_WRITE` handler special-cases straight to serial, `2` reserved
/// the same way for a future stderr) so a real file's fd can never
/// collide with one of those. It did once: the first version of this
/// started real fds at 0, and a program whose *second* `open()` call
/// happened to land on fd 1 had its writes silently redirected to
/// stdout instead of the file, with `sys_close` then persisting an
/// empty file where real content should have been.
const FIRST_REAL_FD: u32 = 3;

/// Where `sbrk` starts growing a process's heap from — comfortably clear
/// of the ELF loader's own load addresses (`elf.rs`'s test program links
/// at `0x1000000`) and the isolated user code/stack constants above.
const HEAP_BASE: u64 = 0x2000_0000_0000;
const PAGE_SIZE: u64 = 4096;

/// How many 10ms PIT ticks make up one thread's time slice.
const TICKS_PER_SLICE: u32 = 5; // 50ms

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ThreadId(u64);

impl ThreadId {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1); // 0 is reserved for the boot thread
        ThreadId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

struct Thread {
    /// Where this thread's saved 15-GPR area starts — exactly the `rsp`
    /// the timer trampoline should resume from to put this thread back
    /// on the CPU. Only meaningful while not running. For a ring-0
    /// thread this points into its own private kernel stack (`_stack`,
    /// below); for a ring-3 thread it points into `frame_buf` instead —
    /// see `on_timer_tick`'s save step for why those can't be treated
    /// the same way.
    saved_gpr_rsp: u64,
    /// Kept alive for as long as the thread exists; never read again
    /// after `spawn` other than to keep the allocation from being freed
    /// (there's no thread-exit path yet, matching the existing
    /// `Task`/`Executor` model, which also never reclaims finished work).
    _stack: Option<Box<[u8]>>,
    /// Ring-3 threads only: a private `FRAME_TOTAL_SIZE`-byte staging
    /// buffer for this thread's saved context. Needed because an
    /// interrupt landing while CPL=3 makes the CPU *automatically*
    /// switch to the TSS's RSP0 stack before pushing anything — a stack
    /// shared by every ring-3-interrupting event, not private per
    /// thread, so a saved frame left sitting there would be clobbered by
    /// the next interrupt (even one for a different thread) before this
    /// one gets resumed. `on_timer_tick` copies the frame out of there
    /// and into this buffer on every switch-out instead of just
    /// remembering a pointer, the way ring-0 threads' own private stacks
    /// safely allow.
    frame_buf: Option<Box<[u8]>>,
    /// `Some` only for threads with their own private address space
    /// (`spawn_isolated_user`) — `on_timer_tick` `CR3`-switches to this
    /// when resuming the thread. `None` (every kernel thread, and
    /// Milestone 2's original non-isolated `spawn_user` demo) means
    /// "keep whatever's currently loaded", the common case.
    cr3: Option<PhysFrame<Size4KiB>>,
    /// Next unallocated address in this thread's `sbrk` heap — only
    /// meaningful (and only ever grown) for isolated threads (`cr3` is
    /// `Some`); see `sbrk`'s own doc comment for why non-isolated ones
    /// aren't supported.
    heap_next: u64,
    /// Same idea as `heap_next`, for the independent `mmap`/`munmap`
    /// arena (`crate::mmap`) — a separate cursor so the two never
    /// collide, initialized to `crate::mmap::MMAP_BASE` for isolated
    /// threads and unused (`0`) otherwise, same convention as `heap_next`.
    mmap_next: u64,
    /// Whichever thread was `sched.current` at the moment this one was
    /// spawned (`SYS_GETPPID`) — real for every isolated thread, including
    /// a boot-spawned `spawn_userland_demos` process (parent is thread 0,
    /// the boot thread, not a special-cased `None`). `None` only for the
    /// three non-isolated kernel-thread demo paths, which never go
    /// through `finish_spawn_isolated` at all.
    parent: Option<ThreadId>,
    /// This thread's working directory (`SYS_GETCWD`/`SYS_CHDIR`) — a
    /// real, if minimal, feature: `chdir` validates the target is an
    /// actual directory via `fs::stat` before accepting it, this isn't a
    /// no-op stub. Every thread starts at `"/"`.
    cwd: alloc::string::String,
    /// `SYS_UMASK`'s stored value — accepted and returned faithfully, but
    /// nothing in `fs.rs` actually enforces file permissions yet (this
    /// FAT32 filesystem has no permission bits at all), so this is
    /// honestly inert bookkeeping, not real access control.
    umask: u64,
    /// This thread's open files (`sys_open`/`sys_read`/`sys_close`),
    /// keyed by file descriptor. Whole-file, not streamed — `fs::read`
    /// itself only ever reads a complete file at once (see its own doc
    /// comment), so `sys_open` just calls that once and `sys_read`
    /// serves slices of the result; there's no per-sector re-reading.
    open_files: BTreeMap<u32, OpenFile>,
    next_fd: u32,
    /// Every data frame (ELF segment pages, the isolated stack page,
    /// `sbrk` heap growth) allocated specifically for this thread's own
    /// address space — empty for non-isolated threads. Reclaimed via
    /// `memory::free_frames` in `exit_current`; see that call site and
    /// `memory::free_address_space`'s doc comment for why table-structure
    /// frames and these need separate tracking/teardown paths.
    owned_frames: alloc::vec::Vec<PhysFrame<Size4KiB>>,
    /// The single argument string `sys_spawn` was called with (empty for
    /// every thread not started that way — kernel threads,
    /// `spawn_user`/`spawn_isolated_user`, and boot's own
    /// `spawn_userland_demos` calls, which all pass an empty arg to
    /// `spawn_elf`). Read back via `SYS_GETARG` (`sys_getarg`) — there's
    /// no argv array, just one string, since nothing yet needs more than
    /// that.
    arg: alloc::vec::Vec<u8>,
}

enum OpenFile {
    /// `sys_open` mode 0 — `data` is the whole file, read at open time
    /// (see the module doc comment on why whole-file, not streamed).
    Read { data: alloc::vec::Vec<u8>, pos: usize },
    /// `sys_open` mode 1 — bytes accumulate here as `sys_write_fd` is
    /// called; nothing touches disk until `sys_close`, which does the
    /// actual `fs::write(path, &buffer)`. `path` is owned (not just the
    /// fd) because the user's own copy of the path bytes has no
    /// guaranteed lifetime past the `sys_open` call that read them.
    Write { path: alloc::string::String, buffer: alloc::vec::Vec<u8> },
    /// `sys_connect` — a live TCP connection into `net::stack::STACK`'s
    /// shared `SocketSet`, addressed through the same `sys_read`/
    /// `sys_write_fd`/`sys_close` calls a file `fd` already uses. Reuses
    /// the *synchronous* half of the network stack (`stack::with_stack`/
    /// `stack::poll`) — the same calls `net::tcp_stream::TcpStream`
    /// wraps as `.await`-able for the boot-time HTTP fetch — rather than
    /// anything `async`: a syscall handler is a plain function with
    /// nowhere to `.await` from. Progress still happens: `net_poll_loop`
    /// (an async task on thread 0) keeps calling `stack::poll()` on its
    /// own schedule regardless of what any *other* thread's syscalls are
    /// doing, since `task::thread`'s preemptive scheduler keeps giving
    /// thread 0 its own turns independently.
    Socket { handle: smoltcp::iface::SocketHandle },
    /// `sys_resolve` — a pending DNS query on `net::stack::STACK`'s
    /// shared `dns_handle` socket. One-shot: `sys_resolve_status`
    /// consumes (removes) this entry the moment the query reaches a
    /// terminal state (success or failure), matching smoltcp's own
    /// `get_query_result` — it auto-frees the query slot on completion
    /// and panics if called again on an already-freed one, so this fd
    /// must not survive past its first non-pending poll either.
    Resolve { handle: smoltcp::socket::dns::QueryHandle },
    /// `sys_pipe` — one end of an in-kernel pipe (`crate::pipe`). A pipe's
    /// two fds are otherwise ordinary fds to `sys_read`/`sys_write_fd`/
    /// `sys_close`; which end a given fd is just picks which of
    /// `crate::pipe`'s read/write functions gets called and which half of
    /// the refcount gets dropped on teardown.
    PipeRead { id: crate::pipe::PipeId },
    PipeWrite { id: crate::pipe::PipeId },
    /// `sys_usock_create`/`sys_usock_accept` — an in-kernel Unix domain
    /// socket (`crate::unixsocket`), routed through the regular fd table
    /// (unlike a raw `UnixSocketId`, which has no per-thread ownership or
    /// teardown on its own) so `SYS_CLOSE` and thread-exit's
    /// `finalize_open_files` reclaim it the same way every other fd kind
    /// already does — without this, a thread that exits (or crashes)
    /// without an explicit close would leak the socket, and worse, a
    /// listener's bound name would stay claimed forever, permanently
    /// blocking anyone else from ever binding it again.
    UnixSocket { id: crate::unixsocket::UnixSocketId },
}

struct Scheduler {
    threads: BTreeMap<ThreadId, Thread>,
    ready: VecDeque<ThreadId>,
    current: ThreadId,
    ticks_since_switch: u32,
    /// `ThreadId`s `exit_current` has removed from `threads` but that
    /// nothing has `sys_wait`-ed for yet, mapped to the status code
    /// `SYS_EXIT` was called with — a minimal "zombie" table, just
    /// enough for `SYS_WAIT` to know a given child is done and hand back
    /// what it exited with. Consumed (removed) the first time something
    /// waits successfully; never consumed at all (e.g. a `spawn` whose
    /// caller never `wait`s) leaks one entry per process forever — the
    /// same "nothing here frees everything yet" tradeoff
    /// `BootInfoFrameAllocator` and `sbrk`'s own bump allocator already
    /// make, not something new.
    exited: alloc::collections::BTreeMap<ThreadId, u64>,
}

static SCHEDULER: Mutex<Option<Scheduler>> = Mutex::new(None);

/// Must run once, from the boot thread, before any `spawn` — registers
/// the calling context as thread 0 so the scheduler has an entry to save
/// its state into the first time it's switched away from.
pub fn init() {
    without_interrupts(|| {
        let mut threads = BTreeMap::new();
        threads.insert(ThreadId(0), Thread { saved_gpr_rsp: 0, _stack: None, frame_buf: None, cr3: None, heap_next: 0, mmap_next: 0, parent: None, cwd: alloc::string::String::from("/"), umask: 0o022, open_files: BTreeMap::new(), next_fd: FIRST_REAL_FD, owned_frames: alloc::vec::Vec::new(), arg: alloc::vec::Vec::new() });
        *SCHEDULER.lock() = Some(Scheduler {
            threads,
            ready: VecDeque::new(),
            current: ThreadId(0),
            ticks_since_switch: 0,
            exited: alloc::collections::BTreeMap::new(),
        });
    });
}

/// Spawns a new kernel thread running `entry`, which must never return —
/// there's no thread-exit path yet (matching `Task`, which also never
/// completes in this kernel's usage).
pub fn spawn(entry: extern "C" fn() -> !) -> ThreadId {
    let id = ThreadId::new();

    // Stack layout, high address to low:
    //   [ RawInterruptFrame (40B) ][ 15 GPRs (120B) ][ fxsave area (512B) ]
    // `gpr_rsp` (the boundary between the GPR area and the fxsave area)
    // is kept 16-byte aligned so the timer trampoline can always find the
    // matching fxsave area at `align_down(gpr_rsp, 16) - 512`, whether
    // it's resuming a freshly spawned thread (this function) or one
    // that's been switched out before (interrupts.rs's own fxsave).
    let total_reserved = FXSAVE_SIZE + GPR_AREA_SIZE + FRAME_SIZE;
    assert!(STACK_SIZE > total_reserved, "thread stack too small");

    let layout = Layout::from_size_align(STACK_SIZE, 16).unwrap();
    let stack: Box<[u8]> = unsafe {
        let ptr = alloc_zeroed(layout);
        assert!(!ptr.is_null(), "out of memory allocating a kernel thread stack");
        Box::from_raw(core::slice::from_raw_parts_mut(ptr, STACK_SIZE))
    };

    let stack_top = stack.as_ptr() as u64 + STACK_SIZE as u64;
    let frame_addr = stack_top - FRAME_SIZE as u64;
    let gpr_rsp = frame_addr - GPR_AREA_SIZE as u64;
    debug_assert_eq!(gpr_rsp % 16, 0, "gpr_rsp must stay 16-byte aligned — see fxsave note above");

    unsafe {
        (frame_addr as *mut RawInterruptFrame).write(RawInterruptFrame {
            instruction_pointer: entry as usize as u64,
            code_segment: gdt::kernel_code_selector().0 as u64,
            cpu_flags: 0x200, // IF set, nothing else — interrupts stay enabled once this thread runs
            // -8: mimics the stack alignment an ordinary `call` would
            // leave (rsp % 16 == 8 at function entry), in case `entry`'s
            // prologue ever assumes it for spilled SSE locals.
            stack_pointer: stack_top - 8,
            stack_segment: gdt::kernel_data_selector().0 as u64,
        });
        // The GPR area and the fxsave area were already zeroed by
        // `alloc_zeroed` — an all-zero image is a valid FPU/SSE reset
        // state, and the GPRs themselves don't matter (nothing has run
        // yet to have put meaningful values in them).
    }

    without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("task::thread::init() must run before spawn()");
        sched.threads.insert(id, Thread { saved_gpr_rsp: gpr_rsp, _stack: Some(stack), frame_buf: None, cr3: None, heap_next: 0, mmap_next: 0, parent: None, cwd: alloc::string::String::from("/"), umask: 0o022, open_files: BTreeMap::new(), next_fd: FIRST_REAL_FD, owned_frames: alloc::vec::Vec::new(), arg: alloc::vec::Vec::new() });
        sched.ready.push_back(id);
    });

    id
}

/// Spawns a ring-3 ("user mode") thread running the raw machine code at
/// `payload[..payload.len()]` (see `interrupts::SYSCALL_VECTOR` for the
/// only way back into the kernel it has) — this milestone's proof that
/// privilege separation is real, not just that some code runs elsewhere.
/// `payload` must be position-independent (only short relative
/// jumps/no absolute addressing): it's copied verbatim into a freshly
/// mapped page at `USER_CODE_ADDR`, a different address than wherever
/// the caller's own copy of those bytes actually lives.
pub fn spawn_user(payload: &[u8]) -> Result<ThreadId, &'static str> {
    assert!(payload.len() <= USER_PAYLOAD_COPY_LEN, "user payload longer than the reserved copy length");

    memory::map_page(
        VirtAddr::new(USER_CODE_ADDR),
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    )?;
    memory::map_page(
        VirtAddr::new(USER_STACK_ADDR),
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    )?;
    unsafe {
        // Safe to dereference directly: this is CPL-0 code writing to a
        // page in the one shared address space everything here runs in
        // (no SMAP enabled) — no aliasing/remapping trick needed.
        core::ptr::copy_nonoverlapping(payload.as_ptr(), USER_CODE_ADDR as *mut u8, payload.len());
    }

    let id = ThreadId::new();

    let layout = Layout::from_size_align(FRAME_TOTAL_SIZE, 16).unwrap();
    let frame_buf: Box<[u8]> = unsafe {
        let ptr = alloc_zeroed(layout);
        assert!(!ptr.is_null(), "out of memory allocating a ring-3 thread's frame buffer");
        Box::from_raw(core::slice::from_raw_parts_mut(ptr, FRAME_TOTAL_SIZE))
    };

    // Same [fxsave | GPRs | frame] layout as spawn()'s kernel stack, just
    // packed into this small buffer instead — see FRAME_TOTAL_SIZE.
    let frame_addr = frame_buf.as_ptr() as u64 + (FXSAVE_SIZE + GPR_AREA_SIZE) as u64;
    let gpr_rsp = frame_buf.as_ptr() as u64 + FXSAVE_SIZE as u64;

    unsafe {
        (frame_addr as *mut RawInterruptFrame).write(RawInterruptFrame {
            instruction_pointer: USER_CODE_ADDR,
            code_segment: gdt::user_code_selector().0 as u64,
            cpu_flags: 0x200, // IF set
            stack_pointer: USER_STACK_ADDR + USER_STACK_SIZE - 8, // -8: see spawn()'s equivalent note
            stack_segment: gdt::user_data_selector().0 as u64,
        });
    }

    without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("task::thread::init() must run before spawn_user()");
        sched.threads.insert(id, Thread { saved_gpr_rsp: gpr_rsp, _stack: None, frame_buf: Some(frame_buf), cr3: None, heap_next: 0, mmap_next: 0, parent: None, cwd: alloc::string::String::from("/"), umask: 0o022, open_files: BTreeMap::new(), next_fd: FIRST_REAL_FD, owned_frames: alloc::vec::Vec::new(), arg: alloc::vec::Vec::new() });
        sched.ready.push_back(id);
    });

    Ok(id)
}

/// Spawns a ring-3 thread in its *own*, private address space (see
/// `memory::new_address_space`) instead of the one shared address space
/// `spawn_user` uses. Unlike `spawn_user`, two threads made this way can
/// use the exact same virtual addresses (`ISOLATED_USER_CODE_ADDR`/
/// `ISOLATED_USER_STACK_ADDR` — see their doc comment for why those are
/// different constants than `spawn_user`'s) for completely different,
/// non-aliasing physical memory — this is what actually proves
/// address-space isolation, not just the privilege separation
/// Milestone 2 already covered.
pub fn spawn_isolated_user(payload: &[u8]) -> Result<ThreadId, &'static str> {
    assert!(payload.len() <= USER_PAYLOAD_COPY_LEN, "user payload longer than the reserved copy length");

    let (cr3_frame, mut mapper) = memory::new_address_space()?;
    let code_frame = memory::map_page_in(
        &mut mapper,
        VirtAddr::new(ISOLATED_USER_CODE_ADDR),
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    )?;
    let stack_frame = memory::map_page_in(
        &mut mapper,
        VirtAddr::new(ISOLATED_USER_STACK_ADDR),
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    )?;
    unsafe {
        // Unlike `spawn_user`, `ISOLATED_USER_CODE_ADDR` itself isn't
        // dereferencable here — it only resolves through *this*
        // mapping once something switches `CR3` to `mapper`'s table,
        // which hasn't happened yet. Reach the same physical frame via
        // the physical-memory-offset alias instead, present in every
        // address space (it was cloned along with everything else in
        // `new_address_space`).
        let dest = memory::phys_to_virt(code_frame.start_address().as_u64()) as *mut u8;
        core::ptr::copy_nonoverlapping(payload.as_ptr(), dest, payload.len());
    }

    let owned_frames = alloc::vec![code_frame, stack_frame];
    Ok(finish_spawn_isolated(cr3_frame, ISOLATED_USER_CODE_ADDR, ISOLATED_USER_STACK_ADDR + USER_STACK_SIZE, owned_frames, alloc::vec::Vec::new()))
}

/// Loads a real, separately-compiled ELF64 executable (`elf::load` —
/// parses its program headers, maps each `PT_LOAD` segment, copies its
/// bytes in) into its own address space and spawns it in ring 3. Unlike
/// `spawn_isolated_user`'s hand-copied payload, the code here — and its
/// entry point — come entirely from the file; this function only ever
/// decides where the *stack* goes, since that's never part of an ELF.
/// `arg` becomes readable in the new process via `SYS_GETARG` — pass
/// `&[]` for none (every call site before `sys_spawn` existed does this).
pub fn spawn_elf(elf_bytes: &[u8], arg: &[u8]) -> Result<ThreadId, &'static str> {
    let (cr3_frame, mut mapper) = memory::new_address_space()?;
    let mut owned_frames = alloc::vec::Vec::new();
    let entry_point = crate::elf::load(&mut mapper, elf_bytes, &mut owned_frames)?;
    let stack_frame = memory::map_page_in(
        &mut mapper,
        VirtAddr::new(ISOLATED_USER_STACK_ADDR),
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    )?;
    owned_frames.push(stack_frame);

    Ok(finish_spawn_isolated(cr3_frame, entry_point, ISOLATED_USER_STACK_ADDR + USER_STACK_SIZE, owned_frames, alloc::vec::Vec::from(arg)))
}

/// Shared tail for `spawn_isolated_user` and `spawn_elf`: both end up
/// with a ring-3 thread that has its own address space (`cr3_frame`) and
/// its own private saved-context buffer (`Thread::frame_buf` — see its
/// doc comment for why isolated/ring-3 threads need one at all). The
/// only real difference between the two callers is *how* code got into
/// that address space and what `entry_point`/`stack_top` are.
fn finish_spawn_isolated(cr3_frame: PhysFrame<Size4KiB>, entry_point: u64, stack_top: u64, owned_frames: alloc::vec::Vec<PhysFrame<Size4KiB>>, arg: alloc::vec::Vec<u8>) -> ThreadId {
    let id = ThreadId::new();

    let layout = Layout::from_size_align(FRAME_TOTAL_SIZE, 16).unwrap();
    let frame_buf: Box<[u8]> = unsafe {
        let ptr = alloc_zeroed(layout);
        assert!(!ptr.is_null(), "out of memory allocating a ring-3 thread's frame buffer");
        Box::from_raw(core::slice::from_raw_parts_mut(ptr, FRAME_TOTAL_SIZE))
    };

    let frame_addr = frame_buf.as_ptr() as u64 + (FXSAVE_SIZE + GPR_AREA_SIZE) as u64;
    let gpr_rsp = frame_buf.as_ptr() as u64 + FXSAVE_SIZE as u64;

    unsafe {
        (frame_addr as *mut RawInterruptFrame).write(RawInterruptFrame {
            instruction_pointer: entry_point,
            code_segment: gdt::user_code_selector().0 as u64,
            cpu_flags: 0x200,
            stack_pointer: stack_top - 8, // -8: see spawn()'s equivalent note
            stack_segment: gdt::user_data_selector().0 as u64,
        });
    }

    without_interrupts(|| {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("task::thread::init() must run before spawning an isolated thread");
        let parent = Some(sched.current);
        sched
            .threads
            .insert(id, Thread { saved_gpr_rsp: gpr_rsp, _stack: None, frame_buf: Some(frame_buf), cr3: Some(cr3_frame), heap_next: HEAP_BASE, mmap_next: crate::mmap::MMAP_BASE, parent, cwd: alloc::string::String::from("/"), umask: 0o022, open_files: BTreeMap::new(), next_fd: FIRST_REAL_FD, owned_frames, arg });
        sched.ready.push_back(id);
    });

    id
}

/// The kernel side of `SYS_GETPID`: no args, no failure mode — every
/// thread is always registered under some `ThreadId` for the entire time
/// it can be running this syscall.
pub fn sys_getpid() -> u64 {
    let guard = SCHEDULER.lock();
    guard.as_ref().expect("scheduler not initialized").current.0
}

/// The kernel side of `SYS_YIELD`: forces `on_timer_tick`'s own
/// slice-expired switch path to run immediately instead of waiting for
/// `TICKS_PER_SLICE` more real PIT ticks, by presetting the counter it
/// checks. A no-op (returns the same frame) if nothing else is ready —
/// same as an expired slice finding an empty ready queue.
pub fn sys_yield(gpr_rsp: u64) -> u64 {
    {
        let mut guard = SCHEDULER.lock();
        if let Some(sched) = guard.as_mut() {
            sched.ticks_since_switch = TICKS_PER_SLICE;
        }
    }
    on_timer_tick(gpr_rsp)
}

/// Called from the timer trampoline on every PIT tick, with
/// `current_gpr_rsp` = where the currently-running thread's register
/// frame now lives on its own stack (valid only for this one call).
/// Returns the `gpr_rsp` the trampoline should actually resume from —
/// either the same value (no switch this tick) or a different thread's.
pub fn on_timer_tick(current_gpr_rsp: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let Some(sched) = guard.as_mut() else {
        return current_gpr_rsp; // scheduler not initialized yet
    };

    sched.ticks_since_switch += 1;
    if sched.ticks_since_switch < TICKS_PER_SLICE {
        return current_gpr_rsp;
    }
    sched.ticks_since_switch = 0;

    let Some(next_id) = sched.ready.pop_front() else {
        return current_gpr_rsp; // nothing else runnable
    };
    if next_id == sched.current {
        sched.ready.push_back(next_id);
        return current_gpr_rsp;
    }

    if let Some(outgoing) = sched.threads.get_mut(&sched.current) {
        if let Some(buf) = outgoing.frame_buf.as_mut() {
            // Ring-3 thread: `current_gpr_rsp` points into the *shared*
            // RSP0 stack (see `Thread::frame_buf`'s doc comment), which
            // the very next interrupt — for any thread — will overwrite.
            // Copy the frame out to safety instead of just remembering
            // where it is. `& !0xF` mirrors the trampoline's own
            // `and rsp, -16` to find the matching fxsave area below it.
            let aligned = current_gpr_rsp & !0xF;
            let src = (aligned - FXSAVE_SIZE as u64) as *const u8;
            unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), FRAME_TOTAL_SIZE) };
            outgoing.saved_gpr_rsp = buf.as_ptr() as u64 + FXSAVE_SIZE as u64;
        } else {
            outgoing.saved_gpr_rsp = current_gpr_rsp;
        }
        sched.ready.push_back(sched.current);
    }

    let next = sched.threads.get(&next_id).expect("ready thread must be registered");
    let next_rsp = next.saved_gpr_rsp;
    let next_cr3 = next.cr3;
    sched.current = next_id;
    switch_cr3(next_cr3);

    next_rsp
}

/// Called from `interrupts.rs`'s `SYS_EXIT` handler: the calling thread
/// is gone for good — dropped from the scheduler entirely, never
/// rejoining `ready` the way a normal switch-out would. Dropping its
/// `Thread` frees its kernel stack / frame buffer. For a
/// `spawn_isolated_user`/`spawn_elf` thread (`cr3: Some`), its address
/// space is fully torn down: page-table *structure* frames via
/// `memory::free_address_space`, and the data frames it privately mapped
/// (ELF segments, stack, `sbrk` growth — tracked in `Thread::owned_frames`
/// as they were allocated) via `memory::free_frames` — but only after
/// `switch_cr3` has already moved the hardware off of it (freeing frames
/// still backing the active `CR3` would be a use-after-free).
///
/// Never returns to its caller in the normal sense — like `on_timer_tick`,
/// its return value is the `gpr_rsp` `syscall_trampoline` resumes from,
/// which by construction can never again be this thread's.
///
/// `status` is whatever the exiting thread's own `SYS_EXIT` call passed —
/// an arbitrary `u64`, no fixed meaning here (this kernel doesn't
/// interpret it, just stores and hands it back via `sys_wait`), matching
/// the classic "exit code" convention closely enough for a program that
/// wants to report simple success/failure to whatever `wait`s on it.
pub fn exit_current(status: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");

    let exiting = sched.current;
    let exited_thread = sched.threads.remove(&exiting);
    sched.exited.insert(exiting, status);

    let Some(next_id) = sched.ready.pop_front() else {
        panic!("thread {exiting:?} exited and nothing else is runnable");
    };
    let next = sched.threads.get(&next_id).expect("ready thread must be registered");
    let next_rsp = next.saved_gpr_rsp;
    let next_cr3 = next.cr3;
    sched.current = next_id;
    switch_cr3(next_cr3);

    // Dropped only once the scheduler lock is released below and CR3 has
    // already moved on — `free_address_space`/`free_frames` take
    // FRAME_ALLOCATOR's own lock, and `finalize_open_files` takes
    // `net::stack::STACK`'s/`fs::DISK_LOCK`'s own locks, so holding
    // SCHEDULER across any of them here would be an unnecessary (if not
    // currently deadlocking) nesting.
    let exited_cr3 = exited_thread.as_ref().and_then(|t| t.cr3);
    let (exited_frames, exited_files) = match exited_thread {
        Some(t) => (t.owned_frames, t.open_files),
        None => (alloc::vec::Vec::new(), BTreeMap::new()),
    };
    drop(guard);
    // Order doesn't matter for correctness between these three — each
    // only ever touches state this exited thread alone owned — just
    // grouped by what it reclaims: open fds (sockets/DNS queries
    // released, pending writes flushed — see `finalize_open_files`'s own
    // doc comment for why this wasn't always done), then data frames,
    // then page-table structure.
    finalize_open_files(exited_files);
    memory::free_frames(&exited_frames);
    if let Some(frame) = exited_cr3 {
        memory::free_address_space(frame);
    }

    next_rsp
}

/// The kernel side of `SYS_KILL`: forcibly terminates another thread —
/// `pid` must not be the caller's own (`FD_ERROR`; use `SYS_EXIT` for
/// that) — tearing it down exactly like `exit_current` does for itself
/// (fds finalized, data frames and page-table structure freed), plus
/// removing it from `Scheduler::ready` if it was sitting there waiting
/// for a turn it'll now never get. Marks it `exited` with status
/// `u64::MAX` (an arbitrary "killed, not a real chosen exit code"
/// sentinel — nothing here enforces that convention on real exit codes
/// either, so it's a soft one) so anything already `sys_wait`-ing on it
/// unblocks instead of polling forever. Safe to free the victim's
/// address space immediately, no `switch_cr3` dance needed first — unlike
/// `exit_current`, the victim (being neither `sched.current` nor
/// currently executing) can never have its own `CR3` be the active one.
pub fn sys_kill(pid: u64) -> u64 {
    let target = ThreadId(pid);
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");

    if target == sched.current {
        return FD_ERROR;
    }
    let Some(victim) = sched.threads.remove(&target) else {
        return FD_ERROR;
    };
    sched.ready.retain(|&id| id != target);
    sched.exited.insert(target, u64::MAX);
    drop(guard);

    finalize_open_files(victim.open_files);
    memory::free_frames(&victim.owned_frames);
    if let Some(frame) = victim.cr3 {
        memory::free_address_space(frame);
    }
    0
}

/// Switches `CR3` to `target` — the thread being resumed's own address
/// space (`Some`), or `memory::boot_pml4_frame()`, the shared one, for a
/// non-isolated thread (`None`). Always resolving an explicit target
/// (not "leave CR3 alone" for `None`) matters: once *any* isolated
/// thread has run, `CR3` no longer defaults to the shared table on its
/// own — something has to point it back, every time, or a later
/// non-isolated thread (this includes `kernel_main` itself, still
/// running as thread 0 during its own synchronous setup) silently keeps
/// running with whatever *other* process's address space was last
/// active. That was a real, previously-shipped bug: two different real
/// ELF programs spawned back to back ended up cloning from an isolated
/// thread's address space instead of the shared one, because nothing
/// had switched `CR3` back after the scheduler first touched an
/// isolated thread.
fn switch_cr3(target: Option<PhysFrame<Size4KiB>>) {
    let target = target.unwrap_or_else(memory::boot_pml4_frame);
    let (current_frame, flags) = Cr3::read();
    if current_frame != target {
        unsafe { Cr3::write(target, flags) };
    }
}

/// The kernel side of `SYS_SBRK`: grows (or, for `increment <= 0`,
/// just queries) the calling thread's heap, mapping whatever new pages
/// that needs into *its own* address space, and returns the break's
/// value *before* this call (the classic `sbrk` convention — the caller
/// gets a pointer to the start of the newly available region, which is
/// `[old_break, old_break + increment)`).
///
/// Only isolated threads (`spawn_isolated_user`/`spawn_elf` — `cr3` is
/// `Some`) are supported: a non-isolated one (`spawn_user`, or a kernel
/// thread) would be growing the *shared* address space's heap region,
/// which every other non-isolated thread would silently collide with —
/// not something worth building real support for when every real
/// "process" going forward is isolated anyway. Called with a non-isolated
/// thread current, this just returns the break unchanged (0 bytes
/// growable), rather than either panicking or silently doing the unsafe
/// shared-growth thing.
pub fn sbrk(increment: i64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get(&id).expect("current thread must be registered");

    let Some(cr3) = thread.cr3 else {
        return thread.heap_next; // non-isolated thread — see doc comment
    };
    let old_break = thread.heap_next;
    let new_break = old_break.checked_add_signed(increment).unwrap_or(old_break);

    let mut new_frames = alloc::vec::Vec::new();
    if new_break > old_break {
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        let mut mapper = memory::mapper_for(cr3);
        let mut page = old_break & !(PAGE_SIZE - 1);
        while page < new_break {
            // Already-mapped pages (this call's range starts mid-page,
            // the common case after a prior sbrk) are expected here, not
            // an error — only map what's actually missing.
            if let Ok(frame) = memory::map_page_in(&mut mapper, VirtAddr::new(page), flags) {
                new_frames.push(frame);
            }
            page += PAGE_SIZE;
        }
    }

    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    thread.heap_next = new_break;
    thread.owned_frames.extend(new_frames);
    old_break
}

/// The kernel side of `SYS_MMAP_ANON`: same isolated-threads-only
/// restriction as `sbrk` (see its doc comment) — `FD_ERROR` for a
/// non-isolated thread, since there's no per-thread `mmap_next` cursor
/// worth having when the address space is shared. `prot` bit 0 =
/// writable, bit 1 = executable (`myos_userlib::PROT_WRITE`/`PROT_EXEC`).
pub fn sys_mmap_anon(bytes: u64, prot: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let Some(cr3) = thread.cr3 else {
        return FD_ERROR;
    };

    let mut mapper = memory::mapper_for(cr3);
    let writable = prot & 0b01 != 0;
    let executable = prot & 0b10 != 0;
    match crate::mmap::map_anon(&mut mapper, &mut thread.mmap_next, bytes as usize, writable, executable) {
        Ok((addr, frames)) => {
            thread.owned_frames.extend(frames);
            addr
        }
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_MUNMAP`: `addr`/`bytes` must exactly match the
/// most recent `SYS_MMAP_ANON` call (see `mmap::unmap_anon`'s doc
/// comment for the LIFO-only restriction). Removes the freed frames from
/// `owned_frames` *before* handing them to `memory::free_frames` — see
/// `mmap::unmap_anon`'s doc comment for why doing that in the other order
/// (or not at all) would double-free them when this process eventually
/// exits and `exit_current` frees whatever's left in `owned_frames`.
pub fn sys_munmap(addr: u64, bytes: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let Some(cr3) = thread.cr3 else {
        return FD_ERROR;
    };

    let mut mapper = memory::mapper_for(cr3);
    match crate::mmap::unmap_anon(&mut mapper, &mut thread.mmap_next, addr, bytes as usize) {
        Ok(freed) => {
            thread.owned_frames.retain(|f| !freed.contains(f));
            memory::free_frames(&freed);
            0
        }
        Err(_) => FD_ERROR,
    }
}

/// This kernel is single-user (no real accounts, no permission
/// enforcement anywhere in `fs.rs`) — `SYS_GETUID`/`SYS_GETGID`/
/// `SYS_GETEUID`/`SYS_GETEGID` all honestly return `0` ("root"), the same
/// simplification a lot of embedded/hobby kernels make rather than
/// building a real user/group model nothing here would enforce anyway.
pub fn sys_getuid() -> u64 {
    0
}

/// The kernel side of `SYS_GETPPID`: this thread's `parent` field, or
/// `u64::MAX` if it has none (a non-isolated kernel-thread demo — see
/// `Thread::parent`'s own doc comment).
pub fn sys_getppid() -> u64 {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("scheduler not initialized");
    let thread = sched.threads.get(&sched.current).expect("current thread must be registered");
    thread.parent.map(|p| p.0).unwrap_or(FD_ERROR)
}

/// The kernel side of `SYS_GETCWD`: copies this thread's `cwd` string
/// into `buf_ptr[..buf_len]`, returning how many bytes actually landed
/// (truncated, not erroring, if `buf_len` is too small — same permissive
/// posture `sys_getarg` already takes).
pub fn sys_getcwd(buf_ptr: u64, buf_len: u64) -> u64 {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("scheduler not initialized");
    let thread = sched.threads.get(&sched.current).expect("current thread must be registered");
    let bytes = thread.cwd.as_bytes();
    let n = bytes.len().min(buf_len as usize);
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf_ptr as *mut u8, n) };
    n as u64
}

/// The kernel side of `SYS_CHDIR`: a real check, not a no-op — the target
/// must actually exist and be a directory per `crate::fs::stat`, matching
/// what a real `chdir` refuses. `path` is stored as an owned `String`
/// (independent of the caller's own copy, which has no guaranteed
/// lifetime past this call — same reasoning `sys_open`'s `OpenFile::Write`
/// path already documents).
pub fn sys_chdir(path_ptr: u64, path_len: u64) -> u64 {
    let path_bytes = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return FD_ERROR;
    };
    let stat = crate::fs::stat(path);
    if !stat.exists || !stat.is_dir {
        return FD_ERROR;
    }

    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    thread.cwd = alloc::string::String::from(path);
    0
}

/// The kernel side of `SYS_UMASK`: stores `new_mask` and returns whatever
/// was stored before — the classic `umask` return convention. Accepted
/// and returned faithfully; see `Thread::umask`'s own doc comment for why
/// nothing actually enforces it yet.
pub fn sys_umask(new_mask: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let old = thread.umask;
    thread.umask = new_mask;
    old
}

/// The kernel side of `SYS_ISATTY`: `1` only for `fd == 1` (the `stdout`
/// convention every demo's `Writer` already targets — see
/// `interrupts.rs`'s `SYS_WRITE` handler), `0` for everything else. There
/// is no real terminal/tty layer here at all; this is an honest
/// reflection of the one fd this kernel treats specially, not a faked
/// general tty subsystem.
pub fn sys_isatty(fd: u64) -> u64 {
    (fd == 1) as u64
}

/// A sentinel, not a real fd — every syscall below uses `u64::MAX` for
/// "this failed" (bad path, bad utf8, unknown fd), the same loose
/// convention `spawn`/`sbrk`'s callers already tolerate. There's no
/// proper errno-style story yet (see the syscall handlers' own notes).
const FD_ERROR: u64 = u64::MAX;
/// `sys_read`/`sys_write_fd` on a `Socket` fd return this instead of `0`
/// when there's genuinely nothing to do *yet* — `0` stays reserved for
/// real EOF (peer closed its side)/a real send failure, which a caller
/// needs to tell apart from "try again once the network stack has made
/// more progress" (driven by `net_poll_loop`, not this call itself —
/// see `OpenFile::Socket`'s doc comment).
const WOULD_BLOCK: u64 = u64::MAX - 1;

/// The kernel side of `SYS_USOCK_CREATE`: allocates a fresh
/// `crate::unixsocket` id and immediately wraps it in a regular fd (see
/// `OpenFile::UnixSocket`'s doc comment for why — teardown on close/exit
/// needs it), so every other unix-socket syscall below takes a normal fd
/// number, not a raw `UnixSocketId`. Always succeeds.
pub fn sys_usock_create() -> u64 {
    let socket_id = crate::unixsocket::create();
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let fd = thread.next_fd;
    thread.next_fd += 1;
    thread.open_files.insert(fd, OpenFile::UnixSocket { id: socket_id });
    fd as u64
}

/// Looks up `fd`'s underlying `UnixSocketId`, or `None` if `fd` isn't a
/// `UnixSocket` fd at all. Shared by every `sys_usock_*` call below that
/// needs to translate a caller's fd into the id `crate::unixsocket`
/// actually operates on.
fn usock_id_for_fd(fd: u64) -> Option<crate::unixsocket::UnixSocketId> {
    let guard = SCHEDULER.lock();
    let sched = guard.as_ref().expect("scheduler not initialized");
    let thread = sched.threads.get(&sched.current).expect("current thread must be registered");
    match thread.open_files.get(&(fd as u32)) {
        Some(OpenFile::UnixSocket { id }) => Some(*id),
        _ => None,
    }
}

/// The kernel side of `SYS_USOCK_BIND_LISTEN`: `rdi`-equivalent `fd` must
/// be a `UnixSocket` fd from `sys_usock_create`. `0` success, `FD_ERROR`
/// failure (bad fd, or the name is already bound).
pub fn sys_usock_bind_listen(fd: u64, name_ptr: u64, name_len: u64, backlog: u64) -> u64 {
    let Some(socket_id) = usock_id_for_fd(fd) else {
        return FD_ERROR;
    };
    let name_bytes = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(name_bytes) else {
        return FD_ERROR;
    };
    match crate::unixsocket::bind_and_listen(socket_id, name, backlog as usize) {
        Ok(()) => 0,
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_USOCK_ACCEPT`: on a real pending connection,
/// mints a *new* fd (own `OpenFile::UnixSocket` entry, own lifetime —
/// independent of the listening fd) for the accepted end and returns it.
/// `WOULD_BLOCK` if nothing's pending yet, `FD_ERROR` if `fd` isn't a
/// listening `UnixSocket` fd at all.
pub fn sys_usock_accept(fd: u64) -> u64 {
    let Some(socket_id) = usock_id_for_fd(fd) else {
        return FD_ERROR;
    };
    match crate::unixsocket::accept(socket_id) {
        Ok(accepted_id) => {
            let mut guard = SCHEDULER.lock();
            let sched = guard.as_mut().expect("scheduler not initialized");
            let id = sched.current;
            let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
            let new_fd = thread.next_fd;
            thread.next_fd += 1;
            thread.open_files.insert(new_fd, OpenFile::UnixSocket { id: accepted_id });
            new_fd as u64
        }
        Err(crate::unixsocket::UnixSocketError::WouldBlock) => WOULD_BLOCK,
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_USOCK_CONNECT`: `0` success, `FD_ERROR`
/// failure (bad fd, or nothing listening under `name` / its backlog is
/// full — see `unixsocket::connect`'s own doc comment on why those two
/// cases share one error).
pub fn sys_usock_connect(fd: u64, name_ptr: u64, name_len: u64) -> u64 {
    let Some(socket_id) = usock_id_for_fd(fd) else {
        return FD_ERROR;
    };
    let name_bytes = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let Ok(name) = core::str::from_utf8(name_bytes) else {
        return FD_ERROR;
    };
    match crate::unixsocket::connect(socket_id, name) {
        Ok(()) => 0,
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_USOCK_READ`. `WOULD_BLOCK` if connected but
/// nothing's buffered yet, `FD_ERROR` for a bad fd or a genuinely broken
/// connection (should not arise in normal use — see `unixsocket::read`'s
/// own doc comment on when it returns `BrokenPipe`).
pub fn sys_usock_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let Some(socket_id) = usock_id_for_fd(fd) else {
        return FD_ERROR;
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len as usize) };
    match crate::unixsocket::read(socket_id, buf) {
        Ok(n) => n as u64,
        Err(crate::unixsocket::UnixSocketError::WouldBlock) => WOULD_BLOCK,
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_USOCK_WRITE`. Same return convention as
/// `sys_usock_read`.
pub fn sys_usock_write(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let Some(socket_id) = usock_id_for_fd(fd) else {
        return FD_ERROR;
    };
    let buf = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len as usize) };
    match crate::unixsocket::write(socket_id, buf) {
        Ok(n) => n as u64,
        Err(crate::unixsocket::UnixSocketError::WouldBlock) => WOULD_BLOCK,
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_OPEN`: reads `path_ptr[..path_len]` (user
/// memory — valid to dereference directly here for the same reason
/// `spawn_isolated_user`'s payload copy doesn't need one, except this
/// time the address space *is* already active, since a syscall doesn't
/// change `CR3`) as UTF-8. `mode` 0 opens for reading, via `fs::read`
/// (Milestone 1) — whole-file, not streamed; see `OpenFile::Read`'s doc
/// comment. `mode` 1 opens for writing: nothing touches disk yet (see
/// `OpenFile::Write`), so this always "succeeds" the way a real `open`
/// with `O_CREAT` would, even for a path that doesn't exist yet.
pub fn sys_open(path_ptr: u64, path_len: u64, mode: u64) -> u64 {
    let path_bytes = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return FD_ERROR;
    };

    let file = if mode == 0 {
        let Ok(data) = crate::fs::read(path) else {
            return FD_ERROR;
        };
        OpenFile::Read { data, pos: 0 }
    } else {
        OpenFile::Write { path: alloc::string::String::from(path), buffer: alloc::vec::Vec::new() }
    };

    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let fd = thread.next_fd;
    thread.next_fd += 1;
    thread.open_files.insert(fd, file);
    fd as u64
}

/// The kernel side of `SYS_PIPE`: allocates a new `crate::pipe` buffer and
/// hands the caller two fresh fds for its two ends, written as
/// `[read_fd, write_fd]` into the caller-owned buffer at `out_ptr` (same
/// direct-user-pointer trust as `SYS_GETARG`/`SYS_MEMINFO` — the syscall
/// handler still runs with the caller's own `CR3` active). Always
/// succeeds; there's no allocation failure mode `crate::pipe::create`
/// itself can hit.
pub fn sys_pipe(out_ptr: u64) -> u64 {
    let pipe_id = crate::pipe::create();

    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");

    let read_fd = thread.next_fd;
    thread.next_fd += 1;
    let write_fd = thread.next_fd;
    thread.next_fd += 1;
    thread.open_files.insert(read_fd, OpenFile::PipeRead { id: pipe_id });
    thread.open_files.insert(write_fd, OpenFile::PipeWrite { id: pipe_id });
    drop(guard);

    let out = out_ptr as *mut u64;
    unsafe {
        out.write(read_fd as u64);
        out.add(1).write(write_fd as u64);
    }
    0
}

/// The kernel side of `SYS_SPAWN`: reads `path_ptr[..path_len]` as a
/// UTF-8 path (same convention as `sys_open`), reads that file off the
/// filesystem, and loads it as a brand new isolated ring-3 process via
/// `spawn_elf` — the exact same path `main.rs::spawn_userland_demos`
/// uses at boot, just reachable from a *running* userland program now
/// instead of only from kernel code. Returns the new process's `ThreadId`
/// as a plain `u64` (`FD_ERROR` on a bad path/UTF-8/ELF), not a real PID
/// namespace — nothing here needs one yet, since nothing consumes this
/// value except as an opaque "spawn worked" signal.
/// `arg_ptr`/`arg_len` (`0`/`0` for none) become readable in the new
/// process via `SYS_GETARG`.
pub fn sys_spawn(path_ptr: u64, path_len: u64, arg_ptr: u64, arg_len: u64) -> u64 {
    let path_bytes = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Ok(path) = core::str::from_utf8(path_bytes) else {
        return FD_ERROR;
    };
    let Ok(data) = crate::fs::read(path) else {
        return FD_ERROR;
    };
    let arg = unsafe { core::slice::from_raw_parts(arg_ptr as *const u8, arg_len as usize) };
    match spawn_elf(&data, arg) {
        Ok(id) => id.0,
        Err(_) => FD_ERROR,
    }
}

/// The kernel side of `SYS_GETARG`: copies up to `buf_len` bytes of the
/// calling thread's own `Thread::arg` (set by whatever `sys_spawn` call
/// created it — empty for anything not spawned that way) into
/// `buf_ptr`, returning how many bytes actually landed. A program that
/// wants the whole thing when it might be longer than one buffer can
/// just call this once with a generously sized buffer — nothing here
/// streams or tracks a read position the way `sys_read` does, since an
/// arg is set once at spawn time and never grows.
pub fn sys_getarg(buf_ptr: u64, buf_len: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get(&id).expect("current thread must be registered");
    let n = thread.arg.len().min(buf_len as usize);
    unsafe { core::ptr::copy_nonoverlapping(thread.arg.as_ptr(), buf_ptr as *mut u8, n) };
    n as u64
}

/// The kernel side of `SYS_WAIT`: a non-blocking poll, not a real
/// blocking `wait(2)` — this kernel has no mechanism to suspend a thread
/// mid-syscall and resume it later (every context switch only ever
/// happens at the `timer_trampoline`/`syscall_trampoline` entry boundary
/// — see `on_timer_tick`'s doc comment), so a genuinely blocking syscall
/// isn't reachable without deeper surgery than this deserves yet.
/// Instead: `1` means `pid` is done — its exit status (whatever it
/// passed to `SYS_EXIT`) is written to `*status_ptr` (skipped if
/// `status_ptr == 0`) and the entry is removed from `Scheduler::exited`
/// (reaped); `0` means still running, call again. A never-spawned or
/// already-reaped `pid` also returns `1` with status `0` — indistinguishable
/// from "done" here, matching real `wait`'s "no such child" case closely
/// enough not to be a footgun. `myos_userlib::wait` wraps the retry loop
/// so callers see an ordinary blocking call; each retry is a full
/// syscall round-trip back through ring 3, which is exactly what lets
/// the scheduler actually preempt this thread and make progress on the
/// child in between attempts (interrupts are only disabled *during* one
/// syscall's handling — see `interrupts.rs`'s IDT setup note on
/// interrupt gates — not across the whole wait).
pub fn sys_wait(pid: u64, status_ptr: u64) -> u64 {
    let target = ThreadId(pid);
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    if let Some(status) = sched.exited.remove(&target) {
        if status_ptr != 0 {
            unsafe { (status_ptr as *mut u64).write(status) };
        }
        return 1;
    }
    if sched.threads.contains_key(&target) {
        return 0;
    }
    if status_ptr != 0 {
        unsafe { (status_ptr as *mut u64).write(0) };
    }
    1
}

/// Distinct ephemeral source ports across concurrent `sys_connect` calls
/// — `net::tcp_stream::TcpStream` (the boot-time HTTPS fetch's own
/// connection) hardcodes a single port since it only ever opens one
/// connection at a time; this needs a fresh one per call so two ring-3
/// programs connecting concurrently don't collide.
static NEXT_EPHEMERAL_PORT: AtomicU64 = AtomicU64::new(49152);

/// The kernel side of `SYS_CONNECT`: starts a TCP connection to
/// `ip`(a big-endian-packed IPv4 address)`:port` and returns a real `fd`
/// immediately — *not* once established. The handshake itself only
/// progresses via `net_poll_loop` (an async task on thread 0, driven by
/// NIC IRQs and the periodic timer — see `net::stack`), which this
/// synchronous syscall handler has no way to wait on directly (see
/// `sys_wait`'s doc comment on why a blocking syscall isn't reachable
/// here). Callers poll `sys_connect_status` — `myos_userlib::connect`
/// wraps the retry loop, same shape as `wait`.
pub fn sys_connect(ip: u32, port: u64) -> u64 {
    let addr = smoltcp::wire::Ipv4Address::from_bytes(&ip.to_be_bytes());
    let local_port = 49152 + (NEXT_EPHEMERAL_PORT.fetch_add(1, Ordering::Relaxed) % 16000) as u16;

    let handle = crate::net::stack::with_stack(|s| {
        let socket = crate::net::stack::new_tcp_socket();
        let handle = s.sockets.add(socket);
        let (sockets, iface) = (&mut s.sockets, &mut s.iface);
        let sock = sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
        let endpoint = smoltcp::wire::IpEndpoint::new(smoltcp::wire::IpAddress::Ipv4(addr), port as u16);
        match sock.connect(iface.context(), endpoint, local_port) {
            Ok(()) => Some(handle),
            Err(_) => {
                // Don't leak the socket we just added if connect()
                // itself rejected the endpoint (e.g. port 0).
                s.sockets.remove(handle);
                None
            }
        }
    });
    let Some(handle) = handle else {
        return FD_ERROR;
    };

    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let fd = thread.next_fd;
    thread.next_fd += 1;
    thread.open_files.insert(fd, OpenFile::Socket { handle });
    fd as u64
}

/// The kernel side of `SYS_CONNECT_STATUS`: `1` established (ready for
/// `sys_read`/`sys_write_fd`), `0` still connecting (call again), `2`
/// closed/failed/not-a-socket-fd.
pub fn sys_connect_status(fd: u64) -> u64 {
    let handle = {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");
        let id = sched.current;
        let thread = sched.threads.get(&id).expect("current thread must be registered");
        let Some(OpenFile::Socket { handle }) = thread.open_files.get(&(fd as u32)) else {
            return 2;
        };
        *handle
    };
    crate::net::stack::with_stack(|s| {
        let sock = s.sockets.get::<smoltcp::socket::tcp::Socket>(handle);
        match sock.state() {
            smoltcp::socket::tcp::State::Established => 1,
            smoltcp::socket::tcp::State::Closed | smoltcp::socket::tcp::State::TimeWait => 2,
            _ => 0,
        }
    })
}

/// The kernel side of `SYS_RESOLVE`: starts an A-record DNS query for
/// `host_ptr[..host_len]` against `net::stack::STACK`'s shared
/// `dns_handle` socket (the same one `net::http::resolve` uses for the
/// kernel's own boot-time fetch — smoltcp's `dns::Socket` supports
/// multiple concurrent queries, each tracked by its own `QueryHandle`,
/// so this doesn't collide with that). Returns a fd immediately, same
/// non-blocking-start shape as `sys_connect`; poll it with
/// `sys_resolve_status`.
pub fn sys_resolve(host_ptr: u64, host_len: u64) -> u64 {
    let host_bytes = unsafe { core::slice::from_raw_parts(host_ptr as *const u8, host_len as usize) };
    let Ok(host) = core::str::from_utf8(host_bytes) else {
        return FD_ERROR;
    };

    let handle = crate::net::stack::with_stack(|s| {
        let (sockets, iface) = (&mut s.sockets, &mut s.iface);
        let socket = sockets.get_mut::<smoltcp::socket::dns::Socket>(s.dns_handle);
        socket.start_query(iface.context(), host, smoltcp::wire::DnsQueryType::A).ok()
    });
    let Some(handle) = handle else {
        return FD_ERROR;
    };

    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    let fd = thread.next_fd;
    thread.next_fd += 1;
    thread.open_files.insert(fd, OpenFile::Resolve { handle });
    fd as u64
}

/// The kernel side of `SYS_RESOLVE_STATUS`: `WOULD_BLOCK` while the
/// query is still in flight (leaves the fd alone — call again), or on a
/// terminal result, consumes the fd and returns the resolved IPv4
/// address zero-extended into the low 32 bits of the return value
/// (`FD_ERROR` on failure/bad fd — every real 32-bit address, including
/// `0xFFFFFFFF`, zero-extends to something other than `FD_ERROR`'s all-
/// ones 64 bits or `WOULD_BLOCK`'s `u64::MAX - 1`, so there's no
/// collision with either sentinel). IPv6 results are skipped — this
/// kernel's `sys_connect` only speaks IPv4.
pub fn sys_resolve_status(fd: u64) -> u64 {
    let handle = {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");
        let id = sched.current;
        let thread = sched.threads.get(&id).expect("current thread must be registered");
        let Some(OpenFile::Resolve { handle }) = thread.open_files.get(&(fd as u32)) else {
            return FD_ERROR;
        };
        *handle
    };

    let result = crate::net::stack::with_stack(|s| {
        let socket = s.sockets.get_mut::<smoltcp::socket::dns::Socket>(s.dns_handle);
        socket.get_query_result(handle)
    });

    match result {
        Err(smoltcp::socket::dns::GetQueryResultError::Pending) => WOULD_BLOCK,
        terminal => {
            // Either outcome is terminal — smoltcp already freed its own
            // query slot (see `get_query_result`'s doc comment), so this
            // fd must be dropped too; calling `get_query_result` again on
            // a freed slot panics.
            let mut guard = SCHEDULER.lock();
            let sched = guard.as_mut().expect("scheduler not initialized");
            let id = sched.current;
            let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
            thread.open_files.remove(&(fd as u32));

            match terminal {
                Ok(addrs) => addrs
                    .into_iter()
                    .find_map(|addr| match addr {
                        smoltcp::wire::IpAddress::Ipv4(v4) => Some(u32::from_be_bytes(v4.0) as u64),
                    })
                    .unwrap_or(FD_ERROR),
                Err(_) => FD_ERROR,
            }
        }
    }
}

/// The kernel side of `SYS_LSEEK`: only meaningful for a `Read` fd
/// (`SYS_OPEN` mode 0) — the whole file already sits in memory (see
/// `OpenFile::Read`'s doc comment), so seeking is just moving `pos`.
/// `whence` follows the classic convention: `0` = from start (`offset`
/// must be non-negative), `1` = from current `pos`, `2` = from end.
/// Clamps the result into `[0, data.len()]` rather than erroring on an
/// out-of-range request — the same permissive posture `sbrk` already
/// takes for a bad `increment`. Any other fd kind (`Write`/`Socket`/
/// `Pipe*`/`Resolve` — none of which have a seekable position) returns
/// `FD_ERROR`. Returns the new absolute position, or `FD_ERROR`.
pub fn sys_lseek(fd: u64, offset: i64, whence: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    match thread.open_files.get_mut(&(fd as u32)) {
        Some(OpenFile::Read { data, pos }) => {
            let base: i64 = match whence {
                0 => 0,
                1 => *pos as i64,
                2 => data.len() as i64,
                _ => return FD_ERROR,
            };
            let new_pos = (base + offset).clamp(0, data.len() as i64);
            *pos = new_pos as usize;
            new_pos as u64
        }
        _ => FD_ERROR,
    }
}

/// The kernel side of `SYS_DUP`: makes a second fd in the *same* thread
/// refer to the same underlying resource as `fd`, picking the new fd
/// number itself (like POSIX `dup`, as opposed to `SYS_DUP2` picking a
/// caller-chosen one). Only meaningful for kinds that actually have a
/// notion of shared state to duplicate: a `Pipe*` fd increments the
/// pipe's own refcount (see `crate::pipe`) so both fds' `SYS_CLOSE` are
/// independently accounted for; a `Read` fd gets its own independent copy
/// of the buffered file data and its *current* `pos` (not a truly shared
/// position the way a real OS's duplicated file description would be —
/// documented simplification, not a bug: this kernel's `Read` fd is
/// already a whole-file snapshot with no underlying open file description
/// to share, unlike a real kernel's). `Write`/`Socket`/`Resolve` fds have
/// no sensible duplicate (an in-progress DNS query in particular can't be
/// duplicated at all — see `OpenFile::Resolve`'s doc comment on why it's
/// consumed on first read) and return `FD_ERROR`.
pub fn sys_dup(fd: u64) -> u64 {
    dup_to(fd, None)
}

/// The kernel side of `SYS_DUP2`: like `sys_dup`, but the caller picks
/// the destination fd (`new_fd`) instead of getting whatever's next. If
/// `new_fd` was already open, its previous resource is torn down first
/// (same `finalize_open_file` teardown `SYS_CLOSE` uses) exactly like
/// POSIX `dup2`. A `new_fd == fd` no-op still succeeds (matching POSIX),
/// short-circuiting before any of that.
pub fn sys_dup2(fd: u64, new_fd: u64) -> u64 {
    if fd == new_fd {
        let guard = SCHEDULER.lock();
        let sched = guard.as_ref().expect("scheduler not initialized");
        let thread = sched.threads.get(&sched.current).expect("current thread must be registered");
        return if thread.open_files.contains_key(&(fd as u32)) { new_fd } else { FD_ERROR };
    }
    dup_to(fd, Some(new_fd as u32))
}

/// Shared implementation: duplicates `fd`'s resource into `dest` (a
/// specific fd number) or, if `None`, into a fresh one from `next_fd`.
fn dup_to(fd: u64, dest: Option<u32>) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");

    let duplicated = match thread.open_files.get(&(fd as u32)) {
        Some(OpenFile::Read { data, pos }) => OpenFile::Read { data: data.clone(), pos: *pos },
        Some(OpenFile::PipeRead { id }) => {
            let id = *id;
            crate::pipe::dup_read_end(id);
            OpenFile::PipeRead { id }
        }
        Some(OpenFile::PipeWrite { id }) => {
            let id = *id;
            crate::pipe::dup_write_end(id);
            OpenFile::PipeWrite { id }
        }
        _ => return FD_ERROR,
    };

    let new_fd = match dest {
        Some(explicit) => {
            if let Some(old) = thread.open_files.insert(explicit, duplicated) {
                drop(guard);
                finalize_open_file(old);
                return explicit as u64;
            }
            explicit
        }
        None => {
            let allocated = thread.next_fd;
            thread.next_fd += 1;
            thread.open_files.insert(allocated, duplicated);
            allocated
        }
    };
    new_fd as u64
}

/// The kernel side of `SYS_CLOCK_GETTIME`: `rdi` = ptr to a caller-owned
/// buffer of 7 consecutive `u32`s (28 bytes) — `[year, month, day, hour,
/// minute, second, nanos_within_second]`. The first six come from
/// `crate::rtc::now()` (the CMOS wall clock, whole-second resolution);
/// `nanos_within_second` comes from `crate::tsc::now_ns()` modulo one
/// second, giving sub-second precision *within* whatever second the RTC
/// read landed in — not a true fused reading (the two clocks are read
/// back to back, not atomically), but close enough for this kernel's own
/// demos, and documented rather than presented as more precise than it
/// is. Always succeeds.
pub fn sys_clock_gettime(out_ptr: u64) -> u64 {
    let t = crate::rtc::now();
    let ns = crate::tsc::now_ns() % 1_000_000_000;
    let out = out_ptr as *mut u32;
    unsafe {
        out.write(t.year);
        out.add(1).write(t.month as u32);
        out.add(2).write(t.day as u32);
        out.add(3).write(t.hour as u32);
        out.add(4).write(t.minute as u32);
        out.add(5).write(t.second as u32);
        out.add(6).write(ns as u32);
    }
    0
}

/// The kernel side of `SYS_READ`: copies up to `len` bytes from `fd`'s
/// current position into `buf_ptr` (user memory, same "already the
/// active address space" reasoning as `sys_open`), advances that
/// position, and returns how many bytes actually landed (`0` at EOF).
/// For a `mode` 0 (read) file `fd`. For a `Socket` fd, returns
/// `WOULD_BLOCK` instead of `0` when the connection is simply idle
/// (`0` stays reserved for genuine EOF — the peer closing its side) —
/// see `OpenFile::Socket`'s and `WOULD_BLOCK`'s own doc comments.
pub fn sys_read(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    match thread.open_files.get_mut(&(fd as u32)) {
        Some(OpenFile::Read { data, pos }) => {
            let remaining = &data[(*pos).min(data.len())..];
            let n = remaining.len().min(len as usize);
            unsafe { core::ptr::copy_nonoverlapping(remaining.as_ptr(), buf_ptr as *mut u8, n) };
            *pos += n;
            n as u64
        }
        Some(OpenFile::Socket { handle }) => {
            let handle = *handle;
            drop(guard);
            let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len as usize) };
            crate::net::stack::with_stack(|s| {
                let sock = s.sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                let n = sock.recv_slice(buf).unwrap_or(0);
                if n > 0 {
                    n as u64
                } else if !sock.may_recv() {
                    0 // peer closed its side: real EOF
                } else {
                    WOULD_BLOCK
                }
            })
        }
        Some(OpenFile::PipeRead { id }) => {
            let id = *id;
            drop(guard);
            let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, len as usize) };
            match crate::pipe::read(id, buf) {
                Ok(n) => n as u64,
                Err(crate::pipe::PipeError::WouldBlock) => WOULD_BLOCK,
                Err(crate::pipe::PipeError::BrokenPipe) => FD_ERROR,
            }
        }
        _ => FD_ERROR,
    }
}

/// The kernel side of `SYS_WRITE` when `fd` isn't the stdout convention
/// (see `interrupts.rs`'s `syscall_handler`, which handles `fd == 1`
/// itself and only calls this for anything else): for a `mode` 1
/// (write) file `fd`, appends `len` bytes from `buf_ptr` to its
/// in-memory buffer — nothing is persisted until `sys_close`. For a
/// `Socket` fd, sends directly (TCP has no separate "close to flush"
/// step); `WOULD_BLOCK` if the send window is currently full but the
/// connection is still open, `FD_ERROR` if it's gone.
pub fn sys_write_fd(fd: u64, buf_ptr: u64, len: u64) -> u64 {
    let mut guard = SCHEDULER.lock();
    let sched = guard.as_mut().expect("scheduler not initialized");
    let id = sched.current;
    let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
    match thread.open_files.get_mut(&(fd as u32)) {
        Some(OpenFile::Write { buffer, .. }) => {
            let bytes = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len as usize) };
            buffer.extend_from_slice(bytes);
            len
        }
        Some(OpenFile::Socket { handle }) => {
            let handle = *handle;
            drop(guard);
            let bytes = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len as usize) };
            crate::net::stack::with_stack(|s| {
                let sock = s.sockets.get_mut::<smoltcp::socket::tcp::Socket>(handle);
                if sock.can_send() {
                    sock.send_slice(bytes).unwrap_or(0) as u64
                } else if !sock.may_send() {
                    FD_ERROR
                } else {
                    WOULD_BLOCK
                }
            })
        }
        Some(OpenFile::PipeWrite { id }) => {
            let id = *id;
            drop(guard);
            let bytes = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, len as usize) };
            match crate::pipe::write(id, bytes) {
                Ok(n) => n as u64,
                Err(crate::pipe::PipeError::WouldBlock) => WOULD_BLOCK,
                Err(crate::pipe::PipeError::BrokenPipe) => FD_ERROR,
            }
        }
        _ => FD_ERROR,
    }
}

/// The kernel side of `SYS_CLOSE`: for a `mode` 1 (write) `fd`, this is
/// the only point anything actually reaches disk — `fs::write` (see its
/// own doc comment: create-or-truncate, whole-file) runs here, once,
/// with everything `sys_write_fd` accumulated. A `mode` 0 (read) `fd`
/// just drops its buffered contents; nothing to flush. A `Socket` fd is
/// removed from the shared `SocketSet` — smoltcp itself handles sending
/// the actual FIN.
pub fn sys_close(fd: u64) -> u64 {
    let file = {
        let mut guard = SCHEDULER.lock();
        let sched = guard.as_mut().expect("scheduler not initialized");
        let id = sched.current;
        let thread = sched.threads.get_mut(&id).expect("current thread must be registered");
        thread.open_files.remove(&(fd as u32))
    };

    match file {
        Some(f) => finalize_open_file(f),
        None => FD_ERROR,
    }
}

/// Does whatever a given `fd` needs done when it goes away, regardless of
/// whether that's a real `SYS_CLOSE` call or a whole thread disappearing
/// (`sys_kill`, `exit_current`) with fds it never explicitly closed —
/// both need the *same* teardown (flush a pending write, release a
/// socket/DNS query back to the network stack), just reached from
/// different call sites. Before this was factored out, `exit_current`
/// simply dropped a thread's whole `open_files` map, which — for a
/// `Socket`/`Resolve` fd — leaked it in `net::stack::STACK`'s shared
/// `SocketSet`/DNS query table forever (a real, if slow, resource leak:
/// nothing ever freed those slots), and for a `Write` fd silently
/// discarded whatever had been buffered instead of persisting it the way
/// an explicit `close()` would have.
fn finalize_open_file(file: OpenFile) -> u64 {
    match file {
        OpenFile::Socket { handle } => {
            crate::net::stack::with_stack(|s| s.sockets.remove(handle));
            0
        }
        // A resolve fd closed before it ever reached a terminal state
        // (`sys_resolve_status` normally consumes it, at which point it's
        // already gone from `open_files` — this only fires for one still
        // `Pending`) — cancel it so its query slot doesn't leak forever.
        OpenFile::Resolve { handle } => {
            crate::net::stack::with_stack(|s| {
                let socket = s.sockets.get_mut::<smoltcp::socket::dns::Socket>(s.dns_handle);
                socket.cancel_query(handle);
            });
            0
        }
        OpenFile::Write { path, buffer } => match crate::fs::write(&path, &buffer) {
            Ok(()) => 0,
            Err(e) => {
                // Previously silent — a write `fd` whose `fs::write`
                // failed at close time (e.g. `embedded-sdmmc`'s 8.3
                // short-filename limit rejecting a too-long path) looked
                // identical to success from here up, with no way to
                // learn why short of guessing. `sys_write_fd`'s own
                // in-memory append always "succeeds" regardless, so this
                // is genuinely the only point such an error can surface.
                crate::serial_println!("fs: failed to persist {path}: {e}");
                FD_ERROR
            }
        },
        OpenFile::Read { .. } => 0,
        OpenFile::PipeRead { id } => {
            crate::pipe::close_read_end(id);
            0
        }
        OpenFile::PipeWrite { id } => {
            crate::pipe::close_write_end(id);
            0
        }
        OpenFile::UnixSocket { id } => {
            crate::unixsocket::close(id);
            0
        }
    }
}

/// Finalizes every fd a thread still had open when it went away — see
/// `finalize_open_file`'s own doc comment for why this matters and what
/// it fixes. Called from `exit_current` and `sys_kill`, never while
/// holding `SCHEDULER`'s lock (each entry's teardown takes
/// `net::stack::STACK`'s or `fs::DISK_LOCK`'s own lock instead).
fn finalize_open_files(open_files: BTreeMap<u32, OpenFile>) {
    for (_, file) in open_files {
        finalize_open_file(file);
    }
}
