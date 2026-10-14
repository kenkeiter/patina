//! User address space: VMAs over the arch user page tables (design §4).
//!
//! One [`AddressSpace`] per VM (it lives in `task::Process`). Every mapping
//! is anonymous zero-fill memory (no file mappings in Phase 1); ELF segments
//! are populated eagerly by the loader, everything else lazily on the first
//! touch (a user fault inside a VMA with sufficient permissions, or a
//! kernel uaccess, allocates a zeroed frame and maps it with the VMA's
//! permissions).
//!
//! Bookkeeping:
//! * `vmas`: non-overlapping, page-aligned [`Vma`]s keyed by start.
//!   Adjacent VMAs with identical attributes are merged.
//! * `resident`: the set of user pages that currently have a frame (mapped
//!   in `tables`, possibly as PROT_NONE). It makes `munmap`/`madvise`/
//!   `mprotect` over Go's huge PROT_NONE reservations cost
//!   O(resident pages in range) instead of O(pages in range).
//!
//! `PROT_NONE` VMAs never get frames; pages already resident when a range
//! is `mprotect`ed to PROT_NONE keep their frame (contents survive a later
//! `mprotect` back) via the arch's PROT_NONE entries.
//!
//! The kernel never dereferences user virtual addresses (PAN/SMAP): user
//! memory is reached page by page through the direct map, see the uaccess
//! methods at the end of this file.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use patina_abi::errno::{EEXIST, EFAULT, EINVAL, ENODEV, ENOMEM, Errno};
use patina_abi::mm::{
    self as abimm, MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE, MapFlags as MmapFlags, ProtFlags,
};
use patina_abi::zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::arch::paging::MapFlags as PteFlags;
use crate::arch::uspace::{USER_VA_END, UserTables};
use crate::mm::{PAGE_SIZE, PhysAddr, frame, page_align_down, phys_to_virt};
use crate::trap::Access;

/// Lowest address `mmap` hands out or accepts (Linux `mmap_min_addr`).
pub const MMAP_MIN_ADDR: usize = 0x1_0000;
/// Top-down `mmap` allocation starts below this (under the stack).
pub const MMAP_BASE: usize = 0x7f00_0000_0000;

pub type Result<T> = core::result::Result<T, Errno>;

/// What a VMA was created for (diagnostics; all are anonymous memory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmaKind {
    /// `mmap(MAP_ANONYMOUS)`.
    Anon,
    /// A `PT_LOAD` segment of `/app`.
    Elf,
    /// The initial thread's stack.
    Stack,
}

/// One mapping `[start, end)`, page aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vma {
    pub start: usize,
    pub end: usize,
    /// READ/WRITE/EXEC only; empty = PROT_NONE.
    pub prot: ProtFlags,
    /// The `mmap` flags it was created with (FIXED etc. stripped).
    pub flags: MmapFlags,
    pub kind: VmaKind,
}

impl Vma {
    fn mergeable(&self, next: &Vma) -> bool {
        self.end == next.start
            && self.prot == next.prot
            && self.flags == next.flags
            && self.kind == next.kind
    }

    /// Whether `access` is permitted by this VMA's protection. Like Linux on
    /// both arches, WRITE and EXEC imply READ.
    pub fn allows(&self, access: Access) -> bool {
        match access {
            Access::Read => !self.prot.is_empty(),
            Access::Write => self.prot.contains(ProtFlags::WRITE),
            Access::Exec => self.prot.contains(ProtFlags::EXEC),
        }
    }
}

/// Page-table permissions for a VMA protection.
fn pte_flags(prot: ProtFlags) -> PteFlags {
    let mut f = PteFlags::USER;
    if prot.contains(ProtFlags::READ) {
        f |= PteFlags::READ;
    }
    if prot.contains(ProtFlags::WRITE) {
        f |= PteFlags::READ | PteFlags::WRITE;
    }
    if prot.contains(ProtFlags::EXEC) {
        f |= PteFlags::READ | PteFlags::EXEC;
    }
    f
}

/// Validate a user `prot` argument; returns the R/W/X part.
fn parse_prot(prot: u32) -> Result<ProtFlags> {
    #[cfg(target_arch = "aarch64")]
    let extra = patina_abi::aarch64::PROT_BTI;
    #[cfg(not(target_arch = "aarch64"))]
    let extra = 0;
    let known =
        (ProtFlags::READ | ProtFlags::WRITE | ProtFlags::EXEC | ProtFlags::SEM).bits() | extra;
    if prot & !known != 0 {
        return Err(Errno(EINVAL));
    }
    Ok(
        ProtFlags::from_bits_truncate(prot)
            & (ProtFlags::READ | ProtFlags::WRITE | ProtFlags::EXEC),
    )
}

