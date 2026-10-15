//! The scheduler (design §5): per-CPU round robin with a time slice,
//! wake-ups placed on idle CPUs (IPI), idle CPUs park in `cpu::idle_wait`
//! and steal from busy ones. No priorities.
//!
//! # Model
//! Each CPU has a [`PerCpu`] (indexed by `cpu::id()`): the current thread,
//! a FIFO runqueue, the idle context (the CPU's boot / AP entry stack), the
//! `prev` hand-off slot, `need_resched` and its armed one-shot deadline.
//!
//! Thread states: `Runnable` (on exactly one runqueue, or being placed by
//! the one waker that moved it there) → `Running` (current on one CPU) →
//! back to `Runnable` (preempted / yield), to `Blocked` (`wait::block`,
//! sleeping) or to `Dead` (`exit`). The compare-exchange in
//! `Thread::try_wake` is the single point that moves Blocked → Runnable;
//! the waker that wins it enqueues the thread exactly once.
//!
//! # Which CPU am I on?
//! The kernel is not preemptible and a thread only migrates while it is
//! not running, so `cpu::id()` / [`this_cpu`] can only change under a
//! thread across a call to [`schedule`] (directly, or through anything that
//! blocks: `wait::block`, `sleep_until`, a futex wait, blocking fd I/O). It
//! is stable inside an IRQ-masked region and within a syscall up to the
//! first block. Code after a block must re-read [`this_cpu`].
//!
//! # The `on_cpu` protocol (the SMP switch race)
//! A thread that blocks on CPU A can be woken by CPU B before A has saved
//! its kernel context (between `begin_block` and the end of `ctx::switch`).
//! `Thread::on_cpu` is set when a CPU switches the thread in and cleared by
//! the CPU that switched it out only after the switch (in
//! [`finish_switch`], run on the new stack, which finds the outgoing thread
//! in `PerCpu::prev`). So:
//! * a waker whose `try_wake` succeeded spins (IRQs masked) until `on_cpu`
//!   is clear and only then enqueues the thread — it never runs on two
//!   CPUs and its saved context is complete when another CPU loads it;
//!   the window is a few hundred instructions on A, which holds no lock a
//!   waker could hold (A only takes its own runqueue lock, and nobody
//!   spins on `on_cpu` while holding a runqueue lock);
//! * [`schedule`] requeues `prev` only if it is still `Running` (preempt /
//!   yield), on its own runqueue, so it is popped there only after the
//!   switch; a stealer skips runqueue entries whose `on_cpu` is still set;
//! * a blocked `prev` that was woken in the window is `Runnable` when
//!   `schedule` looks at it: it is switched out like a blocked thread and
//!   the waker enqueues it.
//!
//! A dead thread's `Arc` sits in `PerCpu::prev` across the switch away
//! from it and is dropped (with its kernel stack freed, D39) by
//! [`finish_switch`] on that same CPU.
//!
//! # Placement (wake affinity, D38 SMP)
//! [`wake`] and [`make_runnable`] put the thread on the waker's CPU, unless
//! the waker is running a thread and some other CPU is idle: then the idle
//! CPU (the thread's last CPU if it is idle, else the lowest idle one) is
//! claimed by clearing its bit in `IDLE` and gets the thread plus a
//! `Reschedule` IPI. A waker running in its CPU's idle loop context (tick
//! in idle) keeps the thread local. New threads (`clone`) follow the same
//! rule, so Go's Ms spread over idle CPUs. An idle CPU with an empty
//! runqueue steals the oldest stealable thread from the longest other
//! runqueue (`try_lock` only: on contention it retries without sleeping).
//!
//! # Time slice and deadlines
//! A thread may run [`QUANTUM_NS`] (20 ms, measured with `timer::now_ns`:
//! one-shot deadline interrupts also call the tick, so ticks are not
//! quanta) before the tick sets `need_resched`. Sleep-list deadlines
//! program the per-CPU one-shot timer: [`arm_deadline`] when a sleeper is
//! added (on the adding CPU, if earlier than what it has armed), and every
//! ticking CPU's tick re-arms its one-shot to the global earliest deadline
//! (sleepers and wall-clock timers).
//!
//! # Tickless idle (D77)
//! A CPU about to wait in its idle loop with nothing to run stops its
//! periodic tick (`timer::set_periodic(false)`) and arms its one-shot only
//! for the earliest deadline it *owns*: sleepers it added and wall-clock
//! timers it last set (`sleep::next_deadline_for`,
//! `timers::next_wall_deadline_for`). It restarts the tick when it
//! switches to a thread. Nothing else needs an idle CPU's tick: CPU time
//! is accounted at switches, CPU-clock timers only advance on CPUs that
//! run threads (and so tick), the quantum belongs to running threads,
//! deferred TLB reclaim also runs here before waiting and at every vm
//! operation, and wake-ups / signals / TLB flushes reach an idle CPU by
//! IPI.
//!
//! # Stop (exit_group / exit / panic)
//! [`stop_other_cpus`] sends `Stop` to every other CPU and waits up to
//! 10 ms for their bits in `STOPPED` ([`ipi_stop`] sets the bit and halts).
//! Only the first CPU to start an exit proceeds; any other CPU calling it
//! later halts. A CPU only takes the IPI with IRQs unmasked, i.e. while it
//! holds no `SpinLock` (they mask IRQs), so a stopped CPU never holds the
//! console lock; a CPU that does not stop in time is stuck with IRQs
//! masked and cannot run user code; the exit sentinel is printed with
//! `console::panic_write_fmt`, which force-unlocks a stuck console lock.
//!
//! # Lock order
//! futex bucket → address space → frame allocator;
//! wait-queue lock (futex bucket / file / `PollWaiter` / epoll) → sleep
//! list → per-CPU runqueue. A runqueue lock is never held while taking
//! another runqueue lock: enqueue/pop lock one runqueue; the stealer
//! `try_lock`s the victim's only. `Thread::sig` (signals) is a leaf.
//! `IDLE`, `ONLINE`, `STOPPED`, `on_cpu`, `need_resched` are atomics.
//! The console lock is a leaf and is taken from IRQ context (fine: every
//! `SpinLock` masks IRQs). Nobody spins on `on_cpu` with a runqueue lock
//! held.
//!
//! # Rules
//! * [`schedule`] is called with no spinlock held (it masks IRQs itself);
//!   `wait::block` calls it with IRQs still masked after releasing the
//!   wait-queue lock. The tick only sets `need_resched`; switches happen on
//!   the way back to user mode (`trap::before_return_to_user`), when a
//!   thread blocks, yields or exits, or at an explicit [`preempt_point`].
//! * FP state (x86_64; no-ops on aarch64): `fp_save` into the outgoing
//!   thread, `fp_restore` from the incoming one if it has run before
//!   (fresh threads load their start state in the trampoline).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::{Thread, ThreadKind, ThreadState};
use crate::arch::cpu::MAX_CPUS;
use crate::arch::{cpu, ctx, irq, smp, timer};
use crate::sync::SpinLock;
use crate::trap::IpiKind;

