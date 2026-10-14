//! x86_64 (long mode, ring 0) support. See `arch/mod.rs` for the interface.
//!
//! Boot protocol: PVH (`XEN_ELFNOTE_PHYS32_ENTRY`), entered in 32-bit
//! protected mode with paging off and `ebx` = physical address of
//! `hvm_start_info`. The image is a static ELF linked at
//! `KERNEL_VA_BASE + 0x20_0000` and loaded at physical `0x20_0000`; the
//! kernel-image window maps `KERNEL_VA_BASE + pa -> pa` (Linux-style
//! "-2 GiB" mapping), so `kernel_load_phys()` is the physical address of
//! `__kernel_start`, not of `KERNEL_VA_BASE`.
//!
//! Paging: 4-level, 48-bit VA, NX enabled (EFER.NXE), CR0.WP set, CR4.PGE
//! set (kernel mappings are global). PML4[256..] is the direct map
//! (`PHYS_OFFSET`), PML4[511] holds the kernel image.
//!
//! GDT selectors (see `gdt.rs`): 0x08 kernel code64, 0x10 kernel data,
//! 0x18 user code32 (placeholder for SYSRET), 0x20 user data, 0x28 user
//! code64, 0x30 TSS (16-byte descriptor). IST1 = double-fault stack.
//!
//! User mode (Phase 1): per-CPU block via GS (`percpu.rs`; `swapgs` on every
//! ring-3 entry and exit), SYSCALL/SYSRET and exception/IRQ entry sharing one
//! `ctx::TrapFrame` layout (`entry.rs`, `exceptions.rs`), the LAPIC timer
//! with PIT calibration (`lapic.rs`, `timer.rs`), user PML4s (`uspace.rs`).

pub mod boot;
pub mod console;
pub mod cpu;
pub mod ctx;
mod entry;
mod exceptions;
mod gdt;
pub mod irq;
mod lapic;
pub mod paging;
mod percpu;
mod pic;
pub mod port;
pub mod power;
pub mod sigframe;
mod start_info;
pub mod timer;
pub mod uspace;

pub const NAME: &str = "x86_64";

/// Base of the direct physical map.
pub const PHYS_OFFSET: usize = 0xffff_8000_0000_0000;
/// Base of the kernel-image window: `KERNEL_VA_BASE + pa` maps `pa` for the
/// image (the image itself starts at `KERNEL_VA_BASE + KERNEL_LOAD_PHYS`).
pub const KERNEL_VA_BASE: usize = 0xffff_ffff_8000_0000;
/// Fixed physical load address of the image (ELF `p_paddr` of the first
/// PT_LOAD segment; PVH loaders honour it).
pub const KERNEL_LOAD_PHYS: usize = 0x20_0000;

/// Physical address the VMM loaded the image at (= PA of `__kernel_start`).
pub fn kernel_load_phys() -> usize {
    KERNEL_LOAD_PHYS
}

/// Physical address of a kernel-image virtual address.
#[inline]
pub(crate) fn image_virt_to_phys(va: usize) -> usize {
    va - KERNEL_VA_BASE
}

/// Size of the kernel image in memory, including bss and the boot stack.
pub(crate) fn kernel_size() -> usize {
    layout::kernel_end() - layout::kernel_start()
}

/// Linker-script symbols. Only their addresses are meaningful.
pub(crate) mod layout {
    unsafe extern "C" {
        pub static __kernel_start: u8;
        pub static __text_start: u8;
        pub static __text_end: u8;
        pub static __rodata_start: u8;
        pub static __rodata_end: u8;
        pub static __data_start: u8;
        pub static __stack_guard: u8;
        pub static __boot_stack_bottom: u8;
        pub static __boot_stack_top: u8;
        pub static __kernel_end: u8;
    }

    macro_rules! addr_fn {
        ($($name:ident => $sym:ident),* $(,)?) => {
            $(#[inline] pub fn $name() -> usize { core::ptr::addr_of!($sym) as usize })*
        };
    }

    addr_fn! {
        kernel_start => __kernel_start,
        text_start => __text_start,
        text_end => __text_end,
        rodata_start => __rodata_start,
        rodata_end => __rodata_end,
        data_start => __data_start,
        stack_guard => __stack_guard,
        boot_stack_bottom => __boot_stack_bottom,
        boot_stack_top => __boot_stack_top,
        kernel_end => __kernel_end,
    }
}
