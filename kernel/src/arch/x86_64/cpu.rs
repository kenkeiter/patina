//! CPU identification, idling and control registers.

use core::arch::asm;

/// Logical CPU index. Phase 0 runs on the boot CPU only (APs are never
/// started); Phase 2 replaces this with a per-CPU block reached via GS.
#[inline]
pub fn id() -> usize {
    0
}

/// Mask interrupts and halt forever.
pub fn halt() -> ! {
    loop {
        // SAFETY: cli/hlt are always permitted in ring 0.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Halt until the next interrupt, once.
#[inline]
pub fn wait_for_interrupt() {
    // SAFETY: hlt has no memory effects.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr4() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// Changes paging-related CPU state.
#[inline]
pub unsafe fn write_cr4(v: u64) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags)) };
}

/// # Safety
/// `v` must be the physical address of a valid PML4 that maps the running
/// code, stack and everything the kernel touches next.
#[inline]
pub unsafe fn write_cr3(v: u64) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags)) };
}