/// How long a thread may run before the tick asks for a reschedule.
pub const QUANTUM_NS: u64 = 20_000_000;

/// "No deadline armed".
const NO_DEADLINE: u64 = u64::MAX;

/// Per-CPU scheduler state.
pub struct PerCpu {
    /// The running thread; `None` while this CPU runs its idle loop.
    current: UnsafeCell<Option<Arc<Thread>>>,
    /// The thread this CPU just switched away from, handed to
    /// [`finish_switch`] in the context switched to (clears its `on_cpu`;
    /// frees its stack if it is dead).
    prev: UnsafeCell<Option<Arc<Thread>>>,
    /// Runnable threads, round robin.
    runq: SpinLock<VecDeque<Arc<Thread>>>,
    /// `runq.len()`, readable without the lock (stealing heuristics).
    runq_len: AtomicUsize,
    /// The idle loop's kernel context (the CPU's boot / entry stack).
    idle_ctx: UnsafeCell<ctx::KernelContext>,
    /// Switch at the next opportunity (return to user, preempt point).
    need_resched: AtomicBool,
    /// When the current thread's time slice started (`now_ns`).
    slice_start: AtomicU64,
    /// When the current thread's CPU time was last accounted.
    run_start: AtomicU64,
    /// The one-shot deadline armed on this CPU (`NO_DEADLINE`: none).
    armed: AtomicU64,
    /// Context switches into a thread on this CPU (diagnostics).
    switches: AtomicU64,
    /// `tick` calls on this CPU, and those taken in the idle loop
    /// (diagnostics, `patina.stats`).
    ticks: AtomicU64,
    idle_ticks: AtomicU64,
    /// Times the idle loop waited for an interrupt (`patina.stats`).
    idle_waits: AtomicU64,
}

// SAFETY: the UnsafeCells are only accessed on this CPU with IRQs masked;
// the runqueue has its own lock; everything else is atomic.
unsafe impl Sync for PerCpu {}

