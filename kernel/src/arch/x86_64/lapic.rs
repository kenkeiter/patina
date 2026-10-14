//! Local APIC: register access, enable, EOI.
//!
//! Mode: xAPIC (MMIO at the base from `IA32_APIC_BASE`, mapped uncached
//! with `paging::map_device`), unless the firmware already switched the CPU
//! to x2APIC (`IA32_APIC_BASE.EXTD`), in which case the MSR interface
//! (0x800 + reg/16) is used. xAPIC works on every VMM we target (QEMU TCG
//! and KVM, Firecracker, Cloud Hypervisor); x2APIC only becomes necessary
//! with more than 255 CPUs.
//!
//! All CPUs see their own LAPIC at the same physical address, so one
//! mapping serves every CPU (Phase 2).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::cpu::{self, MSR_APIC_BASE};
use super::paging;
use crate::mm::PhysAddr;

pub const REG_ID: u32 = 0x20;
pub const REG_TPR: u32 = 0x80;
pub const REG_EOI: u32 = 0xb0;
pub const REG_SVR: u32 = 0xf0;
pub const REG_LVT_TIMER: u32 = 0x320;
pub const REG_LVT_LINT0: u32 = 0x350;
pub const REG_LVT_ERROR: u32 = 0x370;
pub const REG_TIMER_INIT: u32 = 0x380;
pub const REG_TIMER_CUR: u32 = 0x390;
pub const REG_TIMER_DIV: u32 = 0x3e0;

pub const LVT_MASKED: u32 = 1 << 16;
pub const LVT_TIMER_PERIODIC: u32 = 1 << 17;
const SVR_ENABLE: u32 = 1 << 8;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_EXTD: u64 = 1 << 10;
const MSR_X2APIC_BASE: u32 = 0x800;

/// VA of the xAPIC MMIO page (0 until [`init`] or in x2APIC mode).
static MMIO: AtomicUsize = AtomicUsize::new(0);
static X2APIC: AtomicBool = AtomicBool::new(false);
static PHYS: AtomicUsize = AtomicUsize::new(0);

/// Map (once) and software-enable this CPU's LAPIC: SVR enabled with the
/// spurious vector, TPR 0, LINT0 and the error LVT masked (the 8259 is not
/// used; external IRQs will come through the IOAPIC).
pub fn init(spurious_vector: u8) {
    // SAFETY: IA32_APIC_BASE exists on every CPU with an APIC (all x86_64).
    let base = unsafe { cpu::rdmsr(MSR_APIC_BASE) };
    if base & APIC_BASE_EXTD != 0 {
        X2APIC.store(true, Ordering::Relaxed);
    } else {
        if base & APIC_BASE_ENABLE == 0 {
            // SAFETY: globally enable the xAPIC at its current base.
            unsafe { cpu::wrmsr(MSR_APIC_BASE, base | APIC_BASE_ENABLE) };
        }
        if MMIO.load(Ordering::Acquire) == 0 {
            let pa = (base & 0x000f_ffff_ffff_f000) as usize;
            let va = paging::map_device(PhysAddr(pa), 4096);
            PHYS.store(pa, Ordering::Relaxed);
            MMIO.store(va.0, Ordering::Release);
        }
    }
    write(REG_TPR, 0);
    write(REG_LVT_LINT0, LVT_MASKED);
    write(REG_LVT_ERROR, LVT_MASKED);
    write(REG_SVR, SVR_ENABLE | spurious_vector as u32);
}

/// ("xapic"|"x2apic", physical MMIO base) for the boot log.
pub fn mode() -> (&'static str, usize) {
    if X2APIC.load(Ordering::Relaxed) {
        ("x2apic", 0)
    } else {
        ("xapic", PHYS.load(Ordering::Relaxed))
    }
}

pub fn read(reg: u32) -> u32 {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: x2APIC mode is on, so the 0x800 MSR range exists.
        unsafe { cpu::rdmsr(MSR_X2APIC_BASE + (reg >> 4)) as u32 }
    } else {
        let base = MMIO.load(Ordering::Acquire);
        assert!(base != 0, "lapic: not initialized");
        // SAFETY: uncached mapping of the LAPIC page; `reg` is a register
        // offset inside it.
        unsafe { ((base + reg as usize) as *const u32).read_volatile() }
    }
}

pub fn write(reg: u32, v: u32) {
    if X2APIC.load(Ordering::Relaxed) {
        // SAFETY: as in `read`.
        unsafe { cpu::wrmsr(MSR_X2APIC_BASE + (reg >> 4), v as u64) };
    } else {
        let base = MMIO.load(Ordering::Acquire);
        assert!(base != 0, "lapic: not initialized");
        // SAFETY: as in `read`.
        unsafe { ((base + reg as usize) as *mut u32).write_volatile(v) };
    }
}

/// End of interrupt for the in-service vector.
#[inline]
pub fn eoi() {
    write(REG_EOI, 0);
}
