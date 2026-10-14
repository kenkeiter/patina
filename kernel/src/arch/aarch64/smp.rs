//! Secondary CPU bring-up (PSCI CPU_ON), IPIs (GIC SGIs) and the
//! `patina.selftest=smp` self-test (which also covers the one-shot timer
//! deadline on the boot CPU).
//!
//! CPU enumeration: the boot CPU is logical CPU 0; the other enabled
//! `/cpus/cpu@N` nodes (PSCI enable-method) follow in DT order, capped at
//! `cpu::MAX_CPUS`. `MPIDR[i]` holds each one's affinity (Aff3..Aff0).
//!
//! Bring-up ([`start_secondaries`]) is serial, one CPU at a time, through
//! the single boot slot [`AP_BOOT`]: the boot CPU fills it (logical id,
//! TTBR1 root, the empty TTBR0 table, stack top), cleans it to the PoC
//! (the secondary reads it with its MMU and caches off), and calls
//! `PSCI CPU_ON(mpidr, PA of __ap_entry, PA of AP_BOOT)`. `__ap_entry`
//! (boot.rs) drops from EL2 if needed, turns the MMU on with the boot
//! identity map in TTBR0 and the final kernel tables in TTBR1, jumps to the
//! link address, parks TTBR0, switches to the stack and calls
//! [`ap_rust_entry`]: PAN, counter access, GIC CPU interface (SGIs
//! enabled), periodic tick, then the CPU marks itself online and runs
//! `entry(cpu)` with IRQs masked. The boot CPU waits up to 1 s for each
//! CPU to come online before reusing the slot; on a timeout it stops
//! starting more CPUs (a late CPU would read a rewritten slot).

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use super::cpu::{self, MAX_CPUS};
use super::dtb::{PSCI_0_2_CPU_ON_64, Platform};
use super::power::{self, Conduit};
use super::{KERNEL_VA_BASE, gic, irq, kernel_load_phys, paging, timer};
use crate::trap::{self, IpiKind};

/// MPIDR_EL1 affinity fields (Aff3 [39:32], Aff2..Aff0 [23:0]).
const MPIDR_AFF_MASK: u64 = 0xff_00ff_ffff;

static NCPUS: AtomicUsize = AtomicUsize::new(1);
static MPIDR: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Bit i set: logical CPU i is running kernel code (CPU 0 from boot).
static ONLINE: AtomicU64 = AtomicU64::new(1);
static CPU_ON_FID: AtomicU32 = AtomicU32::new(PSCI_0_2_CPU_ON_64);
static HAVE_PSCI: AtomicBool = AtomicBool::new(false);
/// Exception level each secondary entered `__ap_entry` at (diagnostics).
static ENTRY_EL: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];
/// `entry` of the current `start_secondaries` call.
static AP_ENTRY: AtomicUsize = AtomicUsize::new(0);

/// Boot parameters for the CPU being started; read by `__ap_entry`.
#[repr(C)]
pub struct ApBoot {
    cpu: u64,
    ttbr1: u64,
    ttbr0: u64,
    stack: u64,
}
pub const AP_OFF_CPU: usize = 0;
pub const AP_OFF_TTBR1: usize = 8;
pub const AP_OFF_TTBR0: usize = 16;
pub const AP_OFF_STACK: usize = 24;

#[repr(C, align(128))]
pub struct ApBootCell(UnsafeCell<ApBoot>);
// SAFETY: written only by the boot CPU while no secondary is reading it
// (bring-up is serialized on the ONLINE handshake).
unsafe impl Sync for ApBootCell {}

pub static AP_BOOT: ApBootCell = ApBootCell(UnsafeCell::new(ApBoot {
    cpu: 0,
    ttbr1: 0,
    ttbr0: 0,
    stack: 0,
}));

unsafe extern "C" {
    static __ap_entry: u8;
}

/// Record the CPUs from the DTB (boot CPU first) and the PSCI CPU_ON id.
/// Called by `early_main` once the console is up.
pub(super) fn early_init(plat: &Platform) {
    let me = cpu::mpidr() & MPIDR_AFF_MASK;
    MPIDR[0].store(me, Ordering::Relaxed);
    let mut n = 1;
    let mut dropped = 0;
    for &m in &plat.cpus[..plat.ncpus] {
        let m = m & MPIDR_AFF_MASK;
        if m == me {
            continue;
        }
        if n < MAX_CPUS {
            MPIDR[n].store(m, Ordering::Relaxed);
            n += 1;
        } else {
            dropped += 1;
        }
    }
    NCPUS.store(n, Ordering::Relaxed);
    if let Some(f) = plat.psci_cpu_on {
        CPU_ON_FID.store(f, Ordering::Relaxed);
    }
    HAVE_PSCI.store(plat.psci.is_some(), Ordering::Relaxed);
    let method = match plat.psci {
        Some(Conduit::Hvc) => "psci hvc",
        Some(Conduit::Smc) => "psci smc",
        None => "no psci",
    };
    if dropped > 0 {
        crate::kprintln!(
            "[patina] smp: {} cpus ({}), {} more above MAX_CPUS ignored",
            n,
            method,
            dropped
        );
    } else {
        crate::kprintln!("[patina] smp: {} cpus ({})", n, method);
    }
}