impl PerCpu {
    const fn new() -> PerCpu {
        PerCpu {
            current: UnsafeCell::new(None),
            prev: UnsafeCell::new(None),
            runq: SpinLock::new(VecDeque::new()),
            runq_len: AtomicUsize::new(0),
            idle_ctx: UnsafeCell::new(ctx::KernelContext::new()),
            need_resched: AtomicBool::new(false),
            slice_start: AtomicU64::new(0),
            run_start: AtomicU64::new(0),
            armed: AtomicU64::new(NO_DEADLINE),
            switches: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            idle_ticks: AtomicU64::new(0),
            idle_waits: AtomicU64::new(0),
        }
    }

    /// Request a reschedule at the next opportunity.
    pub fn set_need_resched(&self) {
        self.need_resched.store(true, Ordering::Relaxed);
    }

    /// Context switches into a thread on this CPU so far.
    pub fn switches(&self) -> u64 {
        self.switches.load(Ordering::Relaxed)
    }

    /// Runnable threads waiting on this CPU.
    pub fn runnable(&self) -> usize {
        self.runq_len.load(Ordering::Relaxed)
    }

    fn push(&self, t: Arc<Thread>) {
        let mut q = self.runq.lock();
        q.push_back(t);
        self.runq_len.store(q.len(), Ordering::Relaxed);
    }

    fn pop(&self) -> Option<Arc<Thread>> {
        let mut q = self.runq.lock();
        let t = q.pop_front();
        self.runq_len.store(q.len(), Ordering::Relaxed);
        t
    }

    fn is_idle_context(&self) -> bool {
        // SAFETY: read of this CPU's `current` by code running on this
        // CPU; only this CPU writes it, in non-interruptible sections.
        unsafe { (*self.current.get()).is_none() }
    }
}

static CPUS: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];
/// Set once `run`/`run_smp` started the idle loop; before that the tick
/// only counts and nothing switches (arch self-tests run without a
/// scheduler).
static RUNNING: AtomicBool = AtomicBool::new(false);
/// CPUs running the scheduler (bit i = CPU i).
static ONLINE: AtomicU64 = AtomicU64::new(0);
/// CPUs in their idle loop with nothing to run (claimable by wakers).
static IDLE: AtomicU64 = AtomicU64::new(0);
/// CPUs that took a `Stop` IPI (or halted in a second exit / panic).
static STOPPED: AtomicU64 = AtomicU64::new(0);
/// 1 + the CPU that started the VM's exit (0: none yet).
static EXIT_OWNER: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn bit(c: usize) -> u64 {
    1u64 << c
}

/// This CPU's scheduler state. Stable within an IRQ-masked region and
/// until the calling thread next blocks (see the module docs).
#[inline]
pub fn this_cpu() -> &'static PerCpu {
    &CPUS[cpu::id()]
}

/// Whether the scheduler is running.
#[inline]
pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// Mask of CPUs running the scheduler (`sched_getaffinity`); CPU 0 alone
/// before the scheduler starts.
pub fn online_mask() -> u64 {
    match ONLINE.load(Ordering::Acquire) {
        0 => 1,
        m => m,
    }
}

/// Number of CPUs running the scheduler (>= 1).
pub fn online_cpus() -> usize {
    online_mask().count_ones() as usize
}

/// The thread running on this CPU. Panics in the idle loop / before the
/// scheduler starts.
///
/// The reference stays valid for as long as the caller runs on this
/// thread: a thread is kept alive by `PerCpu::current` while it runs and
/// cannot be freed before it exits (and an exited thread never returns to
/// its callers).
pub fn current() -> &'static Thread {
    // SAFETY: this CPU's `current` changes only in `context_switch` with
    // IRQs masked, and is this thread again whenever this thread runs (on
    // whichever CPU it runs on).
    let cur = unsafe { (*this_cpu().current.get()).as_ref() };
    let t = cur.expect("task: no current thread");
    // SAFETY: see above (the Arc outlives every use by the running thread).
    unsafe { &*Arc::as_ptr(t) }
}

/// The current thread as an `Arc`.
pub fn current_arc() -> Arc<Thread> {
    let flags = irq::save_disable();
    // SAFETY: IRQs masked, this CPU's data.
    let t = unsafe { (*this_cpu().current.get()).clone() };
    irq::restore(flags);
    t.expect("task: no current thread")
}

/// CPU time (user + system, ns) the calling thread has used so far,
/// including the running slice (`CLOCK_THREAD_CPUTIME_ID`).
pub fn current_cpu_time_ns() -> u64 {
    let flags = irq::save_disable();
    let c = this_cpu();
    let t = current();
    let v = t.cpu_time_ns.load(Ordering::Relaxed)
        + timer::now_ns().saturating_sub(c.run_start.load(Ordering::Relaxed));
    irq::restore(flags);
    v
}

