//! Memory management: address types, the direct physical map, the frame
//! allocator and the kernel heap.
//!
//! Layout (same numbers on every arch):
//! * `PHYS_OFFSET` (0xffff_8000_0000_0000): direct map, `va = PHYS_OFFSET + pa`.
//!   Covers RAM (Normal memory); MMIO is mapped on demand at the same offset
//!   with device attributes by `arch::paging::map_device`.
//! * `KERNEL_VA_BASE` (0xffff_ffff_8000_0000): the kernel image is linked here.

pub mod frame;
pub mod heap;
pub mod vm;

use core::fmt;

pub use crate::arch::{KERNEL_VA_BASE, PHYS_OFFSET};

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct PhysAddr(pub usize);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct VirtAddr(pub usize);

impl PhysAddr {
    pub const fn as_usize(self) -> usize {
        self.0
    }
    pub const fn is_page_aligned(self) -> bool {
        self.0 & (PAGE_SIZE - 1) == 0
    }
}

impl VirtAddr {
    pub const fn as_usize(self) -> usize {
        self.0
    }
    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }
    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PA({:#x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VA({:#x})", self.0)
    }
}

/// Direct-map virtual address of a physical address.
#[inline]
pub const fn phys_to_virt(pa: PhysAddr) -> VirtAddr {
    VirtAddr(PHYS_OFFSET + pa.0)
}

/// Physical address of a direct-map virtual address. Returns `None` for
/// addresses outside the direct map (e.g. kernel-image addresses; use
/// [`kernel_virt_to_phys`] for those).
#[inline]
pub const fn virt_to_phys(va: VirtAddr) -> Option<PhysAddr> {
    if va.0 >= PHYS_OFFSET && va.0 < KERNEL_VA_BASE {
        Some(PhysAddr(va.0 - PHYS_OFFSET))
    } else {
        None
    }
}

/// Physical address of a kernel-image virtual address (statics, the boot
/// stack, page-table pool).
pub fn kernel_virt_to_phys(va: VirtAddr) -> PhysAddr {
    debug_assert!(va.0 >= KERNEL_VA_BASE);
    // aarch64: KERNEL_VA_BASE maps the load address. x86_64: the image is
    // linked at KERNEL_VA_BASE + load_phys (KERNEL_VA_BASE maps phys 0).
    #[cfg(target_arch = "x86_64")]
    return PhysAddr(va.0 - KERNEL_VA_BASE);
    #[cfg(not(target_arch = "x86_64"))]
    PhysAddr(va.0 - KERNEL_VA_BASE + crate::arch::kernel_load_phys())
}

pub const fn page_align_down(x: usize) -> usize {
    x & !(PAGE_SIZE - 1)
}

pub const fn page_align_up(x: usize) -> usize {
    (x + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}
