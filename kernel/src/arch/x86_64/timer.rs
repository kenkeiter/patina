//! Tick + one-shot deadlines (LAPIC timer) and monotonic time (TSC).
//!
//! Every CPU runs its LAPIC timer in one-shot mode: each interrupt re-arms
//! it for min(next boundary of this CPU's `TICK_HZ` grid, pending
//! [`set_deadline`]), so the periodic tick and sub-tick deadlines share the
//! one timer. (TSC-deadline mode is not used: TCG does not always offer it
//! and the one-shot path works everywhere.)
//!
//! Calibration: one ~10 ms window on PIT channel 0 (mode 0, counted down
//! from 0xffff and read back with the latch command, so it works without
//! the port-0x61 speaker gate that `microvm` lacks), measuring the TSC and
//! the LAPIC timer (divide-by-16, masked one-shot from 0xffffffff) at the
//! same time. If the PIT does not count, CPUID 0x15/0x16 are used instead
//! (TSC and crystal = LAPIC timer clock). Calibration runs once, on the first
//! [`now_ns`] or [`init`], whichever comes first.
//!
//! The 8259 PICs are remapped and fully masked at boot (`pic.rs`), and
//! LAPIC LINT0 is masked here; the IOAPIC is left as reset (all pins
//! masked).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::cpu::MAX_CPUS;
use super::entry::{SPURIOUS_VECTOR, TIMER_VECTOR};
use super::lapic::{self, LVT_MASKED, REG_LVT_TIMER, REG_TIMER_CUR, REG_TIMER_DIV, REG_TIMER_INIT};
use super::port::{inb, outb};
use super::{cpu, irq};
use crate::sync::SpinLock;
use crate::trap::TICK_HZ;

const PIT_HZ: u64 = 1_193_182;
const PIT_CH0: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
/// ~10 ms of PIT ticks.
const PIT_WINDOW: u64 = PIT_HZ / 100;
/// LAPIC timer divide configuration value for /16, and the divisor.
const LAPIC_DIV16: u32 = 0b0011;
const LAPIC_DIVISOR: u64 = 16;

/// TSC value recorded at boot (the zero of `now_ns`).
static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
/// ns per TSC tick as 32.32 fixed point; 0 = not calibrated.
static TSC_MULT: AtomicU64 = AtomicU64::new(0);
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// LAPIC timer input clock after the /16 divider.
static LAPIC_HZ: AtomicU64 = AtomicU64::new(0);
static CALIBRATION: SpinLock<&'static str> = SpinLock::new("");

/// Remember the boot TSC (called first thing in `early_main`).
pub(super) fn record_boot_tsc() {
    TSC_BOOT.store(cpu::rdtsc(), Ordering::Relaxed);
}

/// Monotonic nanoseconds since boot (TSC, calibrated against the PIT).
pub fn now_ns() -> u64 {
    let mut mult = TSC_MULT.load(Ordering::Acquire);
    if mult == 0 {
        calibrate();
        mult = TSC_MULT.load(Ordering::Acquire);
    }
    let dt = cpu::rdtsc().wrapping_sub(TSC_BOOT.load(Ordering::Relaxed));
    patina_vdso::vvar::mult32_to_ns(dt, mult)
}

/// (counter base, scale) for the vDSO's vvar page (D75): the TSC at
/// monotonic 0 and ns per TSC tick as 32.32 fixed point, the same math as
/// [`now_ns`].
pub fn vdso_clock() -> (u64, u64) {
    if TSC_MULT.load(Ordering::Acquire) == 0 {
        calibrate();
    }
    (
        TSC_BOOT.load(Ordering::Relaxed),
        TSC_MULT.load(Ordering::Acquire),
    )
}

/// Tick period in ns.
const PERIOD_NS: u64 = 1_000_000_000 / TICK_HZ;
const NONE: u64 = u64::MAX;

/// Per-CPU timer state. Only the owning CPU writes it, with IRQs masked;
/// atomics just make the static shareable (and let the self-test read the
/// counters of other CPUs).
struct CpuTimer {
    /// Absolute ns of the next periodic tick (the 100 Hz grid of this CPU,
    /// advanced by whole periods so it does not drift).
    next_tick: AtomicU64,
    /// Pending one-shot deadline, or NONE.
    deadline: AtomicU64,
    /// Target the LAPIC timer is currently counting down to, or NONE.
    armed: AtomicU64,
    /// Periodic ticks seen / deadlines fired on this CPU (diagnostics).
    ticks: AtomicU64,
    fires: AtomicU64,
    /// How late (ns) the last deadline was handled, measured in the IRQ.
    late: AtomicU64,
    running: AtomicBool,
}