/// CPU time (ns) accounted to `t` up to its last switch out (a running
/// thread's current slice is not included; use [`current_cpu_time_ns`]
/// for the caller).
pub fn cpu_time_ns(t: &Thread) -> u64 {
    t.cpu_time_ns.load(Ordering::Relaxed)
}

/// CPU time (ns) of any thread including its running slice (CPU-clock
/// timers checked from the tick, `clock_gettime` of another thread's
/// clock). Lock-free: retried if the thread is switched out while being
/// read. A reader racing the few instructions of a switch-in (state
/// already `Running`, `run_start` not yet updated) can overcount once by
/// the previous thread's slice; callers tolerate that.
pub fn thread_cpu_time_ns(t: &Thread) -> u64 {
    loop {
        let acc = t.cpu_time_ns.load(Ordering::Acquire);
        if t.state() != ThreadState::Running {
            if t.cpu_time_ns.load(Ordering::Acquire) == acc {
                return acc;
            }
            continue;
        }
        let c = t.cpu.load(Ordering::Relaxed);
        let start = CPUS[c].run_start.load(Ordering::Acquire);
        let now = timer::now_ns();
        if t.cpu_time_ns.load(Ordering::Acquire) == acc
            && t.state() == ThreadState::Running
            && t.cpu.load(Ordering::Relaxed) == c
        {
            return acc + now.saturating_sub(start);
        }
    }
}

/// Choose the CPU for a thread that becomes runnable (see the module docs)
/// and claim it if it is an idle remote CPU. Returns (cpu, needs IPI).
fn place(t: &Thread) -> (usize, bool) {
    let me = cpu::id();
    let mut idle = IDLE.load(Ordering::SeqCst) & !bit(me);
    if idle == 0 || this_cpu().is_idle_context() {
        return (me, false);
    }
    let last = t.cpu.load(Ordering::Relaxed);
    while idle != 0 {
        let target = if last < 64 && idle & bit(last) != 0 {
            last
        } else {
            idle.trailing_zeros() as usize
        };
        if IDLE.fetch_and(!bit(target), Ordering::SeqCst) & bit(target) != 0 {
            return (target, true);
        }
        idle = IDLE.load(Ordering::SeqCst) & !bit(me);
    }
    (me, false)
}

/// Enqueue a Runnable thread (not on any CPU) where [`place`] says.
fn enqueue_placed(t: Arc<Thread>) {
    let (target, ipi) = place(&t);
    crate::net::prof::on_enqueue(t.tid, cpu::id(), ipi);
    CPUS[target].push(t);
    if ipi {
        smp::send_ipi(target, IpiKind::Reschedule);
    }
}

/// Make a new (never run) thread runnable: on an idle CPU if any, else
/// on this one.
pub fn make_runnable(t: Arc<Thread>) {
    t.set_state(ThreadState::Runnable);
    enqueue_placed(t);
}

/// Wake `t` if it is still blocked in block `seq`; true if this call woke
/// it. May be called with wait-queue / sleep-list locks held and IRQs
/// masked (lock order: those → runqueue).
pub fn wake(t: &Arc<Thread>, seq: u64, reason: super::WakeReason) -> bool {
    if !t.try_wake(seq, reason) {
        return false;
    }
    // The thread may still be switching out on another CPU (it blocked
    // and we woke it before its context was saved): wait until it is off.
    while t.on_cpu.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    enqueue_placed(t.clone());
    true
}

/// Interrupt `t` if it is running on another CPU (signal queued for it):
/// its CPU gets a `Reschedule` IPI and runs the return-to-user path
/// (`trap::before_return_to_user`) before going back to user code. A
/// thread that is not running picks the signal up when it next returns to
/// user mode; a blocked one must be woken by the caller.
pub fn kick_remote(t: &Thread) {
    if t.state() != ThreadState::Running {
        return;
    }
    let c = t.cpu.load(Ordering::Acquire);
    if c != cpu::id() {
        smp::send_ipi(c, IpiKind::Reschedule);
    }
}

/// Turn the calling (boot) context into this CPU's idle loop without
/// starting the other CPUs (kernel self-tests). Never returns.
pub fn run() -> ! {
    // The tick must run before anything switches; it also unmasks IRQs.
    timer::init();
    ONLINE.fetch_or(bit(cpu::id()), Ordering::SeqCst);
    RUNNING.store(true, Ordering::Release);
    idle_loop()
}

