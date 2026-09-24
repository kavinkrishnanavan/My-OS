/* Same base address as userland/hello/link.ld, deliberately: each ELF
 * runs in its own isolated address space (kernel/src/task/thread.rs's
 * spawn_elf calls memory::new_address_space() per program), so reusing
 * the same virtual address across different, unrelated programs is fine
 * — exactly the same guarantee task::thread's Milestone 3 isolation demo
 * already proved for two copies of the same code. See userland/hello's
 * own link.ld for the deep-page-table-clone fix that makes a low address
 * like this one actually safe to reuse. */
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
