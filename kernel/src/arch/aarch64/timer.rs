//! Generic timer: the EL1 virtual timer (CNTV_*) as the periodic tick, and
//! CNTVCT_EL0 as the monotonic clock.
//!
//! The tick uses absolute compare values on a fixed grid per CPU
//! (`start + k * period`), so it does not drift with interrupt latency;
//! missed ticks are skipped, not replayed. Each CPU also has an optional
//! one-shot deadline ([`set_deadline`]): CNTV_CVAL_EL0 (per CPU) holds the
//! earlier of the next grid point and the deadline, and [`rearm`] (on
//! whichever CPU took the interrupt) advances the grid if it was reached,
//! drops the deadline if it passed, and reprograms the compare value. Both
//! kinds of expiry call `trap::timer_tick` (the arch contract).

use core::arch::asm;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::cpu::{self, MAX_CPUS};
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

/// Per-CPU timer state, in counter units (CNTVCT_EL0 values).
struct PerCpuTimer {
    /// Next periodic boundary (on the `start + k * period` grid); 0 = the
    /// tick is not running on this CPU.
    next_tick: AtomicU64,
    /// Pending one-shot deadline; 0 = none.
    deadline: AtomicU64,
    /// Timer interrupts taken on this CPU (diagnostics / self-tests).
    irqs: AtomicU64,
}

static PER_CPU: [PerCpuTimer; MAX_CPUS] = [const {
    PerCpuTimer {
        next_tick: AtomicU64::new(0),
        deadline: AtomicU64::new(0),
        irqs: AtomicU64::new(0),
    }
}; MAX_CPUS];

fn this_cpu() -> &'static PerCpuTimer {
    &PER_CPU[cpu::id().min(MAX_CPUS - 1)]
}

/// Program this CPU's compare value: the earlier of the next periodic tick
/// and the pending deadline. Call with IRQs masked (the per-CPU state and
/// CNTV_CVAL_EL0 change together).
fn program(t: &PerCpuTimer) {
    let next = t.next_tick.load(Ordering::Relaxed);
    let dl = t.deadline.load(Ordering::Relaxed);
    let cval = if dl != 0 { next.min(dl) } else { next };
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
}

/// Start this CPU's periodic tick: enable the PPI at this CPU's GIC
/// interface and arm the first tick one period from now. Leaves the IRQ
/// mask as it was.
fn start_tick() {
    let period = PERIOD.load(Ordering::Relaxed);
    gic::enable(intid());
    let t = this_cpu();
    let f = irq::save_disable();
    t.next_tick.store(cntvct() + period, Ordering::Relaxed);
    program(t);
    irq::restore(f);
}

/// Start the periodic tick at `trap::TICK_HZ` on this CPU and unmask IRQs.
/// Initializes the GIC on first use.
pub fn init() {
    let freq = cntfrq();
    assert!(freq != 0, "timer: CNTFRQ_EL0 is 0");
    let period = (freq / TICK_HZ).max(1);
    PERIOD.store(period, Ordering::Relaxed);
    gic::init();
    start_tick();
    crate::kprintln!(
        "[patina] timer: virtual timer INTID {}, {} Hz counter, tick {} Hz",
        intid(),
        freq,
        TICK_HZ
    );
    irq::enable();
}

/// Secondary CPU: start this CPU's periodic tick (after `gic::init_cpu`).
/// Does not unmask IRQs.
pub fn init_cpu() {
    if PERIOD.load(Ordering::Relaxed) == 0 {
        PERIOD.store((cntfrq() / TICK_HZ).max(1), Ordering::Relaxed);
    }
    start_tick();
}

/// Timer interrupt (from the IRQ handler, before EOI): if the periodic
/// boundary was reached, move it to the first grid point after now
/// (skipping missed ticks, never drifting off the grid); drop the deadline
/// if it passed; program the next compare value.
pub(super) fn rearm() {
    let period = PERIOD.load(Ordering::Relaxed);
    let t = this_cpu();
    t.irqs.fetch_add(1, Ordering::Relaxed);
    let now = cntvct();
    let next = t.next_tick.load(Ordering::Relaxed);
    if next <= now {
        let missed = (now - next) / period;
        t.next_tick
            .store(next + (missed + 1) * period, Ordering::Relaxed);
    }
    let dl = t.deadline.load(Ordering::Relaxed);
    if dl != 0 && dl <= now {
        t.deadline.store(0, Ordering::Relaxed);
    }
    program(t);
}

/// Per-CPU one-shot deadline at monotonic time `ns` (as [`now_ns`]), in
/// addition to the periodic tick; `None` cancels. Replaces the previous
/// deadline; one in the past fires immediately. The interrupt goes through
/// the normal tick path (`trap::timer_tick`). Before this CPU's tick was
/// started the deadline is only recorded.
pub fn set_deadline(ns: Option<u64>) {
    let t = this_cpu();
    let target = match ns {
        None => 0,
        Some(ns) => ns_to_count(ns).max(1),
    };
    let f = irq::save_disable();
    t.deadline.store(target, Ordering::Relaxed);
    if t.next_tick.load(Ordering::Relaxed) != 0 {
        program(t);
    }
    irq::restore(f);
}

/// Counter value at monotonic time `ns` (saturating).
fn ns_to_count(ns: u64) -> u64 {
    let ticks = (ns as u128 * cntfrq() as u128) / 1_000_000_000;
    let c = BOOT_COUNT.load(Ordering::Relaxed) as u128 + ticks;
    c.min(u64::MAX as u128) as u64
}

/// Timer interrupts taken on `cpu` so far (periodic + deadline).
pub fn irq_count(cpu: usize) -> u64 {
    PER_CPU[cpu.min(MAX_CPUS - 1)].irqs.load(Ordering::Relaxed)
}

/// Whether a deadline is still pending on this CPU.
pub fn deadline_pending() -> bool {
    this_cpu().deadline.load(Ordering::Relaxed) != 0
}

/// Monotonic nanoseconds since boot (CNTVCT_EL0 scaled by CNTFRQ_EL0).
pub fn now_ns() -> u64 {
    let freq = cntfrq();
    let t = cntvct().wrapping_sub(BOOT_COUNT.load(Ordering::Relaxed));
    // Split to avoid overflow: (t % freq) * 1e9 < freq * 1e9 < 2^64 for any
    // counter frequency below 18 GHz.
    (t / freq) * 1_000_000_000 + (t % freq) * 1_000_000_000 / freq
}
