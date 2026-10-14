//! GIC driver: GICv3 (system-register CPU interface, redistributors) and a
//! GICv2 fallback (memory-mapped CPU interface), found through the DTB.
//!
//! [`init`] sets up the distributor once and the boot CPU's interface;
//! Phase 2 secondary CPUs call [`init_cpu`]. Interrupts are configured as
//! non-secure Group 1 (GICv3) with priority 0x80, EOImode 0 (EOI both drops
//! priority and deactivates). [`ack`] / [`eoi`] work on the calling CPU.
//!
//! SGIs (IPIs): [`send_sgi`] writes `ICC_SGI1R_EL1` (v3: target by the
//! affinity of the destination CPU's MPIDR) or `GICD_SGIR` (v2: target by
//! the CPU-interface mask each CPU read from its banked `GICD_ITARGETSR0`
//! in [`init_cpu`]). [`init_cpu`] enables [`SGI_RESCHEDULE`],
//! [`SGI_STOP`] and [`SGI_TLB_FLUSH`] on every CPU. On GICv2 the IAR value of an SGI carries the
//! source CPU in bits 12:10 and must be written back unchanged to EOIR, so
//! [`ack`] returns the raw value for [`eoi`].

use core::arch::asm;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use super::cpu;
use super::dtb::GicDesc;
use super::paging::map_device;
use crate::mm::PhysAddr;
use crate::sync::{OnceCell, SpinLock};

/// Default priority for every enabled interrupt (lower = more urgent).
const PRIORITY: u8 = 0x80;
/// INTIDs 1020-1023 are special (1023 = spurious).
const SPECIAL_INTID: u32 = 1020;

// GICD registers (both versions).
const GICD_CTLR: usize = 0x0000;
const GICD_IGROUPR: usize = 0x0080;
const GICD_ISENABLER: usize = 0x0100;
const GICD_IPRIORITYR: usize = 0x0400;
const GICD_ITARGETSR: usize = 0x0800;
const GICD_SGIR: usize = 0x0f00;
const GICD_IROUTER: usize = 0x6000;
const GICD_CTLR_RWP: u32 = 1 << 31;
// GICv3 GICD_CTLR (non-secure view / DS=1 view share these bit positions
// for what we set): EnableGrp1(NS) bit 0 (DS=1: Grp0), EnableGrp1A bit 1
// (DS=1: Grp1), ARE(_NS) bit 4. Same value as Linux's gic_dist_init.
const GICD_CTLR_V3: u32 = (1 << 4) | (1 << 1) | (1 << 0);

// GICv3 redistributor: RD_base frame, then SGI_base frame at +64 KiB.
const GICR_TYPER: usize = 0x0008;
const GICR_WAKER: usize = 0x0014;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_SGI: usize = 0x1_0000;
const GICR_IGROUPR0: usize = GICR_SGI + 0x0080;
const GICR_ISENABLER0: usize = GICR_SGI + 0x0100;
const GICR_IPRIORITYR: usize = GICR_SGI + 0x0400;
const GICR_IGRPMODR0: usize = GICR_SGI + 0x0d00;

// GICv2 CPU interface.
const GICC_CTLR: usize = 0x00;
const GICC_PMR: usize = 0x04;
const GICC_BPR: usize = 0x08;
const GICC_IAR: usize = 0x0c;
const GICC_EOIR: usize = 0x10;

#[derive(Clone, Copy)]
struct Gic {
    version: u32,
    /// GICD virtual base.
    dist: usize,
    /// GICv3: redistributor region VA and size; GICv2: GICC VA.
    cpu: usize,
    cpu_size: usize,
}

static DESC: OnceCell<GicDesc> = OnceCell::new();
static GIC: OnceCell<Gic> = OnceCell::new();
static INIT_LOCK: SpinLock<()> = SpinLock::new(());
/// Per-CPU GICv3 RD_base VA (0 = not yet located).
static RD_BASE: [AtomicUsize; cpu::MAX_CPUS] = [const { AtomicUsize::new(0) }; cpu::MAX_CPUS];
/// Per-CPU GICv2 CPU-interface mask (`GICD_ITARGETSR0` as read on that CPU).
static V2_TARGET: [AtomicU8; cpu::MAX_CPUS] = [const { AtomicU8::new(0) }; cpu::MAX_CPUS];

