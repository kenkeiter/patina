//! Minimal GICv3 ITS driver: MSI/MSI-X for PCI devices (Phase 4, D70).
//!
//! Cloud Hypervisor's aarch64 virtio-pci devices have no INTx at all (pin
//! 0, empty `interrupt-map`): their only interrupt is MSI-X, which KVM
//! delivers through the ITS as an LPI. QEMU `virt` (`gic-version=3`) has
//! an ITS too, which is how this is tested.
//!
//! Set up once, on the first [`alloc`] (boot CPU, after the frame
//! allocator):
//! * LPIs on the boot CPU's redistributor: a configuration table for
//!   INTIDs 8192..16383 (`GICR_PROPBASER`, IDbits 14; one byte per LPI:
//!   priority | group 1 | enable) and a zeroed 64 KiB-aligned pending
//!   table (`GICR_PENDBASER`), then `GICR_CTLR.EnableLPIs`.
//! * The ITS: a one-page command queue (`GITS_CBASER`), flat device and
//!   collection tables (`GITS_BASER<n>` by type), `GITS_CTLR.Enabled`,
//!   then `MAPC` collection 0 -> the boot CPU's redistributor (physical
//!   address or processor number, per `GITS_TYPER.PTA`) and `SYNC`.
//!
//! Each [`alloc`] maps one event: `MAPD` (device id = PCI requester id
//! through `msi-map`, an ITT page, 2 event ids), `MAPTI` event 0 -> the
//! next free LPI in collection 0, `INV`, `SYNC`. The MSI message is
//! address = `GITS_TRANSLATER`, data = 0 (the event id). Commands are
//! polled to completion (`GITS_CREADR` catches up with `GITS_CWRITER`).
//! Every LPI targets the boot CPU, like the SPIs (D59).

use core::sync::atomic::{Ordering, fence};

use super::paging::map_device;
use super::gic;
use crate::mm::{PAGE_SIZE, PhysAddr, frame, phys_to_virt};
use crate::sync::SpinLock;

/// First LPI INTID.
pub const LPI_BASE: u32 = 8192;
/// INTID bits configured (LPIs 8192..16383).
const ID_BITS: u32 = 14;
/// LPIs this driver hands out (handler table size in `irq.rs`).
pub const MAX_LPIS: u32 = 64;
/// LPI configuration byte: priority 0x80 (as SPIs), group 1 (RES1),
/// enabled.
const LPI_PROP: u8 = 0x80 | 0b10 | 1;

// ITS control frame.
const GITS_CTLR: usize = 0x0000;
const GITS_TYPER: usize = 0x0008;
const GITS_CBASER: usize = 0x0080;
const GITS_CWRITER: usize = 0x0088;
const GITS_CREADR: usize = 0x0090;
const GITS_BASER: usize = 0x0100;
/// Translation frame (second 64 KiB page) + GITS_TRANSLATER.
const GITS_TRANSLATER: usize = 0x1_0040;

// Redistributor RD_base frame.
const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008;
const GICR_PROPBASER: usize = 0x0070;
const GICR_PENDBASER: usize = 0x0078;

/// Inner shareable, inner write-back read/write-allocate (Linux's
/// defaults for GIC tables).
const SH_INNER: u64 = 1 << 10;
const CACHE_RAWAWB: u64 = 7;

const BASER_VALID: u64 = 1 << 63;
const BASER_TYPE_DEVICE: u64 = 1;
const BASER_TYPE_COLLECTION: u64 = 4;

const CMD_SYNC: u64 = 0x05;
const CMD_MAPD: u64 = 0x08;
const CMD_MAPC: u64 = 0x09;
const CMD_MAPTI: u64 = 0x0a;
const CMD_INV: u64 = 0x0c;

struct Its {
    /// ITS frame VA and PA.
    va: usize,
    pa: usize,
    /// Command queue PA (one page) and the next write offset.
    cmdq: usize,
    cw: usize,
    /// MAPC/SYNC target (RDbase field value, already shifted).
    target: u64,
    /// LPI configuration table PA.
    prop: usize,
    next_lpi: u32,
    /// Device ids covered by the device table.
    max_devid: u32,
    itt_entry: usize,
}

/// `None` inside: not tried yet; `Some(None)`: no usable ITS.
static ITS: SpinLock<Option<Option<Its>>> = SpinLock::new(None);
static DESC: SpinLock<Option<(usize, usize)>> = SpinLock::new(None);

/// Record the DTB's ITS frame (boot).
pub(super) fn set_desc(d: Option<(usize, usize)>) {
    *DESC.lock() = d;
}

