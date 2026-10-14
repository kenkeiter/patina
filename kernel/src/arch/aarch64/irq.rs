//! Local interrupt masking (PSTATE.DAIF) and IRQ dispatch.
//!
//! [`handle`] runs from the IRQ vectors (EL0 and EL1) with IRQs masked:
//! acknowledge at the GIC, then either the timer path (rearm, EOI,
//! `trap::timer_tick`) or a handler registered with [`register`] (then EOI).
//! The timer's EOI happens before `trap::timer_tick` so the core may switch
//! threads from the tick without leaving the interrupt active. Nothing here
//! assumes the boot CPU: the GIC CPU interface is per-CPU and the
//! redistributor is looked up per CPU.

use core::arch::asm;

use super::ctx::TrapFrame;
use super::{gic, timer};
use crate::sync::SpinLock;

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

/// Install `handler` for `intid` and enable the interrupt at the GIC
/// (SGIs/PPIs on this CPU, SPIs routed to this CPU). Requires the GIC to be
/// initialized (`timer::init` does it).
pub fn register(intid: u32, handler: Handler) {
    assert!(
        (intid as usize) < MAX_INTID,
        "irq::register: bad INTID {}",
        intid
    );
    HANDLERS.lock()[intid as usize] = Some(handler);
    gic::enable(intid);
}

/// IRQ entry from the vectors (IRQs masked). `from_user`: taken from EL0.
pub(super) fn handle(tf: &mut TrapFrame, from_user: bool) {
    let Some(intid) = gic::ack() else {
        return; // spurious
    };
    if intid == timer::intid() {
        timer::rearm();
        gic::eoi(intid);
        crate::trap::timer_tick(tf, from_user);
        return;
    }
    let h = HANDLERS.lock().get(intid as usize).copied().flatten();
    match h {
        Some(h) => h(intid),
        None => crate::kprintln!(
            "[patina] irq: unexpected INTID {} on cpu {}",
            intid,
            super::cpu::id()
        ),
    }
    gic::eoi(intid);
}