/// Boot CPU: start the tick and the secondary CPUs (each enters
/// [`ap_main`]), make thread 0 runnable here and become this CPU's idle
/// loop. Never returns.
pub fn run_smp(t0: Arc<Thread>) -> ! {
    // Before `start_secondaries`: aarch64 sets up the GIC and the tick
    // period here. Also unmasks IRQs on this CPU.
    timer::init();
    ONLINE.fetch_or(bit(cpu::id()), Ordering::SeqCst);
    RUNNING.store(true, Ordering::Release);
    let started = smp::start_secondaries(ap_main);
    // `start_secondaries` returns once each AP reached `ap_main`; give the
    // last one a moment to publish its ONLINE bit.
    let t = timer::now_ns();
    while online_cpus() < started + 1 && timer::now_ns() - t < 100_000_000 {
        core::hint::spin_loop();
    }
    crate::kprintln!("[patina] smp: {} cpus online", online_cpus());
    t0.set_state(ThreadState::Runnable);
    this_cpu().push(t0);
    idle_loop()
}

/// Secondary CPU entry (from `arch::smp::start_secondaries`, IRQs masked,
/// on the AP's own kernel stack, which becomes its idle context).
pub fn ap_main(cpu_idx: usize) -> ! {
    debug_assert_eq!(cpu_idx, cpu::id());
    if super::try_process().is_some() {
        super::activate_user_tables();
    }
    ONLINE.fetch_or(bit(cpu_idx), Ordering::SeqCst);
    idle_loop()
}

/// Steal a thread for this (idle) CPU from the longest other runqueue.
/// Returns `Err(())` if there was something to steal that could not be
/// taken right now (lock contention, or only threads still switching out).
fn steal(me: usize) -> Result<Option<Arc<Thread>>, ()> {
    let online = ONLINE.load(Ordering::Acquire) & !bit(me);
    if online == 0 {
        return Ok(None);
    }
    let mut victim = None;
    let mut longest = 0;
    let mut m = online;
    while m != 0 {
        let c = m.trailing_zeros() as usize;
        m &= m - 1;
        let l = CPUS[c].runq_len.load(Ordering::Relaxed);
        if l > longest {
            longest = l;
            victim = Some(c);
        }
    }
    let Some(v) = victim else {
        return Ok(None);
    };
    let v = &CPUS[v];
    let Some(mut q) = v.runq.try_lock() else {
        return Err(());
    };
    let Some(pos) = q.iter().position(|t| !t.on_cpu.load(Ordering::Acquire)) else {
        return if q.is_empty() { Ok(None) } else { Err(()) };
    };
    let t = q.remove(pos);
    v.runq_len.store(q.len(), Ordering::Relaxed);
    Ok(t)
}

fn idle_loop() -> ! {
    let _ = irq::save_disable();
    let me = cpu::id();
    let cpu = &CPUS[me];
    let mut retries = 0u32;
    loop {
        let mut next = cpu.pop();
        if next.is_none() {
            // Publish "idle" before the last checks: a waker either sees
            // the bit (and IPIs us after enqueueing) or enqueued before our
            // re-check below.
            IDLE.fetch_or(bit(me), Ordering::SeqCst);
            next = cpu.pop();
            if next.is_none() {
                match steal(me) {
                    Ok(t) => next = t,
                    Err(()) if retries < 10_000 => {
                        retries += 1;
                        core::hint::spin_loop();
                        continue;
                    }
                    Err(()) => {}
                }
            }
        }
        retries = 0;
        match next {
            Some(next) => {
                IDLE.fetch_and(!bit(me), Ordering::SeqCst);
                cpu.need_resched.store(false, Ordering::Relaxed);
                // Leaving idle: the running thread gets the periodic tick
                // (quantum, CPU-clock timers) back.
                timer::set_periodic(true);
                // SAFETY: IRQs masked; the idle loop is the current context
                // of this CPU.
                unsafe { context_switch(cpu, None, Some(next)) };
            }
            // Nothing to run: go tickless and sleep until an interrupt
            // (lost-wakeup-free, also for IPIs, see `cpu::idle_wait`); a
            // deadline may have woken sleepers, an IPI may have brought
            // work.
            None => {
                enter_tickless(cpu, me);
                cpu.idle_waits.fetch_add(1, Ordering::Relaxed);
                cpu::idle_wait();
            }
        }
    }
}

/// Idle with nothing to run (IRQs masked): stop the periodic tick and arm
/// the one-shot for the earliest deadline this CPU owns (D77).
fn enter_tickless(cpu: &PerCpu, me: usize) {
    crate::mm::tlb::reclaim();
    let own = super::sleep::next_deadline_for(me)
        .into_iter()
        .chain(super::timers::next_wall_deadline_for(me))
        .min()
        .unwrap_or(NO_DEADLINE);
    timer::set_periodic(false);
    // Unconditionally: the arch drops a deadline once it passed, so
    // `armed` may be stale here.
    cpu.armed.store(own, Ordering::Relaxed);
    timer::set_deadline((own != NO_DEADLINE).then_some(own));
}