/// `len` rounded up to pages; `None` on overflow or if the range would
/// leave user space.
fn page_len(start: usize, len: usize) -> Option<usize> {
    let l = len.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
    let end = start.checked_add(l)?;
    (end <= USER_VA_END).then_some(l)
}

/// The user address space: page tables + VMAs + resident-page set.
pub struct AddressSpace {
    pub tables: UserTables,
    vmas: BTreeMap<usize, Vma>,
    resident: BTreeSet<usize>,
    /// Program break (end of the highest ELF segment); never moves.
    brk: usize,
}

impl AddressSpace {
    pub fn new() -> Option<AddressSpace> {
        Some(AddressSpace {
            tables: UserTables::new()?,
            vmas: BTreeMap::new(),
            resident: BTreeSet::new(),
            brk: 0,
        })
    }

    // ---------------------------------------------------------------
    // VMA map primitives.

    /// The VMA containing `addr`.
    pub fn find(&self, addr: usize) -> Option<&Vma> {
        self.vmas
            .range(..=addr)
            .next_back()
            .map(|(_, v)| v)
            .filter(|v| addr < v.end)
    }

    /// Iterate the VMAs in address order.
    pub fn vmas(&self) -> impl Iterator<Item = &Vma> {
        self.vmas.values()
    }

    /// Whether no VMA overlaps `[start, end)`.
    pub fn is_free(&self, start: usize, end: usize) -> bool {
        match self.vmas.range(..end).next_back() {
            Some((_, v)) => v.end <= start,
            None => true,
        }
    }

    /// Whether `[start, end)` is entirely covered by VMAs (no holes).
    fn is_covered(&self, start: usize, end: usize) -> bool {
        let mut cur = start;
        while cur < end {
            match self.find(cur) {
                Some(v) => cur = v.end,
                None => return false,
            }
        }
        true
    }

    /// Split the VMA containing `addr` so that a VMA starts at `addr`.
    fn split_at(&mut self, addr: usize) {
        let Some(v) = self.find(addr).copied() else {
            return;
        };
        if v.start == addr {
            return;
        }
        self.vmas.get_mut(&v.start).unwrap().end = addr;
        self.vmas.insert(addr, Vma { start: addr, ..v });
    }

    /// Merge the VMA starting at `start` with its neighbours if possible.
    fn merge_around(&mut self, start: usize) {
        let mut start = start;
        if let Some((_, prev)) = self.vmas.range(..start).next_back() {
            let prev = *prev;
            let cur = self.vmas[&start];
            if prev.mergeable(&cur) {
                self.vmas.remove(&start);
                self.vmas.get_mut(&prev.start).unwrap().end = cur.end;
                start = prev.start;
            }
        }
        let cur = self.vmas[&start];
        if let Some(next) = self.vmas.get(&cur.end).copied()
            && cur.mergeable(&next)
        {
            self.vmas.remove(&next.start);
            self.vmas.get_mut(&start).unwrap().end = next.end;
        }
    }

    /// Insert a VMA over a free range and merge it with its neighbours.
    fn insert(&mut self, vma: Vma) {
        debug_assert!(self.is_free(vma.start, vma.end));
        self.vmas.insert(vma.start, vma);
        self.merge_around(vma.start);
    }

    /// Unmap and free every resident frame in `[start, end)`.
    fn release_frames(&mut self, start: usize, end: usize) {
        let pages: Vec<usize> = self.resident.range(start..end).copied().collect();
        for va in pages {
            self.resident.remove(&va);
            if let Some(pa) = self.tables.unmap(va) {
                frame::free_frames(pa, 0);
            }
        }
    }

    /// Remove every VMA (and frame) in `[start, end)`.
    fn remove_range(&mut self, start: usize, end: usize) {
        self.split_at(start);
        self.split_at(end);
        let keys: Vec<usize> = self.vmas.range(start..end).map(|(k, _)| *k).collect();
        for k in keys {
            self.vmas.remove(&k);
        }
        self.release_frames(start, end);
    }

