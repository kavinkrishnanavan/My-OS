/* Links this as a plain static ELF64 executable, entry point _start,
 * base address comfortably clear of the kernel's own low-memory
 * footprint. (A higher address, past what 32-bit-signed relocations —
 * this target's default code model — can reach, would need a
 * "large" code model, which needs `core`/`alloc` themselves rebuilt to
 * match via `-Z build-std` — nightly-only, more than this is worth.
 * `memory::new_address_space`, kernel-side, now deep-clones the whole
 * page-table *structure* instead of aliasing shared lower levels, which
 * is what actually makes a low address like this one safe to reuse
 * across independent processes.) */
ENTRY(_start)

SECTIONS
{
    . = 0x01000000;

    .text : {
        *(.text .text.*)
    }

    .rodata : {
        *(.rodata .rodata.*)
    }

    .data : {
        *(.data .data.*)
    }

    .bss : {
        *(.bss .bss.* COMMON)
    }

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.note.*)
    }
}
