//! aarch64 stage-1 page tables (4 KiB granule, 4 levels, 48-bit VA).
//!
//! Kernel mappings live in the TTBR1 tables whose root is [`kernel_root`].
//! Table frames come from a static pool in `.bss` until the frame allocator
//! is up, then from `mm::frame`. Tables are reached through the kernel-image
//! mapping (pool frames) or the direct map (everything else), so the walker
//! works both on the boot tables (direct map covers only the DTB) and on the
//! final tables.
//!
//! Phase 0 never replaces a valid entry, so no break-before-make is needed;
//! `map_range` fails with `AlreadyMapped` instead. User (TTBR0) tables,
//! with per-page unmap/protect/translate, are in `uspace.rs` and share the
//! descriptor helpers below.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use bitflags::bitflags;

use super::{KERNEL_VA_BASE, PHYS_OFFSET, kernel_load_phys, kernel_size, layout};
use crate::mm::{
    PAGE_SIZE, PhysAddr, VirtAddr, frame, kernel_virt_to_phys, page_align_down, page_align_up,
};
use crate::sync::SpinLock;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MapFlags: u32 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC = 1 << 2;
        /// Device-nGnRE memory (MMIO). Implies non-executable.
        const DEVICE = 1 << 3;
        /// Accessible from EL0.
        const USER = 1 << 4;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// va, pa or len not page aligned.
    Misaligned,
    /// A different mapping already covers this address.
    AlreadyMapped(VirtAddr),
    /// No memory for a page-table frame.
    NoMemory,
}

// Descriptor bits.
pub(super) const DESC_VALID: u64 = 1 << 0;
pub(super) const DESC_TABLE: u64 = 1 << 1; // at levels 0-2: table; at level 3: page
pub(super) const ATTR_IDX_DEVICE: u64 = 0 << 2;
const ATTR_IDX_NORMAL: u64 = 1 << 2;
const AP_EL0: u64 = 1 << 6; // AP[1]
pub(super) const AP_RO: u64 = 1 << 7; // AP[2]
const SH_INNER: u64 = 3 << 8;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11;
const PXN: u64 = 1 << 53;
pub(super) const UXN: u64 = 1 << 54;
pub(super) const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

/// MAIR_EL1: Attr0 = Device-nGnRE (0x04), Attr1 = Normal WB RA/WA (0xff).
pub const MAIR_VALUE: u64 = 0xff04;

pub(super) const ENTRIES: usize = 512;

pub(super) fn leaf_attrs(flags: MapFlags) -> u64 {
    let mut d = AF;
    if flags.contains(MapFlags::DEVICE) {
        d |= ATTR_IDX_DEVICE;
    } else {
        d |= ATTR_IDX_NORMAL | SH_INNER;
    }
    if flags.contains(MapFlags::USER) {
        d |= AP_EL0 | NG;
    }
    if !flags.contains(MapFlags::WRITE) {
        d |= AP_RO;
    }
    let exec = flags.contains(MapFlags::EXEC) && !flags.contains(MapFlags::DEVICE);
    if flags.contains(MapFlags::USER) {
        // The kernel never executes user pages.
        d |= PXN;
        if !exec {
            d |= UXN;
        }
    } else {
        d |= UXN;
        if !exec {
            d |= PXN;
        }
    }
    d
}

// ---------------------------------------------------------------------------
// Page-table frame allocation.

const POOL_PAGES: usize = 64;

#[repr(C, align(4096))]
struct Pool(UnsafeCell<[[u8; PAGE_SIZE]; POOL_PAGES]>);
// SAFETY: pages are handed out exactly once via the atomic index.
unsafe impl Sync for Pool {}

static PT_POOL: Pool = Pool(UnsafeCell::new([[0; PAGE_SIZE]; POOL_PAGES]));
static PT_POOL_NEXT: AtomicUsize = AtomicUsize::new(0);

/// A zeroed frame for a page table.
pub(super) fn alloc_table() -> Option<PhysAddr> {
    if frame::is_initialized() {
        return frame::alloc_frames_zeroed(0);
    }
    let i = PT_POOL_NEXT.fetch_add(1, Ordering::Relaxed);
    if i >= POOL_PAGES {
        return None;
    }
    let va = PT_POOL.0.get() as usize + i * PAGE_SIZE;
    // .bss is zeroed by the boot code.
    Some(kernel_virt_to_phys(VirtAddr(va)))
}

