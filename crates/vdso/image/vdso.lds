/*
 * Link script for the patina vDSO (decision D75). One read-exec PT_LOAD at
 * address 0 holding the ELF and program headers, the dynamic linking
 * tables and the code, plus PT_DYNAMIC. The vvar page follows the image:
 * the kernel maps it at the page-aligned end of the PT_LOAD.
 */
PHDRS
{
    text    PT_LOAD    FILEHDR PHDRS FLAGS(5);  /* R+X */
    dynamic PT_DYNAMIC FLAGS(4);                /* R */
}

SECTIONS
{
    . = SIZEOF_HEADERS;

    .hash           : { *(.hash) }              :text
    .gnu.hash       : { *(.gnu.hash) }
    .dynsym         : { *(.dynsym) }
    .dynstr         : { *(.dynstr) }
    .gnu.version    : { *(.gnu.version) }
    .gnu.version_d  : { *(.gnu.version_d) }
    .gnu.version_r  : { *(.gnu.version_r) }

    .dynamic        : { *(.dynamic) }           :text :dynamic

    .rodata         : { *(.rodata .rodata.*) }  :text
    .text           : { *(.text .text.*) }      :text

    /* No writable data, no GOT/PLT: the host tests reject the image if any
     * of these is non-empty. */
    .data           : { *(.data .data.* .bss .bss.* .got .got.* .plt .plt.*) }

    /DISCARD/       : { *(.eh_frame .eh_frame_hdr .note.* .comment) }

    . = ALIGN(4096);
    __vvar_page = .;
}
