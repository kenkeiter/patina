//! Local interrupt masking (RFLAGS.IF).

use core::arch::asm;

/// Saved RFLAGS value.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct Flags(u64);

const RFLAGS_IF: u64 = 1 << 9;

/// Mask interrupts; return the previous RFLAGS.
#[inline]
pub fn save_disable() -> Flags {
    let f: u64;
    // SAFETY: only changes the local interrupt flag. Not `nomem`: acts as a
    // compiler barrier like the lock it usually guards.
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) f, options(preserves_flags)) };
    Flags(f)
}

/// Restore the interrupt state returned by [`save_disable`].
#[inline]
pub fn restore(f: Flags) {
    if f.0 & RFLAGS_IF != 0 {
        // SAFETY: re-enables interrupts that were enabled before.
        unsafe { asm!("sti", options(nostack)) };
    }
}

/// Unmask interrupts on this CPU (arch-internal: entry paths, timer init).
#[inline]
pub(super) fn enable() {
    // SAFETY: callers enable IRQs only where the IDT and stacks are ready.
    unsafe { asm!("sti", options(nostack)) };
}

/// Whether interrupts are currently enabled on this CPU.
#[inline]
pub fn enabled() -> bool {
    let f: u64;
    // SAFETY: read-only.
    unsafe { asm!("pushfq", "pop {}", out(reg) f, options(nomem, preserves_flags)) };
    f & RFLAGS_IF != 0
}
