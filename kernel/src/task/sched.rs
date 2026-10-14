//! The scheduler (design §5): per-CPU round robin with a tick.
//!
//! # Model
//! Each CPU has a [`PerCpu`]: the current thread, a FIFO runqueue of
//! runnable threads, the idle context (the CPU's boot stack) and the
//! `need_resched` flag. Phase 1 runs on CPU 0 only ([`this_cpu`] returns
//! the one static instance); Phase 2 makes it an array indexed by
//! `cpu::id()` and adds: wake-up to the waker's CPU + IPI if that CPU is
//! idle, an `on_cpu` flag so a waker on another CPU cannot run a thread
//! whose context is still being saved (see [`schedule`]), and stealing.
//!
//! Thread states: `Runnable` (on exactly one runqueue) → `Running`
//! (current on one CPU) → back to `Runnable` (preempted / yield), to
//! `Blocked` (`wait::block`, sleeping) or to `Dead` (`exit`). Wake-ups
//! (`Thread::try_wake`) move Blocked → Runnable and enqueue.
//!
//! # Rules
//! * [`schedule`] is called with no spinlock held (it masks IRQs itself).
//!   The kernel is not preemptible: the tick only sets `need_resched`; the
//!   switch happens on the way back to user mode
//!   (`trap::before_return_to_user`), when a thread blocks, yields or
//!   exits, or at an explicit [`preempt_point`].
//! * A thread never frees its own kernel stack: [`exit_current`] parks it
//!   in `PerCpu::zombie` and the next context to run on this CPU frees it
//!   in [`finish_switch`].
//! * FP state (x86_64; no-ops on aarch64): `fp_save` into the outgoing
//!   thread, `fp_restore` from the incoming one if it has run before
//!   (fresh threads load their start state in the trampoline).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::{Thread, ThreadKind, ThreadState};
use crate::arch::{cpu, ctx, irq, timer};
use crate::sync::SpinLock;

/// Ticks a thread may run before it is preempted (20 ms at 100 Hz).
pub const QUANTUM_TICKS: u32 = 2;

/// Per-CPU scheduler state.
pub struct PerCpu {
    /// The running thread; `None` while this CPU runs its idle loop.
    current: UnsafeCell<Option<Arc<Thread>>>,
    /// Runnable threads, round robin.
    runq: SpinLock<VecDeque<Arc<Thread>>>,
    /// The idle loop's kernel context (the CPU's boot stack).
    idle_ctx: UnsafeCell<ctx::KernelContext>,
    /// A thread that exited on this CPU; its stack is freed after the
    /// switch away from it.
    zombie: UnsafeCell<Option<Arc<Thread>>>,
    /// Switch at the next opportunity (return to user, preempt point).
    need_resched: AtomicBool,
    /// Ticks left in the current thread's quantum.
    quantum: AtomicU32,
    /// Context switches into a thread on this CPU (diagnostics).
    switches: AtomicU64,
}

// SAFETY: the UnsafeCells are only accessed on this CPU with IRQs masked
// (the runqueue has its own lock for Phase 2 remote wake-ups).
unsafe impl Sync for PerCpu {}

impl PerCpu {
    const fn new() -> PerCpu {
        PerCpu {
            current: UnsafeCell::new(None),
            runq: SpinLock::new(VecDeque::new()),
            idle_ctx: UnsafeCell::new(ctx::KernelContext::new()),
            zombie: UnsafeCell::new(None),
            need_resched: AtomicBool::new(false),
            quantum: AtomicU32::new(QUANTUM_TICKS),
            switches: AtomicU64::new(0),
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
        self.runq.lock().len()
    }
}

static CPU0: PerCpu = PerCpu::new();
/// Set once `run` started the idle loop; before that the tick only counts
/// and nothing switches (arch self-tests run without a scheduler).
static RUNNING: AtomicBool = AtomicBool::new(false);

/// This CPU's scheduler state. Phase 2: index a per-CPU array by
/// `cpu::id()` (or reach it through TPIDR_EL1 / GS).
#[inline]
pub fn this_cpu() -> &'static PerCpu {
    &CPU0
}

/// Whether the scheduler is running.
#[inline]
pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// The thread running on this CPU. Panics in the idle loop / before the
/// scheduler starts.
///
/// The reference stays valid for as long as the caller runs on this
/// thread: a thread is kept alive by `PerCpu::current` while it runs and
/// cannot be freed before it exits (and an exited thread never returns to
/// its callers).
pub fn current() -> &'static Thread {
    // SAFETY: this CPU's `current` changes only in `schedule`/`exit_current`
    // with IRQs masked, and is this thread again whenever this thread runs.
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