/// Global earliest deadline (sleepers, wall-clock timers).
fn earliest_deadline() -> u64 {
    let s = super::sleep::next_deadline().unwrap_or(NO_DEADLINE);
    let t = super::timers::next_wall_deadline().unwrap_or(NO_DEADLINE);
    s.min(t)
}

/// `patina.stats=1` at exit (other CPUs stopped): per-CPU timer
/// interrupts, ticks (all / in idle), idle waits, context switches.
pub fn print_stats() {
    let mut m = ONLINE.load(Ordering::SeqCst);
    while m != 0 {
        let c = m.trailing_zeros() as usize;
        m &= m - 1;
        let p = &CPUS[c];
        crate::console::panic_write_fmt(format_args!(
            "[patina] stats cpu{}: timer irqs {}, ticks {} (idle {}), idle waits {}, switches {}\n",
            c,
            timer::irq_count(c),
            p.ticks.load(Ordering::Relaxed),
            p.idle_ticks.load(Ordering::Relaxed),
            p.idle_waits.load(Ordering::Relaxed),
            p.switches.load(Ordering::Relaxed),
        ));
    }
    let (heap_user, heap_actual, heap_total) = crate::mm::heap::stats();
    let (slots, pooled, unmapped) = super::kstack::stats();
    crate::console::panic_write_fmt(format_args!(
        "[patina] stats mem: heap {} B used ({} B actual) / {} B, kstack slots {} ({} pooled, {} unmapped), tlb-pending frames {}, threads {}\n",
        heap_user,
        heap_actual,
        heap_total,
        slots,
        pooled,
        unmapped,
        crate::mm::tlb::pending_frames(),
        super::threads().len(),
    ));
}

/// Switch this CPU from `prev` (`None`: the idle loop) to `next` (`None`:
/// the idle loop). Returns when the context of `prev` is resumed — for a
/// thread possibly on another CPU, so callers re-read [`this_cpu`].
///
/// # Safety
/// IRQs masked; `cpu` is this CPU; `prev` is what runs now (the current
/// thread or the idle loop); `next` is Runnable and owned by no runqueue.
unsafe fn context_switch(cpu: &PerCpu, prev: Option<Arc<Thread>>, next: Option<Arc<Thread>>) {
    let now = timer::now_ns();
    let from = match &prev {
        Some(p) => {
            let ran = now.saturating_sub(cpu.run_start.load(Ordering::Relaxed));
            p.cpu_time_ns.fetch_add(ran, Ordering::Relaxed);
            if p.state() != ThreadState::Dead {
                // SAFETY: `p` is current on this CPU, IRQs masked.
                ctx::fp_save(&mut unsafe { p.switch_state() }.fp);
            }
            p.kctx_ptr()
        }
        None => cpu.idle_ctx.get(),
    };
    let to = match &next {
        Some(n) => {
            // Never true by the on_cpu protocol (wakers wait for it before
            // enqueueing, stealers skip such threads); cheap insurance.
            while n.on_cpu.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            n.on_cpu.store(true, Ordering::Relaxed);
            n.cpu.store(cpu::id(), Ordering::Relaxed);
            n.set_state(ThreadState::Running);
            cpu.slice_start.store(now, Ordering::Relaxed);
            cpu.switches.fetch_add(1, Ordering::Relaxed);
            cpu::set_kernel_stack(n.kstack.top);
            // SAFETY: `n` is being switched in on this CPU with IRQs masked
            // and is on no other CPU (on_cpu was clear).
            let sw = unsafe { n.switch_state() };
            if sw.started {
                ctx::fp_restore(&sw.fp);
            } else {
                sw.started = true;
            }
            n.kctx_ptr()
        }
        None => cpu.idle_ctx.get() as *const _,
    };
    cpu.run_start.store(now, Ordering::Relaxed);
    // SAFETY: this CPU's data, IRQs masked. `prev` (and with it a dead
    // thread's stack and context) stays alive in the `prev` slot until
    // `finish_switch` in the context switched to; `next` is kept alive by
    // `current`.
    unsafe {
        *cpu.current.get() = next;
        *cpu.prev.get() = prev;
        ctx::switch(from, to);
    }
    finish_switch();
}

/// Run after every switch, in the context that was switched to (on this
/// CPU): release the thread this CPU switched away from — clear its
/// `on_cpu` so other CPUs may run it, free its stack if it exited.
fn finish_switch() {
    let cpu = this_cpu();
    // SAFETY: IRQs masked (all switch paths), this CPU's data.
    let p = unsafe { (*cpu.prev.get()).take() };
    if let Some(p) = p {
        if p.state() == ThreadState::Dead {
            super::kstack::free(p.kstack);
        }
        p.on_cpu.store(false, Ordering::Release);
        drop(p);
    }
}