/// CPUs the platform reports (>= 1, capped at `MAX_CPUS`).
pub fn cpu_count() -> usize {
    NCPUS.load(Ordering::Relaxed)
}

/// Whether logical CPU `cpu` is online.
pub fn is_online(cpu: usize) -> bool {
    cpu < 64 && ONLINE.load(Ordering::Acquire) & (1 << cpu) != 0
}

/// Hardware affinity (MPIDR Aff3..Aff0) of logical CPU `cpu`.
pub fn mpidr_of(cpu: usize) -> u64 {
    MPIDR[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed)
}

fn psci_error(r: i64) -> &'static str {
    match r {
        -1 => "NOT_SUPPORTED",
        -2 => "INVALID_PARAMETERS",
        -3 => "DENIED",
        -4 => "ALREADY_ON",
        -5 => "ON_PENDING",
        -6 => "INTERNAL_FAILURE",
        -7 => "NOT_PRESENT",
        -8 => "DISABLED",
        -9 => "INVALID_ADDRESS",
        _ => "unknown",
    }
}

/// Clean `[start, start + len)` to the point of coherency, so a CPU
/// reading it with its caches off sees the data.
fn clean_to_poc(start: usize, len: usize) {
    let ctr: u64;
    // SAFETY: read-only system register.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let line = 4usize << ((ctr >> 16) & 0xf);
    let mut a = start & !(line - 1);
    while a < start + len {
        // SAFETY: cache maintenance on mapped kernel data.
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack, preserves_flags)) };
        a += line;
    }
    // SAFETY: barrier only.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Image VA -> PA.
fn image_pa(va: usize) -> usize {
    va - KERNEL_VA_BASE + kernel_load_phys()
}

/// Start every secondary CPU; each runs `entry(cpu)` with IRQs masked and
/// its tick running. Returns how many started. See the module comment.
pub fn start_secondaries(entry: fn(usize) -> !) -> usize {
    let n = cpu_count();
    if n <= 1 {
        return 0;
    }
    if !HAVE_PSCI.load(Ordering::Relaxed) {
        crate::kprintln!("[patina] smp: no /psci node, secondaries not started");
        return 0;
    }
    AP_ENTRY.store(entry as usize, Ordering::Release);
    let fid = CPU_ON_FID.load(Ordering::Relaxed);
    let entry_pa = image_pa(core::ptr::addr_of!(__ap_entry) as usize);
    let slot_va = AP_BOOT.0.get() as usize;
    let slot_pa = image_pa(slot_va);
    let mut started = 0;
    for i in 1..n {
        if is_online(i) {
            continue;
        }
        let mpidr = mpidr_of(i);
        let Some(stack) = crate::task::kstack::alloc() else {
            crate::kprintln!("[patina] smp: no memory for cpu {}'s stack", i);
            break;
        };
        // SAFETY: no secondary is reading the slot (the previous one is
        // online, or none was started yet).
        unsafe {
            AP_BOOT.0.get().write_volatile(ApBoot {
                cpu: i as u64,
                ttbr1: paging::kernel_root().0 as u64,
                ttbr0: paging::empty_user_table().0 as u64,
                stack: stack.top as u64,
            })
        };
        clean_to_poc(slot_va, core::mem::size_of::<ApBoot>());
        let r = power::psci_call(fid, mpidr, entry_pa as u64, slot_pa as u64) as i64;
        if r != 0 {
            crate::kprintln!(
                "[patina] smp: CPU_ON cpu {} (mpidr {:#x}) failed: {} ({})",
                i,
                mpidr,
                psci_error(r),
                r
            );
            crate::task::kstack::free(stack);
            continue;
        }
        let deadline = timer::now_ns() + 1_000_000_000;
        while !is_online(i) {
            if timer::now_ns() > deadline {
                crate::kprintln!(
                    "[patina] smp: cpu {} (mpidr {:#x}) did not come online within 1 s; not starting more",
                    i,
                    mpidr
                );
                return started;
            }
            core::hint::spin_loop();
        }
        started += 1;
    }
    started
}

