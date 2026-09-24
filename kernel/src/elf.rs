//! A minimal ELF64 loader: just enough to walk `PT_LOAD` program headers
//! and map them into a caller-supplied (not necessarily active) address
//! space — no relocations, no dynamic linking, no sections beyond what
//! the program headers already describe. `userland/hello` is exactly
//! the kind of statically-linked, non-PIE binary this handles; anything
//! needing a dynamic linker or position-independent loading is out of
//! scope (see the Milestone 4 plan).

use crate::memory;
use alloc::vec::Vec;
use x86_64::structures::paging::{Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::VirtAddr;

const EI_MAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const PT_LOAD: u32 = 1;
const PF_W: u32 = 1 << 1;

const PAGE_SIZE: u64 = 4096;

/// Parses `elf_bytes` and maps every `PT_LOAD` segment into `mapper`,
/// copying its file contents in and zero-filling the rest (bss) via the
/// `phys_to_virt` alias — the same not-yet-active-address-space
/// technique `task::thread::spawn_isolated_user` already uses, since
/// `mapper` generally isn't the currently loaded one. Returns the
/// entry point (`e_entry`) for the caller to build a thread around.
/// Every freshly allocated data frame (skipping pages an earlier segment
/// already mapped — see `load_segment`) is appended to `owned_frames`, so
/// the caller (`task::thread::spawn_elf`) can hand it to `finish_spawn_isolated`
/// for `memory::free_frames` to reclaim once the process exits — without
/// this, these frames would leak forever the same way page-table
/// structure frames did before `memory::free_address_space` existed.
pub fn load(mapper: &mut OffsetPageTable, elf_bytes: &[u8], owned_frames: &mut Vec<PhysFrame<Size4KiB>>) -> Result<u64, &'static str> {
    if elf_bytes.len() < 64 {
        return Err("ELF file too short for a header");
    }
    if elf_bytes[0..4] != EI_MAG {
        return Err("not an ELF file (bad magic)");
    }
    if elf_bytes[4] != ELFCLASS64 {
        return Err("not a 64-bit ELF file");
    }
    if elf_bytes[5] != ELFDATA2LSB {
        return Err("not a little-endian ELF file");
    }

    let e_entry = read_u64(elf_bytes, 24)?;
    let e_phoff = read_u64(elf_bytes, 32)? as usize;
    let e_phentsize = read_u16(elf_bytes, 54)? as usize;
    let e_phnum = read_u16(elf_bytes, 56)? as usize;

    if e_phentsize < 56 {
        return Err("program header entry too small");
    }

    for i in 0..e_phnum {
        let off = e_phoff + i * e_phentsize;
        if off + 56 > elf_bytes.len() {
            return Err("program header table runs past end of file");
        }

        let p_type = read_u32(elf_bytes, off)?;
        if p_type != PT_LOAD {
            continue;
        }
        let p_flags = read_u32(elf_bytes, off + 4)?;
        let p_offset = read_u64(elf_bytes, off + 8)? as usize;
        let p_vaddr = read_u64(elf_bytes, off + 16)?;
        let p_filesz = read_u64(elf_bytes, off + 32)? as usize;
        let p_memsz = read_u64(elf_bytes, off + 40)? as usize;

        if p_offset + p_filesz > elf_bytes.len() {
            return Err("segment file range runs past end of file");
        }

        load_segment(mapper, p_vaddr, p_flags, &elf_bytes[p_offset..p_offset + p_filesz], p_memsz, owned_frames)?;
    }

    Ok(e_entry)
}

/// Maps every page `[vaddr, vaddr + mem_size)` touches, then copies
/// `file_data` in at the start of that range and zero-fills the rest
/// (`mem_size - file_data.len()`, i.e. bss) — segment-at-a-time, one
/// `map_page_in` call per page, since that's the granularity the
/// existing frame allocator/mapper already work at.
fn load_segment(mapper: &mut OffsetPageTable, vaddr: u64, p_flags: u32, file_data: &[u8], mem_size: usize, owned_frames: &mut Vec<PhysFrame<Size4KiB>>) -> Result<(), &'static str> {
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | if p_flags & PF_W != 0 { PageTableFlags::WRITABLE } else { PageTableFlags::empty() };

    let start_page = vaddr & !(PAGE_SIZE - 1);
    let end = vaddr + mem_size as u64;
    let end_page = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

    let mut page_addr = start_page;
    while page_addr < end_page {
        let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(page_addr));
        match memory::map_page_in(mapper, VirtAddr::new(page_addr), flags) {
            Ok(frame) => {
                let dest = memory::phys_to_virt(frame.start_address().as_u64()) as *mut u8;
                unsafe { core::ptr::write_bytes(dest, 0, PAGE_SIZE as usize) };
                owned_frames.push(frame);
            }
            Err(_) => {
                // Already mapped by an earlier segment sharing this page
                // — small programs routinely pack .text/.rodata/.data
                // into one 4 KiB page despite each being its own PT_LOAD
                // entry (different permissions), so this isn't a real
                // error. Don't re-zero it (that would stomp on the
                // earlier segment's own copied bytes); do still honor
                // *this* segment's flags — most commonly WRITABLE, if a
                // .data segment happens to share a page with read-only
                // .text/.rodata that was mapped first.
                if flags.contains(PageTableFlags::WRITABLE) {
                    unsafe {
                        mapper
                            .update_flags(page, flags)
                            .map_err(|_| "failed to widen flags on a shared segment page")?
                            .flush();
                    }
                }
            }
        }
        page_addr += PAGE_SIZE;
    }

    // Now copy the file bytes in, byte-accurate across whatever page
    // boundaries they cross — done as a second pass so partial-page
    // segments (the common case: a segment's start/end rarely land on a
    // page boundary) don't fight the zero-fill above, which needed the
    // whole range mapped first regardless.
    let mut remaining = file_data;
    let mut dest_vaddr = vaddr;
    while !remaining.is_empty() {
        let page_base = dest_vaddr & !(PAGE_SIZE - 1);
        let page_offset = (dest_vaddr - page_base) as usize;
        let n = remaining.len().min(PAGE_SIZE as usize - page_offset);

        let frame: x86_64::structures::paging::PhysFrame<Size4KiB> = mapper
            .translate_page(Page::containing_address(VirtAddr::new(dest_vaddr)))
            .map_err(|_| "segment page vanished between mapping and copy")?;
        let dest = (memory::phys_to_virt(frame.start_address().as_u64()) + page_offset as u64) as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(remaining.as_ptr(), dest, n) };

        remaining = &remaining[n..];
        dest_vaddr += n as u64;
    }

    Ok(())
}

fn read_u16(data: &[u8], off: usize) -> Result<u16, &'static str> {
    data.get(off..off + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or("read past end of ELF header")
}

fn read_u32(data: &[u8], off: usize) -> Result<u32, &'static str> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or("read past end of ELF header")
}

fn read_u64(data: &[u8], off: usize) -> Result<u64, &'static str> {
    data.get(off..off + 8)
        .map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
        .ok_or("read past end of ELF header")
}