impl CpuTimer {
    const fn new() -> Self {
        Self {
            next_tick: AtomicU64::new(0),
            deadline: AtomicU64::new(NONE),
            armed: AtomicU64::new(NONE),
            ticks: AtomicU64::new(0),
            fires: AtomicU64::new(0),
            late: AtomicU64::new(0),
            running: AtomicBool::new(false),
        }
    }
}

static TIMERS: [CpuTimer; MAX_CPUS] = [const { CpuTimer::new() }; MAX_CPUS];

fn this_cpu() -> &'static CpuTimer {
    &TIMERS[cpu::id()]
}

/// Program this CPU's one-shot for min(next tick, deadline). IRQs masked.
fn arm(t: &CpuTimer, now: u64) {
    let target = t
        .next_tick
        .load(Ordering::Relaxed)
        .min(t.deadline.load(Ordering::Relaxed));
    let delta = target.saturating_sub(now) as u128;
    let hz = LAPIC_HZ.load(Ordering::Relaxed) as u128;
    let count = (delta * hz)
        .div_ceil(1_000_000_000)
        .clamp(1, u32::MAX as u128) as u32;
    t.armed.store(target, Ordering::Relaxed);
    lapic::write(REG_TIMER_INIT, count);
}

/// Start the tick at `TICK_HZ` on the boot CPU and unmask IRQs.
pub fn init() {
    calibrate();
    init_cpu();
    let (mode, base) = lapic::mode();
    let (eax15, ebx15, ecx15) = if cpu::cpuid(0, 0).0 >= 0x15 {
        let (a, b, c, _) = cpu::cpuid(0x15, 0);
        (a, b, c)
    } else {
        (0, 0, 0)
    };
    crate::kprintln!(
        "[patina] timer: lapic {} @ {:#x}, vector {:#x}, one-shot: {} Hz tick grid + per-cpu deadlines (period {} counts at /16), lapic clock {} kHz, tsc {} kHz ({}; cpuid 0x15 = {}/{} x {} Hz)",
        mode,
        base,
        TIMER_VECTOR,
        TICK_HZ,
        LAPIC_HZ.load(Ordering::Relaxed) / TICK_HZ,
        LAPIC_HZ.load(Ordering::Relaxed) * LAPIC_DIVISOR / 1000,
        TSC_HZ.load(Ordering::Relaxed) / 1000,
        *CALIBRATION.lock(),
        ebx15,
        eax15,
        ecx15
    );
    irq::enable();
}

/// Start this CPU's tick (LAPIC timer in one-shot mode, re-armed by
/// [`on_interrupt`] for the next 1/`TICK_HZ` boundary or the pending
/// deadline, whichever is first). Does not unmask IRQs. Used by
/// [`init`] on the boot CPU and by `smp::ap_main` on the others.
pub fn init_cpu() {
    calibrate();
    lapic::init(SPURIOUS_VECTOR);
    let f = irq::save_disable();
    let t = this_cpu();
    lapic::write(REG_TIMER_DIV, LAPIC_DIV16);
    // One-shot mode (LVT timer mode 0), unmasked.
    lapic::write(REG_LVT_TIMER, TIMER_VECTOR as u32);
    let now = now_ns();
    t.next_tick.store(now + PERIOD_NS, Ordering::Relaxed);
    t.running.store(true, Ordering::Relaxed);
    arm(t, now);
    irq::restore(f);
}

/// Timer interrupt on this CPU (IRQs masked, EOI sent): account a
/// periodic tick and/or an expired deadline, re-arm. Returns whether
/// `trap::timer_tick` should run (false for an early wake-up, e.g. after a
/// cancelled deadline or LAPIC/TSC rounding).
pub(super) fn on_interrupt() -> bool {
    let t = this_cpu();
    if !t.running.load(Ordering::Relaxed) {
        return false;
    }
    let now = now_ns();
    let mut due = false;
    let nt = t.next_tick.load(Ordering::Relaxed);
    if now >= nt {
        due = true;
        t.ticks.fetch_add(1, Ordering::Relaxed);
        // Next grid point strictly after `now` (missed ticks are skipped,
        // the phase is kept).
        let next = nt + ((now - nt) / PERIOD_NS + 1) * PERIOD_NS;
        t.next_tick.store(next, Ordering::Relaxed);
    }
    let d = t.deadline.load(Ordering::Relaxed);
    if d != NONE && now >= d {
        due = true;
        t.deadline.store(NONE, Ordering::Relaxed);
        t.late.store(now - d, Ordering::Relaxed);
        t.fires.fetch_add(1, Ordering::Relaxed);
    }
    arm(t, now);
    due
}

