//! Local interrupt masking (PSTATE.DAIF).

use core::arch::asm;

/// Saved DAIF value.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct Flags(u64);

const DAIF_I: u64 = 1 << 7;

/// Mask IRQ and FIQ; return the previous DAIF state.
#[inline]
pub fn save_disable() -> Flags {
    let daif: u64;
    // SAFETY: only changes the local interrupt mask.
    unsafe {
        asm!("mrs {}, daif", "msr daifset, #3", out(reg) daif, options(nomem, nostack, preserves_flags))
    };
    Flags(daif)
}

/// Restore a DAIF state returned by [`save_disable`].
#[inline]
pub fn restore(f: Flags) {
    // SAFETY: restores a value previously read from DAIF.
    unsafe { asm!("msr daif, {}", in(reg) f.0, options(nomem, nostack, preserves_flags)) };
}

/// Whether IRQs are currently unmasked on this CPU.
#[inline]
pub fn enabled() -> bool {
    let daif: u64;
    // SAFETY: read-only.
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    daif & DAIF_I == 0
}
