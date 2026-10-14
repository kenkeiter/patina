//! I/O APIC: device interrupt routing (Phase 3).
//!
//! The I/O APICs come from the MADT (address + GSI base, `platform.rs`);
//! without ACPI one I/O APIC at the architectural default 0xFEC00000 with
//! GSI base 0 is assumed (Firecracker's MP table has the same one). [`init`]
//! maps each (uncached) and masks every redirection entry. [`route`]
//! programs one entry: fixed delivery, physical destination = the boot
//! CPU's APIC id (device interrupts are handled on CPU 0; the scheduler
//! still runs everywhere), trigger/polarity as given, and unmasks it.
//! Legacy ISA IRQs go through the MADT interrupt source overrides
//! ([`isa_to_gsi`]); virtio-mmio GSIs come from the DSDT or the cmdline.

use core::sync::atomic::{AtomicBool, Ordering};

use super::acpi::MAX_IOAPICS;
use super::{paging, platform, smp};
use crate::mm::PhysAddr;
use crate::sync::SpinLock;

const REG_VER: u32 = 0x01;
const REG_REDTBL: u32 = 0x10;
const RTE_MASKED: u32 = 1 << 16;
const RTE_LEVEL: u32 = 1 << 15;
const RTE_ACTIVE_LOW: u32 = 1 << 13;
const DEFAULT_BASE: u32 = 0xfec0_0000;

#[derive(Clone, Copy)]
struct Ioapic {
    pa: u32,
    va: usize,
    gsi_base: u32,
    pins: u32,
}

static IOAPICS: SpinLock<([Option<Ioapic>; MAX_IOAPICS], bool)> =
    SpinLock::new(([None; MAX_IOAPICS], false));
static INIT: AtomicBool = AtomicBool::new(false);

fn read(va: usize, reg: u32) -> u32 {
    // SAFETY: `va` is the uncached mapping of an I/O APIC (IOREGSEL at 0,
    // IOWIN at 0x10); callers hold the IOAPICS lock, so the select/access
    // pair is not interleaved.
    unsafe {
        (va as *mut u32).write_volatile(reg);
        ((va + 0x10) as *const u32).read_volatile()
    }
}

fn write(va: usize, reg: u32, v: u32) {
    // SAFETY: as in `read`.
    unsafe {
        (va as *mut u32).write_volatile(reg);
        ((va + 0x10) as *mut u32).write_volatile(v);
    }
}

/// Map the I/O APICs and mask every pin (once; after the final page
/// tables are live).
pub fn init() {
    if INIT.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut list = [(0u8, DEFAULT_BASE, 0u32); MAX_IOAPICS];
    let mut n = platform::with(|f| match f.irq {
        Some(i) if i.n_ioapics > 0 => {
            list[..i.n_ioapics].copy_from_slice(&i.ioapics[..i.n_ioapics]);
            i.n_ioapics
        }
        _ => 0,
    });
    let from_madt = n > 0;
    if n == 0 {
        n = 1;
    }
    let mut g = IOAPICS.lock();
    for (slot, &(_, pa, gsi_base)) in g.0.iter_mut().zip(&list[..n]) {
        let va = paging::map_device(PhysAddr(pa as usize), 0x20).as_usize();
        let ver = read(va, REG_VER);
        if ver == 0xffff_ffff {
            continue; // nothing there
        }
        let pins = ((ver >> 16) & 0xff) + 1;
        for pin in 0..pins {
            write(va, REG_REDTBL + 2 * pin, RTE_MASKED);
            write(va, REG_REDTBL + 2 * pin + 1, 0);
        }
        *slot = Some(Ioapic {
            pa,
            va,
            gsi_base,
            pins,
        });
    }
    g.1 = true;
    let found: [Option<Ioapic>; MAX_IOAPICS] = g.0;
    drop(g);
    for a in found.iter().flatten() {
        crate::kprintln!(
            "[patina] ioapic: @ {:#x} gsi {}-{} ({}), all pins masked",
            a.pa,
            a.gsi_base,
            a.gsi_base + a.pins - 1,
            if from_madt { "madt" } else { "default address" }
        );
    }
}

/// Program `gsi` to deliver `vector` to the boot CPU and unmask it.
/// Returns false if no I/O APIC serves `gsi`.
pub fn route(gsi: u32, vector: u8, level: bool, active_low: bool) -> bool {
    init();
    let dest = smp::apic_id(0);
    let g = IOAPICS.lock();
    for a in g.0.iter().flatten() {
        if gsi >= a.gsi_base && gsi < a.gsi_base + a.pins {
            let pin = gsi - a.gsi_base;
            let mut low = vector as u32;
            if level {
                low |= RTE_LEVEL;
            }
            if active_low {
                low |= RTE_ACTIVE_LOW;
            }
            write(a.va, REG_REDTBL + 2 * pin + 1, (dest & 0xff) << 24);
            write(a.va, REG_REDTBL + 2 * pin, low);
            return true;
        }
    }
    false
}

/// Mask `gsi` again.
pub fn mask(gsi: u32) {
    let g = IOAPICS.lock();
    for a in g.0.iter().flatten() {
        if gsi >= a.gsi_base && gsi < a.gsi_base + a.pins {
            let pin = gsi - a.gsi_base;
            let low = read(a.va, REG_REDTBL + 2 * pin);
            write(a.va, REG_REDTBL + 2 * pin, low | RTE_MASKED);
        }
    }
}

/// GSI, level-triggered and active-low for ISA IRQ `irq` after the MADT
/// interrupt source overrides (ISA default: same number, edge, high).
pub fn isa_to_gsi(irq: u8) -> (u32, bool, bool) {
    platform::with(|f| {
        let Some(i) = f.irq else {
            return (irq as u32, false, false);
        };
        for &(src, gsi, flags) in &i.isos[..i.n_isos] {
            if src == irq {
                // MPS INTI flags: polarity [1:0] (3 = low), trigger [3:2] (3 = level).
                return (gsi, (flags >> 2) & 3 == 3, flags & 3 == 3);
            }
        }
        (irq as u32, false, false)
    })
}