/// Rust entry of a secondary CPU (from `__ap_high`): kernel tables live,
/// TTBR0 parked, on its own stack, VBAR set, IRQs masked.
/// `entry_el` is the exception level PSCI started the CPU at (1 or 2).
pub extern "C" fn ap_rust_entry(cpu: usize, entry_el: usize) -> ! {
    ENTRY_EL[cpu.min(MAX_CPUS - 1)].store(entry_el as u8, Ordering::Relaxed);
    cpu::enable_pan();
    timer::init_cpu_access();
    gic::init_cpu();
    timer::init_cpu();
    // SAFETY: AP_ENTRY holds the `fn(usize) -> !` passed to
    // `start_secondaries`, which is waiting for this CPU.
    let entry: fn(usize) -> ! =
        unsafe { core::mem::transmute::<usize, fn(usize) -> !>(AP_ENTRY.load(Ordering::Acquire)) };
    ONLINE.fetch_or(1 << cpu, Ordering::Release);
    entry(cpu)
}

fn sgi_for(kind: IpiKind) -> u32 {
    match kind {
        IpiKind::Reschedule => gic::SGI_RESCHEDULE,
        IpiKind::Stop => gic::SGI_STOP,
        IpiKind::TlbFlush => gic::SGI_TLB_FLUSH,
    }
}

/// Send an IPI to `cpu` (ignored if it is not online). Lock-free.
pub fn send_ipi(cpu: usize, kind: IpiKind) {
    if !is_online(cpu) || cpu >= cpu_count() {
        return;
    }
    gic::send_sgi(cpu, mpidr_of(cpu), sgi_for(kind));
}

/// `send_ipi` to every other online CPU.
pub fn send_ipi_others(kind: IpiKind) {
    let me = cpu::id();
    for c in 0..cpu_count() {
        if c != me {
            send_ipi(c, kind);
        }
    }
}

// ---------------------------------------------------------------------------
// Self-test (`patina.selftest=smp`).

fn selftest_ap(cpu: usize) -> ! {
    crate::kprintln!(
        "[patina] smp: cpu {} online (mpidr {:#x}, entered at EL{})",
        cpu,
        cpu::mpidr() & MPIDR_AFF_MASK,
        ENTRY_EL[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed)
    );
    irq::enable();
    loop {
        cpu::wait_for_interrupt();
    }
}

/// Spin until `cond()` or `ns` elapsed (interrupts are taken meanwhile if
/// IRQs are unmasked).
fn wait_until(ns: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = timer::now_ns() + ns;
    loop {
        if cond() {
            return true;
        }
        if timer::now_ns() > end {
            return cond();
        }
        core::hint::spin_loop();
    }
}

/// Wait for the next timer interrupt on this CPU (IRQs enabled).
fn wait_tick() {
    let c = timer::irq_count(cpu::id());
    if !wait_until(1_000_000_000, || timer::irq_count(cpu::id()) != c) {
        panic!(
            "selftest smp: no timer interrupt within 1 s on cpu {}",
            cpu::id()
        );
    }
}

/// `patina.selftest=smp`: start the secondaries, IPI them, check their
/// ticks and the boot CPU's one-shot deadline, stop them, exit 0.
pub fn selftest() -> ! {
    let n = cpu_count();
    crate::kprintln!(
        "[patina] selftest smp: {} cpus, hw_random {}",
        n,
        cpu::hw_random().map(|_| "yes").unwrap_or("none")
    );
    timer::init();
    let t0 = timer::now_ns();
    let started = start_secondaries(selftest_ap);
    crate::kprintln!(
        "[patina] selftest smp: {} secondaries started in {} us",
        started,
        (timer::now_ns() - t0) / 1000
    );
    assert_eq!(started, n - 1, "selftest smp: not every secondary started");

    // IPIs: rounds of Reschedule to each AP one by one, then to all via
    // send_ipi_others; each round must be delivered within 100 ms (SGIs to
    // a CPU that still has one pending would coalesce, so wait per round).
    const ROUNDS: u64 = 5;
    let base = trap::IPIS.load(Ordering::Relaxed);
    let mut expect = base;
    for r in 0..ROUNDS {
        if r % 2 == 0 {
            for c in 1..n {
                send_ipi(c, IpiKind::Reschedule);
            }
        } else {
            send_ipi_others(IpiKind::Reschedule);
        }
        expect += (n - 1) as u64;
        if !wait_until(100_000_000, || trap::IPIS.load(Ordering::Relaxed) >= expect) {
            panic!(
                "selftest smp: round {}: {} of {} IPIs delivered after 100 ms",
                r,
                trap::IPIS.load(Ordering::Relaxed) - base,
                expect - base
            );
        }
    }
    let ipis = trap::IPIS.load(Ordering::Relaxed) - base;

    // Every CPU's periodic tick runs.
    let before: [u64; MAX_CPUS] = core::array::from_fn(timer::irq_count);
    let ok = wait_until(500_000_000, || {
        (0..n).all(|c| timer::irq_count(c) >= before[c] + 3)
    });
    let mut line = alloc::string::String::new();
    for (c, b) in before.iter().enumerate().take(n) {
        use core::fmt::Write;
        let _ = write!(line, " cpu{}:+{}", c, timer::irq_count(c) - b);
    }
    crate::kprintln!("[patina] selftest smp: timer irqs{}", line);
    assert!(ok, "selftest smp: a CPU's periodic tick is not running");

    let dl_us = deadline_check();

    let expect = trap::IPIS.load(Ordering::Relaxed) + (n - 1) as u64;
    send_ipi_others(IpiKind::Stop);
    if !wait_until(100_000_000, || trap::IPIS.load(Ordering::Relaxed) >= expect) {
        panic!("selftest smp: Stop IPIs not delivered within 100 ms");
    }
    // Stopped CPUs halt with IRQs masked: their ticks stop.
    let stopped: [u64; MAX_CPUS] = core::array::from_fn(timer::irq_count);
    wait_until(30_000_000, || false);
    for (c, s) in stopped.iter().enumerate().take(n).skip(1) {
        assert_eq!(
            timer::irq_count(c),
            *s,
            "selftest smp: cpu {} still takes interrupts after Stop",
            c
        );
    }
    crate::kprintln!(
        "[patina] selftest smp: ok ({} cpus, {} ipis, deadline {} us)",
        n,
        ipis,
        dl_us
    );
    crate::exit(0)
}

