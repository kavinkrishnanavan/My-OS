//! Anonymous memory mapping: a second bump-pointer arena, independent of
//! `task::thread`'s `sbrk` heap, for callers that want a fresh page range
//! rather than heap growth (e.g. a future `SYS_MMAP`/`SYS_MUNMAP` pair).
//!
//! Same underlying trick as `sbrk` — `memory::map_page_in` against
//! whichever address space's `OffsetPageTable` the caller hands in — just
//! with its own cursor and base address, so the two arenas can never
//! collide with each other even though both grow upward from a fixed
//! start inside the same address space.

use crate::memory;
use alloc::vec::Vec;
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::structures::paging::{Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

/// Arena base for anonymous mappings. Clear of every other fixed range
/// this kernel already hands out: the ELF load address (`0x0100_0000`,
/// `elf.rs`), the kernel heap (`0x4444_4444_0000`, `allocator.rs`), the
/// `sbrk` heap (`0x2000_0000_0000`, `task::thread::HEAP_BASE`), and the
/// isolated-thread code/stack pair (`0x6666_6666_0000`/`0x6666_6667_0000`,
/// `task::thread::ISOLATED_USER_CODE_ADDR`/`ISOLATED_USER_STACK_ADDR`) —
/// all read directly from their source before picking this, not guessed.
/// `0x6000_0000_0000` sits well below the isolated-thread pair and has
/// over 100 TiB of headroom below it before reaching them, so even an
/// arena that grew unreasonably large would never walk into that range.
pub const MMAP_BASE: u64 = 0x6000_0000_0000;

const PAGE_SIZE: u64 = 4096;

/// `EFER.NXE` (bit 11) must be set before the page-table `NO_EXECUTE` bit
/// means anything — with it clear, that bit is architecturally reserved,
/// and setting it on any entry faults (reserved-bit violation) the moment
/// the entry is used, not merely "ignored". Nothing else in this kernel
/// enables it (`elf.rs` never sets `NO_EXECUTE` at all, so it never had to)
/// — `map_anon` enables it lazily, once, the first time it's needed, since
/// this is the first code path in the kernel that actually depends on the
/// bit doing something.
fn ensure_nxe_enabled() {
    use spin::Once;
    static NXE: Once<()> = Once::new();
    NXE.call_once(|| unsafe {
        Efer::update(|flags| *flags |= EferFlags::NO_EXECUTE_ENABLE);
    });
}

/// Maps `bytes` (rounded up to whole 4 KiB pages) of fresh, zeroed
/// anonymous memory into `mapper`'s address space, starting at whatever
/// `*next_addr` currently is, and advances `*next_addr` past it. The
/// cursor is caller-owned (one per thread/address space, mirroring
/// `Thread::heap_next`) rather than a global here, so two independent
/// address spaces never contend over it. Returns the mapped region's
/// starting virtual address plus the exact physical frames backing it,
/// or `Err` if the frame allocator is exhausted (same failure mode as
/// `memory::map_page_in`/`sbrk`). The frames are the caller's to track
/// (mirroring `sbrk`'s own local `new_frames` collection in
/// `task::thread`) — typically pushed onto `Thread::owned_frames` so a
/// process that exits without ever calling `unmap_anon` still gets this
/// memory reclaimed, the same way `sbrk` growth already is.
///
/// Freshly allocated frames are *not* zeroed by the frame allocator
/// itself (`allocator::BootInfoFrameAllocator` hands out raw bump-cursor
/// physical memory, and reused `freed` frames still hold whatever a
/// torn-down process's page tables last wrote) — so this explicitly
/// zeroes each page through its `phys_to_virt` alias before handing the
/// mapping back, which is the only way the "zeroed" guarantee is actually
/// true.
pub fn map_anon(
    mapper: &mut OffsetPageTable,
    next_addr: &mut u64,
    bytes: usize,
    writable: bool,
    executable: bool,
) -> Result<(u64, Vec<PhysFrame<Size4KiB>>), &'static str> {
    let pages = bytes.div_ceil(4096) as u64;
    let start = *next_addr;

    if !executable {
        ensure_nxe_enabled();
    }

    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }
    if !executable {
        flags |= PageTableFlags::NO_EXECUTE;
    }

    let mut frames = Vec::with_capacity(pages as usize);
    for i in 0..pages {
        let addr = start + i * PAGE_SIZE;
        let frame = memory::map_page_in(mapper, VirtAddr::new(addr), flags)?;
        unsafe {
            let virt = memory::phys_to_virt(frame.start_address().as_u64()) as *mut u8;
            core::ptr::write_bytes(virt, 0, PAGE_SIZE as usize);
        }
        frames.push(frame);
    }

    *next_addr = start + pages * PAGE_SIZE;
    Ok((start, frames))
}

/// Unmaps the region at `addr..addr+bytes` — but only if it's exactly the
/// most recent `map_anon` call against this same `next_addr` cursor
/// (`addr == *next_addr - round_up(bytes)`); this is a bump arena, same
/// "nothing frees except the most recent thing" limitation `sbrk` and
/// `BootInfoFrameAllocator`'s bump side already have, just made strict
/// (`Err`, no partial/best-effort unmap) rather than silently ignored,
/// since freeing the wrong pages here would be a real use-after-free for
/// whatever's still mapped above them. On success, unmaps the page table
/// entries, rewinds `*next_addr` so the reclaimed range becomes mappable
/// again, and returns the now-freed physical frames — unlike `sbrk`,
/// which never frees at all. Deliberately does *not* call
/// `memory::free_frames` itself: those same frames are also sitting in
/// `Thread::owned_frames` (see `map_anon`'s doc comment), so the caller
/// must remove them from there *and then* free them, in that order —
/// freeing here too would double-free the instant the process later
/// exits and `exit_current` frees everything left in `owned_frames`,
/// corrupting the allocator's free list with two entries for one frame.
pub fn unmap_anon(
    mapper: &mut OffsetPageTable,
    next_addr: &mut u64,
    addr: u64,
    bytes: usize,
) -> Result<Vec<PhysFrame<Size4KiB>>, &'static str> {
    let pages = bytes.div_ceil(4096) as u64;
    let region_size = pages * PAGE_SIZE;
    let expected_addr = next_addr.checked_sub(region_size).ok_or("unmap_anon: region larger than what's mapped")?;
    if addr != expected_addr {
        return Err("unmap_anon: not the most recent allocation (LIFO-only arena)");
    }

    let mut frames = Vec::with_capacity(pages as usize);
    for i in 0..pages {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr + i * PAGE_SIZE));
        let (frame, flush) = mapper.unmap(page).map_err(|_| "unmap_anon: page not mapped")?;
        flush.flush();
        frames.push(frame);
    }

    *next_addr = addr;
    Ok(frames)
}
