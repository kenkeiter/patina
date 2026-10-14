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

/// Idle until an interrupt and let it run: called with IRQs masked,
/// returns with IRQs masked after any pending interrupt was handled.
///
/// No lost wake-up: `WFI` with PSTATE.I set still completes when an IRQ
/// becomes pending (WFI wake-up events are not gated by PSTATE masking;
/// Linux's `cpu_do_idle` relies on the same), so a wake-up that raced
/// with the caller's "nothing to run" check is pending and is taken as
/// soon as IRQs are unmasked below. The unmask is a self-synchronising
/// PSTATE write; the `isb` makes the window explicit.
#[inline]
pub fn idle_wait() {
    // SAFETY: WFI and DAIF changes only; IRQs end masked as on entry.
    unsafe {
        asm!(
            "wfi",
            "msr daifclr, #2",
            "isb",
            "msr daifset, #3",
            options(nostack, preserves_flags)
        )
    };
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

/// Upper bound on CPUs with dedicated per-CPU arch state (overflow stacks,
/// GIC redistributor slots). CPU ids at or above it share the last slot.
pub const MAX_CPUS: usize = 8;

/// Kernel stack for the next entry from user mode: a no-op on aarch64.
/// Entries from EL0 use SP_EL1, which is wherever the kernel left it at the
/// `eret` (the top of the frame area on the current thread's kernel stack,
/// see `ctx::enter_user`), so switching threads switches it implicitly.
#[inline]
pub fn set_kernel_stack(_top: usize) {}

macro_rules! read_sysreg {
    ($name:ident, $reg:literal) => {
        #[inline]
        pub fn $name() -> u64 {
            let v: u64;
            // SAFETY: read-only ID register.
            unsafe { asm!(concat!("mrs {}, ", $reg), out(reg) v, options(nomem, nostack, preserves_flags)) };
            v
        }
    };
}

read_sysreg!(read_midr, "midr_el1");
read_sysreg!(read_id_aa64isar0, "id_aa64isar0_el1");
read_sysreg!(read_id_aa64pfr0, "id_aa64pfr0_el1");

/// A 64-bit value from FEAT_RNG (`RNDR`), if ID_AA64ISAR0_EL1.RNDR
/// (bits 63:60) says it exists. `RNDR` reports failure (entropy not
/// available right now) with PSTATE.Z set and a zero result; retried a few
/// times before giving up.
pub fn hw_random() -> Option<u64> {
    if (read_id_aa64isar0() >> 60) & 0xf == 0 {
        return None;
    }
    for _ in 0..16 {
        let v: u64;
        let ok: u64;
        // SAFETY: RNDR (S3_3_C2_C4_0) is implemented (checked above); it only
        // writes the destination register and NZCV.
        unsafe {
            asm!(
                "mrs {v}, S3_3_C2_C4_0",
                "cset {ok}, ne",
                v = out(reg) v,
                ok = out(reg) ok,
                options(nomem, nostack)
            )
        };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

/// (AT_HWCAP, AT_HWCAP2) for auxv: the ID_AA64ISAR0_EL1-derived bits from
/// `patina_abi`, plus FP/ASIMD (always), CPUID (EL0 `MRS` of ID registers is
/// emulated, see `exceptions::emulate_mrs`), and from ID_AA64PFR0_EL1
/// FPHP/ASIMDHP (FP/AdvSIMD field == 1) and DIT, as Linux sets them.
/// HWCAP2 is 0.
pub fn hwcap() -> (u64, u64) {
    use patina_abi::aarch64::hwcap;
    let mut h =
        hwcap::from_id_aa64isar0(read_id_aa64isar0()) | hwcap::FP | hwcap::ASIMD | hwcap::CPUID;
    let pfr0 = read_id_aa64pfr0();
    if (pfr0 >> 16) & 0xf == 1 {
        h |= hwcap::FPHP;
    }
    if (pfr0 >> 20) & 0xf == 1 {
        h |= hwcap::ASIMDHP;
    }
    if (pfr0 >> 48) & 0xf >= 1 {
        h |= hwcap::DIT;
    }
    (h, 0)
}

/// Full system barrier before a device doorbell (virtio notify): makes
/// earlier normal-memory writes (descriptor/avail rings) visible to the
/// device before the MMIO write (`dsb sy`).
#[inline]
pub fn io_barrier() {
    // SAFETY: barrier only.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}
