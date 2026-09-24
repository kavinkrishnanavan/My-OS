//! Paging setup and the physical<->virtual translation every DMA-capable
//! driver (i.e. the NIC) needs.
//!
//! The bootloader identity-maps all physical RAM starting at
//! `PHYS_MEM_OFFSET` before it ever jumps into our kernel. That single
//! fact is what lets the RTL8139 driver hand the NIC a *physical* ring
//! buffer address while the CPU keeps reading/writing it through an
//! ordinary Rust pointer — no manual page-table walking per packet.

use crate::allocator::BootInfoFrameAllocator;
use spin::{Mutex, Once};
use x86_64::instructions::interrupts::without_interrupts;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{
    structures::paging::{OffsetPageTable, PageTable},
    VirtAddr,
};

static PHYS_MEM_OFFSET: Once<u64> = Once::new();
static FRAME_ALLOCATOR: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
/// The PML4 frame `CR3` held at boot, before any isolated address space
/// existed — captured once so `task::thread::on_timer_tick` can switch
/// back to it explicitly when resuming a non-isolated thread. Without
/// this, "non-isolated" threads (`cr3: None`) had no defined target to
/// switch to and just left `CR3` wherever the *previous* thread happened
/// to leave it — fine as long as that was always the shared table, but
/// wrong forever after the first time an isolated thread ever ran, since
/// nothing then ever pointed `CR3` back.
static BOOT_PML4_FRAME: Once<PhysFrame<Size4KiB>> = Once::new();

/// Hands over ownership of the page table mapper `init` built, so code
/// that runs later (after `main`'s local `mapper` would otherwise have
/// gone out of scope) can still map new pages — right now, just
/// `task::thread`'s ring-3 code/stack pages.
pub fn set_mapper(mapper: OffsetPageTable<'static>) {
    *MAPPER.lock() = Some(mapper);
}

/// Maps one fresh 4 KiB page at `virt_addr` in `mapper` specifically —
/// not necessarily the shared/active one (`Mapper::map_to` only needs a
/// `&mut PageTable` reference; the table it modifies doesn't have to be
/// the one `CR3` currently points at). `flags` is what actually
/// restricts a ring-3 thread from touching pages it shouldn't (e.g.
/// omitting `USER_ACCESSIBLE` on kernel-only pages, which every page
/// mapped before this function existed already implicitly does).
/// Returns the physical frame backing the new mapping — needed by
/// callers (e.g. `task::thread::spawn_isolated_user`) writing to a page
/// in an address space that *isn't* the currently active one: the
/// virtual address itself isn't dereferencable yet (it only resolves
/// correctly once something actually switches `CR3` to this table), but
/// `phys_to_virt` on the returned frame reaches the same physical memory
/// from any address space, since the physical-memory-offset mapping
/// itself is present (cloned) in every one of them.
pub fn map_page_in(mapper: &mut OffsetPageTable, virt_addr: VirtAddr, flags: PageTableFlags) -> Result<PhysFrame<Size4KiB>, &'static str> {
    // Without this, a timer tick landing mid-critical-section (holding
    // FRAME_ALLOCATOR) could switch to another thread that also wants
    // it — on a single CPU with no way back to this thread until the
    // scheduler gets to it again, everything waiting on that lock (very
    // possibly including the timer handler's *own* next scheduling
    // decision, on a future tick) just spins forever. Same reasoning
    // `task::thread`'s own SCHEDULER-locking call sites already apply.
    without_interrupts(|| {
        let mut frame_guard = FRAME_ALLOCATOR.lock();
        let frame_allocator = frame_guard.as_mut().ok_or("frame allocator not initialized")?;

        let page = Page::<Size4KiB>::containing_address(virt_addr);
        let frame = frame_allocator.allocate_frame().ok_or("out of physical memory")?;
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|_| "failed to map page (already mapped?)")?
                .flush();
        }
        Ok(frame)
    })
}

/// `map_page_in` against the one shared address space everything in this
/// kernel ran in before per-process address spaces existed (Milestone 2's
/// ring-3 demo still uses this — it was never given its own isolated
/// space, deliberately: see the Milestone 3 plan for why that distinction
/// matters).
pub fn map_page(virt_addr: VirtAddr, flags: PageTableFlags) -> Result<PhysFrame<Size4KiB>, &'static str> {
    let mut mapper_guard = MAPPER.lock();
    let mapper = mapper_guard.as_mut().ok_or("memory::set_mapper not called yet")?;
    map_page_in(mapper, virt_addr, flags)
}