/// SGI used for `IpiKind::Reschedule`.
pub const SGI_RESCHEDULE: u32 = 0;
/// SGI used for `IpiKind::Stop`.
pub const SGI_STOP: u32 = 1;
/// SGI used for `IpiKind::TlbFlush` (never sent on aarch64: `tlbi ...is`
/// broadcasts; handled for symmetry).
pub const SGI_TLB_FLUSH: u32 = 2;

/// Record the DTB's GIC description (boot code).
pub(super) fn set_desc(d: GicDesc) {
    let _ = DESC.set(d);
}

fn rd32(addr: usize) -> u32 {
    // SAFETY: `addr` is inside a GIC register block mapped by `init`.
    unsafe { (addr as *const u32).read_volatile() }
}
fn wr32(addr: usize, v: u32) {
    // SAFETY: as above.
    unsafe { (addr as *mut u32).write_volatile(v) }
}
fn rd64(addr: usize) -> u64 {
    // SAFETY: as above.
    unsafe { (addr as *const u64).read_volatile() }
}
fn wr64(addr: usize, v: u64) {
    // SAFETY: as above.
    unsafe { (addr as *mut u64).write_volatile(v) }
}
fn wr8(addr: usize, v: u8) {
    // SAFETY: as above (IPRIORITYR is byte-accessible).
    unsafe { (addr as *mut u8).write_volatile(v) }
}

fn spin_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..10_000_000 {
        if done() {
            return;
        }
        core::hint::spin_loop();
    }
    panic!("gic: timeout waiting for {}", what);
}

macro_rules! sysreg_w {
    ($reg:literal, $v:expr) => {
        // SAFETY: GICv3 CPU-interface register write at EL1 (ICC_SRE_EL1.SRE
        // is set first); affects only this CPU's interrupt delivery.
        unsafe { asm!(concat!("msr ", $reg, ", {}"), in(reg) $v as u64, options(nostack, preserves_flags)) }
    };
}
macro_rules! sysreg_r {
    ($reg:literal) => {{
        let v: u64;
        // SAFETY: GICv3 CPU-interface register read at EL1.
        unsafe { asm!(concat!("mrs {}, ", $reg), out(reg) v, options(nostack, preserves_flags)) };
        v
    }};
}

// GICv3 CPU-interface registers by encoding (no assembler feature needed).
// ICC_SRE_EL1 S3_0_C12_C12_5, ICC_PMR_EL1 S3_0_C4_C6_0, ICC_BPR1_EL1
// S3_0_C12_C12_3, ICC_CTLR_EL1 S3_0_C12_C12_4, ICC_IGRPEN1_EL1
// S3_0_C12_C12_7, ICC_IAR1_EL1 S3_0_C12_C12_0, ICC_EOIR1_EL1 S3_0_C12_C12_1.

/// Map the GIC and initialize the distributor (once) and this CPU's
/// interface. Idempotent per CPU.
pub fn init() {
    let _g = INIT_LOCK.lock();
    if GIC.get().is_none() {
        let d = *DESC
            .get()
            .expect("gic: no GICv2/GICv3 node in the device tree");
        let dist = map_device(PhysAddr(d.dist.0), d.dist.1).as_usize();
        let cpu = map_device(PhysAddr(d.cpu.0), d.cpu.1).as_usize();
        let gic = Gic {
            version: d.version,
            dist,
            cpu,
            cpu_size: d.cpu.1,
        };
        init_dist(&gic);
        let _ = GIC.set(gic);
        crate::kprintln!(
            "[patina] gic: v{} dist @ {:#x}, {} @ {:#x}",
            d.version,
            d.dist.0,
            if d.version == 3 { "redist" } else { "cpu" },
            d.cpu.0
        );
    }
    drop(_g);
    init_cpu();
}

fn gic() -> &'static Gic {
    GIC.get().expect("gic: not initialized")
}

