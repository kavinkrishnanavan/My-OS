/* Same base address as every other userland program (see
 * userland/hello/link.ld's comment for why that's safe: each one gets
 * its own isolated address space via memory::new_address_space's deep
 * page-table clone). */
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
