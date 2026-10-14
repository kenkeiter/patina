//! PCI (Phase 4, decision D70): config space access, bus enumeration, BAR
//! decoding/assignment, capability walk, and discovery of virtio-pci
//! functions for `drivers::virtio::pci`.
//!
//! Host bridges come from the arch layer (`arch::devices::pci_hosts`):
//! aarch64 from the DTB `pci-host-ecam-generic` node (QEMU `virt`, Cloud
//! Hypervisor), x86_64 from the ACPI MCFG (QEMU q35, Cloud Hypervisor) or,
//! without one (QEMU `pc`), the legacy 0xCF8/0xCFC port mechanism on bus 0.
//!
//! Enumeration starts at the host's first bus and follows PCI-PCI bridges
//! whose secondary bus the firmware programmed (nothing is renumbered).
//! Memory BARs are sized (32- and 64-bit; I/O BARs ignored). A memory BAR
//! that reads 0 is unassigned (QEMU `virt` booted with `-kernel` has no
//! firmware): it gets an address from the host's 32-bit non-prefetchable
//! window (else the 64-bit one), bump-allocated past every BAR that is
//! already assigned, aligned to its size, written back and verified. A
//! host without a window (x86: firmware assigns, no `_CRS` parsing) skips
//! a device whose needed BAR is unassigned.

use alloc::vec::Vec;

use crate::mm::PhysAddr;

/// Max `interrupt-map` entries recorded from the DTB.
pub const MAX_INTX_MAP: usize = 64;

/// One `interrupt-map` entry: (child phys.hi, child pin) -> parent IRQ.
#[derive(Clone, Copy, Debug)]
pub struct IntxEntry {
    pub hi: u32,
    pub pin: u32,
    /// Arch IRQ number (GIC INTID).
    pub irq: u32,
    pub edge: bool,
}

/// A memory window of the host bridge: CPU address `cpu` maps to PCI bus
/// address `pci`, `size` bytes.
#[derive(Clone, Copy, Debug)]
pub struct PciWindow {
    pub cpu: u64,
    pub pci: u64,
    pub size: u64,
}

/// A PCI host bridge as the platform describes it. Fixed-size and `Copy`
/// (aarch64 builds it while parsing the DTB, before the heap exists).
#[derive(Clone, Copy, Debug)]
pub struct PciHostDesc {
    /// Physical address of the ECAM window of `bus_start` (0: no ECAM, use
    /// the x86 0xCF8/0xCFC port mechanism).
    pub ecam: u64,
    pub segment: u16,
    pub bus_start: u8,
    pub bus_end: u8,
    /// 32-bit non-prefetchable memory window (`ranges` space code 0b10).
    pub mem32: Option<PciWindow>,
    /// 64-bit memory window (space code 0b11), if any.
    pub mem64: Option<PciWindow>,
    /// `interrupt-map-mask`: (phys.hi mask, pin mask).
    pub intx_mask: (u32, u32),
    pub intx: [Option<IntxEntry>; MAX_INTX_MAP],
    pub n_intx: usize,
    /// The DTB names an MSI controller (`msi-parent` / `msi-map`); recorded
    /// only (no ITS driver yet).
    pub msi_parent: bool,
    /// MSI device id = requester id + this (`msi-map`: msi-base -
    /// rid-base of its first entry; 0 = identity).
    pub msi_devid_offset: u32,
    /// How it was found, for the discovery lines.
    pub source: &'static str,
}

impl PciHostDesc {
    pub const fn empty(source: &'static str) -> PciHostDesc {
        PciHostDesc {
            ecam: 0,
            segment: 0,
            bus_start: 0,
            bus_end: 0,
            mem32: None,
            mem64: None,
            intx_mask: (0, 0),
            intx: [None; MAX_INTX_MAP],
            n_intx: 0,
            msi_parent: false,
            msi_devid_offset: 0,
            source,
        }
    }

    /// INTx line of function `bdf` with interrupt pin `pin` (1 = INTA)
    /// through the `interrupt-map` (which already encodes the slot
    /// swizzle for devices on the root bus).
    pub fn intx_lookup(&self, bdf: Bdf, pin: u8) -> Option<IntxEntry> {
        let hi = bdf.phys_hi() & self.intx_mask.0;
        let p = pin as u32 & self.intx_mask.1;
        self.intx[..self.n_intx]
            .iter()
            .flatten()
            .find(|e| e.hi & self.intx_mask.0 == hi && e.pin & self.intx_mask.1 == p)
            .copied()
    }
}

/// Bus / device / function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bdf {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl Bdf {
    /// The `phys.hi` cell of an OF PCI address (`00000000 bbbbbbbb
    /// dddddfff 00000000`).
    pub fn phys_hi(self) -> u32 {
        ((self.bus as u32) << 16) | ((self.dev as u32) << 11) | ((self.func as u32) << 8)
    }

    /// PCI requester id (MSI device id).
    pub fn rid(self) -> u32 {
        ((self.bus as u32) << 8) | ((self.dev as u32) << 3) | self.func as u32
    }
}