fn r64(a: usize) -> u64 {
    // SAFETY: `a` is inside a GIC register frame mapped uncached.
    unsafe { (a as *const u64).read_volatile() }
}
fn w64(a: usize, v: u64) {
    // SAFETY: as in `r64`.
    unsafe { (a as *mut u64).write_volatile(v) }
}
fn r32(a: usize) -> u32 {
    // SAFETY: as in `r64`.
    unsafe { (a as *const u32).read_volatile() }
}
fn w32(a: usize, v: u32) {
    // SAFETY: as in `r64`.
    unsafe { (a as *mut u32).write_volatile(v) }
}

/// Make normal-memory table writes visible to the GIC.
fn publish() {
    fence(Ordering::SeqCst);
    // SAFETY: barrier only.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
}

fn alloc_zeroed(bytes: usize) -> Option<usize> {
    Some(frame::alloc_frames_zeroed(frame::order_for(bytes.max(PAGE_SIZE)))?.0)
}

impl Its {
    fn setup() -> Result<Its, &'static str> {
        let (pa, size) = (*DESC.lock()).ok_or("no ITS in the device tree")?;
        if !gic::is_v3() {
            return Err("GIC is not v3");
        }
        if !gic::dist_has_lpis() {
            return Err("GICD_TYPER.LPIS = 0");
        }
        let rd = gic::rd_base_va(0).ok_or("boot CPU redistributor unknown")?;
        if r64(rd + GICR_TYPER) & 1 == 0 {
            return Err("GICR_TYPER.PLPIS = 0");
        }
        let va = map_device(PhysAddr(pa), size.max(0x2_0000)).as_usize();

        // LPI tables on the boot CPU's redistributor.
        let n_lpis = (1usize << ID_BITS) - LPI_BASE as usize;
        let prop = alloc_zeroed(n_lpis).ok_or("no memory")?;
        let pend = alloc_zeroed(64 * 1024).ok_or("no memory")?; // 64 KiB aligned
        publish();
        w64(
            rd + GICR_PROPBASER,
            prop as u64 | SH_INNER | (CACHE_RAWAWB << 7) | (ID_BITS as u64 - 1),
        );
        w64(
            rd + GICR_PENDBASER,
            (1 << 62) | pend as u64 | SH_INNER | (CACHE_RAWAWB << 7),
        );
        w32(rd + GICR_CTLR, r32(rd + GICR_CTLR) | 1); // EnableLPIs

        // ITS: disabled while programming.
        w32(va + GITS_CTLR, 0);
        for _ in 0..10_000_000 {
            if r32(va + GITS_CTLR) & (1 << 31) != 0 {
                break; // quiescent
            }
            core::hint::spin_loop();
        }
        let typer = r64(va + GITS_TYPER);
        let itt_entry = (((typer >> 4) & 0xf) + 1) as usize;
        let devbits = (((typer >> 13) & 0x1f) + 1) as u32;
        let pta = typer & (1 << 19) != 0;

        let cmdq = alloc_zeroed(PAGE_SIZE).ok_or("no memory")?;
        w64(
            va + GITS_CBASER,
            BASER_VALID | (CACHE_RAWAWB << 59) | cmdq as u64 | SH_INNER,
        );
        w64(va + GITS_CWRITER, 0);

        let mut max_devid = 0;
        for n in 0..8 {
            let reg = va + GITS_BASER + 8 * n;
            let b = r64(reg);
            let ty = (b >> 56) & 7;
            let esz = (((b >> 48) & 0x1f) + 1) as usize;
            let want = match ty {
                BASER_TYPE_DEVICE => (1usize << devbits.min(16)) * esz,
                BASER_TYPE_COLLECTION => 256 * esz,
                _ => continue,
            };
            // Flat table, 4 KiB pages, at most 64 pages (device ids up to
            // 32768 with 8-byte entries; PCI requester ids are far below).
            let pages = want.div_ceil(PAGE_SIZE).clamp(1, 64);
            let t = alloc_zeroed(pages * PAGE_SIZE).ok_or("no memory")?;
            let v = BASER_VALID
                | (CACHE_RAWAWB << 59)
                | (b & (0x7 << 56 | 0x1f << 48))
                | t as u64
                | SH_INNER
                | (pages as u64 - 1);
            w64(reg, v);
            if (r64(reg) >> 8) & 3 != 0 {
                return Err("ITS does not take 4 KiB table pages");
            }
            if ty == BASER_TYPE_DEVICE {
                max_devid = ((pages * PAGE_SIZE) / esz).min(u32::MAX as usize) as u32;
            }
        }
        if max_devid == 0 {
            return Err("ITS has no device table");
        }
        w32(va + GITS_CTLR, 1);