    /// Highest free gap of `len` bytes below `MMAP_BASE` (top-down, like
    /// Linux's default `mmap` layout).
    fn find_free_top_down(&self, len: usize) -> Option<usize> {
        let mut top = MMAP_BASE;
        for v in self.vmas.range(..MMAP_BASE).rev().map(|(_, v)| v) {
            if v.end <= top {
                if top - v.end >= len {
                    return Some(top - len);
                }
                top = v.start;
            } else {
                top = top.min(v.start);
            }
        }
        (top >= MMAP_MIN_ADDR && top - MMAP_MIN_ADDR >= len).then(|| top - len)
    }

    // ---------------------------------------------------------------
    // Building the initial image (loader).

    /// Create a VMA at a fixed, free range (loader use). The range must be
    /// page aligned.
    pub fn add_vma(
        &mut self,
        start: usize,
        end: usize,
        prot: ProtFlags,
        kind: VmaKind,
    ) -> Result<()> {
        if !start.is_multiple_of(PAGE_SIZE)
            || !end.is_multiple_of(PAGE_SIZE)
            || end <= start
            || end > USER_VA_END
        {
            return Err(Errno(EINVAL));
        }
        if !self.is_free(start, end) {
            return Err(Errno(EEXIST));
        }
        self.insert(Vma {
            start,
            end,
            prot,
            flags: MmapFlags::PRIVATE | MmapFlags::ANONYMOUS,
            kind,
        });
        Ok(())
    }

    /// Map `pa` (already filled) at page `va` inside a VMA, with the VMA's
    /// permissions. On aarch64 an EXEC page's I-cache is synced here, so
    /// the frame contents must be final.
    pub fn map_filled(&mut self, va: usize, pa: PhysAddr) -> Result<()> {
        let prot = self.find(va).ok_or(Errno(EFAULT))?.prot;
        self.tables
            .map(va, pa, pte_flags(prot))
            .map_err(|_| Errno(ENOMEM))?;
        self.resident.insert(va);
        Ok(())
    }

    pub fn set_brk(&mut self, brk: usize) {
        self.brk = brk;
    }

    // ---------------------------------------------------------------
    // Faults.

    /// Make page `va` (aligned) resident for `access`. Returns its frame.
    fn fault_in(&mut self, va: usize, access: Access) -> Result<PhysAddr> {
        let vma = *self.find(va).ok_or(Errno(EFAULT))?;
        if !vma.allows(access) {
            return Err(Errno(EFAULT));
        }
        if self.resident.contains(&va) {
            let (pa, _) = self.tables.translate(va).ok_or(Errno(EFAULT))?;
            return Ok(pa);
        }
        let pa = frame::alloc_frames_zeroed(0).ok_or(Errno(ENOMEM))?;
        if self.tables.map(va, pa, pte_flags(vma.prot)).is_err() {
            frame::free_frames(pa, 0);
            return Err(Errno(ENOMEM));
        }
        self.resident.insert(va);
        Ok(pa)
    }

    /// Resolve a user fault at `addr`. Returns true if the access can be
    /// retried (a page was filled in, or it was already mapped with
    /// sufficient permissions: a spurious fault); false if the fault is a
    /// real SIGSEGV.
    pub fn handle_fault(&mut self, addr: usize, access: Access) -> bool {
        let va = page_align_down(addr);
        let was_resident = self.resident.contains(&va);
        match self.fault_in(va, access) {
            Ok(_) => {
                if was_resident {
                    // The VMA allows the access but the page faulted:
                    // bring the entry back in line with the VMA (this
                    // also flushes a stale TLB entry) so the retry cannot
                    // loop forever.
                    let prot = self.find(va).map(|v| v.prot).unwrap_or(ProtFlags::empty());
                    self.tables.protect(va, pte_flags(prot));
                }
                true
            }
            Err(e) => {
                if e.0 == ENOMEM {
                    crate::kprintln!("[patina] out of memory filling user page {:#x}", va);
                }
                false
            }
        }
    }

    /// Print the VMA table (fatal fault diagnostics).
    pub fn dump(&self) {
        crate::kprintln!("[patina] VMAs ({} resident pages):", self.resident.len());
        for v in self.vmas.values() {
            let p = |f: ProtFlags, c: char| if v.prot.contains(f) { c } else { '-' };
            crate::kprintln!(
                "[patina]   {:#014x}-{:#014x} {}{}{} {:?}",
                v.start,
                v.end,
                p(ProtFlags::READ, 'r'),
                p(ProtFlags::WRITE, 'w'),
                p(ProtFlags::EXEC, 'x'),
                v.kind
            );
        }
    }

