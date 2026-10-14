//! Wait queues and blocking.
//!
//! A [`WaitQueue`] is a FIFO of [`Waiter`]s and lives inside the caller's
//! `SpinLock` (a futex bucket, a self-test queue, later pipes/epoll), so
//! the condition check and the enqueue happen under one lock. [`block`]
//! takes that lock's guard and implements the classic lost-wakeup-free
//! sequence:
//!
//! 1. mark the current thread Blocked (new block sequence `seq`) and
//!    enqueue it, still under the lock;
//! 2. optionally put it on the sleep list with a deadline;
//! 3. release the lock but keep IRQs masked, then `schedule()`.
//!
//! A waker takes the same lock, removes waiters and calls
//! `sched::wake(thread, seq)`, which moves the thread Blocked → Runnable
//! only if it is still in block `seq`. Since wakers need the lock, a
//! wake-up cannot slip in between the condition check and step 1; one that
//! comes between 3 and the end of the switch (SMP: a waker on another CPU)
//! finds the thread already Blocked, moves it to Runnable, waits until the
//! blocking CPU has saved its context (`Thread::on_cpu`, see `sched`) and
//! only then enqueues it; `schedule` sees the thread no longer Running and
//! does not requeue it. Stale
//! entries (the thread timed out, or was woken through another path) fail
//! the sequence check and are skipped.
//!
//! [`block_interruptible`] / [`sleep_until_interruptible`] are also ended
//! by a pending signal ([`WakeReason::Interrupted`], → `EINTR`/restart).

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use super::{Thread, WakeReason, sched, signal, sleep};
use crate::arch::irq;
use crate::sync::SpinLockGuard;

/// One parked thread.
pub struct Waiter {
    pub thread: Arc<Thread>,
    /// Block sequence the thread was in when it queued.
    seq: u64,
    /// Caller-defined key (futex: the user address).
    pub key: usize,
    /// Caller-defined match bits (futex: the bitset).
    pub bits: u32,
}

/// A FIFO of waiters. Not locked itself: embed it in a `SpinLock`.
pub struct WaitQueue {
    list: VecDeque<Waiter>,
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue {
            list: VecDeque::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Wake up to `max` waiters (oldest first) for which `pred` holds,
    /// removing them. Returns how many were actually woken (stale entries
    /// are dropped without counting).
    pub fn wake_matching(&mut self, max: usize, pred: impl Fn(&Waiter) -> bool) -> usize {
        let mut woken = 0;
        let mut i = 0;
        while i < self.list.len() && woken < max {
            if pred(&self.list[i]) {
                let w = self.list.remove(i).expect("index in range");
                if sched::wake(&w.thread, w.seq, WakeReason::Woken) {
                    woken += 1;
                }
            } else {
                i += 1;
            }
        }
        woken
    }

    /// Wake up to `max` waiters.
    pub fn wake(&mut self, max: usize) -> usize {
        self.wake_matching(max, |_| true)
    }

    /// Wake everyone.
    pub fn wake_all(&mut self) -> usize {
        self.wake(usize::MAX)
    }

    fn remove(&mut self, t: &Arc<Thread>, seq: u64) {
        self.list
            .retain(|w| !(Arc::ptr_eq(&w.thread, t) && w.seq == seq));
    }
}

/// Block the current thread on the wait queue `queue(&mut *guard)` until
/// a waker removes it (`Woken`) or, with `deadline` (monotonic ns), until
/// the deadline passes (`TimedOut`). `key`/`bits` are stored in the
/// [`Waiter`] for the waker's matching. The guard is released before the
/// switch; the lock is not held on return. Signals do not end this block
/// (Linux `TASK_UNINTERRUPTIBLE`); see [`block_interruptible`].
pub fn block<T: ?Sized>(
    guard: SpinLockGuard<'_, T>,
    queue: fn(&mut T) -> &mut WaitQueue,
    key: usize,
    bits: u32,
    deadline: Option<u64>,
) -> WakeReason {
    block_impl(guard, queue, key, bits, deadline, false)
}

/// As [`block`], but the block also ends with [`WakeReason::Interrupted`]
/// when a signal the thread does not block is (or already was) pending
/// for it (`task::signal` queues it and calls `sched::kick`). The caller
/// then returns one of the internal restart codes
/// (`signal::ERESTARTSYS` for "restart if `SA_RESTART`, else `EINTR`",
/// `ERESTARTNOHAND`, `ERESTART_RESTARTBLOCK`); signal delivery on the way
/// back to user mode turns it into `EINTR` or re-executes the syscall.
/// The thread is off the wait queue and the sleep list on return.
pub fn block_interruptible<T: ?Sized>(
    guard: SpinLockGuard<'_, T>,
    queue: fn(&mut T) -> &mut WaitQueue,
    key: usize,
    bits: u32,
    deadline: Option<u64>,
) -> WakeReason {
    block_impl(guard, queue, key, bits, deadline, true)
}

fn block_impl<T: ?Sized>(
    mut guard: SpinLockGuard<'_, T>,
    queue: fn(&mut T) -> &mut WaitQueue,
    key: usize,
    bits: u32,
    deadline: Option<u64>,
    intr: bool,
) -> WakeReason {
    let t = sched::current_arc();
    let seq = if intr {
        t.begin_block_interruptible()
    } else {
        t.begin_block()
    };
    queue(&mut guard).list.push_back(Waiter {
        thread: t.clone(),
        seq,
        key,
        bits,
    });
    if let Some(d) = deadline {
        sleep::add(d, t.clone(), seq);
    }
    let (lock, flags) = guard.unlock_irqs_masked();
    // A signal that was queued before `begin_block` published the
    // interruptible state was not able to kick us: check now (SeqCst on
    // both sides, see `Thread::begin_block_interruptible`).
    if !(intr && signal::deliverable(&t) && t.cancel_block(seq)) {
        sched::schedule();
    }
    let reason = t.wake_reason();
    match reason {
        WakeReason::TimedOut | WakeReason::Interrupted => {
            let mut g = lock.lock();
            queue(&mut g).remove(&t, seq);
            drop(g);
            if reason == WakeReason::Interrupted && deadline.is_some() {
                sleep::cancel(&t, seq);
            }
        }
        WakeReason::Woken => {
            if deadline.is_some() {
                sleep::cancel(&t, seq);
            }
        }
    }
    irq::restore(flags);
    reason
}

/// Sleep until `deadline` (monotonic ns) with no wait queue.
pub fn sleep_until(deadline: u64) {
    let flags = irq::save_disable();
    if crate::arch::timer::now_ns() < deadline {
        let t = sched::current_arc();
        let seq = t.begin_block();
        sleep::add(deadline, t, seq);
        sched::schedule();
    }
    irq::restore(flags);
}

/// Sleep until `deadline` (monotonic ns; `u64::MAX` = until a signal)
/// unless a signal interrupts it: `TimedOut` when the deadline passed,
/// `Interrupted` when a deliverable signal is pending.
pub fn sleep_until_interruptible(deadline: u64) -> WakeReason {
    let flags = irq::save_disable();
    let mut reason = WakeReason::TimedOut;
    if crate::arch::timer::now_ns() < deadline {
        let t = sched::current_arc();
        let seq = t.begin_block_interruptible();
        sleep::add(deadline, t.clone(), seq);
        if !(signal::deliverable(&t) && t.cancel_block(seq)) {
            sched::schedule();
        }
        reason = t.wake_reason();
        if reason == WakeReason::Interrupted {
            sleep::cancel(&t, seq);
        }
    }
    irq::restore(flags);
    reason
}