/// Recursively deep-clones a page-table hierarchy from `level` (4 =
/// PML4, down to 1 = PT) down: every intermediate table gets its own
/// fresh physical frame and independent entries, so a mapping added to
/// the clone *later* can never mutate the original's (or any other
/// clone's) tables. This replaced an earlier version that only copied
/// the top-level PML4 *entries* — which alias every lower level below
/// whatever's already mapped, so adding a *new* mapping inside an
/// already-populated PML4 slot (e.g. anywhere in the low ~512 GiB, where
/// the kernel's own image lives) silently mutated the *shared* tables
/// instead of a private copy. That's exactly how two ELF programs both
/// linked at a low address (`userland/hello`, `userland/counter`) ended
/// up with one's code overwriting the other's the first time this was
/// tried. Only the table *structure* is deep-copied; leaf entries
/// (actual data frames, or huge pages at levels 2/3) are copied as
/// plain values, sharing the underlying physical data — correct, since
/// nothing here needs to copy-on-write existing *contents*, only let
/// each address space add its own *new* mappings independently.
fn deep_clone_table(offset: u64, frame_allocator: &mut BootInfoFrameAllocator, src: &PageTable, level: u8) -> PhysFrame<Size4KiB> {
    let new_frame = frame_allocator.allocate_frame().expect("out of physical memory cloning a page table");
    let new_table: &mut PageTable = unsafe {
        let ptr = (new_frame.start_address().as_u64() + offset) as *mut PageTable;
        ptr.write(PageTable::new());
        &mut *ptr
    };

    for i in 0..512 {
        let entry = &src[i];
        if entry.is_unused() {
            continue;
        }
        if level > 1 && !entry.flags().contains(PageTableFlags::HUGE_PAGE) {
            // Points at a lower-level table — recurse, so it gets its
            // own private copy too, not just this level.
            let child_table: &PageTable = unsafe { &*((entry.addr().as_u64() + offset) as *const PageTable) };
            let cloned_child = deep_clone_table(offset, frame_allocator, child_table, level - 1);
            new_table[i].set_addr(cloned_child.start_address(), entry.flags());
        } else {
            // A leaf (a PT entry's actual data frame, or a huge page at
            // levels 2/3) — copy the value as-is, sharing the data.
            new_table[i] = entry.clone();
        }
    }

    new_frame
}

