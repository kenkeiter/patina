//! Arch-neutral description of the machine, produced by arch boot code and
//! consumed by `kernel_main`.
//!
//! Arch code builds a [`MemoryMap`] by adding RAM with [`MemoryMap::add_ram`]
//! and then carving reserved ranges out of it with [`MemoryMap::reserve`]
//! (kernel image, DTB / start_info, initrd, firmware reservations). The result
//! is sorted and non-overlapping; `Usable` regions are page-aligned.

use core::fmt;

use crate::mm::{PAGE_SIZE, PhysAddr};

/// Half-open physical range `[start, end)`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PhysRange {
    pub start: PhysAddr,
    pub end: PhysAddr,
}

impl PhysRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self {
            start: PhysAddr(start),
            end: PhysAddr(end),
        }
    }
    pub const fn len(&self) -> usize {
        self.end.0 - self.start.0
    }
    pub const fn is_empty(&self) -> bool {
        self.end.0 <= self.start.0
    }
}

impl fmt::Debug for PhysRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:#x}-{:#x})", self.start.0, self.end.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemKind {
    /// Free RAM, handed to the frame allocator.
    Usable,
    /// RAM reserved by firmware/VMM (`/memreserve/`, `/reserved-memory`, e820 reserved).
    Reserved,
    /// The kernel image (text, data, bss).
    Kernel,
    /// The initrd as loaded by the VMM.
    Initrd,
    /// Firmware tables the kernel keeps using: DTB, PVH start_info, ACPI.
    Firmware,
}