        let target = if pta {
            (gic::rd_base_pa(0).ok_or("boot CPU redistributor unknown")? as u64 >> 16) << 16
        } else {
            ((r64(rd + GICR_TYPER) >> 8) & 0xffff) << 16
        };
        let mut its = Its {
            va,
            pa,
            cmdq,
            cw: 0,
            target,
            prop,
            next_lpi: LPI_BASE,
            max_devid,
            itt_entry,
        };
        its.cmd([CMD_MAPC, 0, (1 << 63) | target, 0]); // ICID 0
        its.sync()?;
        crate::kprintln!(
            "[patina] gic: its @ {:#x} ({} device ids, itt entry {} bytes, {}), lpis {}-{}",
            pa,
            max_devid,
            itt_entry,
            if pta { "pta" } else { "processor numbers" },
            LPI_BASE,
            LPI_BASE + MAX_LPIS - 1
        );
        Ok(its)
    }

    /// Queue one 32-byte command (not yet waited for).
    fn cmd(&mut self, c: [u64; 4]) {
        let slot = phys_to_virt(PhysAddr(self.cmdq + self.cw)).as_usize();
        for (i, w) in c.iter().enumerate() {
            // SAFETY: a 32-byte slot inside the command queue page we own.
            unsafe { ((slot + 8 * i) as *mut u64).write_volatile(*w) };
        }
        publish();
        self.cw = (self.cw + 32) % PAGE_SIZE;
        w64(self.va + GITS_CWRITER, self.cw as u64);
    }

    /// `SYNC` and wait until the ITS consumed every queued command.
    fn sync(&mut self) -> Result<(), &'static str> {
        self.cmd([CMD_SYNC, 0, self.target, 0]);
        for _ in 0..10_000_000 {
            let r = r64(self.va + GITS_CREADR);
            if r & 1 != 0 {
                return Err("ITS command queue stalled");
            }
            if (r & 0xf_ffe0) as usize == self.cw {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err("ITS command timeout")
    }

    fn map(&mut self, devid: u32) -> Result<crate::drivers::pci::MsiMsg, &'static str> {
        if self.next_lpi >= LPI_BASE + MAX_LPIS {
            return Err("out of LPIs");
        }
        if devid >= self.max_devid {
            return Err("device id beyond the device table");
        }
        let lpi = self.next_lpi;
        // The LPI's configuration byte before it is mapped.
        let p = phys_to_virt(PhysAddr(self.prop + (lpi - LPI_BASE) as usize)).as_usize();
        // SAFETY: a byte of the LPI configuration table we own.
        unsafe { (p as *mut u8).write_volatile(LPI_PROP) };
        publish();
        // ITT: 2 events (Size = event id bits - 1 = 0), 256-byte aligned.
        let itt = alloc_zeroed((2 * self.itt_entry).max(256)).ok_or("no memory")?;
        publish();
        let dev = (devid as u64) << 32;
        self.cmd([CMD_MAPD | dev, 0, (1 << 63) | itt as u64, 0]);
        self.cmd([CMD_MAPTI | dev, (lpi as u64) << 32, 0, 0]); // event 0, ICID 0
        self.cmd([CMD_INV | dev, 0, 0, 0]);
        self.sync()?;
        self.next_lpi += 1;
        Ok(crate::drivers::pci::MsiMsg {
            irq: lpi,
            addr: (self.pa + GITS_TRANSLATER) as u64,
            data: 0,
        })
    }
}

/// Map an MSI from device `devid` (PCI requester id after `msi-map`) to a
/// fresh LPI on the boot CPU. `None` without a usable ITS (logged once).
pub fn alloc(devid: u32) -> Option<crate::drivers::pci::MsiMsg> {
    let mut g = ITS.lock();
    if g.is_none() {
        *g = Some(match Its::setup() {
            Ok(i) => Some(i),
            Err(e) => {
                if DESC.lock().is_some() {
                    crate::kprintln!("[patina] gic: its unusable: {}", e);
                }
                None
            }
        });
    }
    let its = g.as_mut()?.as_mut()?;
    match its.map(devid) {
        Ok(m) => Some(m),
        Err(e) => {
            crate::kprintln!("[patina] gic: its: device {:#x}: {}", devid, e);
            None
        }
    }
}