/// Builds a new, independent address space: a deep clone (see
/// `deep_clone_table`) of the *shared boot* address space's whole
/// page-table structure (`BOOT_PML4_FRAME`, not whatever `CR3` currently
/// is) — same physical data for kernel code/data, the heap, every
/// existing thread's stack, the physical-memory-offset mapping, all of
/// it, so anything mapped before this call remains valid seen through
/// the new table too, but every level of the *table structure itself*
/// is independent. Callers then use `map_page_in` on the returned
/// mapper to add mappings private to just this new address space (see
/// `task::thread::spawn_isolated_user`/`spawn_elf`) — those won't appear
/// in the original table or any other address space built this way, no
/// matter which existing PML4 slot they happen to land in.
///
/// Deliberately *not* "clone whatever's currently active": this is
/// reachable from `task::thread::sys_spawn`, called as a syscall from an
/// *already-isolated* ring-3 thread — if it cloned that caller's own
/// table, the new child would inherit the parent's private mappings at
/// the parent's own fixed ELF load address (every `spawn_elf`'d program
/// links at the same `0x01000000`), and `elf::load` mapping the child's
/// segments there would find that address "already mapped" and instead
/// write the new program's bytes into the frame *shared with the
/// parent's own code* (leaf entries are copied by value, not
/// copy-on-write) — corrupting the parent's still-running code out from
/// under it. Real, previously-shipped bug: `userland/spawner` calling
/// `spawn("hello.elf")` on itself page-faulted moments later, mid-way
/// through its own `writeln!`, for exactly this reason. Always cloning
/// from the boot table instead means every new address space starts from
/// the same known-empty-at-user-addresses baseline, regardless of which
/// thread's syscall triggered it.
pub fn new_address_space() -> Result<(PhysFrame<Size4KiB>, OffsetPageTable<'static>), &'static str> {
    let offset = *PHYS_MEM_OFFSET.get().ok_or("memory::init not called")?;

    let current_frame = boot_pml4_frame();
    let current_table: &PageTable = unsafe { &*((current_frame.start_address().as_u64() + offset) as *const PageTable) };

    // Same reasoning as `map_page_in`'s own `without_interrupts` — this
    // one matters even more here: `deep_clone_table`'s recursion can
    // hold FRAME_ALLOCATOR for a while (proportional to how much is
    // currently mapped — in practice, hundreds to low thousands of
    // frame clones once the physical-memory-offset mapping's own table
    // structure is included), a much bigger window for a tick to land
    // in than a single-page `map_page_in` call.
    //
    // Cost note: each call consumes that many physical frames for table
    // structure alone, on top of whatever the caller then maps into the
    // new space. `free_address_space` (called from
    // `task::thread::exit_current`) reclaims the table-structure frames
    // once the process exits; data frames the process mapped into its
    // own space (ELF segments, stack, `sbrk` growth) still leak — see
    // that function's doc comment for why.
    let new_frame = without_interrupts(|| {
        let mut frame_guard = FRAME_ALLOCATOR.lock();
        let frame_allocator = frame_guard.as_mut().ok_or("frame allocator not initialized")?;
        Ok::<_, &'static str>(deep_clone_table(offset, frame_allocator, current_table, 4))
    })?;

    Ok((new_frame, mapper_for(new_frame)))
}

/// Mirror-image of `deep_clone_table`: recursively frees every
/// page-table-*structure* frame in `frame`'s hierarchy (levels 4 down to
/// 1), leaving leaf entries (actual data frames at level 1, or huge pages
/// at levels 2/3) completely untouched. That's safe precisely because
/// `deep_clone_table` gave *every* intermediate table at *every* level a
/// fresh, private frame when this address space was built — nothing
/// walked here is ever shared with another address space's tables, only
/// (potentially) with its *data*, which this never frees.
///
/// Only reclaims table-structure frames, not the data frames a process
/// mapped into its own space afterward (ELF segments, its stack, `sbrk`
/// growth) — the leaf entries pointing at those are skipped exactly like
/// `deep_clone_table` skips them, so which frames those even are isn't
/// recoverable from the table alone without tracking ownership
/// separately. Still reclaims the bulk of what `new_address_space`
/// permanently cost before this existed (per its own doc comment:
/// "hundreds to low thousands of frame clones").
fn free_table(offset: u64, frame_allocator: &mut BootInfoFrameAllocator, frame: PhysFrame<Size4KiB>, level: u8) {
    let table: &PageTable = unsafe { &*((frame.start_address().as_u64() + offset) as *const PageTable) };
    if level > 1 {
        for i in 0..512 {
            let entry = &table[i];
            if entry.is_unused() {
                continue;
            }
            if !entry.flags().contains(PageTableFlags::HUGE_PAGE) {
                let child_frame = PhysFrame::containing_address(entry.addr());
                free_table(offset, frame_allocator, child_frame, level - 1);
            }
        }
    }
    unsafe { frame_allocator.deallocate_frame(frame) };
}

/// Tears down an isolated address space `new_address_space` built (called
/// from `task::thread::exit_current` once a `spawn_isolated_user`/
/// `spawn_elf` thread exits, and only *after* `CR3` has already been
/// switched away from `pml4_frame` — freeing table frames still backing
/// the active address space would be a use-after-free the moment the next
/// timer tick or page-table walk touched them).
pub fn free_address_space(pml4_frame: PhysFrame<Size4KiB>) {
    let offset = *PHYS_MEM_OFFSET.get().expect("memory::init not called");
    without_interrupts(|| {
        let mut frame_guard = FRAME_ALLOCATOR.lock();
        let frame_allocator = frame_guard.as_mut().expect("frame allocator not initialized");
        free_table(offset, frame_allocator, pml4_frame, 4);
    });
}

/// Reclaims data frames a process mapped into its own address space
/// after `new_address_space` built it — ELF segment pages
/// (`elf::load`'s `owned_frames`), a `spawn_isolated_user`/`spawn_elf`
/// thread's stack page, and `sbrk` heap growth — the part
/// `free_address_space` (table structure only) explicitly couldn't
/// reach, since those data frames' leaf entries look identical to ones
/// shared with another address space by the time only the table is left
/// to walk. Called from `task::thread::exit_current` with the frames
/// that thread's own `Thread::owned_frames` tracked as they were
/// allocated, so there's no ambiguity about ownership here at all.
pub fn free_frames(frames: &[PhysFrame<Size4KiB>]) {
    without_interrupts(|| {
        let mut frame_guard = FRAME_ALLOCATOR.lock();
        let frame_allocator = frame_guard.as_mut().expect("frame allocator not initialized");
        for &frame in frames {
            unsafe { frame_allocator.deallocate_frame(frame) };
        }
    });
}

/// Rebuilds a mapper for an *existing* address space from its PML4's
/// physical frame — for touching an address space again after
/// `new_address_space` first built it and its `OffsetPageTable` went out
/// of scope (e.g. `task::thread::sbrk`, growing a process's heap well
/// after `spawn_isolated_user`/`spawn_elf` returned). Safe to call
/// whether or not `frame` is the currently active `CR3` — same as
/// `map_page_in`, this only needs a `&mut PageTable` reference, not the
/// hardware-active one.
pub fn mapper_for(frame: PhysFrame<Size4KiB>) -> OffsetPageTable<'static> {
    let offset = *PHYS_MEM_OFFSET.get().expect("memory::init not called");
    let table: &'static mut PageTable = unsafe { &mut *((frame.start_address().as_u64() + offset) as *mut PageTable) };
    unsafe { OffsetPageTable::new(table, VirtAddr::new(offset)) }
}

