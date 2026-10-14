//! Generic timer: the EL1 virtual timer (CNTV_*) as the periodic tick, and
//! CNTVCT_EL0 as the monotonic clock.
//!
//! The tick uses absolute compare values (CNTV_CVAL_EL0 += period), so it
//! does not drift with interrupt latency; a tick that is already late is
//! re-armed one period from now. CNTV_CVAL_EL0 is per CPU, so [`rearm`] works
//! on whichever CPU took the interrupt.

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::{gic, irq};
use crate::trap::TICK_HZ;

/// Default virtual-timer PPI (INTID 27) if the DTB does not say.
const DEFAULT_VIRT_INTID: u32 = 27;

static INTID: AtomicU32 = AtomicU32::new(DEFAULT_VIRT_INTID);
static PERIOD: AtomicU64 = AtomicU64::new(0);
static BOOT_COUNT: AtomicU64 = AtomicU64::new(0);

const CNTV_CTL_ENABLE: u64 = 1 << 0;
/// CNTKCTL_EL1.EL0VCTEN: EL0 may read CNTVCT_EL0 (Go's `cputicks`) and
/// CNTFRQ_EL0.
const CNTKCTL_EL0VCTEN: u64 = 1 << 1;

#[inline]
fn cntfrq() -> u64 {
    let v: u64;
    // SAFETY: read-only system register.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
fn cntvct() -> u64 {
    let v: u64;
    // SAFETY: read-only; the isb keeps the read from being speculated early.
    unsafe {
        asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags))
    };
    v
}

/// Boot-time setup (boot CPU, early): record the counter base for
/// [`now_ns`], take the timer INTID from the DTB, and let EL0 read the
/// virtual counter (as Linux does).
pub(super) fn early_init(dtb_intid: Option<u32>) {
    BOOT_COUNT.store(cntvct(), Ordering::Relaxed);
    if let Some(i) = dtb_intid {
        INTID.store(i, Ordering::Relaxed);
    }
    init_cpu_access();
}

/// Per-CPU: EL0 access to the virtual counter, nothing else.
pub(super) fn init_cpu_access() {
    // SAFETY: CNTKCTL_EL1 only controls EL0 access to the counters/timers.
    unsafe {
        asm!(
            "msr cntkctl_el1, {}",
            "isb",
            in(reg) CNTKCTL_EL0VCTEN,
            options(nostack, preserves_flags)
        )
    };
}

/// INTID of the tick interrupt.
#[inline]
pub(super) fn intid() -> u32 {
    INTID.load(Ordering::Relaxed)
}

/// Start the periodic tick at `trap::TICK_HZ` on this CPU and unmask IRQs.
/// Initializes the GIC on first use.
pub fn init() {
    let freq = cntfrq();
    assert!(freq != 0, "timer: CNTFRQ_EL0 is 0");
    let period = (freq / TICK_HZ).max(1);
    PERIOD.store(period, Ordering::Relaxed);
    gic::init();
    gic::enable(intid());
    let cval = cntvct() + period;
    // SAFETY: programs this CPU's virtual timer; its interrupt is handled by
    // `irq::handle`.
    unsafe {
        asm!(
            "msr cntv_cval_el0, {cval}",
            "msr cntv_ctl_el0, {ctl}",
            "isb",
            cval = in(reg) cval,
            ctl = in(reg) CNTV_CTL_ENABLE,
            options(nostack, preserves_flags)
        )
    };
    crate::kprintln!(
        "[patina] timer: virtual timer INTID {}, {} Hz counter, tick {} Hz",
        intid(),
        freq,
        TICK_HZ
    );
    irq::enable();
}

/// Program the next tick (from the IRQ handler, before EOI).
pub(super) fn rearm() {
    let period = PERIOD.load(Ordering::Relaxed);
    let cval: u64;
    // SAFETY: reads this CPU's timer compare value.
    unsafe {
        asm!("mrs {}, cntv_cval_el0", out(reg) cval, options(nomem, nostack, preserves_flags))
    };
    let now = cntvct();
    let mut next = cval.wrapping_add(period);
    if next <= now {
        next = now + period;
    }
    // SAFETY: as in `init`.
    unsafe {
        asm!(
            "msr cntv_cval_el0, {}",
            "isb",
            in(reg) next,
            options(nostack, preserves_flags)
        )
    };
}

/// Monotonic nanoseconds since boot (CNTVCT_EL0 scaled by CNTFRQ_EL0).
pub fn now_ns() -> u64 {
    let freq = cntfrq();
    let t = cntvct().wrapping_sub(BOOT_COUNT.load(Ordering::Relaxed));
    // Split to avoid overflow: (t % freq) * 1e9 < freq * 1e9 < 2^64 for any
    // counter frequency below 18 GHz.
    (t / freq) * 1_000_000_000 + (t % freq) * 1_000_000_000 / freq
}