    // ---------------------------------------------------------------
    // Syscalls.

    /// `mmap(addr, len, prot, flags, fd, off)`: anonymous private (or
    /// shared, which is the same thing without fork) mappings only.
    pub fn mmap(
        &mut self,
        addr: usize,
        len: usize,
        prot: u32,
        flags: u32,
        _fd: i32,
        off: u64,
    ) -> Result<usize> {
        let mf = MmapFlags::from_bits_retain(flags);
        let prot = parse_prot(prot)?;
        if !mf.contains(MmapFlags::ANONYMOUS) {
            return Err(Errno(ENODEV));
        }
        match flags & abimm::MAP_TYPE {
            t if t == MmapFlags::PRIVATE.bits() || t == MmapFlags::SHARED.bits() => {}
            _ => return Err(Errno(EINVAL)),
        }
        if len == 0 || !off.is_multiple_of(PAGE_SIZE as u64) {
            return Err(Errno(EINVAL));
        }
        let fixed = mf.contains(MmapFlags::FIXED);
        let noreplace = mf.contains(MmapFlags::FIXED_NOREPLACE);
        let len = len.checked_add(PAGE_SIZE - 1).ok_or(Errno(ENOMEM))? & !(PAGE_SIZE - 1);
        let start = if fixed || noreplace {
            if !addr.is_multiple_of(PAGE_SIZE) {
                return Err(Errno(EINVAL));
            }
            if addr < MMAP_MIN_ADDR || page_len(addr, len).is_none() {
                return Err(Errno(ENOMEM));
            }
            if !self.is_free(addr, addr + len) {
                if noreplace && !fixed {
                    return Err(Errno(EEXIST));
                }
                self.remove_range(addr, addr + len);
            }
            addr
        } else {
            let hint = page_align_down(addr);
            if hint >= MMAP_MIN_ADDR
                && page_len(hint, len).is_some()
                && self.is_free(hint, hint + len)
            {
                hint
            } else {
                self.find_free_top_down(len).ok_or(Errno(ENOMEM))?
            }
        };
        let keep = MmapFlags::PRIVATE
            | MmapFlags::SHARED
            | MmapFlags::ANONYMOUS
            | MmapFlags::NORESERVE
            | MmapFlags::STACK
            | MmapFlags::GROWSDOWN;
        self.insert(Vma {
            start,
            end: start + len,
            prot,
            flags: mf & keep,
            kind: VmaKind::Anon,
        });
        Ok(start)
    }

