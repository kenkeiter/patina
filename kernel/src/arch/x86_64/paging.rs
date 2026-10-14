//! x86_64 4-level page tables (48-bit VA, 4 KiB pages, 2 MiB large pages).
//!
//! Kernel mappings live in the tables rooted at [`kernel_root`] (CR3).
//! Table frames come from a static pool in `.bss` until the frame allocator
//! is up, then from `mm::frame`. Pool tables are reached through the kernel
//! image window (`KERNEL_VA_BASE + pa`), all others through the direct map,
//! which on the boot tables covers the low 4 GiB and on the final tables all
//! of RAM.
//!
//! Phase 0 never replaces a valid entry; `map_range` fails with
//! `AlreadyMapped` instead. 1 GiB pages are not used (not every VMM's CPU
//! model advertises them).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bitflags::bitflags;

use super::{
    KERNEL_VA_BASE, PHYS_OFFSET, cpu, image_virt_to_phys, kernel_load_phys, kernel_size, layout,
};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, frame, page_align_down, page_align_up};
use crate::sync::SpinLock;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct MapFlags: u32 {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC = 1 << 2;
        /// Uncached (PCD|PWT) MMIO. Implies non-executable.
        const DEVICE = 1 << 3;
        /// Accessible from ring 3.
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

// Entry bits.
pub(super) const PTE_P: u64 = 1 << 0;
pub(super) const PTE_RW: u64 = 1 << 1;
pub(super) const PTE_US: u64 = 1 << 2;
pub(super) const PTE_PWT: u64 = 1 << 3;
pub(super) const PTE_PCD: u64 = 1 << 4;
pub(super) const PTE_A: u64 = 1 << 5;
pub(super) const PTE_D: u64 = 1 << 6;
pub(super) const PTE_PS: u64 = 1 << 7;
pub(super) const PTE_G: u64 = 1 << 8;
pub(super) const PTE_NX: u64 = 1 << 63;
pub(super) const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const ENTRIES: usize = 512;

/// Leaf attributes (without P, PS and the address).
pub(super) fn leaf_attrs(flags: MapFlags) -> u64 {
    let mut d = PTE_A | PTE_D;
    if flags.contains(MapFlags::WRITE) {
        d |= PTE_RW;
    }
    if flags.contains(MapFlags::DEVICE) {
        d |= PTE_PCD | PTE_PWT;
    }
    if flags.contains(MapFlags::USER) {
        d |= PTE_US;
    } else {
        d |= PTE_G;
    }
    if !flags.contains(MapFlags::EXEC) || flags.contains(MapFlags::DEVICE) {
        d |= PTE_NX;
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
fn alloc_table() -> Option<PhysAddr> {
    if frame::is_initialized() {
        return frame::alloc_frames_zeroed(0);
    }
    let i = PT_POOL_NEXT.fetch_add(1, Ordering::Relaxed);
    if i >= POOL_PAGES {
        return None;
    }
    let va = PT_POOL.0.get() as usize + i * PAGE_SIZE;
    // .bss is zeroed by the boot code.
    Some(PhysAddr(image_virt_to_phys(va)))
}

/// Pool pages consumed so far (diagnostics).
pub fn pool_used() -> usize {
    PT_POOL_NEXT.load(Ordering::Relaxed).min(POOL_PAGES)
}

/// Virtual address through which the table at `pa` can be accessed.
fn table_ptr(pa: u64) -> *mut u64 {
    let pa = pa as usize;
    let load = kernel_load_phys();
    if pa >= load && pa < load + kernel_size() {
        (KERNEL_VA_BASE + pa) as *mut u64
    } else {
        (PHYS_OFFSET + pa) as *mut u64
    }
}

// ---------------------------------------------------------------------------
// Generic walker.

/// Map `[va, va+len)` to `[pa, pa+len)` in the tables rooted at `root`, using
/// 2 MiB pages where alignment and length allow.
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
    let table_attrs = PTE_P
        | PTE_RW
        | if flags.contains(MapFlags::USER) {
            PTE_US
        } else {
            0
        };
    let mut off = 0;
    while off < len {
        let (v, p, remain) = (va + off, pa + off, len - off);
        let mut table = root.0 as u64;
        let mut level = 0; // 0 = PML4, 1 = PDPT, 2 = PD, 3 = PT
        loop {
            let shift = 39 - 9 * level;
            let idx = (v >> shift) & (ENTRIES - 1);
            // SAFETY: `table` is a live page-table frame reachable via table_ptr.
            let slot = unsafe { table_ptr(table).add(idx) };
            let entry = unsafe { slot.read_volatile() };
            let size = 1usize << shift;
            let large = level == 2 && v % size == 0 && p % size == 0 && remain >= size;
            if level == 3 || large {
                let desc = (p as u64 & ADDR_MASK) | attrs | PTE_P | if large { PTE_PS } else { 0 };
                if entry & PTE_P != 0 {
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
            if entry & PTE_P == 0 {
                let t = alloc_table().ok_or(MapError::NoMemory)?;
                // SAFETY: as above.
                unsafe { slot.write_volatile(t.0 as u64 | table_attrs) };
                table = t.0 as u64;
            } else if entry & PTE_PS != 0 {
                // A large page covers v but we need finer granularity.
                return Err(MapError::AlreadyMapped(VirtAddr(v)));
            } else {
                if entry & table_attrs != table_attrs {
                    // SAFETY: as above; only widens permissions of a table entry.
                    unsafe { slot.write_volatile(entry | table_attrs) };
                }
                table = entry & ADDR_MASK;
            }
            level += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Kernel tables.

static KERNEL_ROOT: AtomicUsize = AtomicUsize::new(0);
static FINAL_TABLES: AtomicBool = AtomicBool::new(false);

/// Physical address of the live kernel PML4.
pub fn kernel_root() -> PhysAddr {
    PhysAddr(KERNEL_ROOT.load(Ordering::Relaxed))
}

/// Map `[va, va+len)` -> `[pa, pa+len)` in the kernel tables.
pub fn map_range(va: VirtAddr, pa: PhysAddr, len: usize, flags: MapFlags) -> Result<(), MapError> {
    map_in(kernel_root(), va.0, pa.0, len, flags)
}

const MAX_DEVICE_MAPS: usize = 32;
/// Device mappings made so far, replayed into the final tables at boot.
static DEVICE_MAPS: SpinLock<([(usize, usize); MAX_DEVICE_MAPS], usize)> =
    SpinLock::new(([(0, 0); MAX_DEVICE_MAPS], 0));

/// Map MMIO `[pa, pa+len)` at `PHYS_OFFSET + pa`, uncached, RW, NX. Returns
/// the virtual address corresponding to `pa`.
///
/// Before the final tables are live the boot tables already direct-map the
/// low 4 GiB with 2 MiB pages (cacheable); requests there are only recorded
/// and get their uncached mapping when the final tables are built.
pub fn map_device(pa: PhysAddr, len: usize) -> VirtAddr {
    let start = page_align_down(pa.0);
    let end = page_align_up(pa.0 + len.max(1));
    let flags = MapFlags::READ | MapFlags::WRITE | MapFlags::DEVICE;
    let boot = !FINAL_TABLES.load(Ordering::Acquire);
    if !(boot && end <= 1 << 32)
        && let Err(e) = map_in(
            kernel_root(),
            PHYS_OFFSET + start,
            start,
            end - start,
            flags,
        )
    {
        panic!("map_device({:#x}, {:#x}): {:?}", pa.0, len, e);
    }
    let mut maps = DEVICE_MAPS.lock();
    let n = maps.1;
    if n < MAX_DEVICE_MAPS {
        maps.0[n] = (start, end);
        maps.1 = n + 1;
    } else if boot {
        panic!("map_device: too many boot-time device mappings");
    }
    VirtAddr(PHYS_OFFSET + pa.0)
}

/// Flush all TLB entries on this CPU, including global ones (CR4.PGE toggle).
/// Phase 0 is single-CPU; Phase 2 adds shootdown IPIs.
pub fn flush_tlb_all() {
    let cr4 = cpu::read_cr4();
    // SAFETY: toggling PGE flushes the TLB; nothing else changes.
    unsafe {
        cpu::write_cr4(cr4 & !(1 << 7));
        cpu::write_cr4(cr4);
    }
}

/// Adopt the boot tables built by the entry code as the kernel root.
pub(super) fn adopt_boot_tables() {
    KERNEL_ROOT.store((cpu::read_cr3() & ADDR_MASK) as usize, Ordering::Relaxed);
}

/// Build the final kernel tables and switch CR3 to them. This drops the boot
/// identity map of the low 4 GiB.
///
/// * Kernel image at `KERNEL_VA_BASE + pa` with 4 KiB pages: `.text` R-X,
///   `.rodata` R-- (NX), `.data`/`.bss`/stack RW- (NX); the guard page below
///   the boot stack is left unmapped.
/// * Direct map of every range in `ram` (RW, NX, write-back; 2 MiB pages
///   where aligned). `ram` must be sorted and non-overlapping.
/// * Every device mapping recorded by [`map_device`].
pub(super) fn init_kernel_tables(ram: &[(usize, usize)]) -> Result<(), MapError> {
    let root = alloc_table().ok_or(MapError::NoMemory)?;
    let k = image_virt_to_phys;
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
        map_in(root, PHYS_OFFSET + s, s, e - s, r | w)?;
    }
    {
        let maps = DEVICE_MAPS.lock();
        for &(s, e) in &maps.0[..maps.1] {
            map_in(root, PHYS_OFFSET + s, s, e - s, r | w | MapFlags::DEVICE)?;
        }
    }

    // SAFETY: the new tables map everything the kernel is currently using
    // (image, stack, start_info and boot data via the RAM direct map) at the
    // same VAs, so switching while running is safe.
    unsafe { cpu::write_cr3(root.0 as u64) };
    KERNEL_ROOT.store(root.0, Ordering::Relaxed);
    FINAL_TABLES.store(true, Ordering::Release);
    flush_tlb_all();
    Ok(())
}

/// Read a PML4/PDPT/PD/PT entry chain for `va` (diagnostics; Phase 1 uses it
/// for fault decoding). Returns the leaf entry or `None` if unmapped.
pub fn translate(va: usize) -> Option<u64> {
    let mut table = kernel_root().0 as u64;
    for level in 0..4 {
        let shift = 39 - 9 * level;
        let idx = (va >> shift) & (ENTRIES - 1);
        // SAFETY: live table.
        let e = unsafe { table_ptr(table).add(idx).read_volatile() };
        if e & PTE_P == 0 {
            return None;
        }
        if level == 3 || e & PTE_PS != 0 {
            return Some(e);
        }
        table = e & ADDR_MASK;
    }
    None
}

/// Pre-populate all 256 kernel-half PML4 slots so later kernel mappings never
/// touch the PML4 (user address spaces can share them by copying the upper
/// half). Needs the frame allocator.
pub(super) fn populate_kernel_pml4() -> Result<(), MapError> {
    let pml4 = table_ptr(kernel_root().0 as u64);
    for i in 256..ENTRIES {
        // SAFETY: live root table.
        let e = unsafe { pml4.add(i).read_volatile() };
        if e & PTE_P == 0 {
            let t = alloc_table().ok_or(MapError::NoMemory)?;
            // SAFETY: as above.
            unsafe { pml4.add(i).write_volatile(t.0 as u64 | PTE_P | PTE_RW) };
        }
    }
    Ok(())
}
