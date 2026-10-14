//! Memory-management ABI: `mmap`/`mprotect`/`madvise`/`mremap` flags.
//! Common to x86_64 and aarch64 except `MAP_32BIT`/`MAP_ABOVE4G` (x86_64)
//! and `PROT_BTI`/`PROT_MTE` (aarch64), which live in the arch modules.

use bitflags::bitflags;

pub const PAGE_SIZE: usize = 4096;
pub const PAGE_SHIFT: u32 = 12;

bitflags! {
    /// `prot` for `mmap`/`mprotect`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ProtFlags: u32 {
        const READ = 0x1;
        const WRITE = 0x2;
        const EXEC = 0x4;
        const SEM = 0x8;
        const GROWSDOWN = 0x0100_0000;
        const GROWSUP = 0x0200_0000;
    }
}

pub const PROT_NONE: u32 = 0;

bitflags! {
    /// `flags` for `mmap` (arch-neutral subset).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct MapFlags: u32 {
        const SHARED = 0x01;
        const PRIVATE = 0x02;
        const SHARED_VALIDATE = 0x03;
        const FIXED = 0x10;
        const ANONYMOUS = 0x20;
        const GROWSDOWN = 0x0100;
        const DENYWRITE = 0x0800;
        const EXECUTABLE = 0x1000;
        const LOCKED = 0x2000;
        const NORESERVE = 0x4000;
        const POPULATE = 0x8000;
        const NONBLOCK = 0x1_0000;
        const STACK = 0x2_0000;
        const HUGETLB = 0x4_0000;
        const SYNC = 0x8_0000;
        const FIXED_NOREPLACE = 0x10_0000;
        const UNINITIALIZED = 0x400_0000;
    }
}

/// Mask of the sharing type bits in `mmap` flags.
pub const MAP_TYPE: u32 = 0x0f;
/// `MAP_HUGETLB` page-size encoding shift.
pub const MAP_HUGE_SHIFT: u32 = 26;
pub const MAP_FAILED: u64 = u64::MAX;

pub const MADV_NORMAL: i32 = 0;
pub const MADV_RANDOM: i32 = 1;
pub const MADV_SEQUENTIAL: i32 = 2;
pub const MADV_WILLNEED: i32 = 3;
pub const MADV_DONTNEED: i32 = 4;
pub const MADV_FREE: i32 = 8;
pub const MADV_REMOVE: i32 = 9;
pub const MADV_DONTFORK: i32 = 10;
pub const MADV_DOFORK: i32 = 11;
pub const MADV_MERGEABLE: i32 = 12;
pub const MADV_UNMERGEABLE: i32 = 13;
pub const MADV_HUGEPAGE: i32 = 14;
pub const MADV_NOHUGEPAGE: i32 = 15;
pub const MADV_DONTDUMP: i32 = 16;
pub const MADV_DODUMP: i32 = 17;
pub const MADV_WIPEONFORK: i32 = 18;
pub const MADV_KEEPONFORK: i32 = 19;
pub const MADV_COLD: i32 = 20;
pub const MADV_PAGEOUT: i32 = 21;
pub const MADV_POPULATE_READ: i32 = 22;
pub const MADV_POPULATE_WRITE: i32 = 23;
pub const MADV_DONTNEED_LOCKED: i32 = 24;
pub const MADV_COLLAPSE: i32 = 25;
pub const MADV_HWPOISON: i32 = 100;
pub const MADV_SOFT_OFFLINE: i32 = 101;

pub const MREMAP_MAYMOVE: u32 = 1;
pub const MREMAP_FIXED: u32 = 2;
pub const MREMAP_DONTUNMAP: u32 = 4;

pub const MS_ASYNC: i32 = 1;
pub const MS_INVALIDATE: i32 = 2;
pub const MS_SYNC: i32 = 4;

/// `mlock2` flags.
pub const MLOCK_ONFAULT: u32 = 1;
/// `mlockall` flags.
pub const MCL_CURRENT: i32 = 1;
pub const MCL_FUTURE: i32 = 2;
pub const MCL_ONFAULT: i32 = 4;

/// Round up to a page boundary (saturating at the top of the address space
/// is the caller's problem: returns `None` on overflow).
pub const fn page_align_up(x: u64) -> Option<u64> {
    match x.checked_add(PAGE_SIZE as u64 - 1) {
        Some(v) => Some(v & !(PAGE_SIZE as u64 - 1)),
        None => None,
    }
}

pub const fn page_align_down(x: u64) -> u64 {
    x & !(PAGE_SIZE as u64 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align() {
        assert_eq!(page_align_up(0), Some(0));
        assert_eq!(page_align_up(1), Some(4096));
        assert_eq!(page_align_up(4096), Some(4096));
        assert_eq!(page_align_up(u64::MAX), None);
        assert_eq!(page_align_down(8191), 4096);
    }

    #[test]
    fn go_mmap_flags() {
        // Go's sysReserve: mmap(v, n, PROT_NONE, MAP_ANON|MAP_PRIVATE, -1, 0).
        let f = MapFlags::from_bits_retain(0x22);
        assert!(f.contains(MapFlags::ANONYMOUS | MapFlags::PRIVATE));
        assert_eq!(f.bits() & MAP_TYPE, MapFlags::PRIVATE.bits());
    }
}