pub fn phys_to_virt(phys: u64) -> u64 {
    phys + *PHYS_MEM_OFFSET.get().expect("memory::init not called")
}

pub fn set_frame_allocator(alloc: BootInfoFrameAllocator) {
    *FRAME_ALLOCATOR.lock() = Some(alloc);
}

/// `(frames_allocated_total, frames_currently_free_for_reuse)` — for
/// `SYS_MEMINFO`. Returns `(0, 0)` rather than panicking if called before
/// `set_frame_allocator` (shouldn't happen once a syscall can even fire,
/// but this has no other invariant forcing that ordering).
pub fn frame_stats() -> (u64, u64) {
    let guard = FRAME_ALLOCATOR.lock();
    match guard.as_ref() {
        Some(alloc) => alloc.stats(),
        None => (0, 0),
    }
}

/// Allocates one physical 4 KiB frame for DMA use (NIC descriptor rings
/// and packet buffers) and returns its physical address. Always a fresh
/// bump-cursor frame, never one out of `freed` — see
/// `BootInfoFrameAllocator::allocate_bump_frame`'s doc comment for why a
/// reused frame can't be allowed here.
pub fn alloc_dma_frame() -> u64 {
    let mut guard = FRAME_ALLOCATOR.lock();
    let allocator = guard.as_mut().expect("frame allocator not initialized");
    let frame = allocator.allocate_bump_frame().expect("out of physical memory for DMA buffer");
    frame.start_address().as_u64()
}

/// Allocates a physically *contiguous* DMA region of at least `bytes`,
/// rounded up to whole 4 KiB frames. The RTL8139's RX ring is one
/// physical buffer the NIC DMAs straight into, so it can't be scattered
/// across unrelated frames the way a normal heap allocation could be.
///
/// Our frame allocator is a simple bump allocator over each usable
/// memory-map region in ascending address order, so consecutive calls
/// return physically adjacent frames as long as nothing else allocates
/// in between. By the time this runs (`net::rtl8139::init`, called from
/// `main.rs` after the kernel-thread/ring-3/ELF-loader demos are already
/// spawned) interrupts are unmasked and other threads are genuinely
/// runnable, so the PIT tick can preempt this function between two of its
/// own `alloc_dma_frame` calls and let another thread's own frame
/// allocation (e.g. `new_address_space`'s page-table clone for a freshly
/// spawned process) land in between, breaking contiguity — this was a
/// real, reproduced panic here (`assert_eq!` below), not a hypothetical
/// one, once enough concurrent boot-time ELF spawns widened the race
/// window. `without_interrupts` makes the *whole* multi-frame allocation
/// atomic with respect to this kernel's only source of preemption (the
/// timer interrupt), which is what the contiguity assumption actually
/// requires — not "runs once during driver init", which nothing enforced.
pub fn alloc_dma_region(bytes: usize) -> u64 {
    without_interrupts(|| {
        let frames = bytes.div_ceil(4096);
        let base = alloc_dma_frame();
        let mut expected = base + 4096;
        for _ in 1..frames {
            let next = alloc_dma_frame();
            assert_eq!(
                next, expected,
                "DMA region not physically contiguous — frame allocator invariant broken"
            );
            expected += 4096;
        }
        base
    })
}

/// # Safety
/// `physical_memory_offset` must be the value the bootloader put in
/// `BootInfo`, and this must be called exactly once.
pub unsafe fn init(physical_memory_offset: u64) -> OffsetPageTable<'static> {
    PHYS_MEM_OFFSET.call_once(|| physical_memory_offset);
    BOOT_PML4_FRAME.call_once(|| Cr3::read().0);
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, VirtAddr::new(physical_memory_offset))
}

/// The shared address space's PML4 frame — see `BOOT_PML4_FRAME`'s own
/// doc comment for why `task::thread::on_timer_tick` needs this as an
/// explicit switch target, not just "leave CR3 alone".
pub fn boot_pml4_frame() -> PhysFrame<Size4KiB> {
    *BOOT_PML4_FRAME.get().expect("memory::init not called")
}

unsafe fn active_level_4_table(physical_memory_offset: u64) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address();
    let virt = VirtAddr::new(phys.as_u64() + physical_memory_offset);
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}
