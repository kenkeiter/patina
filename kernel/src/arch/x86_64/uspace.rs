//! User page tables: a PML4 whose lower half (PML4[0..256]) maps user space
//! and whose upper half is a copy of the kernel PML4's 256 entries. Those
//! are pre-populated at boot (`paging::populate_kernel_pml4`), so kernel
//! mappings made later are visible in every user address space without
//! touching user PML4s.
//!
//! 4 KiB pages only; every table entry on the path has U/S set, leaves get
//! U/S and NX unless `EXEC`. Table frames come from `mm::frame` (zeroed) and
//! are reached through the direct map. Tables are not freed on unmap (nor on
//! drop: Phase 1 has one address space for the life of the VM).

use super::cpu;
use super::paging::{
    ADDR_MASK, MapError, MapFlags, PTE_NX, PTE_P, PTE_PS, PTE_RW, PTE_US, kernel_root, leaf_attrs,
};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, frame, phys_to_virt};

/// Lowest address above user space (47-bit user VA).
pub const USER_VA_END: usize = 0x0000_8000_0000_0000;

const ENTRIES: usize = 512;

/// Software bit (AVL, ignored by the MMU in a not-present entry) marking a
/// PROT_NONE page: the entry is not present, so every access faults, but it
/// still records the frame so `translate` reports it (with empty
/// permissions) and `unmap` hands it back. Same contract as aarch64.
const SW_PROT_NONE: u64 = 1 << 9;

/// Leaf entry for a user page; PROT_NONE (none of READ/WRITE/EXEC) is a
/// not-present entry carrying the frame.
fn encode(pa: PhysAddr, flags: MapFlags) -> u64 {
    let pa = pa.0 as u64 & ADDR_MASK;
    if !flags.intersects(MapFlags::READ | MapFlags::WRITE | MapFlags::EXEC) {
        return pa | SW_PROT_NONE;
    }
    pa | leaf_attrs(flags | MapFlags::USER) | PTE_P
}

#[derive(Debug)]
pub struct UserTables {
    root: PhysAddr,
}

fn table(pa: u64) -> *mut u64 {
    phys_to_virt(PhysAddr((pa & ADDR_MASK) as usize)).as_mut_ptr::<u64>()
}

fn invlpg(va: usize) {
    // SAFETY: invalidating a TLB entry has no other effect.
    unsafe { core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags)) };
}

impl UserTables {
    /// A new address space with no user mappings.
    pub fn new() -> Option<UserTables> {
        let root = frame::alloc_frames_zeroed(0)?;
        let k = table(kernel_root().0 as u64);
        let u = table(root.0 as u64);
        for i in ENTRIES / 2..ENTRIES {
            // SAFETY: both are live, direct-mapped PML4 frames.
            unsafe { u.add(i).write(k.add(i).read_volatile()) };
        }
        Some(UserTables { root })
    }

    pub fn root(&self) -> PhysAddr {
        self.root
    }

    /// Pointer to the leaf PTE for `va`, allocating intermediate tables if
    /// `alloc`. None if `va` is outside user space or a table is missing.
    fn pte(&self, va: usize, alloc: bool) -> Result<Option<*mut u64>, MapError> {
        if va >= USER_VA_END || !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }
        let mut t = self.root.0 as u64;
        for level in 0..3 {
            let idx = (va >> (39 - 9 * level)) & (ENTRIES - 1);
            // SAFETY: `t` is a live table frame of this address space.
            let slot = unsafe { table(t).add(idx) };
            // SAFETY: as above.
            let e = unsafe { slot.read_volatile() };
            if e & PTE_P == 0 {
                if !alloc {
                    return Ok(None);
                }
                let f = frame::alloc_frames_zeroed(0).ok_or(MapError::NoMemory)?;
                // SAFETY: as above; installs a zeroed table.
                unsafe { slot.write_volatile(f.0 as u64 | PTE_P | PTE_RW | PTE_US) };
                t = f.0 as u64;
            } else if e & PTE_PS != 0 {
                // Never created by this module.
                return Err(MapError::AlreadyMapped(VirtAddr(va)));
            } else {
                t = e & ADDR_MASK;
            }
        }
        // SAFETY: `t` is the live PT for `va`.
        Ok(Some(unsafe { table(t).add((va >> 12) & (ENTRIES - 1)) }))
    }

    /// Map one 4 KiB page; `AlreadyMapped` if `va` has a translation.
    pub fn map(&mut self, va: usize, pa: PhysAddr, flags: MapFlags) -> Result<(), MapError> {
        if !pa.0.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::Misaligned);
        }
        let slot = self.pte(va, true)?.ok_or(MapError::NoMemory)?;
        // SAFETY: a PTE of this address space.
        let e = unsafe { slot.read_volatile() };
        if e != 0 {
            return Err(MapError::AlreadyMapped(VirtAddr(va)));
        }
        let desc = encode(pa, flags);
        // SAFETY: as above; not-present entries are never cached by the TLB.
        unsafe { slot.write_volatile(desc) };
        Ok(())
    }

    /// Remove the translation for `va` (TLB-invalidated on this CPU);
    /// returns the frame that was mapped.
    pub fn unmap(&mut self, va: usize) -> Option<PhysAddr> {
        let slot = self.pte(va, false).ok()??;
        // SAFETY: a PTE of this address space.
        let e = unsafe { slot.read_volatile() };
        if e == 0 {
            return None;
        }
        // SAFETY: as above.
        unsafe { slot.write_volatile(0) };
        if e & PTE_P != 0 {
            invlpg(va);
        }
        Some(PhysAddr((e & ADDR_MASK) as usize))
    }

    /// Change the permissions of a mapped page (TLB-invalidated on this CPU).
    pub fn protect(&mut self, va: usize, flags: MapFlags) -> bool {
        let Ok(Some(slot)) = self.pte(va, false) else {
            return false;
        };
        // SAFETY: a PTE of this address space.
        let e = unsafe { slot.read_volatile() };
        if e == 0 {
            return false;
        }
        let desc = encode(PhysAddr((e & ADDR_MASK) as usize), flags);
        // SAFETY: as above.
        unsafe { slot.write_volatile(desc) };
        if e & PTE_P != 0 {
            invlpg(va);
        }
        true
    }

    /// Frame and permissions mapped at page `va`.
    pub fn translate(&self, va: usize) -> Option<(PhysAddr, MapFlags)> {
        let slot = self.pte(va & !(PAGE_SIZE - 1), false).ok()??;
        // SAFETY: a PTE of this address space.
        let e = unsafe { slot.read_volatile() };
        if e == 0 {
            return None;
        }
        if e & PTE_P == 0 {
            return Some((PhysAddr((e & ADDR_MASK) as usize), MapFlags::USER));
        }
        let mut f = MapFlags::READ;
        if e & PTE_RW != 0 {
            f |= MapFlags::WRITE;
        }
        if e & PTE_NX == 0 {
            f |= MapFlags::EXEC;
        }
        if e & PTE_US != 0 {
            f |= MapFlags::USER;
        }
        Some((PhysAddr((e & ADDR_MASK) as usize), f))
    }

    /// Make these the live user tables on this CPU (CR3; kernel mappings are
    /// global and identical, so the kernel keeps running).
    ///
    /// # Safety
    /// `self` must outlive its use as CR3 (keep it alive while active).
    pub unsafe fn activate(&self) {
        // SAFETY: the upper half mirrors the kernel PML4.
        unsafe { cpu::write_cr3(self.root.0 as u64) };
    }
}
