//! User (TTBR0) page tables: 4 KiB pages only, every leaf EL0-accessible,
//! non-global (nG) and PXN (see `paging::leaf_attrs`).
//!
//! Tables are frames from `mm::frame` (zeroed) and are always accessed
//! through the direct map. Intermediate tables are never freed (one user
//! address space per VM); `unmap` only clears the leaf and hands the frame
//! back to the caller.
//!
//! TLB maintenance: every user mapping is tagged with ASID 0 for now.
//! `unmap`/`protect` invalidate the page with `TLBI VALE1IS` (ASID 0, all
//! CPUs); `activate` writes TTBR0_EL1 and flushes the local and remote EL1
//! TLBs (`TLBI VMALLE1IS`), since without ASIDs stale entries of the previous
//! table could survive. Phase 2 can allocate an ASID per `UserTables`, put it
//! in TTBR0_EL1[63:48] (TCR_EL1.AS for 16-bit ASIDs) and in the TLBI
//! operands, and drop the full flush from `activate`.
//!
//! PROT_NONE: `map`/`protect` with none of READ/WRITE/EXEC store an
//! *invalid* descriptor that still records the frame (software bit
//! `SW_PROT_NONE`), so any access faults (`FaultKind::NotPresent` from the
//! hardware's point of view) while `translate` still reports the frame with
//! empty flags and `unmap` still returns it.
//!
//! I-cache: mapping or re-protecting a page as EXEC cleans the D-cache and
//! invalidates the I-cache for it (unless CTR_EL0.IDC/DIC say it is not
//! needed). The page's contents must therefore be final before `map` /
//! `protect` with EXEC; after writing to a frame that is already mapped
//! executable, call [`sync_icache`].

use core::arch::asm;

use super::paging::{
    ADDR_MASK, AP_RO, ATTR_IDX_DEVICE, DESC_TABLE, DESC_VALID, ENTRIES, MapError, MapFlags, UXN,
    alloc_table, leaf_attrs, table_ptr,
};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, frame, phys_to_virt};

/// Lowest address above user space (47-bit user VA, as on x86_64).
pub const USER_VA_END: usize = 0x0000_8000_0000_0000;

/// Software bit (ignored by hardware in an invalid descriptor) marking a
/// PROT_NONE page whose frame is still recorded in the descriptor.
const SW_PROT_NONE: u64 = 1 << 58;
/// AttrIndx field of a descriptor.
const ATTR_IDX_MASK: u64 = 0b111 << 2;

/// A user address space's TTBR0 tables.
pub struct UserTables {
    root: PhysAddr,
}

impl UserTables {
    /// Empty tables (one zeroed L0 page).
    pub fn new() -> Option<UserTables> {
        Some(UserTables {
            root: frame::alloc_frames_zeroed(0)?,
        })
    }

    /// Physical address of the L0 table (TTBR0_EL1 BADDR).
    pub fn root(&self) -> PhysAddr {
        self.root
    }

    /// Map one 4 KiB page at user `va` to frame `pa`. `MapFlags::USER` is
    /// implied; DEVICE is honored. `AlreadyMapped` if `va` already has a
    /// translation (including a PROT_NONE one).
    pub fn map(&mut self, va: usize, pa: PhysAddr, flags: MapFlags) -> Result<(), MapError> {
        if !va.is_multiple_of(PAGE_SIZE) || !pa.is_page_aligned() {
            return Err(MapError::Misaligned);
        }
        assert!(
            va < USER_VA_END,
            "uspace::map: {:#x} is not a user address",
            va
        );
        let slot = self.slot(va, true)?.ok_or(MapError::NoMemory)?;
        // SAFETY: `slot` points into a live table frame reached via the direct map.
        if unsafe { slot.read_volatile() } != 0 {
            return Err(MapError::AlreadyMapped(VirtAddr(va)));
        }
        let desc = encode(pa, flags);
        if flags.contains(MapFlags::EXEC) {
            sync_icache(pa);
        }
        // SAFETY: as above. Invalid -> valid needs no TLB maintenance; the
        // barriers make the descriptor visible to the walker before return.
        unsafe {
            slot.write_volatile(desc);
            asm!("dsb ishst", "isb", options(nostack, preserves_flags));
        }
        Ok(())
    }

    /// Remove the translation for `va` (TLB-invalidated on all CPUs) and
    /// return the frame that was mapped.
    pub fn unmap(&mut self, va: usize) -> Option<PhysAddr> {
        let slot = self.slot(va, false).ok()??;
        // SAFETY: live table entry.
        let old = unsafe { slot.read_volatile() };
        if old == 0 {
            return None;
        }
        // SAFETY: as above.
        unsafe { slot.write_volatile(0) };
        if old & DESC_VALID != 0 {
            tlbi_page(va);
        }
        Some(PhysAddr((old & ADDR_MASK) as usize))
    }

    /// Change the permissions of a mapped page (TLB-invalidated). Returns
    /// false if `va` has no translation.
    pub fn protect(&mut self, va: usize, flags: MapFlags) -> bool {
        let Ok(Some(slot)) = self.slot(va, false) else {
            return false;
        };
        // SAFETY: live table entry.
        let old = unsafe { slot.read_volatile() };
        if old == 0 {
            return false;
        }
        let pa = PhysAddr((old & ADDR_MASK) as usize);
        let new = encode(pa, flags);
        if new == old {
            return true;
        }
        if flags.contains(MapFlags::EXEC) {
            sync_icache(pa);
        }
        // Same output address and memory type, only permissions change: no
        // break-before-make needed (Arm ARM D8.16.1).
        // SAFETY: live table entry.
        unsafe { slot.write_volatile(new) };
        if old & DESC_VALID != 0 {
            tlbi_page(va);
        } else {
            // SAFETY: barriers only.
            unsafe { asm!("dsb ishst", "isb", options(nostack, preserves_flags)) };
        }
        true
    }