/// Give up the CPU: pick the next runnable thread (round robin). The
/// current thread is requeued if it is still `Running` (preemption,
/// yield); if it is `Blocked` (or was woken while blocking, see the
/// module docs) it stays off the runqueue. With nothing else to run a
/// running thread just continues and a blocked one switches to the idle
/// loop. Returns when the thread is scheduled again, possibly on another
/// CPU.
///
/// Must not be called with a spinlock held.
pub fn schedule() {
    if !is_running() {
        return;
    }
    let flags = irq::save_disable();
    let cpu = this_cpu();
    cpu.need_resched.store(false, Ordering::Relaxed);
    // SAFETY: IRQs masked, this CPU's data.
    let Some(prev) = (unsafe { (*cpu.current.get()).clone() }) else {
        irq::restore(flags);
        return;
    };
    let still_running = prev.state() == ThreadState::Running;
    match cpu.pop() {
        None if still_running => {
            cpu.slice_start.store(timer::now_ns(), Ordering::Relaxed);
            drop(prev);
        }
        // Blocked with nothing else to run: go idle (the idle loop steals
        // or waits).
        // SAFETY: IRQs masked; `prev` is current on this CPU.
        None => unsafe { context_switch(cpu, Some(prev), None) },
        Some(next) => {
            if still_running {
                prev.set_state(ThreadState::Runnable);
                cpu.push(prev.clone());
            }
            // SAFETY: as above.
            unsafe { context_switch(cpu, Some(prev), Some(next)) };
        }
    }
    irq::restore(flags);
}

/// `sched_yield`: requeue behind every runnable thread on this CPU.
pub fn yield_now() {
    schedule();
}

/// Switch now if a reschedule is due (for long-running kernel code; the
/// kernel is otherwise not preemptible).
pub fn preempt_point() {
    if this_cpu().need_resched.load(Ordering::Relaxed) {
        schedule();
    }
}

/// A sleeper with `deadline` was just added (IRQs masked): make sure this
/// CPU's one-shot fires no later than that.
pub(super) fn arm_deadline(deadline: u64) {
    if !is_running() {
        return;
    }
    let c = this_cpu();
    if deadline < c.armed.load(Ordering::Relaxed) {
        c.armed.store(deadline, Ordering::Relaxed);
        timer::set_deadline(Some(deadline));
    }
}