/// Pool pages consumed so far (diagnostics).
pub fn pool_used() -> usize {
    PT_POOL_NEXT.load(Ordering::Relaxed).min(POOL_PAGES)
}

/// Virtual address through which the table at `pa` can be accessed.
pub(super) fn table_ptr(pa: u64) -> *mut u64 {
    let pa = pa as usize;
    let load = kernel_load_phys();
    if pa >= load && pa < load + kernel_size() {
        (KERNEL_VA_BASE + (pa - load)) as *mut u64
    } else {
        (PHYS_OFFSET + pa) as *mut u64
    }
}

// ---------------------------------------------------------------------------
// Generic walker.

/// Map `[va, va+len)` to `[pa, pa+len)` in the tables rooted at `root`,
/// using 1 GiB / 2 MiB blocks where alignment and length allow.
pub fn map_in(
    root: PhysAddr,
    va: usize,
    pa: usize,
    len: usize,
    flags: MapFlags,
) -> Result<(), MapError> {
    if !va.is_multiple_of(PAGE_SIZE)
        || !pa.is_multiple_of(PAGE_SIZE)
        || !len.is_multiple_of(PAGE_SIZE)
    {
        return Err(MapError::Misaligned);
    }
    let attrs = leaf_attrs(flags);
    let mut off = 0;
    while off < len {
        let (v, p, remain) = (va + off, pa + off, len - off);
        let mut table = root.0 as u64;
        let mut level = 0;
        loop {
            let shift = 39 - 9 * level;
            let idx = (v >> shift) & (ENTRIES - 1);
            // SAFETY: `table` is a live page-table frame reachable via table_ptr.
            let slot = unsafe { table_ptr(table).add(idx) };
            let entry = unsafe { slot.read_volatile() };
            let size = 1usize << shift;
            let leaf_ok = level == 3
                || ((level == 1 || level == 2) && v % size == 0 && p % size == 0 && remain >= size);
            if leaf_ok {
                let desc = (p as u64 & ADDR_MASK)
                    | attrs
                    | DESC_VALID
                    | if level == 3 { DESC_TABLE } else { 0 };
                if entry & DESC_VALID != 0 {
                    if entry != desc {
                        return Err(MapError::AlreadyMapped(VirtAddr(v)));
                    }
                } else {
                    // SAFETY: as above.
                    unsafe { slot.write_volatile(desc) };
                }
                off += size;
                break;
            }
            if entry & DESC_VALID == 0 {
                let t = alloc_table().ok_or(MapError::NoMemory)?;
                // SAFETY: as above. Order the zeroed table before publishing it.
                unsafe {
                    asm!("dsb ishst", options(nostack, preserves_flags));
                    slot.write_volatile(t.0 as u64 | DESC_TABLE | DESC_VALID);
                }
                table = t.0 as u64;
            } else if entry & DESC_TABLE == 0 {
                // A block covers v but we need finer granularity.
                return Err(MapError::AlreadyMapped(VirtAddr(v)));
            } else {
                table = entry & ADDR_MASK;
            }
            level += 1;
        }
    }
    // SAFETY: barriers only.
    unsafe { asm!("dsb ishst", "isb", options(nostack, preserves_flags)) };
    Ok(())
}

// ---------------------------------------------------------------------------
// Kernel tables.

static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);

/// Physical address of the live TTBR1 root table.
pub fn kernel_root() -> PhysAddr {
    PhysAddr(KERNEL_ROOT.load(Ordering::Relaxed))
}

/// Map `[va, va+len)` -> `[pa, pa+len)` in the kernel (TTBR1) tables.
pub fn map_range(va: VirtAddr, pa: PhysAddr, len: usize, flags: MapFlags) -> Result<(), MapError> {
    map_in(kernel_root(), va.0, pa.0, len, flags)
}

