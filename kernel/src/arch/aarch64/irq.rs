//! Local interrupt masking (PSTATE.DAIF) and IRQ dispatch.
//!
//! [`handle`] runs from the IRQ vectors (EL0 and EL1) with IRQs masked:
//! acknowledge at the GIC, then either the timer path (rearm, EOI,
//! `trap::timer_tick`), an IPI (SGI 0/1: EOI, then `trap::ipi`), or a
//! handler registered with [`register`] (then EOI).
//! The timer's EOI happens before `trap::timer_tick` so the core may switch
//! threads from the tick without leaving the interrupt active. Nothing here
//! assumes the boot CPU: the GIC CPU interface is per-CPU and the
//! redistributor is looked up per CPU.
//!
//! MSI (Phase 4, D70): [`alloc_msi`] maps a PCI device's MSI through the
//! GICv3 ITS (`its.rs`) to an LPI (INTID >= 8192), which [`register`]
//! binds like an SPI (LPIs are enabled in the ITS configuration table, not
//! at the distributor) and [`configure`] ignores (LPIs are edge); without
//! an ITS, through a GICv2m frame (`v2m.rs`) to an edge SPI.

use core::arch::asm;

use super::ctx::TrapFrame;
use super::{gic, its, timer};
use crate::sync::SpinLock;
use crate::trap::IpiKind;

/// Saved DAIF value.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct Flags(u64);

const DAIF_I: u64 = 1 << 7;

/// Mask IRQ and FIQ; return the previous DAIF state.
#[inline]
pub fn save_disable() -> Flags {
    let daif: u64;
    // SAFETY: only changes the local interrupt mask. No `nomem`: this is a
    // compiler barrier, so memory accesses are not moved out of the
    // IRQ-masked region.
    unsafe {
        asm!("mrs {}, daif", "msr daifset, #3", out(reg) daif, options(nostack, preserves_flags))
    };
    Flags(daif)
}

/// Restore a DAIF state returned by [`save_disable`].
#[inline]
pub fn restore(f: Flags) {
    // SAFETY: restores a value previously read from DAIF.
    unsafe { asm!("msr daif, {}", in(reg) f.0, options(nostack, preserves_flags)) };
}

/// Whether IRQs are currently unmasked on this CPU.
#[inline]
pub fn enabled() -> bool {
    let daif: u64;
    // SAFETY: read-only.
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    daif & DAIF_I == 0
}

/// Unmask IRQs on this CPU (FIQs stay masked).
#[inline]
pub fn enable() {
    // SAFETY: only changes the local interrupt mask (compiler barrier).
    unsafe { asm!("msr daifclr, #2", options(nostack, preserves_flags)) };
}

/// Mask IRQs and FIQs on this CPU.
#[inline]
pub fn disable() {
    // SAFETY: only changes the local interrupt mask (compiler barrier).
    unsafe { asm!("msr daifset, #3", options(nostack, preserves_flags)) };
}

/// Handler for a device interrupt; gets the INTID.
pub type Handler = fn(u32);

/// INTIDs handled through the table: SGIs, PPIs and SPIs below 1020.
const MAX_INTID: usize = 1020;

static HANDLERS: SpinLock<[Option<Handler>; MAX_INTID]> = SpinLock::new([None; MAX_INTID]);
static LPI_HANDLERS: SpinLock<[Option<Handler>; its::MAX_LPIS as usize]> =
    SpinLock::new([None; its::MAX_LPIS as usize]);

fn lpi_index(intid: u32) -> Option<usize> {
    let i = intid.checked_sub(its::LPI_BASE)?;
    (i < its::MAX_LPIS).then_some(i as usize)
}

/// Map an MSI/MSI-X message of PCI device `requester` (device id after
/// `msi-map`) to an LPI through the GICv3 ITS: returns the INTID (for
/// [`register`]) and the message (GITS_TRANSLATER, event id). Without a
/// usable ITS, an SPI of a GICv2m frame (message = MSI_SETSPI_NS, INTID).
/// `None` if neither exists.
pub fn alloc_msi(requester: u32) -> Option<crate::drivers::pci::MsiMsg> {
    if !gic::is_initialized() {
        gic::init();
    }
    its::alloc(requester).or_else(super::v2m::alloc)
}

/// The message that delivers MSI `msg` (from [`alloc_msi`]) to logical CPU
/// `cpu` (online), keeping its IRQ number (D88): the message itself never
/// changes. A GICv2m SPI is re-routed at the distributor, an ITS LPI moved
/// to that CPU's collection (`MOVI`). `None` if that failed.
pub fn msi_affinity(
    msg: crate::drivers::pci::MsiMsg,
    cpu: usize,
) -> Option<crate::drivers::pci::MsiMsg> {
    let moved = if lpi_index(msg.irq).is_some() {
        its::set_affinity(msg.irq, cpu)
    } else {
        gic::route_spi(msg.irq, cpu)
    };
    moved.then_some(msg)
}

/// Set the trigger mode of device interrupt `intid` (an SPI; from the
/// DTB `interrupts` flags) before [`register`]. GIC SPIs are active-high
/// only, so `active_low` is ignored. Initializes the GIC if needed.
pub fn configure(intid: u32, edge: bool, _active_low: bool) {
    if lpi_index(intid).is_some() {
        return;
    }
    if !gic::is_initialized() {
        gic::init();
    }
    gic::set_trigger(intid, edge);
}

/// Install `handler` for `intid` and enable the interrupt at the GIC
/// (SGIs/PPIs on this CPU, SPIs routed to this CPU). Initializes the GIC
/// if `timer::init` has not done so yet (device drivers register before
/// the scheduler starts); the interrupt is taken once IRQs are unmasked.
/// The handler runs with IRQs masked and must not block.
pub fn register(intid: u32, handler: Handler) {
    if let Some(i) = lpi_index(intid) {
        LPI_HANDLERS.lock()[i] = Some(handler);
        return;
    }
    assert!(
        (intid as usize) < MAX_INTID,
        "irq::register: bad INTID {}",
        intid
    );
    if !gic::is_initialized() {
        gic::init();
    }
    HANDLERS.lock()[intid as usize] = Some(handler);
    gic::enable(intid);
}

/// IRQ entry from the vectors (IRQs masked). `from_user`: taken from EL0.
pub(super) fn handle(tf: &mut TrapFrame, from_user: bool) {
    let Some((intid, token)) = gic::ack() else {
        return; // spurious
    };
    if intid == timer::intid() {
        timer::rearm();
        gic::eoi(token);
        crate::trap::timer_tick(tf, from_user);
        return;
    }
    // IPIs: EOI first (`Stop` never returns).
    let ipi = match intid {
        gic::SGI_RESCHEDULE => Some(IpiKind::Reschedule),
        gic::SGI_STOP => Some(IpiKind::Stop),
        gic::SGI_TLB_FLUSH => Some(IpiKind::TlbFlush),
        _ => None,
    };
    if let Some(kind) = ipi {
        gic::eoi(token);
        crate::trap::ipi(tf, kind);
        return;
    }
    let h = match lpi_index(intid) {
        Some(i) => LPI_HANDLERS.lock()[i],
        None => HANDLERS.lock().get(intid as usize).copied().flatten(),
    };
    match h {
        Some(h) => h(intid),
        None => crate::kprintln!(
            "[patina] irq: unexpected INTID {} on cpu {}",
            intid,
            super::cpu::id()
        ),
    }
    gic::eoi(token);
}