impl MemKind {
    pub const fn name(self) -> &'static str {
        match self {
            MemKind::Usable => "usable",
            MemKind::Reserved => "reserved",
            MemKind::Kernel => "kernel",
            MemKind::Initrd => "initrd",
            MemKind::Firmware => "firmware",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MemRegion {
    pub start: PhysAddr,
    pub end: PhysAddr,
    pub kind: MemKind,
}

impl MemRegion {
    pub const fn len(&self) -> usize {
        self.end.0 - self.start.0
    }
}

pub const MAX_MEM_REGIONS: usize = 128;

#[derive(Debug)]
pub enum MemMapError {
    Full,
}

/// Fixed-capacity, sorted, non-overlapping physical memory map (no heap).
pub struct MemoryMap {
    regions: [MemRegion; MAX_MEM_REGIONS],
    len: usize,
}

impl MemoryMap {
    pub const fn new() -> Self {
        const EMPTY: MemRegion = MemRegion {
            start: PhysAddr(0),
            end: PhysAddr(0),
            kind: MemKind::Reserved,
        };
        Self {
            regions: [EMPTY; MAX_MEM_REGIONS],
            len: 0,
        }
    }

    pub fn regions(&self) -> &[MemRegion] {
        &self.regions[..self.len]
    }

    pub fn iter(&self) -> impl Iterator<Item = &MemRegion> {
        self.regions().iter()
    }

    pub fn usable(&self) -> impl Iterator<Item = &MemRegion> {
        self.iter().filter(|r| r.kind == MemKind::Usable)
    }

    pub fn of_kind(&self, kind: MemKind) -> impl Iterator<Item = &MemRegion> {
        self.iter().filter(move |r| r.kind == kind)
    }

    /// Total bytes described (all kinds).
    pub fn total_bytes(&self) -> usize {
        self.iter().map(|r| r.len()).sum()
    }

    pub fn usable_bytes(&self) -> usize {
        self.usable().map(|r| r.len()).sum()
    }

    fn insert_at(&mut self, idx: usize, r: MemRegion) -> Result<(), MemMapError> {
        if self.len == MAX_MEM_REGIONS {
            return Err(MemMapError::Full);
        }
        self.regions.copy_within(idx..self.len, idx + 1);
        self.regions[idx] = r;
        self.len += 1;
        Ok(())
    }

    fn remove_at(&mut self, idx: usize) {
        self.regions.copy_within(idx + 1..self.len, idx);
        self.len -= 1;
    }

    /// Add RAM `[start, end)` as `Usable` (shrunk inward to page boundaries).
    /// Parts overlapping already-present regions are ignored.
    pub fn add_ram(&mut self, start: usize, end: usize) -> Result<(), MemMapError> {
        let start = align_up(start, PAGE_SIZE);
        let end = end & !(PAGE_SIZE - 1);
        if start >= end {
            return Ok(());
        }
        // Insert the pieces of [start,end) not covered by existing regions.
        let mut cur = start;
        let mut i = 0;
        while cur < end {
            // Skip regions entirely below cur.
            while i < self.len && self.regions[i].end.0 <= cur {
                i += 1;
            }
            let next_start = if i < self.len {
                self.regions[i].start.0
            } else {
                usize::MAX
            };
            if next_start > cur {
                let piece_end = end.min(next_start);
                self.insert_at(
                    i,
                    MemRegion {
                        start: PhysAddr(cur),
                        end: PhysAddr(piece_end),
                        kind: MemKind::Usable,
                    },
                )?;
                i += 1;
                cur = piece_end;
            } else {
                cur = self.regions[i].end.0;
            }
        }
        self.coalesce();
        Ok(())
    }

    /// Mark `[start, end)` (rounded outward to pages) as `kind`, splitting
    /// any regions it overlaps. Only RAM already in the map is affected:
    /// reservations outside RAM are recorded as-is so they still print.
    pub fn reserve(&mut self, start: usize, end: usize, kind: MemKind) -> Result<(), MemMapError> {
        let start = start & !(PAGE_SIZE - 1);
        let end = align_up(end, PAGE_SIZE);
        if start >= end {
            return Ok(());
        }
        let mut i = 0;
        while i < self.len {
            let r = self.regions[i];
            if r.end.0 <= start || r.start.0 >= end {
                i += 1;
                continue;
            }
            // Overlap: split r into [r.start,start) r.kind, [max,min) kind, [end,r.end) r.kind.
            self.remove_at(i);
            let mut j = i;
            if r.start.0 < start {
                self.insert_at(
                    j,
                    MemRegion {
                        start: r.start,
                        end: PhysAddr(start),
                        kind: r.kind,
                    },
                )?;
                j += 1;
            }
            self.insert_at(
                j,
                MemRegion {
                    start: PhysAddr(r.start.0.max(start)),
                    end: PhysAddr(r.end.0.min(end)),
                    kind,
                },
            )?;
            j += 1;
            if r.end.0 > end {
                self.insert_at(
                    j,
                    MemRegion {
                        start: PhysAddr(end),
                        end: r.end,
                        kind: r.kind,
                    },
                )?;
                j += 1;
            }
            i = j;
        }
        self.coalesce();
        Ok(())
    }

    fn coalesce(&mut self) {
        let mut i = 0;
        while i + 1 < self.len {
            let (a, b) = (self.regions[i], self.regions[i + 1]);
            if a.kind == b.kind && a.end == b.start {
                self.regions[i].end = b.end;
                self.remove_at(i + 1);
            } else {
                i += 1;
            }
        }
    }
}

impl Default for MemoryMap {
    fn default() -> Self {
        Self::new()
    }
}

/// How the platform was described to us.
#[derive(Clone, Copy, Debug)]
pub enum PlatformInfo {
    /// Flattened device tree at this physical address (aarch64).
    Dtb(PhysAddr),
    /// PVH `hvm_start_info` at this physical address (x86_64); ACPI RSDP is
    /// reachable from it.
    StartInfo(PhysAddr),
}

pub struct BootInfo {
    pub memory: MemoryMap,
    /// Raw kernel command line (`/chosen/bootargs` or PVH cmdline).
    pub cmdline: &'static str,
    /// Physical range of the initrd, if the VMM supplied one.
    pub initrd: Option<PhysRange>,
    pub platform: PlatformInfo,
    /// Physical range occupied by the kernel image (incl. bss).
    pub kernel: PhysRange,
}

pub const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}
