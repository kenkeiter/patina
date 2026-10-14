//! CPU identification and idling.

use core::arch::asm;

/// Logical CPU index, kept in TPIDR_EL1 (boot CPU = 0, set by boot code).
#[inline]
pub fn id() -> usize {
    let v: usize;
    // SAFETY: reading a system register has no side effects.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Mask all interrupts and wait forever.
pub fn halt() -> ! {
    loop {
        // SAFETY: masking DAIF and WFI are always permitted at EL1.
        unsafe { asm!("msr daifset, #0xf", "wfi", options(nomem, nostack)) };
    }
}

/// Wait for an interrupt (or other wake-up event) once.
#[inline]
pub fn wait_for_interrupt() {
    // SAFETY: WFI has no memory effects.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}

/// Read MPIDR_EL1 (hardware affinity of this CPU).
#[inline]
pub fn mpidr() -> u64 {
    let v: u64;
    // SAFETY: read-only system register.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Enable PAN (Privileged Access Never) if FEAT_PAN is implemented, and clear
/// SCTLR_EL1.SPAN so PSTATE.PAN is set again on every exception entry.
/// Returns whether PAN is now on.
pub(super) fn enable_pan() -> bool {
    let mmfr1: u64;
    // SAFETY: read-only ID register.
    unsafe {
        asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1, options(nomem, nostack, preserves_flags))
    };
    if (mmfr1 >> 20) & 0xf == 0 {
        return false;
    }
    // SAFETY: PAN is implemented; SCTLR_EL1.SPAN (bit 23) and PSTATE.PAN
    // (S3_0_C4_C2_3, written via the generic name so no target feature is
    // needed) only restrict EL1 access to EL0 memory, of which there is none
    // in the kernel's own accesses.
    unsafe {
        asm!(
            "mrs {t}, sctlr_el1",
            "bic {t}, {t}, #(1 << 23)",
            "msr sctlr_el1, {t}",
            "mov {t}, #(1 << 22)",
            "msr S3_0_C4_C2_3, {t}",
            "isb",
            t = out(reg) _,
            options(nostack)
        );
    }
    true
}
