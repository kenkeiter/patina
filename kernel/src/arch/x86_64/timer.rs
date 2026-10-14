//! Periodic tick (LAPIC timer) and monotonic time (TSC).
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

use core::sync::atomic::{AtomicU64, Ordering};

use super::entry::{SPURIOUS_VECTOR, TIMER_VECTOR};
use super::lapic::{
    self, LVT_MASKED, LVT_TIMER_PERIODIC, REG_LVT_TIMER, REG_TIMER_CUR, REG_TIMER_DIV,
    REG_TIMER_INIT,
};
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
    ((dt as u128 * mult as u128) >> 32) as u64
}

/// Start the periodic tick at `TICK_HZ` on this CPU and unmask IRQs.
pub fn init() {
    calibrate();
    lapic::init(SPURIOUS_VECTOR);
    let count = (LAPIC_HZ.load(Ordering::Relaxed) / TICK_HZ).max(1) as u32;
    lapic::write(REG_TIMER_DIV, LAPIC_DIV16);
    lapic::write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
    lapic::write(REG_TIMER_INIT, count);
    let (mode, base) = lapic::mode();
    let (eax15, ebx15, ecx15) = if cpu::cpuid(0, 0).0 >= 0x15 {
        let (a, b, c, _) = cpu::cpuid(0x15, 0);
        (a, b, c)
    } else {
        (0, 0, 0)
    };
    crate::kprintln!(
        "[patina] timer: lapic {} @ {:#x}, vector {:#x}, {} Hz periodic (count {} at /16), lapic clock {} kHz, tsc {} kHz ({}; cpuid 0x15 = {}/{} x {} Hz)",
        mode,
        base,
        TIMER_VECTOR,
        TICK_HZ,
        count,
        LAPIC_HZ.load(Ordering::Relaxed) * LAPIC_DIVISOR / 1000,
        TSC_HZ.load(Ordering::Relaxed) / 1000,
        *CALIBRATION.lock(),
        ebx15,
        eax15,
        ecx15
    );
    irq::enable();
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