// Config space header offsets.
pub const CFG_VENDOR: u16 = 0x00;
pub const CFG_DEVICE: u16 = 0x02;
pub const CFG_COMMAND: u16 = 0x04;
pub const CFG_STATUS: u16 = 0x06;
pub const CFG_HEADER_TYPE: u16 = 0x0e;
pub const CFG_BAR0: u16 = 0x10;
pub const CFG_SECONDARY_BUS: u16 = 0x19;
pub const CFG_SUBSYS_ID: u16 = 0x2e;
pub const CFG_CAP_PTR: u16 = 0x34;
pub const CFG_INT_PIN: u16 = 0x3d;

pub const CMD_MEMORY: u16 = 1 << 1;
pub const CMD_BUS_MASTER: u16 = 1 << 2;
pub const CMD_INTX_DISABLE: u16 = 1 << 10;
const STATUS_CAP_LIST: u16 = 1 << 4;

pub const CAP_VENDOR: u8 = 0x09;
pub const CAP_MSIX: u8 = 0x11;

/// A host bridge with its config space mapped.
pub struct Host {
    pub desc: PciHostDesc,
    ecam_va: usize,
}

impl Host {
    /// Map the ECAM window (whole bus range; 2 MiB pages where possible).
    pub fn new(desc: PciHostDesc) -> Host {
        let ecam_va = if desc.ecam != 0 {
            let buses = desc.bus_end as usize - desc.bus_start as usize + 1;
            crate::arch::paging::map_device(PhysAddr(desc.ecam as usize), buses << 20).as_usize()
        } else {
            0
        };
        Host { desc, ecam_va }
    }

    fn ecam_addr(&self, f: Bdf, off: u16) -> usize {
        self.ecam_va
            + (((f.bus - self.desc.bus_start) as usize) << 20
                | (f.dev as usize) << 15
                | (f.func as usize) << 12
                | (off as usize & 0xfff))
    }

    pub fn read32(&self, f: Bdf, off: u16) -> u32 {
        if self.ecam_va != 0 {
            // SAFETY: inside the mapped ECAM window (bus checked by the
            // enumeration against the bus range; off < 4096), uncached.
            unsafe { (self.ecam_addr(f, off & !3) as *const u32).read_volatile() }
        } else {
            port::read32(f, off & !3)
        }
    }

    pub fn read16(&self, f: Bdf, off: u16) -> u16 {
        (self.read32(f, off) >> ((off & 2) * 8)) as u16
    }

    pub fn read8(&self, f: Bdf, off: u16) -> u8 {
        (self.read32(f, off) >> ((off & 3) * 8)) as u8
    }

    pub fn write32(&self, f: Bdf, off: u16, v: u32) {
        if self.ecam_va != 0 {
            // SAFETY: as in `read32`.
            unsafe { (self.ecam_addr(f, off & !3) as *mut u32).write_volatile(v) }
        } else {
            port::write32(f, off & !3, v)
        }
    }

    /// 16-bit write (COMMAND, MSI-X message control: a 32-bit write would
    /// also hit the neighbouring register).
    pub fn write16(&self, f: Bdf, off: u16, v: u16) {
        if self.ecam_va != 0 {
            // SAFETY: as in `read32`; ECAM allows naturally aligned 16-bit
            // accesses.
            unsafe { (self.ecam_addr(f, off & !1) as *mut u16).write_volatile(v) }
        } else {
            port::write16(f, off & !1, v)
        }
    }

    fn in_range(&self, bus: u8) -> bool {
        bus >= self.desc.bus_start && bus <= self.desc.bus_end
    }
}

#[cfg(target_arch = "x86_64")]
mod port {
    //! Configuration mechanism #1 (0xCF8 address, 0xCFC data), x86 only.
    use super::Bdf;
    use crate::arch::port;
    use crate::sync::SpinLock;

    static LOCK: SpinLock<()> = SpinLock::new(());

    fn select(f: Bdf, off: u16) {
        let a = 0x8000_0000
            | ((f.bus as u32) << 16)
            | ((f.dev as u32) << 11)
            | ((f.func as u32) << 8)
            | (off as u32 & 0xfc);
        // SAFETY: CONFIG_ADDRESS; selects a config register, no other effect.
        unsafe { port::outl(0xcf8, a) };
    }

    pub fn read32(f: Bdf, off: u16) -> u32 {
        let _g = LOCK.lock();
        select(f, off);
        // SAFETY: CONFIG_DATA for the register just selected.
        unsafe { port::inl(0xcfc) }
    }