fn init_dist(g: &Gic) {
    if g.version == 3 {
        wr32(g.dist + GICD_CTLR, 0);
        spin_until("GICD_CTLR.RWP", || {
            rd32(g.dist + GICD_CTLR) & GICD_CTLR_RWP == 0
        });
        wr32(g.dist + GICD_CTLR, GICD_CTLR_V3);
        spin_until("GICD_CTLR.RWP", || {
            rd32(g.dist + GICD_CTLR) & GICD_CTLR_RWP == 0
        });
    } else {
        // Group 0 interrupts signalled as IRQ; enable both groups.
        wr32(g.dist + GICD_CTLR, 0b11);
    }
}

/// Affinity of this CPU in GICR_TYPER[63:32] format (Aff3.Aff2.Aff1.Aff0).
fn my_affinity() -> u64 {
    let m = cpu::mpidr();
    (m & 0xff_ffff) | ((m >> 32) & 0xff) << 24
}

/// Find this CPU's redistributor (GICv3).
fn find_rd(g: &Gic) -> usize {
    let aff = my_affinity();
    let mut off = 0;
    while off < g.cpu_size {
        let typer = rd64(g.cpu + off + GICR_TYPER);
        if typer >> 32 == aff {
            return g.cpu + off;
        }
        if typer & (1 << 4) != 0 {
            break; // Last
        }
        // GICv4 (VLPIS) frames have two extra 64 KiB pages.
        off += if typer & (1 << 1) != 0 {
            0x4_0000
        } else {
            0x2_0000
        };
    }
    panic!("gic: no redistributor for affinity {:#x}", aff);
}

fn rd_base() -> usize {
    let id = cpu::id().min(cpu::MAX_CPUS - 1);
    RD_BASE[id].load(Ordering::Relaxed)
}

/// Initialize this CPU's GIC interface (redistributor + CPU interface).
pub fn init_cpu() {
    let g = gic();
    if g.version == 3 {
        let rd = find_rd(g);
        RD_BASE[cpu::id().min(cpu::MAX_CPUS - 1)].store(rd, Ordering::Relaxed);
        let w = rd32(rd + GICR_WAKER);
        wr32(rd + GICR_WAKER, w & !GICR_WAKER_PROCESSOR_SLEEP);
        spin_until("GICR_WAKER.ChildrenAsleep", || {
            rd32(rd + GICR_WAKER) & GICR_WAKER_CHILDREN_ASLEEP == 0
        });
        // SGIs/PPIs: all Group 1 non-secure (writes are ignored if the
        // registers are secure-only, then firmware set them up).
        wr32(rd + GICR_IGROUPR0, !0);
        wr32(rd + GICR_IGRPMODR0, 0);

        let sre = sysreg_r!("S3_0_C12_C12_5");
        sysreg_w!("S3_0_C12_C12_5", sre | 1);
        // SAFETY: barrier only.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
        if sysreg_r!("S3_0_C12_C12_5") & 1 == 0 {
            panic!("gic: ICC_SRE_EL1.SRE cannot be set (system registers disabled)");
        }
        sysreg_w!("S3_0_C4_C6_0", 0xffu64); // PMR: allow all priorities
        sysreg_w!("S3_0_C12_C12_3", 0u64); // BPR1: no subpriority grouping
        let ctlr = sysreg_r!("S3_0_C12_C12_4");
        sysreg_w!("S3_0_C12_C12_4", ctlr & !(1u64 << 1)); // EOImode = 0
        sysreg_w!("S3_0_C12_C12_7", 1u64); // IGRPEN1
        // SAFETY: barrier only.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
    } else {
        // Banked: the byte for INTID 0 reads as this CPU's interface mask.
        let mask = rd32(g.dist + GICD_ITARGETSR) as u8;
        V2_TARGET[cpu::id().min(cpu::MAX_CPUS - 1)].store(mask, Ordering::Relaxed);
        wr32(g.cpu + GICC_PMR, 0xff);
        wr32(g.cpu + GICC_BPR, 0);
        wr32(g.cpu + GICC_CTLR, 0b11);
    }
    enable(SGI_RESCHEDULE);
    enable(SGI_STOP);
    enable(SGI_TLB_FLUSH);
}