const MAX_DEVICE_MAPS: usize = 32;
/// Device mappings made so far, replayed into the final tables at boot.
static DEVICE_MAPS: SpinLock<([(usize, usize); MAX_DEVICE_MAPS], usize)> =
    SpinLock::new(([(0, 0); MAX_DEVICE_MAPS], 0));

/// Map MMIO `[pa, pa+len)` at `PHYS_OFFSET + pa` as Device-nGnRE, RW, XN.
/// Returns the virtual address corresponding to `pa`.
pub fn map_device(pa: PhysAddr, len: usize) -> VirtAddr {
    let start = page_align_down(pa.0);
    let end = page_align_up(pa.0 + len.max(1));
    let flags = MapFlags::READ | MapFlags::WRITE | MapFlags::DEVICE;
    if let Err(e) = map_in(
        kernel_root(),
        PHYS_OFFSET + start,
        start,
        end - start,
        flags,
    ) {
        panic!("map_device({:#x}, {:#x}): {:?}", pa.0, len, e);
    }
    let mut maps = DEVICE_MAPS.lock();
    let n = maps.1;
    if n < MAX_DEVICE_MAPS {
        maps.0[n] = (start, end);
        maps.1 = n + 1;
    }
    VirtAddr(PHYS_OFFSET + pa.0)
}

/// Invalidate all EL1 TLB entries on all CPUs in the inner-shareable domain.
pub fn flush_tlb_all() {
    // SAFETY: TLB maintenance only.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        )
    };
}

fn read_ttbr1() -> usize {
    let v: usize;
    // SAFETY: read-only.
    unsafe { asm!("mrs {}, ttbr1_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v & ADDR_MASK as usize
}

/// Adopt the boot tables built by the entry code as the kernel root.
pub(super) fn adopt_boot_tables() {
    KERNEL_ROOT.store(read_ttbr1(), Ordering::Relaxed);
}

/// Build the final kernel tables, switch TTBR1 to them and park TTBR0 on an
/// empty table.
///
/// * Kernel image at KERNEL_VA_BASE with 4 KiB pages: `.text` R-X,
///   `.rodata` R--, `.data`/`.bss`/stack RW- (XN); the stack guard page below
///   the boot stack is left unmapped.
/// * Direct map of every RAM range in `ram` (Normal, RW, XN).
/// * Every device mapping made so far via [`map_device`].
pub(super) fn init_kernel_tables(ram: &[(usize, usize)]) -> Result<(), MapError> {
    let root = alloc_table().ok_or(MapError::NoMemory)?;
    let k = |va: usize| va - KERNEL_VA_BASE + kernel_load_phys();
    let r = MapFlags::READ;
    let w = MapFlags::WRITE;
    let x = MapFlags::EXEC;
    let seg = |start: usize, end: usize, flags: MapFlags| -> Result<(), MapError> {
        map_in(root, start, k(start), end - start, flags)
    };
    seg(layout::text_start(), layout::text_end(), r | x)?;
    seg(layout::rodata_start(), layout::rodata_end(), r)?;
    seg(layout::data_start(), layout::stack_guard(), r | w)?;
    seg(layout::boot_stack_bottom(), layout::kernel_end(), r | w)?;

    for &(s, e) in ram {
        let (s, e) = (page_align_down(s), page_align_up(e));
        map_in(root, PHYS_OFFSET + s, s, e - s, r | w)?;
    }

    {
        let maps = DEVICE_MAPS.lock();
        for &(s, e) in &maps.0[..maps.1] {
            map_in(root, PHYS_OFFSET + s, s, e - s, r | w | MapFlags::DEVICE)?;
        }
    }

    let empty_user = alloc_table().ok_or(MapError::NoMemory)?;
    // SAFETY: the new tables map everything the kernel is currently using
    // (image, stack, DTB via the RAM direct map, devices) at the same VAs with
    // compatible attributes, so switching while running is safe.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr1_el1, {root}",
            "msr ttbr0_el1, {user}",
            "isb",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            root = in(reg) root.0,
            user = in(reg) empty_user.0,
            options(nostack, preserves_flags)
        );
    }
    KERNEL_ROOT.store(root.0, Ordering::Relaxed);
    Ok(())
}
