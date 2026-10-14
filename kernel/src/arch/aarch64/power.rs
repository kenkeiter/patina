//! Power control via PSCI (SMCCC calls over HVC or SMC, chosen from the DTB
//! `/psci` `method` property; HVC until the DTB has been parsed).

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

const PSCI_SYSTEM_OFF: u32 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u32 = 0x8400_0009;
pub const PSCI_VERSION: u32 = 0x8400_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conduit {
    Hvc = 0,
    Smc = 1,
}

static CONDUIT: AtomicU8 = AtomicU8::new(Conduit::Hvc as u8);

pub(super) fn set_conduit(c: Conduit) {
    CONDUIT.store(c as u8, Ordering::Relaxed);
}

pub fn conduit() -> Conduit {
    if CONDUIT.load(Ordering::Relaxed) == Conduit::Smc as u8 {
        Conduit::Smc
    } else {
        Conduit::Hvc
    }
}

/// SMCCC v1.0 call with up to three arguments; returns x0.
pub fn psci_call(fid: u32, a1: u64, a2: u64, a3: u64) -> u64 {
    let mut x0 = fid as u64;
    // SAFETY: SMCCC calls only clobber x0-x17; the firmware/VMM does not touch
    // our memory for the PSCI functions we use.
    unsafe {
        match conduit() {
            Conduit::Hvc => asm!(
                "hvc #0",
                inout("x0") x0, inout("x1") a1 => _, inout("x2") a2 => _, inout("x3") a3 => _,
                out("x4") _, out("x5") _, out("x6") _, out("x7") _, out("x8") _, out("x9") _,
                out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _,
                out("x15") _, out("x16") _, out("x17") _,
                options(nostack)
            ),
            Conduit::Smc => asm!(
                "smc #0",
                inout("x0") x0, inout("x1") a1 => _, inout("x2") a2 => _, inout("x3") a3 => _,
                out("x4") _, out("x5") _, out("x6") _, out("x7") _, out("x8") _, out("x9") _,
                out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _,
                out("x15") _, out("x16") _, out("x17") _,
                options(nostack)
            ),
        }
    }
    x0
}

/// Power the machine off. PSCI SYSTEM_OFF carries no exit status: the exit
/// code reaches the host through the serial sentinel (and, from Phase 3, the
/// control channel), which callers print before calling this.
pub fn poweroff(_code: i32) -> ! {
    psci_call(PSCI_SYSTEM_OFF, 0, 0, 0);
    // SYSTEM_OFF returned (VMM without PSCI?): halt instead.
    super::cpu::halt()
}

/// Reset the machine (not used in Phase 0).
#[allow(dead_code)]
pub fn reboot() -> ! {
    psci_call(PSCI_SYSTEM_RESET, 0, 0, 0);
    super::cpu::halt()
}