/// Make `t` runnable on this CPU (new threads, wake-ups).
pub fn make_runnable(t: Arc<Thread>) {
    t.set_state(ThreadState::Runnable);
    this_cpu().runq.lock().push_back(t);
}

/// Wake `t` if it is still blocked in block `seq`; true if this call woke
/// it. Phase 2: enqueue on the waker's (or the thread's last) CPU and IPI
/// it if idle.
pub fn wake(t: &Arc<Thread>, seq: u64, reason: super::WakeReason) -> bool {
    if !t.try_wake(seq, reason) {
        return false;
    }
    this_cpu().runq.lock().push_back(t.clone());
    true
}

/// Turn the calling (boot) context into this CPU's idle loop: start the
/// tick and run threads from the runqueue. Never returns.
pub fn run() -> ! {
    // The tick must run before user code; `timer::init` also unmasks IRQs.
    timer::init();
    RUNNING.store(true, Ordering::Release);
    idle_loop()
}

fn idle_loop() -> ! {
    let cpu = this_cpu();
    let _ = irq::save_disable();
    loop {
        let next = cpu.runq.lock().pop_front();
        match next {
            Some(next) => {
                cpu.need_resched.store(false, Ordering::Relaxed);
                // SAFETY: IRQs masked; idle_ctx belongs to this CPU and is
                // only used by this loop and by switches back to it.
                unsafe { switch_in(cpu, cpu.idle_ctx.get(), next) };
                finish_switch(cpu);
            }
            // Nothing to run: sleep until an interrupt (lost-wakeup-free,
            // see `cpu::idle_wait`); the tick may have woken sleepers.
            None => cpu::idle_wait(),
        }
    }
}

/// Make `next` current and switch to it from the context `from`.
///
/// # Safety
/// IRQs masked; `from` is the context of whatever runs now (the outgoing
/// thread's FP state already saved), and stays valid until resumed.
unsafe fn switch_in(cpu: &PerCpu, from: *mut ctx::KernelContext, next: Arc<Thread>) {
    next.set_state(ThreadState::Running);
    cpu.quantum.store(QUANTUM_TICKS, Ordering::Relaxed);
    cpu.switches.fetch_add(1, Ordering::Relaxed);
    cpu::set_kernel_stack(next.kstack.top);
    // SAFETY: `next` is being switched in on this CPU with IRQs masked.
    let sw = unsafe { next.switch_state() };
    if sw.started {
        ctx::fp_restore(&sw.fp);
    } else {
        sw.started = true;
    }
    let to = next.kctx_ptr();
    // SAFETY: this CPU's data, IRQs masked. Replacing `current` drops this
    // CPU's reference to the previous thread; callers keep it alive
    // (`schedule`'s local Arc, or the zombie slot).
    unsafe { *cpu.current.get() = Some(next) };
    // SAFETY: `to` was built by new_kernel_context or saved by switch; its
    // thread is kept alive by `current`.
    unsafe { ctx::switch(from, to) };
}

/// Run after every switch, in the context that was switched to: free the
/// stack of a thread that exited on this CPU.
fn finish_switch(cpu: &PerCpu) {
    // SAFETY: IRQs masked (all switch paths), this CPU's data.
    let z = unsafe { (*cpu.zombie.get()).take() };
    if let Some(z) = z {
        super::kstack::free(z.kstack);
        drop(z);
    }
}

/// Give up the CPU: pick the next runnable thread (round robin). The
/// current thread is requeued if it is still `Running` (preemption,
/// yield); if it is `Blocked` it stays off the runqueue until woken. With
/// nothing else to run a running thread just continues and a blocked one
/// switches to the idle loop. Returns when the thread is scheduled again.
///
/// Must not be called with a spinlock held.
///
/// Phase 2 (SMP): a thread that blocks may be woken on another CPU before
/// it is off this one; the waker must then wait for `on_cpu` to clear
/// before running it, and this function must not requeue it twice.
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
    let next = cpu.runq.lock().pop_front();
    match next {
        None if still_running => {
            cpu.quantum.store(QUANTUM_TICKS, Ordering::Relaxed);
        }
        None => {
            // Blocked with nothing else to run: go idle.
            // SAFETY: `prev` is current on this CPU, IRQs masked.
            ctx::fp_save(&mut unsafe { prev.switch_state() }.fp);
            // SAFETY: this CPU's data, IRQs masked.
            unsafe { *cpu.current.get() = None };
            // SAFETY: `prev` stays alive through the local Arc; the idle
            // context was saved by the idle loop's switch.
            unsafe { ctx::switch(prev.kctx_ptr(), cpu.idle_ctx.get()) };
            finish_switch(cpu);
        }
        Some(next) => {
            if still_running {
                prev.set_state(ThreadState::Runnable);
                cpu.runq.lock().push_back(prev.clone());
            }
            // SAFETY: `prev` is current on this CPU, IRQs masked.
            ctx::fp_save(&mut unsafe { prev.switch_state() }.fp);
            // SAFETY: IRQs masked; `prev` is kept alive by the local Arc
            // (and by the runqueue or a wait queue).
            unsafe { switch_in(cpu, prev.kctx_ptr(), next) };
            finish_switch(cpu);
        }
    }
    drop(prev);
    irq::restore(flags);
}

