//! `patina.selftest=sched`: the scheduler without user mode.
//!
//! Three kernel threads on guarded kernel stacks:
//! * `spinner` busy-loops (never blocks or yields on its own) and only
//!   passes through `sched::preempt_point`, so it gives up the CPU solely
//!   because the tick used up its quantum;
//! * `sleeper` sleeps 3 × 30 ms on the sleep list, checking that no sleep
//!   ends early, then sets the condition and wakes `waiter`;
//! * `waiter` blocks on a wait queue until woken, then does a 20 ms timed
//!   wait nobody wakes (→ `TimedOut`), then signals completion.
//!
//! The spinner then checks: all sleeps done, waiter woken and timed out,
//! at least 3 tick preemptions, and both exited threads' stacks freed.
//! Expected: `selftest sched: ok (...)`, exit 0.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::wait::{self, WaitQueue};
use super::{WakeReason, kstack, sched};
use crate::arch::timer;
use crate::sync::SpinLock;

static Q: SpinLock<WaitQueue> = SpinLock::new(WaitQueue::new());
static READY: AtomicBool = AtomicBool::new(false);
static WOKEN: AtomicBool = AtomicBool::new(false);
static TIMED_OUT: AtomicBool = AtomicBool::new(false);
static DONE: AtomicBool = AtomicBool::new(false);
static SLEEPS: AtomicU32 = AtomicU32::new(0);

const MS: u64 = 1_000_000;

fn id(q: &mut WaitQueue) -> &mut WaitQueue {
    q
}

fn spinner() {
    let t0 = timer::now_ns();
    let mut preempted = 0u32;
    while !DONE.load(Ordering::Acquire) {
        // A reschedule is only due once the tick ended the quantum (or
        // woke a sleeper).
        let before = sched::this_cpu().switches();
        sched::preempt_point();
        if sched::this_cpu().switches() != before {
            preempted += 1;
        }
        if timer::now_ns() - t0 > 5_000 * MS {
            panic!("selftest sched: timed out (sleeps {})", SLEEPS.load(Ordering::Relaxed));
        }
        core::hint::spin_loop();
    }
    // The waiter exited right before switching back here; this thread's
    // `finish_switch` freed its stack (and the sleeper's earlier).
    let (slots, free) = kstack::stats();
    let ms = (timer::now_ns() - t0) / MS;
    crate::kprintln!(
        "[patina] selftest sched: spinner preempted {} times, sleeps {}, woken {}, timed out {}, stacks {} allocated / {} free, {} ms",
        preempted,
        SLEEPS.load(Ordering::Relaxed),
        WOKEN.load(Ordering::Relaxed),
        TIMED_OUT.load(Ordering::Relaxed),
        slots,
        free,
        ms
    );
    assert!(SLEEPS.load(Ordering::Relaxed) == 3, "sleeps incomplete");
    assert!(WOKEN.load(Ordering::Relaxed), "waiter not woken");
    assert!(TIMED_OUT.load(Ordering::Relaxed), "timed wait did not time out");
    assert!(preempted >= 3, "spinner was not preempted by the tick");
    assert!(slots == 3 && free == 2, "exited threads' stacks not freed");
    crate::kprintln!("[patina] selftest sched: ok");
    crate::exit(0)
}

fn sleeper() {
    for _ in 0..3 {
        let start = timer::now_ns();
        wait::sleep_until(start + 30 * MS);
        let slept = timer::now_ns() - start;
        assert!(slept >= 30 * MS, "slept only {} ns", slept);
        SLEEPS.fetch_add(1, Ordering::Relaxed);
    }
    let mut q = Q.lock();
    READY.store(true, Ordering::Release);
    let n = q.wake_all();
    crate::kprintln!("[patina] selftest sched: sleeper woke {} waiter(s)", n);
}

fn waiter() {
    let q = Q.lock();
    if !READY.load(Ordering::Acquire) {
        let r = wait::block(q, id, 0, 0, None);
        assert!(r == WakeReason::Woken);
    } else {
        drop(q);
    }
    WOKEN.store(true, Ordering::Release);
    let start = timer::now_ns();
    let r = wait::block(Q.lock(), id, 0, 0, Some(start + 20 * MS));
    let waited = timer::now_ns() - start;
    assert!(r == WakeReason::TimedOut && waited >= 20 * MS);
    assert!(Q.lock().is_empty(), "timed-out waiter left on the queue");
    TIMED_OUT.store(true, Ordering::Release);
    DONE.store(true, Ordering::Release);
}

#[inline(never)]
#[allow(unconditional_recursion)] // ends in the guard page by design
fn recurse(depth: usize) -> usize {
    let mut buf = [0u8; 512];
    // SAFETY: a plain volatile write to a local array (keeps the frame).
    unsafe { core::ptr::write_volatile(&mut buf[depth % 512], depth as u8) };
    recurse(depth + 1) + core::hint::black_box(buf[0]) as usize
}

fn overflow() {
    let st = super::current().kstack;
    crate::kprintln!(
        "[patina] selftest kstack: stack {:#x}-{:#x}, guard page below; recursing",
        st.base,
        st.top
    );
    let n = recurse(0);
    crate::kprintln!("[patina] selftest kstack: recursion returned {} (no fault!)", n);
}

/// `patina.selftest=kstack`: overflow a kernel thread's stack. Expected:
/// a fatal kernel fault reported as a stack overflow (aarch64: "kernel
/// stack overflow" from the overflow stack, D29; x86_64: #DF on IST1),
/// exit code 255.
pub fn run_overflow() -> ! {
    super::spawn_kernel_thread(overflow);
    sched::run()
}

/// Run the test; never returns.
pub fn run() -> ! {
    crate::kprintln!("[patina] selftest sched: starting 3 kernel threads");
    super::spawn_kernel_thread(spinner);
    super::spawn_kernel_thread(sleeper);
    super::spawn_kernel_thread(waiter);
    sched::run()
}
