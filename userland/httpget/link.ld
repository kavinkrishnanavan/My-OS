/* Same simple static ELF layout as the other userland demos. */
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