/// Per-CPU one-shot deadline (absolute `now_ns` time): the timer interrupt
/// (and `trap::timer_tick`) also fires at or just after `ns`. `None`
/// cancels. Replaces the previous deadline; a past deadline fires at once.
pub fn set_deadline(ns: Option<u64>) {
    let f = irq::save_disable();
    let t = this_cpu();
    match ns {
        None => t.deadline.store(NONE, Ordering::Relaxed),
        Some(d) => {
            let d = d.min(NONE - 1);
            t.deadline.store(d, Ordering::Relaxed);
            if t.running.load(Ordering::Relaxed) && d < t.armed.load(Ordering::Relaxed) {
                arm(t, now_ns());
            }
        }
    }
    irq::restore(f);
}

/// (periodic ticks, deadlines fired) on `cpu` so far (self-tests).
pub(super) fn cpu_stats(cpu: usize) -> (u64, u64) {
    let t = &TIMERS[cpu];
    (
        t.ticks.load(Ordering::Relaxed),
        t.fires.load(Ordering::Relaxed),
    )
}

/// How late (ns after the deadline) `cpu`'s last deadline interrupt ran.
pub(super) fn last_deadline_lateness(cpu: usize) -> u64 {
    TIMERS[cpu].late.load(Ordering::Relaxed)
}

/// Latch and read PIT channel 0's current count.
fn pit_read() -> u64 {
    // SAFETY: PIT ports; latch command for channel 0, then lo/hi bytes.
    unsafe {
        outb(PIT_CMD, 0x00);
        let lo = inb(PIT_CH0) as u64;
        let hi = inb(PIT_CH0) as u64;
        (hi << 8) | lo
    }
}

/// Calibrate the TSC and LAPIC timer once (idempotent).
fn calibrate() {
    let mut how = CALIBRATION.lock();
    if TSC_MULT.load(Ordering::Acquire) != 0 {
        return;
    }
    lapic::init(SPURIOUS_VECTOR);
    lapic::write(REG_TIMER_DIV, LAPIC_DIV16);
    lapic::write(REG_LVT_TIMER, LVT_MASKED | TIMER_VECTOR as u32);
    lapic::write(REG_TIMER_INIT, u32::MAX);

    // SAFETY: PIT channel 0, lo/hi access, mode 0 (interrupt on terminal
    // count; IRQ0 is masked at the PIC/IOAPIC), binary, count 0xffff.
    unsafe {
        outb(PIT_CMD, 0x30);
        outb(PIT_CH0, 0xff);
        outb(PIT_CH0, 0xff);
    }
    // Wait for the count to load and start moving (bounded: no PIT).
    let mut p0 = pit_read();
    let mut spins = 0u32;
    while p0 >= 0xffff && spins < 1_000_000 {
        p0 = pit_read();
        spins += 1;
    }
    let (tsc0, l0) = (cpu::rdtsc(), lapic::read(REG_TIMER_CUR) as u64);
    let mut p1 = p0;
    let mut ok = p0 < 0xffff;
    let mut spins = 0u32;
    while ok && p0.saturating_sub(p1) < PIT_WINDOW {
        p1 = pit_read();
        spins += 1;
        if p1 > p0 || spins > 50_000_000 {
            ok = false;
        }
    }
    let (tsc1, l1) = (cpu::rdtsc(), lapic::read(REG_TIMER_CUR) as u64);
    lapic::write(REG_TIMER_INIT, 0);

    let (tsc_hz, lapic_hz) = if ok {
        let pit = p0 - p1;
        *how = "PIT ch0 ~10 ms";
        (
            (tsc1 - tsc0) * PIT_HZ / pit,
            l0.saturating_sub(l1) * PIT_HZ / pit,
        )
    } else {
        *how = "cpuid 0x15/0x16";
        cpuid_freqs().unwrap_or_else(|| {
            panic!("timer: PIT does not count and CPUID 0x15/0x16 give no frequencies")
        })
    };
    assert!(tsc_hz > 0 && lapic_hz > 0, "timer: calibration failed");
    TSC_HZ.store(tsc_hz, Ordering::Relaxed);
    LAPIC_HZ.store(lapic_hz, Ordering::Relaxed);
    TSC_MULT.store(
        ((1_000_000_000u128 << 32) / tsc_hz as u128) as u64,
        Ordering::Release,
    );
}

/// (TSC Hz, LAPIC timer Hz after /16) from CPUID 0x15 (TSC/crystal ratio,
/// crystal = LAPIC timer clock) or 0x16 (base MHz).
fn cpuid_freqs() -> Option<(u64, u64)> {
    let (max, ..) = cpu::cpuid(0, 0);
    if max < 0x15 {
        return None;
    }
    let (den, num, crystal, _) = cpu::cpuid(0x15, 0);
    if den == 0 || num == 0 || crystal == 0 {
        return None;
    }
    let tsc = crystal as u64 * num as u64 / den as u64;
    Some((tsc, crystal as u64 / LAPIC_DIVISOR))
}