/// `sched_yield`: requeue behind every runnable thread.
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

/// Timer tick (IRQs masked): expire sleepers, account the quantum.
pub fn tick() {
    if !is_running() {
        return;
    }
    let cpu = this_cpu();
    let woke = super::sleep::expire(timer::now_ns());
    // SAFETY: IRQs masked (tick hook contract), this CPU's data.
    let idle = unsafe { (*cpu.current.get()).is_none() };
    if idle {
        return; // the idle loop re-checks the runqueue after the IRQ
    }
    if woke > 0 {
        // Run a timer-woken thread promptly (sysmon, time.Sleep).
        cpu.set_need_resched();
    }
    let q = cpu.quantum.load(Ordering::Relaxed);
    if q <= 1 {
        cpu.set_need_resched();
        cpu.quantum.store(QUANTUM_TICKS, Ordering::Relaxed);
    } else {
        cpu.quantum.store(q - 1, Ordering::Relaxed);
    }
}

/// Hook for `trap::before_return_to_user` (IRQs masked): run a pending
/// reschedule. Phase 2: signal delivery follows here.
pub fn before_return_to_user() {
    if is_running() && this_cpu().need_resched.load(Ordering::Relaxed) {
        schedule();
    }
}

/// End the calling thread: mark it Dead and switch away for good. Its
/// kernel stack is freed by the next context that runs here. The caller
/// has already done the exit bookkeeping (clear_child_tid, thread table).
pub fn exit_current() -> ! {
    let _ = irq::save_disable();
    let cpu = this_cpu();
    // SAFETY: IRQs masked, this CPU's data.
    let prev = unsafe { (*cpu.current.get()).take() }.expect("exit without a current thread");
    prev.set_state(ThreadState::Dead);
    let from = prev.kctx_ptr();
    // SAFETY: as above. The zombie slot keeps the Thread (and its kctx)
    // alive across the switch; it is empty because every switch path runs
    // `finish_switch`.
    unsafe { *cpu.zombie.get() = Some(prev) };
    let next = cpu.runq.lock().pop_front();
    match next {
        // SAFETY: IRQs masked; the dead thread's FP state is not needed.
        Some(next) => unsafe { switch_in(cpu, from, next) },
        // SAFETY: as above; the idle loop resumes and frees the zombie.
        None => unsafe { ctx::switch(from, cpu.idle_ctx.get()) },
    }
    unreachable!("a dead thread was switched back in");
}

/// First code a new user thread runs (from `ctx::new_kernel_context`, IRQs
/// masked, on its own kernel stack): load its start state and enter user
/// mode.
pub(super) extern "C" fn user_thread_entry(_tid: usize) -> ! {
    finish_switch(this_cpu());
    let t = current();
    let start = t.start.lock().take().expect("thread started twice");
    let frame: ctx::TrapFrame = (*start.frame).clone();
    ctx::fp_restore(&start.fp);
    drop(start);
    cpu::set_kernel_stack(t.kstack.top);
    // SAFETY: the process's user tables are live (one address space for
    // the VM), `frame` is a user frame from the loader or `clone`, the FP
    // state is loaded and entries from user mode use this thread's stack.
    unsafe { ctx::enter_user(&frame) }
}

/// First code a kernel thread runs: call its function with IRQs enabled,
/// then exit.
pub(super) extern "C" fn kernel_thread_entry(f: usize) -> ! {
    finish_switch(this_cpu());
    let ThreadKind::Kernel(func) = current().kind else {
        unreachable!("kernel_thread_entry for a user thread");
    };
    debug_assert_eq!(func as usize, f);
    irq::enable();
    func();
    super::unregister_current();
    exit_current()
}