    pub fn munmap(&mut self, addr: usize, len: usize) -> Result<()> {
        if !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
            return Err(Errno(EINVAL));
        }
        let len = page_len(addr, len).ok_or(Errno(EINVAL))?;
        self.remove_range(addr, addr + len);
        Ok(())
    }

    pub fn mprotect(&mut self, addr: usize, len: usize, prot: u32) -> Result<()> {
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno(EINVAL));
        }
        let pflags = ProtFlags::from_bits_retain(prot);
        let prot = parse_prot(prot & !(ProtFlags::GROWSDOWN | ProtFlags::GROWSUP).bits())?;
        if pflags.contains(ProtFlags::GROWSDOWN | ProtFlags::GROWSUP) {
            return Err(Errno(EINVAL));
        }
        let len = page_len(addr, len).ok_or(Errno(ENOMEM))?;
        if len == 0 {
            return Ok(());
        }
        let end = addr + len;
        if !self.is_covered(addr, end) {
            return Err(Errno(ENOMEM));
        }
        self.split_at(addr);
        self.split_at(end);
        let keys: Vec<usize> = self.vmas.range(addr..end).map(|(k, _)| *k).collect();
        for k in &keys {
            self.vmas.get_mut(k).unwrap().prot = prot;
        }
        let pages: Vec<usize> = self.resident.range(addr..end).copied().collect();
        let f = pte_flags(prot);
        for va in pages {
            self.tables.protect(va, f);
        }
        for k in keys {
            if self.vmas.contains_key(&k) {
                self.merge_around(k);
            }
        }
        if let Some(v) = self.find(end).map(|v| v.start) {
            self.merge_around(v);
        }
        Ok(())
    }

    pub fn madvise(&mut self, addr: usize, len: usize, advice: i32) -> Result<()> {
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno(EINVAL));
        }
        let len = page_len(addr, len).ok_or(Errno(EINVAL))?;
        let end = addr + len;
        match advice {
            abimm::MADV_DONTNEED | abimm::MADV_FREE => {
                if len == 0 {
                    return Ok(());
                }
                self.release_frames(addr, end);
                if !self.is_covered(addr, end) {
                    return Err(Errno(ENOMEM));
                }
                Ok(())
            }
            abimm::MADV_NORMAL
            | abimm::MADV_RANDOM
            | abimm::MADV_SEQUENTIAL
            | abimm::MADV_WILLNEED
            | abimm::MADV_HUGEPAGE
            | abimm::MADV_NOHUGEPAGE
            | abimm::MADV_COLLAPSE
            | abimm::MADV_DONTDUMP
            | abimm::MADV_DODUMP
            | abimm::MADV_DONTFORK
            | abimm::MADV_DOFORK
            | abimm::MADV_MERGEABLE
            | abimm::MADV_UNMERGEABLE
            | abimm::MADV_COLD
            | abimm::MADV_PAGEOUT => {
                if len != 0 && !self.is_covered(addr, end) {
                    return Err(Errno(ENOMEM));
                }
                Ok(())
            }
            _ => Err(Errno(EINVAL)),
        }
    }

    /// `mremap(old, old_len, new_len, flags)`: grow in place when the
    /// following range is free, else move (MREMAP_MAYMOVE) by re-mapping the
    /// existing frames. MREMAP_FIXED / MREMAP_DONTUNMAP are not supported.
    pub fn mremap(
        &mut self,
        old: usize,
        old_len: usize,
        new_len: usize,
        flags: u32,
    ) -> Result<usize> {
        if !old.is_multiple_of(PAGE_SIZE)
            || flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0
        {
            return Err(Errno(EINVAL));
        }
        if flags & (MREMAP_FIXED | MREMAP_DONTUNMAP) != 0 {
            crate::kprintln!("[patina] mremap: MREMAP_FIXED/DONTUNMAP not supported");
            return Err(Errno(EINVAL));
        }
        let old_len = page_len(old, old_len).ok_or(Errno(EINVAL))?;
        let new_len = new_len
            .checked_add(PAGE_SIZE - 1)
            .map(|l| l & !(PAGE_SIZE - 1))
            .ok_or(Errno(EINVAL))?;
        if new_len == 0 || old_len == 0 {
            return Err(Errno(EINVAL));
        }
        let vma = *self.find(old).ok_or(Errno(EFAULT))?;
        if old + old_len > vma.end {
            return Err(Errno(EFAULT));
        }
        if new_len <= old_len {
            self.remove_range(old + new_len, old + old_len);
            return Ok(old);
        }
        // Grow in place?
        let old_end = old + old_len;
        if old.checked_add(new_len).is_some_and(|e| e <= USER_VA_END)
            && old_end == vma.end
            && self.is_free(old_end, old + new_len)
        {
            self.insert(Vma {
                start: old_end,
                end: old + new_len,
                ..vma
            });
            return Ok(old);
        }
        if flags & MREMAP_MAYMOVE == 0 {
            return Err(Errno(ENOMEM));
        }
        let new = self.find_free_top_down(new_len).ok_or(Errno(ENOMEM))?;
        self.insert(Vma {
            start: new,
            end: new + new_len,
            ..vma
        });
        let pages: Vec<usize> = self.resident.range(old..old_end).copied().collect();
        for va in pages {
            self.resident.remove(&va);
            let Some((_, f)) = self.tables.translate(va) else {
                continue;
            };
            let Some(pa) = self.tables.unmap(va) else {
                continue;
            };
            let nva = new + (va - old);
            if self.tables.map(nva, pa, f).is_err() {
                frame::free_frames(pa, 0);
            } else {
                self.resident.insert(nva);
            }
        }
        self.remove_range(old, old_end);
        Ok(new)
    }

    /// `brk`: the break is fixed at the end of the ELF image (Go does not
    /// use brk). Like Linux, a request that cannot be satisfied returns the
    /// current break rather than an error.
    pub fn brk(&mut self, _req: usize) -> usize {
        self.brk
    }

    /// `mincore`: 1 per resident page, 0 otherwise. `ENOMEM` if the range
    /// is not fully mapped.
    pub fn mincore(&mut self, addr: usize, len: usize) -> Result<Vec<u8>> {
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno(EINVAL));
        }
        let len = page_len(addr, len).ok_or(Errno(ENOMEM))?;
        if !self.is_covered(addr, addr + len) {
            return Err(Errno(ENOMEM));
        }
        Ok((0..len / PAGE_SIZE)
            .map(|i| self.resident.contains(&(addr + i * PAGE_SIZE)) as u8)
            .collect())
    }

    // ---------------------------------------------------------------
    // uaccess: user memory through the direct map, page by page.

    /// Walk `[va, va+len)` page by page, faulting pages in for `access`,
    /// calling `f(kernel_ptr, offset_in_buffer, chunk_len)`.
    fn walk(
        &mut self,
        va: usize,
        len: usize,
        access: Access,
        mut f: impl FnMut(*mut u8, usize, usize),
    ) -> Result<()> {
        let end = va.checked_add(len).ok_or(Errno(EFAULT))?;
        if end > USER_VA_END {
            return Err(Errno(EFAULT));
        }
        let mut cur = va;
        while cur < end {
            let page = page_align_down(cur);
            let pa = self.fault_in(page, access)?;
            let off = cur - page;
            let n = (PAGE_SIZE - off).min(end - cur);
            // Pointer into the direct map of a frame owned by this space.
            let p = phys_to_virt(pa).as_mut_ptr::<u8>().wrapping_add(off);
            f(p, cur - va, n);
            cur += n;
        }
        Ok(())
    }

    /// Copy `buf.len()` bytes from user `va` (needs read permission).
    pub fn copy_from_user(&mut self, va: usize, buf: &mut [u8]) -> Result<()> {
        let len = buf.len();
        self.walk(va, len, Access::Read, |p, off, n| {
            // SAFETY: `p..p+n` is inside one resident user frame reached
            // through the direct map; `buf[off..off+n]` is in bounds.
            unsafe { core::ptr::copy_nonoverlapping(p, buf.as_mut_ptr().add(off), n) }
        })
    }

    /// Copy `buf` to user `va` (needs write permission).
    pub fn copy_to_user(&mut self, va: usize, buf: &[u8]) -> Result<()> {
        self.walk(va, buf.len(), Access::Write, |p, off, n| {
            // SAFETY: as in copy_from_user. On aarch64 a page that is both
            // writable and executable would need an I-cache sync after
            // this; Go never maps W+X memory.
            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr().add(off), p, n) }
        })
    }

    /// Zero `len` user bytes at `va`.
    pub fn zero_user(&mut self, va: usize, len: usize) -> Result<()> {
        self.walk(va, len, Access::Write, |p, _, n| {
            // SAFETY: as in copy_from_user.
            unsafe { core::ptr::write_bytes(p, 0, n) }
        })
    }

    /// Read a `T` from user `va`.
    pub fn read_val<T: FromBytes + IntoBytes>(&mut self, va: usize) -> Result<T> {
        let mut v = T::new_zeroed();
        self.copy_from_user(va, v.as_mut_bytes())?;
        Ok(v)
    }

    /// Write a `T` to user `va`.
    pub fn write_val<T: IntoBytes + Immutable>(&mut self, va: usize, v: &T) -> Result<()> {
        self.copy_to_user(va, v.as_bytes())
    }

    /// Read a NUL-terminated string (without the NUL) of at most `max`
    /// bytes. `ENAMETOOLONG` if no NUL within `max`.
    pub fn read_cstr(&mut self, va: usize, max: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur = va;
        loop {
            let page = page_align_down(cur);
            if page >= USER_VA_END {
                return Err(Errno(EFAULT));
            }
            let pa = self.fault_in(page, Access::Read)?;
            let off = cur - page;
            // SAFETY: the rest of one resident user frame via the direct map.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    phys_to_virt(pa).as_ptr::<u8>().add(off),
                    PAGE_SIZE - off,
                )
            };
            match bytes.iter().position(|&b| b == 0) {
                Some(n) => {
                    out.extend_from_slice(&bytes[..n]);
                    if out.len() > max {
                        return Err(Errno(patina_abi::errno::ENAMETOOLONG));
                    }
                    return Ok(out);
                }
                None => out.extend_from_slice(bytes),
            }
            if out.len() > max {
                return Err(Errno(patina_abi::errno::ENAMETOOLONG));
            }
            cur = page + PAGE_SIZE;
        }
    }
}

