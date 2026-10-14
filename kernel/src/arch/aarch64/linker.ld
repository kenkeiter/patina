/*
 * patina aarch64 kernel image layout.
 *
 * Linked at KERNEL_VA_BASE; loaded by the VMM at any 2 MiB aligned physical
 * address (Linux arm64 Image protocol, text_offset = 0). Boot code only uses
 * PC-relative addressing until the MMU maps KERNEL_VA_BASE -> load address.
 *
 * Every output section boundary the page tables care about is 4 KiB aligned
 * so the final kernel mapping can use per-section permissions:
 *   [__text_start,   __text_end)    R-X
 *   [__rodata_start, __rodata_end)  R--
 *   [__data_start,   __kernel_end)  RW-  (minus the stack guard page)
 */

ENTRY(_start)

KERNEL_VA_BASE = 0xffffffff80000000;

SECTIONS
{
    . = KERNEL_VA_BASE;
    __kernel_start = .;

    .text : {
        __text_start = .;
        KEEP(*(.text.head))      /* Image header: must be at offset 0 */
        *(.text.boot)
        *(.text .text.*)
    }
    . = ALIGN(4096);
    __text_end = .;

    .rodata : {
        __rodata_start = .;
        *(.rodata .rodata.*)
    }
    .eh_frame_hdr : { *(.eh_frame_hdr) }
    .eh_frame : { *(.eh_frame) }
    . = ALIGN(4096);
    __rodata_end = .;

    .data : {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss (NOLOAD) : ALIGN(4096) {
        __bss_start = .;
        *(.bss.boot_pt)          /* boot page-table pool (page aligned) */
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4096);
        __stack_guard = .;       /* left unmapped in the final tables */
        . += 4096;
        __boot_stack_bottom = .;
        . += 0x10000;            /* 64 KiB boot stack */
        __boot_stack_top = .;
        __bss_end = .;
    }
    . = ALIGN(4096);
    __kernel_end = .;

    /* Includes bss: the VMM must not place the DTB/initrd over it. */
    __image_size = __kernel_end - __kernel_start;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.interp)
        *(.dynamic .dynsym .dynstr .hash .gnu.hash)
    }
}
