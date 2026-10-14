/*
 * patina x86_64 kernel image layout (PVH ELF).
 *
 * Linked at KERNEL_VA_BASE + 0x200000 (virtual), loaded at physical 0x200000:
 * every output section has AT(ADDR - KERNEL_VA_BASE), so p_paddr is the
 * physical load address (PVH loaders in QEMU, Firecracker and Cloud
 * Hypervisor use p_paddr) and p_vaddr the high VA. The PVH note
 * (.note.Xen, XEN_ELFNOTE_PHYS32_ENTRY) is in its own PT_NOTE segment.
 *
 * Every output section boundary the page tables care about is 4 KiB aligned
 * so the final kernel mapping can use per-section permissions:
 *   [__text_start,   __text_end)    R-X
 *   [__rodata_start, __rodata_end)  R--
 *   [__data_start,   __kernel_end)  RW-  (minus the stack guard page)
 */

ENTRY(_start32)

KERNEL_VA_BASE = 0xffffffff80000000;
KERNEL_LOAD_PHYS = 0x200000;

PHDRS
{
    text   PT_LOAD FLAGS(5);   /* R X */
    rodata PT_LOAD FLAGS(4);   /* R   */
    data   PT_LOAD FLAGS(6);   /* R W */
    note   PT_NOTE FLAGS(4);
}

SECTIONS
{
    . = KERNEL_VA_BASE + KERNEL_LOAD_PHYS;
    __kernel_start = .;

    .text : AT(ADDR(.text) - KERNEL_VA_BASE) {
        __text_start = .;
        KEEP(*(.text.boot32))    /* PVH 32-bit entry: first thing in the image */
        *(.text.boot)
        *(.text .text.*)
    } :text
    . = ALIGN(4096);
    __text_end = .;

    .rodata : AT(ADDR(.rodata) - KERNEL_VA_BASE) {
        __rodata_start = .;
        *(.rodata .rodata.*)
    } :rodata
    .note.Xen : AT(ADDR(.note.Xen) - KERNEL_VA_BASE) {
        KEEP(*(.note.Xen))
    } :rodata :note
    .eh_frame_hdr : AT(ADDR(.eh_frame_hdr) - KERNEL_VA_BASE) { *(.eh_frame_hdr) } :rodata
    .eh_frame : AT(ADDR(.eh_frame) - KERNEL_VA_BASE) { *(.eh_frame) } :rodata
    . = ALIGN(4096);
    __rodata_end = .;

    .data : AT(ADDR(.data) - KERNEL_VA_BASE) {
        __data_start = .;
        *(.data.boot)
        *(.data .data.*)
        *(.got .got.*)
    } :data

    .bss (NOLOAD) : AT(ADDR(.bss) - KERNEL_VA_BASE) ALIGN(4096) {
        __bss_start = .;
        *(.bss.boot_pt)          /* boot page tables (page aligned) */
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4096);
        __stack_guard = .;       /* left unmapped in the final tables */
        . += 4096;
        __boot_stack_bottom = .;
        . += 0x10000;            /* 64 KiB boot stack */
        __boot_stack_top = .;
        __bss_end = .;
    } :data
    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note.GNU-stack)
        *(.note.gnu.*)
        *(.interp)
        *(.dynamic .dynsym .dynstr .hash .gnu.hash)
    }
}