    /// The frame and permissions mapped at `va` (page-aligned or not).
    pub fn translate(&self, va: usize) -> Option<(PhysAddr, MapFlags)> {
        let slot = self.slot(va & !(PAGE_SIZE - 1), false).ok()??;
        // SAFETY: live table entry.
        let d = unsafe { slot.read_volatile() };
        if d == 0 {
            return None;
        }
        let pa = PhysAddr((d & ADDR_MASK) as usize);
        if d & DESC_VALID == 0 {
            return Some((pa, MapFlags::USER));
        }
        let mut f = MapFlags::USER | MapFlags::READ;
        if d & AP_RO == 0 {
            f |= MapFlags::WRITE;
        }
        if d & UXN == 0 {
            f |= MapFlags::EXEC;
        }
        if d & ATTR_IDX_MASK == ATTR_IDX_DEVICE {
            f |= MapFlags::DEVICE;
        }
        Some((pa, f))
    }

    /// Make these the live user tables on this CPU.
    ///
    /// # Safety
    /// The tables must stay alive while active; nothing else on this CPU may
    /// rely on the previous TTBR0 mappings.
    pub unsafe fn activate(&self) {
        // SAFETY: TTBR0 holds only user mappings (the kernel is in TTBR1);
        // ASID 0. The flush drops entries of the previous tables.
        unsafe {
            asm!(
                "dsb ish",
                "msr ttbr0_el1, {root}",
                "isb",
                "tlbi vmalle1is",
                "dsb ish",
                "isb",
                root = in(reg) self.root.0 as u64,
                options(nostack, preserves_flags)
            )
        };
    }

    /// Pointer to the level-3 descriptor for `va`, allocating intermediate
    /// tables if `alloc`. `Ok(None)` if a table is missing and `!alloc`.
    fn slot(&self, va: usize, alloc: bool) -> Result<Option<*mut u64>, MapError> {
        if va >= USER_VA_END {
            return Ok(None);
        }
        let mut table = self.root.0 as u64;
        for level in 0..3 {
            let idx = (va >> (39 - 9 * level)) & (ENTRIES - 1);
            // SAFETY: `table` is a live table frame (from mm::frame, reached
            // through the direct map by table_ptr).
            let slot = unsafe { table_ptr(table).add(idx) };
            // SAFETY: as above.
            let e = unsafe { slot.read_volatile() };
            if e & DESC_VALID == 0 {
                if !alloc {
                    return Ok(None);
                }
                let t = alloc_table().ok_or(MapError::NoMemory)?;
                // SAFETY: as above. Order the zeroed table before publishing.
                unsafe {
                    asm!("dsb ishst", options(nostack, preserves_flags));
                    slot.write_volatile(t.0 as u64 | DESC_TABLE | DESC_VALID);
                }
                table = t.0 as u64;
            } else if e & DESC_TABLE == 0 {
                // Block mapping: never created for user tables.
                return Err(MapError::AlreadyMapped(VirtAddr(va)));
            } else {
                table = e & ADDR_MASK;
            }
        }
        let idx = (va >> 12) & (ENTRIES - 1);
        // SAFETY: as above.
        Ok(Some(unsafe { table_ptr(table).add(idx) }))
    }
}

/// Level-3 descriptor for a user page.
fn encode(pa: PhysAddr, flags: MapFlags) -> u64 {
    let pa = pa.0 as u64 & ADDR_MASK;
    if !flags.intersects(MapFlags::READ | MapFlags::WRITE | MapFlags::EXEC) {
        return pa | SW_PROT_NONE;
    }
    pa | leaf_attrs(flags | MapFlags::USER) | DESC_VALID | DESC_TABLE
}

/// Invalidate the stage-1 EL1 TLB entry for user `va` (ASID 0) on all CPUs.
fn tlbi_page(va: usize) {
    let arg = (va as u64 >> 12) & ((1 << 44) - 1);
    // SAFETY: TLB maintenance only.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vale1is, {}",
            "dsb ish",
            "isb",
            in(reg) arg,
            options(nostack, preserves_flags)
        )
    };
}

/// Make instruction fetches from frame `pa` see its current contents: clean
/// the D-cache to the PoU and invalidate the I-cache (skipped when
/// CTR_EL0.IDC / DIC say the hardware keeps them coherent).
pub fn sync_icache(pa: PhysAddr) {
    let ctr: u64;
    // SAFETY: read-only.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let idc = ctr & (1 << 28) != 0;
    let dic = ctr & (1 << 29) != 0;
    if !idc {
        let line = 4usize << ((ctr >> 16) & 0xf);
        let start = phys_to_virt(PhysAddr(pa.0 & !(PAGE_SIZE - 1))).as_usize();
        let mut a = start;
        while a < start + PAGE_SIZE {
            // SAFETY: cache maintenance on a direct-mapped RAM page.
            unsafe { asm!("dc cvau, {}", in(reg) a, options(nostack, preserves_flags)) };
            a += line;
        }
    }
    // SAFETY: barriers / I-cache maintenance only.
    unsafe {
        asm!("dsb ish", options(nostack, preserves_flags));
        if !dic {
            asm!("ic ialluis", "dsb ish", options(nostack, preserves_flags));
        }
        asm!("isb", options(nostack, preserves_flags));
    }
}