    pub fn write32(f: Bdf, off: u16, v: u32) {
        let _g = LOCK.lock();
        select(f, off);
        // SAFETY: as in `read32`.
        unsafe { port::outl(0xcfc, v) }
    }

    pub fn write16(f: Bdf, off: u16, v: u16) {
        let _g = LOCK.lock();
        select(f, off);
        // SAFETY: as in `read32`; the 16-bit half at `off & 2`.
        unsafe { port::outw(0xcfc + (off & 2), v) }
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod port {
    //! No port I/O config mechanism outside x86: hosts always have ECAM.
    use super::Bdf;
    pub fn read32(_f: Bdf, _off: u16) -> u32 {
        u32::MAX
    }
    pub fn write32(_f: Bdf, _off: u16, _v: u32) {}
    pub fn write16(_f: Bdf, _off: u16, _v: u16) {}
}

/// A decoded memory BAR.
#[derive(Clone, Copy, Debug)]
pub struct Bar {
    /// PCI bus address (0: unassigned).
    pub addr: u64,
    pub size: u64,
    pub is64: bool,
}

/// An enumerated function.
#[derive(Clone, Debug)]
pub struct Function {
    pub bdf: Bdf,
    pub vendor: u16,
    pub device: u16,
    pub header: u8,
    pub bars: [Option<Bar>; 6],
}

/// A capability: (id, config offset).
pub fn capabilities(h: &Host, f: Bdf) -> Vec<(u8, u16)> {
    let mut out = Vec::new();
    if h.read16(f, CFG_STATUS) & STATUS_CAP_LIST == 0 {
        return out;
    }
    let mut p = (h.read8(f, CFG_CAP_PTR) & 0xfc) as u16;
    // At most 48 capabilities fit in the 192 bytes after the header.
    for _ in 0..48 {
        if p < 0x40 {
            break;
        }
        out.push((h.read8(f, p), p));
        p = (h.read8(f, p + 1) & 0xfc) as u16;
    }
    out
}

/// Size the memory BARs of a type-0 header (memory decoding is turned off
/// while sizing and restored afterwards).
fn size_bars(h: &Host, f: Bdf) -> [Option<Bar>; 6] {
    let mut bars = [None; 6];
    let cmd = h.read16(f, CFG_COMMAND);
    h.write16(f, CFG_COMMAND, cmd & !(CMD_MEMORY | 1));
    let mut i = 0;
    while i < 6 {
        let off = CFG_BAR0 + 4 * i as u16;
        let orig = h.read32(f, off);
        if orig & 1 != 0 {
            i += 1; // I/O BAR: ignored
            continue;
        }
        let is64 = (orig >> 1) & 3 == 2 && i < 5;
        h.write32(f, off, u32::MAX);
        let lo = h.read32(f, off);
        h.write32(f, off, orig);
        let (mut mask, mut addr) = ((lo & !0xf) as u64, (orig & !0xf) as u64);
        if is64 {
            let orig_hi = h.read32(f, off + 4);
            h.write32(f, off + 4, u32::MAX);
            let hi = h.read32(f, off + 4);
            h.write32(f, off + 4, orig_hi);
            mask |= (hi as u64) << 32;
            addr |= (orig_hi as u64) << 32;
        } else {
            mask |= 0xffff_ffff_0000_0000;
        }
        let size = (!mask).wrapping_add(1);
        if mask & !0xf != 0 && size != 0 {
            bars[i] = Some(Bar { addr, size, is64 });
        }
        i += if is64 { 2 } else { 1 };
    }
    h.write16(f, CFG_COMMAND, cmd);
    bars
}

/// Enumerate every function reachable from the host's first bus.
pub fn enumerate(h: &Host) -> Vec<Function> {
    let mut out = Vec::new();
    let mut buses = alloc::vec![h.desc.bus_start];
    let mut seen = [false; 256];
    while let Some(bus) = buses.pop() {
        if seen[bus as usize] || !h.in_range(bus) {
            continue;
        }
        seen[bus as usize] = true;
        for dev in 0..32 {
            let f0 = Bdf { bus, dev, func: 0 };
            if h.read16(f0, CFG_VENDOR) == 0xffff {
                continue;
            }
            let nfn = if h.read8(f0, CFG_HEADER_TYPE) & 0x80 != 0 {
                8
            } else {
                1
            };
            for func in 0..nfn {
                let f = Bdf { bus, dev, func };
                let vendor = h.read16(f, CFG_VENDOR);
                if vendor == 0xffff {
                    continue;
                }
                let header = h.read8(f, CFG_HEADER_TYPE) & 0x7f;
                let bars = if header == 0 {
                    size_bars(h, f)
                } else {
                    [None; 6]
                };
                if header == 1 {
                    let sec = h.read8(f, CFG_SECONDARY_BUS);
                    if sec > bus {
                        buses.push(sec);
                    }
                }
                out.push(Function {
                    bdf: f,
                    vendor,
                    device: h.read16(f, CFG_DEVICE),
                    header,
                    bars,
                });
            }
        }
    }
    out
}

/// Bump allocator over the host's memory windows (PCI bus addresses).
pub struct BarAlloc {
    next32: u64,
    next64: u64,
}

impl BarAlloc {
    /// Start past every BAR already assigned inside a window.
    pub fn new(h: &Host, funcs: &[Function]) -> BarAlloc {
        let mut a = BarAlloc {
            next32: h.desc.mem32.map_or(0, |w| w.pci),
            next64: h.desc.mem64.map_or(0, |w| w.pci),
        };
        for b in funcs.iter().flat_map(|f| f.bars.iter().flatten()) {
            if b.addr == 0 {
                continue;
            }
            let end = b.addr + b.size;
            if let Some(w) = h.desc.mem32
                && b.addr >= w.pci
                && b.addr < w.pci + w.size
            {
                a.next32 = a.next32.max(end);
            }
            if let Some(w) = h.desc.mem64
                && b.addr >= w.pci
                && b.addr < w.pci + w.size
            {
                a.next64 = a.next64.max(end);
            }
        }
        a
    }

    fn take(next: &mut u64, w: Option<PciWindow>, size: u64) -> Option<u64> {
        let w = w?;
        let a = next.checked_next_multiple_of(size)?;
        if a.checked_add(size)? > w.pci + w.size {
            return None;
        }
        *next = a + size;
        Some(a)
    }

    /// Assign BAR `i` of `f` if it is unassigned; returns false if it
    /// stays unassigned (no window / window full / write did not stick).
    pub fn assign(&mut self, h: &Host, f: &mut Function, i: usize) -> bool {
        let Some(mut b) = f.bars[i] else {
            return false;
        };
        if b.addr != 0 {
            return true;
        }
        let a = Self::take(&mut self.next32, h.desc.mem32, b.size).or_else(|| {
            if b.is64 {
                Self::take(&mut self.next64, h.desc.mem64, b.size)
            } else {
                None
            }
        });
        let Some(a) = a else {
            return false;
        };
        let off = CFG_BAR0 + 4 * i as u16;
        let cmd = h.read16(f.bdf, CFG_COMMAND);
        h.write16(f.bdf, CFG_COMMAND, cmd & !CMD_MEMORY);
        let flags = h.read32(f.bdf, off) & 0xf;
        h.write32(f.bdf, off, a as u32 | flags);
        let mut back = (h.read32(f.bdf, off) & !0xf) as u64;
        if b.is64 {
            h.write32(f.bdf, off + 4, (a >> 32) as u32);
            back |= (h.read32(f.bdf, off + 4) as u64) << 32;
        }
        h.write16(f.bdf, CFG_COMMAND, cmd);
        if back != a {
            return false;
        }
        b.addr = a;
        f.bars[i] = Some(b);
        true
    }
}

/// CPU physical address of PCI bus address `pci` (through the window that
/// contains it; identity without windows, as on x86).
pub fn bus_to_cpu(h: &Host, pci: u64) -> u64 {
    for w in [h.desc.mem32, h.desc.mem64].into_iter().flatten() {
        if pci >= w.pci && pci < w.pci + w.size {
            return pci - w.pci + w.cpu;
        }
    }
    pci
}

/// An MSI message bound to an arch IRQ number (`arch::irq::alloc_msi`).
#[derive(Clone, Copy, Debug)]
pub struct MsiMsg {
    /// What `arch::irq::register` takes for it (x86_64: `MSI_IRQ_BASE` +
    /// vector; aarch64: the LPI INTID).
    pub irq: u32,
    pub addr: u64,
    pub data: u32,
}

/// Discover the platform's PCI hosts and register every virtio-pci
/// function with `drivers::virtio`. Returns the number registered.
/// `patina.pci.irq=intx` prefers INTx over MSI-X (aarch64 tests).
pub fn init(cmdline: &patina_cmdline::Cmdline) -> usize {
    let pref = match cmdline.get("patina.pci.irq") {
        Some(Some("intx")) => super::virtio::pci::IrqPref::Intx,
        _ => super::virtio::pci::IrqPref::Auto,
    };
    let mut n = 0;
    for desc in crate::arch::devices::pci_hosts() {
        let h = Host::new(desc);
        let mut funcs = enumerate(&h);
        let mut alloc = BarAlloc::new(&h, &funcs);
        for f in funcs.iter_mut() {
            if f.vendor == super::virtio::pci::VENDOR
                && (0x1000..=0x107f).contains(&f.device)
                && f.header == 0
            {
                n += super::virtio::pci::probe(&h, f, &mut alloc, pref) as usize;
            }
        }
    }
    n
}