/// `patina.selftest=vm`: exercise the VMA operations and uaccess on a
/// scratch address space (never activated; all access is via the direct
/// map). Panics on the first mismatch.
pub fn selftest() {
    use patina_abi::mm::{MADV_DONTNEED, MREMAP_MAYMOVE};
    const RW: u32 = 3;
    const NONE: u32 = 0;
    let anon = (MmapFlags::PRIVATE | MmapFlags::ANONYMOUS).bits();
    let fixed = anon | MmapFlags::FIXED.bits();
    let (free0, _) = frame::stats();
    let mut s = AddressSpace::new().expect("vm selftest: tables");

    // Hint honoured, PROT_NONE reservation is free of frames.
    let hint = 0x00c0_0000_0000;
    let r = s.mmap(hint, 64 << 20, NONE, anon, -1, 0).unwrap();
    assert_eq!(r, hint, "hint not honoured");
    assert_eq!(s.resident.len(), 0);
    // Same hint again: taken, so top-down below MMAP_BASE.
    let r2 = s.mmap(hint, 1 << 20, RW, anon, -1, 0).unwrap();
    assert!(r2 != hint && r2 + (1 << 20) <= MMAP_BASE, "fallback {:#x}", r2);
    // FIXED RW over part of the reservation; uaccess faults pages in.
    s.mmap(hint + 0x10000, 0x4000, RW, fixed, -1, 0).unwrap();
    assert_eq!(s.vmas.len(), 4); // none | rw | none, + r2
    s.copy_to_user(hint + 0x10ff0, b"patina vm selftest!").unwrap();
    assert_eq!(s.resident.len(), 2); // crosses a page boundary
    let mut buf = [0u8; 19];
    assert!(s.copy_from_user(hint + 0x0ff0, &mut buf).is_err(), "read of PROT_NONE");
    // mprotect NONE keeps the frames; reads fail; back to RW: data intact.
    s.mprotect(hint + 0x10000, 0x4000, NONE).unwrap();
    assert_eq!(s.vmas.len(), 2, "PROT_NONE ranges not merged"); // reservation + r2
    assert!(s.copy_from_user(hint + 0x10ff0, &mut buf).is_err());
    s.mprotect(hint + 0x10000, 0x2000, RW).unwrap();
    s.copy_from_user(hint + 0x10ff0, &mut buf).unwrap();
    assert_eq!(&buf, b"patina vm selftest!");
    // mremap: grow r2 in place is impossible only if something follows;
    // move a 2-page RW mapping and check the data moved with it.
    let m = s.mmap(0, 0x2000, RW, anon, -1, 0).unwrap();
    s.write_val(m + 0x1000, &0xdead_beef_u64).unwrap();
    let blocker = s.mmap(m + 0x2000, 0x1000, NONE, fixed, -1, 0).unwrap();
    assert_eq!(blocker, m + 0x2000);
    assert!(s.mremap(m, 0x2000, 0x8000, 0).is_err(), "grew over a mapping");
    let m2 = s.mremap(m, 0x2000, 0x8000, MREMAP_MAYMOVE).unwrap();
    assert!(m2 != m);
    assert_eq!(s.read_val::<u64>(m2 + 0x1000).unwrap(), 0xdead_beef);
    assert!(s.find(m).is_none(), "old mapping still present");
    // madvise(DONTNEED) drops the frame; the next read sees zeros.
    s.madvise(m2, 0x8000, MADV_DONTNEED).unwrap();
    assert_eq!(s.read_val::<u64>(m2 + 0x1000).unwrap(), 0);
    // read_cstr across a page boundary.
    s.copy_to_user(m2 + 0xffc, b"abcdefgh\0").unwrap();
    assert_eq!(s.read_cstr(m2 + 0xffc, 64).unwrap(), b"abcdefgh");
    // munmap everything: frames go back (page-table pages stay).
    s.munmap(hint, 64 << 20).unwrap();
    s.munmap(r2, 1 << 20).unwrap();
    s.munmap(m2, 0x8000).unwrap();
    s.munmap(blocker, 0x1000).unwrap();
    assert_eq!(s.vmas.len(), 0);
    assert_eq!(s.resident.len(), 0);
    let (free1, _) = frame::stats();
    crate::kprintln!(
        "[patina] selftest vm: ok ({} frames held by page tables)",
        free0 - free1
    );
}