/// Timer tick (periodic or one-shot deadline; IRQs masked): expire
/// sleepers, re-arm this CPU's one-shot to the earliest remaining
/// deadline, account the time slice.
pub fn tick() {
    if !is_running() {
        return;
    }
    let cpu = this_cpu();
    cpu.ticks.fetch_add(1, Ordering::Relaxed);
    let now = timer::now_ns();
    let woke = super::sleep::expire(now);
    super::timers::tick();
    crate::mm::tlb::reclaim();
    if cpu.is_idle_context() {
        // The idle loop re-checks the runqueue after the IRQ and re-arms
        // for its own deadlines (`enter_tickless`) if it waits again.
        cpu.idle_ticks.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let next = earliest_deadline();
    if next != cpu.armed.load(Ordering::Relaxed) {
        cpu.armed.store(next, Ordering::Relaxed);
        timer::set_deadline((next != NO_DEADLINE).then_some(next));
    }
    if woke > 0 && cpu.runnable() > 0 {
        // Run a timer-woken thread promptly (sysmon, time.Sleep).
        cpu.set_need_resched();
    }
    if now.saturating_sub(cpu.slice_start.load(Ordering::Relaxed)) >= QUANTUM_NS {
        cpu.set_need_resched();
        cpu.slice_start.store(now, Ordering::Relaxed);
    }
}

/// A `Reschedule` IPI arrived on this CPU (IRQs masked): mark
/// `need_resched` so the return-to-user path re-checks the runqueue (and
/// delivers signals); an idle CPU re-checks it after `cpu::idle_wait`.
pub fn ipi_reschedule() {
    if is_running() {
        this_cpu().set_need_resched();
    }
}

/// Halt this CPU for good, recording it in `STOPPED`.
pub fn halt_this_cpu() -> ! {
    let _ = irq::save_disable();
    let me = cpu::id();
    IDLE.fetch_and(!bit(me), Ordering::SeqCst);
    STOPPED.fetch_or(bit(me), Ordering::SeqCst);
    cpu::halt()
}

/// A `Stop` IPI arrived (IRQs masked): halt.
pub fn ipi_stop() -> ! {
    halt_this_cpu()
}

/// Whether some CPU has started the VM exit ([`stop_other_cpus`] ran):
/// device code on the exit/panic path may then break locks a stopped CPU
/// still holds.
pub fn exiting() -> bool {
    EXIT_OWNER.load(Ordering::SeqCst) != 0
}

/// Stop every other CPU before the VM exits (exit_group, `crate::exit`,
/// panic): the first caller sends `Stop` and waits up to 10 ms for the
/// others to halt, then returns; a later caller on another CPU halts
/// (the VM is already going down); the owner calling again returns at
/// once.
pub fn stop_other_cpus() {
    let me = cpu::id();
    match EXIT_OWNER.compare_exchange(0, me + 1, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(_) => {}
        Err(owner) if owner == me + 1 => return,
        Err(_) => halt_this_cpu(),
    }
    // To every online CPU (also ones an arch self-test started); wait only
    // for those running the scheduler.
    smp::send_ipi_others(IpiKind::Stop);
    let others = ONLINE.load(Ordering::SeqCst) & !bit(me);
    let t0 = timer::now_ns();
    while STOPPED.load(Ordering::SeqCst) & others != others
        && timer::now_ns().saturating_sub(t0) < 10_000_000
    {
        core::hint::spin_loop();
    }
}

/// A signal became pending for `t` (and `t` does not block it): make sure
/// `t` notices it promptly (called by `task::signal` after the pending bit
/// is published, any context, any lock but the runqueue's).
/// * blocked interruptibly → woken with `WakeReason::Interrupted`;
/// * current on this CPU → nothing: delivery runs on its way back to user
///   mode (`trap::before_return_to_user`);
/// * runnable → nothing: it passes through the same path when it runs;
/// * running on another CPU → [`kick_remote`] (Reschedule IPI).
pub fn kick(t: &Arc<Thread>) {
    if let Some(seq) = t.interruptible_seq() {
        wake(t, seq, super::WakeReason::Interrupted);
        return;
    }
    if t.state() == ThreadState::Running {
        let flags = irq::save_disable();
        // SAFETY: IRQs masked, this CPU's data.
        let here =
            unsafe { (*this_cpu().current.get()).as_ref() }.is_some_and(|c| Arc::ptr_eq(c, t));
        irq::restore(flags);
        if !here {
            kick_remote(t);
        }
    }
}

/// Hook for `trap::before_return_to_user` (IRQs masked): run a pending
/// reschedule. Signal delivery (`task::signal::deliver`) follows it in
/// `trap::before_return_to_user`.
pub fn before_return_to_user() {
    if is_running() && this_cpu().need_resched.load(Ordering::Relaxed) {
        schedule();
    }
}

/// End the calling thread: mark it Dead and switch away for good. Its
/// kernel stack is freed by the next context that runs on this CPU. The
/// caller has already done the exit bookkeeping (clear_child_tid, thread
/// table).
pub fn exit_current() -> ! {
    let _ = irq::save_disable();
    let cpu = this_cpu();
    // SAFETY: IRQs masked, this CPU's data.
    let prev = unsafe { (*cpu.current.get()).clone() }.expect("exit without a current thread");
    prev.set_state(ThreadState::Dead);
    let next = cpu.pop();
    // SAFETY: IRQs masked; `prev` is current here; the `prev` slot keeps
    // the dead thread (and its kctx/stack) alive across the switch.
    unsafe { context_switch(cpu, Some(prev), next) };
    unreachable!("a dead thread was switched back in");
}

/// First code a new user thread runs (from `ctx::new_kernel_context`, IRQs
/// masked, on its own kernel stack): load its start state and enter user
/// mode.
pub(super) extern "C" fn user_thread_entry(_tid: usize) -> ! {
    finish_switch();
    let t = current();
    let start = t.start.lock().take().expect("thread started twice");
    let frame: ctx::TrapFrame = (*start.frame).clone();
    ctx::fp_restore(&start.fp);
    drop(start);
    cpu::set_kernel_stack(t.kstack.top);
    // SAFETY: the process's user tables are live on every CPU running the
    // scheduler (one address space for the VM), `frame` is a user frame
    // from the loader or `clone`, the FP state is loaded and entries from
    // user mode use this thread's stack.
    unsafe { ctx::enter_user(&frame) }
}

/// First code a kernel thread runs: call its function with IRQs enabled,
/// then exit.
pub(super) extern "C" fn kernel_thread_entry(f: usize) -> ! {
    finish_switch();
    let ThreadKind::Kernel(func) = current().kind else {
        unreachable!("kernel_thread_entry for a user thread");
    };
    debug_assert_eq!(func as usize, f);
    irq::enable();
    func();
    super::unregister_current();
    exit_current()
}
