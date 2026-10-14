//! aarch64 (ARMv8-A, EL1) support. See `arch/mod.rs` for the interface.
//!
//! Translation regime: 4 KiB granule, 48-bit VA (T0SZ = T1SZ = 16).
//! TTBR1 holds the kernel tables (higher-half image + direct map); TTBR0 is
//! reserved for user mode and points at an empty table after boot.
//! MAIR index 0 = Device-nGnRE, index 1 = Normal write-back cacheable.

pub mod boot;
pub mod console;
pub mod cpu;
mod dtb;
mod exceptions;
pub mod irq;
pub mod paging;
pub mod power;

use core::sync::atomic::{AtomicUsize, Ordering};

pub const NAME: &str = "aarch64";

/// Base of the direct physical map.
pub const PHYS_OFFSET: usize = 0xffff_8000_0000_0000;
/// Link address of the kernel image.
pub const KERNEL_VA_BASE: usize = 0xffff_ffff_8000_0000;

static LOAD_PHYS: AtomicUsize = AtomicUsize::new(0);
static KERNEL_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Physical address the VMM loaded the image at (= PA of `KERNEL_VA_BASE`).
pub fn kernel_load_phys() -> usize {
    LOAD_PHYS.load(Ordering::Relaxed)
}

/// Size of the kernel image in memory, including bss and the boot stack.
pub(crate) fn kernel_size() -> usize {
    KERNEL_SIZE.load(Ordering::Relaxed)
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