/// One-shot deadline on this CPU (tick running, IRQs on): a deadline 2 ms
/// out fires within 3 ms (between two 10 ms periodic ticks), a cancelled
/// one never fires, and one in the past fires at once. Returns the
/// best measured latency of the 2 ms deadline in microseconds.
///
/// The latency check gets three attempts to come in at <= 3 ms: a vCPU
/// preempted by a busy host can see a late interrupt, and QEMU TCG on a
/// macOS host adds ~0.6 ms of host-timer slack (occasionally > 1 ms). If
/// none does, the best attempt must still be <= 5 ms (half a tick period),
/// which a deadline that is not programmed at all can never meet: it would
/// only be noticed at the next periodic tick, ~10 ms after the last one.
fn deadline_check() -> u64 {
    let me = cpu::id();
    let mut best = u64::MAX;
    for _ in 1..=3 {
        // Right after a periodic tick, so the next one is ~10 ms away.
        wait_tick();
        let c0 = timer::irq_count(me);
        let t0 = timer::now_ns();
        timer::set_deadline(Some(t0 + 2_000_000));
        let fired = wait_until(50_000_000, || !timer::deadline_pending());
        let dt = timer::now_ns() - t0;
        assert!(
            fired,
            "selftest smp: 2 ms deadline did not fire within 50 ms"
        );
        let extra = timer::irq_count(me) - c0;
        crate::kprintln!(
            "[patina] selftest smp: deadline +2 ms fired after {} us ({} timer irq)",
            dt / 1000,
            extra
        );
        assert!(dt >= 2_000_000, "selftest smp: deadline fired early");
        best = best.min(dt);
        if dt <= 3_000_000 {
            break;
        }
    }
    if best > 3_000_000 {
        crate::kprintln!(
            "[patina] selftest smp: warning: deadline latency {} us > 3000 us in 3 attempts (slow host?)",
            best / 1000
        );
    }
    assert!(
        best <= 5_000_000,
        "selftest smp: deadline latency {} us > 5000 us: one-shot not programmed?",
        best / 1000
    );

    // Cancel: no interrupt in the following 5 ms (same three attempts: a
    // preempted vCPU may see the next periodic tick in the window).
    for attempt in 1..=3 {
        wait_tick();
        let c1 = timer::irq_count(me);
        let t1 = timer::now_ns();
        timer::set_deadline(Some(t1 + 2_000_000));
        timer::set_deadline(None);
        while timer::now_ns() < t1 + 5_000_000 {
            core::hint::spin_loop();
        }
        let n1 = timer::irq_count(me) - c1;
        crate::kprintln!(
            "[patina] selftest smp: cancelled deadline: {} timer irqs in 5 ms",
            n1
        );
        if n1 == 0 {
            break;
        }
        assert!(attempt < 3, "selftest smp: cancelled deadline fired");
    }

    // Past: fires immediately.
    wait_tick();
    let t2 = timer::now_ns();
    timer::set_deadline(Some(0));
    let fired = wait_until(50_000_000, || !timer::deadline_pending());
    let dt2 = timer::now_ns() - t2;
    crate::kprintln!(
        "[patina] selftest smp: past deadline fired after {} us",
        dt2 / 1000
    );
    assert!(
        fired && dt2 < 1_000_000,
        "selftest smp: past deadline did not fire at once"
    );
    best / 1000
}