/// Send SGI `sgi` (0-15) to the CPU with logical id `cpu` and hardware
/// affinity `mpidr`. Lock-free; safe with IRQs masked. Prior normal-memory
/// stores are ordered before the interrupt (`dsb ishst`).
pub fn send_sgi(cpu: usize, mpidr: u64, sgi: u32) {
    let g = gic();
    // SAFETY: barrier only.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    if g.version == 3 {
        // ICC_SGI1R_EL1: Aff3 [55:48], RS [47:44], Aff2 [39:32],
        // INTID [27:24], Aff1 [23:16], TargetList [15:0] (Aff0 % 16).
        let aff0 = mpidr & 0xff;
        let aff1 = (mpidr >> 8) & 0xff;
        let aff2 = (mpidr >> 16) & 0xff;
        let aff3 = (mpidr >> 32) & 0xff;
        let v = (aff3 << 48)
            | ((aff0 >> 4) << 44)
            | (aff2 << 32)
            | (((sgi & 0xf) as u64) << 24)
            | (aff1 << 16)
            | (1 << (aff0 & 0xf));
        sysreg_w!("S3_0_C12_C11_5", v);
        // SAFETY: barrier only.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
    } else {
        let mask = V2_TARGET[cpu.min(cpu::MAX_CPUS - 1)].load(Ordering::Relaxed) as u32;
        // TargetListFilter 0 (use the list), CPUTargetList [23:16],
        // NSATT 0 (Group 0, which is where `init_cpu` leaves SGIs on v2).
        wr32(g.dist + GICD_SGIR, (mask << 16) | (sgi & 0xf));
    }
}

/// Enable `intid` with the default priority: SGIs/PPIs (< 32) on this CPU,
/// SPIs routed to this CPU.
pub fn enable(intid: u32) {
    let g = gic();
    let i = intid as usize;
    let bit = 1u32 << (i % 32);
    if g.version == 3 {
        if i < 32 {
            let rd = rd_base();
            wr8(rd + GICR_IPRIORITYR + i, PRIORITY);
            wr32(rd + GICR_ISENABLER0, bit);
        } else {
            let gr = g.dist + GICD_IGROUPR + 4 * (i / 32);
            wr32(gr, rd32(gr) | bit);
            wr8(g.dist + GICD_IPRIORITYR + i, PRIORITY);
            wr64(g.dist + GICD_IROUTER + 8 * i, my_affinity_router());
            wr32(g.dist + GICD_ISENABLER + 4 * (i / 32), bit);
        }
    } else {
        wr8(g.dist + GICD_IPRIORITYR + i, PRIORITY);
        if i >= 32 {
            wr8(g.dist + GICD_ITARGETSR + i, 1 << cpu::id().min(7));
        }
        wr32(g.dist + GICD_ISENABLER + 4 * (i / 32), bit);
    }
}

/// This CPU's affinity in GICD_IROUTER format (Aff3 at [39:32]).
fn my_affinity_router() -> u64 {
    let m = cpu::mpidr();
    (m & 0xff_ffff) | (m & (0xff << 32))
}

/// Acknowledge the highest-priority pending interrupt on this CPU:
/// `(intid, token)`, where `token` is what [`eoi`] needs (the raw IAR value,
/// which on GICv2 includes the SGI source CPU). `None` for a
/// spurious/special INTID (nothing to EOI).
pub fn ack() -> Option<(u32, u32)> {
    let g = gic();
    let (intid, raw) = if g.version == 3 {
        let v = sysreg_r!("S3_0_C12_C12_0") as u32 & 0xff_ffff;
        // SAFETY: barrier only; orders the ack before the device accesses.
        unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
        (v, v)
    } else {
        let v = rd32(g.cpu + GICC_IAR);
        (v & 0x3ff, v & 0x1fff)
    };
    (intid < SPECIAL_INTID).then_some((intid, raw))
}

/// End of interrupt for the token returned by [`ack`] (priority drop +
/// deactivate).
pub fn eoi(token: u32) {
    let g = gic();
    if g.version == 3 {
        sysreg_w!("S3_0_C12_C12_1", token);
        // SAFETY: barrier only.
        unsafe { asm!("isb", options(nostack, preserves_flags)) };
    } else {
        wr32(g.cpu + GICC_EOIR, token);
    }
}
