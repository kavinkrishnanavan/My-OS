//! Kernel heap.
//!
//! smoltcp's socket buffers and our async task queue both need `alloc`
//! (`Box`, `Vec`, `Arc`). The bootloader maps our address space and hands
//! us `physical_memory_offset`, but doesn't set up a heap — we carve one
//! out of virtual address space ourselves and back it with freshly
//! allocated physical frames.

use alloc::vec::Vec;
use linked_list_allocator::LockedHeap;
use x86_64::{
    structures::paging::{
        FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTableFlags,
        PhysFrame, Size4KiB,
    },
    PhysAddr, VirtAddr,
};

pub const HEAP_START: u64 = 0x_4444_4444_0000;
pub const HEAP_SIZE: usize = 32 * 1024 * 1024; // 32 MiB — real-world pages (e.g. youtube.com) run several MB

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

/// Hands out physical frames from the bootloader's memory map, in order,
/// falling back to a bump cursor (`region_idx`/`next_addr`) only once
/// `freed` (frames handed back via `deallocate_frame` — see
/// `memory::free_address_space`) is empty. Freeing only ever happens for
/// page-table *structure* frames reclaimed when an isolated process
/// exits; the bump side still never runs backwards.
///
/// `region_idx`/`next_addr` track a persistent cursor rather than the
/// original design's `next: usize` count (re-deriving `usable_frames()`
/// and calling `.nth(next)` on it fresh every single call) — that
/// earlier version was O(next) *per allocation*, so O(n²) over the
/// allocator's lifetime. Fine for a couple hundred frames; a real bug
/// once callers started allocating thousands per call site: the first
/// `SYS_SPAWN` increment added a *third* boot-time `new_address_space`
/// deep-clone (~1000+ frames each) on top of the two already there, and
/// with `next` already in the thousands by then, that third clone's
/// allocations alone needed tens of millions of iterations — a boot that
/// genuinely never completed (confirmed by a 3+ minute wait with zero
/// scheduler ticks reaching *any* other thread, not just slowness).
pub struct BootInfoFrameAllocator {
    regions: &'static bootloader_api::info::MemoryRegions,
    region_idx: usize,
    next_addr: u64,
    freed: Vec<PhysFrame<Size4KiB>>,
    allocated_total: u64,
}

impl BootInfoFrameAllocator {
    /// # Safety
    /// `regions` must describe genuinely usable, unmapped physical memory
    /// (as the bootloader's `Usable` regions do).
    pub unsafe fn init(regions: &'static bootloader_api::info::MemoryRegions) -> Self {
        let mut a = BootInfoFrameAllocator { regions, region_idx: 0, next_addr: 0, freed: Vec::new(), allocated_total: 0 };
        a.enter_current_region();
        a
    }

    /// `(frames_allocated_total, frames_currently_free_for_reuse)` — for
    /// `SYS_MEMINFO`. The first count includes frames later reclaimed into
    /// `freed` (they were still handed out at some point), so it's a
    /// lifetime total, not a "currently in use" figure; the second is
    /// exactly `freed.len()`, i.e. what the next `allocate_frame` calls
    /// will serve before falling back to the bump cursor.
    pub fn stats(&self) -> (u64, u64) {
        (self.allocated_total, self.freed.len() as u64)
    }

    /// Moves `next_addr` forward to the first allocatable address in
    /// `regions[region_idx]` (or advances `region_idx` past it first, if
    /// it's not `Usable` or `next_addr` already ran past its end) —
    /// called once at `init` and again after every allocation that
    /// exhausts the current region, so the common case (frame still
    /// available in the current region) does no scanning at all.
    fn enter_current_region(&mut self) {
        while self.region_idx < self.regions.len() {
            let r = &self.regions[self.region_idx];
            let usable = r.kind == bootloader_api::info::MemoryRegionKind::Usable;
            if usable && self.next_addr < r.end {
                if self.next_addr < r.start {
                    self.next_addr = r.start;
                }
                return;
            }
            self.region_idx += 1;
            if self.region_idx < self.regions.len() {
                self.next_addr = self.regions[self.region_idx].start;
            }
        }
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if let Some(frame) = self.freed.pop() {
            self.allocated_total += 1;
            return Some(frame);
        }
        self.enter_current_region();
        if self.region_idx >= self.regions.len() {
            return None;
        }
        let addr = self.next_addr;
        self.next_addr += 4096;
        self.allocated_total += 1;
        Some(PhysFrame::containing_address(PhysAddr::new(addr)))
    }
}

impl FrameDeallocator<Size4KiB> for BootInfoFrameAllocator {
    /// # Safety
    /// `frame` must not still be referenced by any live page table or
    /// mapping — callers (`memory::free_address_space`) only ever pass
    /// page-table *structure* frames that a just-exited process's own
    /// deep-cloned hierarchy privately owned, never a frame that might be
    /// shared with another address space.
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame<Size4KiB>) {
        self.freed.push(frame);
    }
}

pub fn init_heap(
    mapper: &mut OffsetPageTable,
    frame_allocator: &mut BootInfoFrameAllocator,
) -> Result<(), &'static str> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or("no physical frames left for heap")?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|_| "failed to map heap page")?
                .flush();
        }
    }

    unsafe {
        ALLOCATOR.lock().init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    Ok(())
}

/// `(bytes_used, bytes_free)` on the kernel heap, for `SYS_MEMINFO`.
/// `LockedHeap` derefs to `spin::Mutex<Heap>`, so locking it here is the
/// same lock `GlobalAlloc`'s own `alloc`/`dealloc` take — safe to call
/// from anywhere that isn't already holding it (a syscall handler never
/// is).
pub fn heap_stats() -> (u64, u64) {
    let heap = ALLOCATOR.lock();
    (heap.used() as u64, heap.free() as u64)
}
