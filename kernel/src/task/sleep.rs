//! The sleep list: blocked threads with a deadline, ordered by deadline and
//! expired from the timer tick. One global list for all CPUs; deadlines
//! are honoured through the per-CPU one-shot timer (`timer::set_deadline`):
//! [`add`] arms the adding CPU's one-shot if the new deadline is earlier
//! than what it has armed, and every CPU's tick re-arms its one-shot to
//! the earliest remaining deadline (`sched::tick`), so a deadline is
//! honoured at the first timer interrupt at or after it (sub-tick, never
//! early) on some CPU. The 100 Hz periodic tick still runs underneath on
//! CPUs that run threads.
//!
//! Tickless idle (D77): each entry records the CPU that added it (its
//! *owner*). An idle CPU stops its periodic tick and arms its one-shot
//! only for the earliest deadline it owns ([`next_deadline_for`]); a busy
//! CPU keeps ticking and arms the global earliest. Every deadline is
//! therefore covered by its owner (idle: armed for it; busy: ticking and
//! armed for the global earliest).
//!
//! Entries quote the thread's block sequence; an entry whose thread was
//! woken another way is removed by [`cancel`] (or would fail the sequence
//! check in `sched::wake`).
//!
//! Later: per-CPU lists or a timer wheel if the list gets long.
//! `timer_create`/`setitimer` arm `sched::arm_deadline` themselves and
//! keep their own owners (`timers.rs`).
//!
//! Lock order: wait-queue lock → sleep list → runqueue (`sched::wake`).

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use super::{Thread, WakeReason, sched};
use crate::sync::SpinLock;

struct Sleeper {
    deadline: u64,
    thread: Arc<Thread>,
    seq: u64,
    /// CPU that added the entry (tickless idle ownership, D77).
    owner: usize,
}

/// Sorted by `deadline`, earliest first (FIFO among equal deadlines).
static SLEEPERS: SpinLock<VecDeque<Sleeper>> = SpinLock::new(VecDeque::new());

/// Add `thread` (blocked in block `seq`) with `deadline` (monotonic ns)
/// and make sure this CPU's one-shot timer fires by then.
pub fn add(deadline: u64, thread: Arc<Thread>, seq: u64) {
    let mut l = SLEEPERS.lock();
    // IRQs are masked under the lock: the CPU id is stable.
    let owner = crate::arch::cpu::id();
    let pos = l.partition_point(|s| s.deadline <= deadline);
    l.insert(
        pos,
        Sleeper {
            deadline,
            thread,
            seq,
            owner,
        },
    );
    drop(l);
    sched::arm_deadline(deadline);
}

/// Remove the entry for `thread`'s block `seq`, if any.
pub fn cancel(thread: &Arc<Thread>, seq: u64) {
    let mut l = SLEEPERS.lock();
    if let Some(i) = l
        .iter()
        .position(|s| s.seq == seq && Arc::ptr_eq(&s.thread, thread))
    {
        l.remove(i);
    }
}

/// Wake every sleeper whose deadline is `<= now` (from the tick, IRQs
/// masked). Returns how many threads were woken.
pub fn expire(now: u64) -> usize {
    let mut l = SLEEPERS.lock();
    let mut woken = 0;
    while l.front().is_some_and(|s| s.deadline <= now) {
        let s = l.pop_front().expect("front exists");
        if sched::wake(&s.thread, s.seq, WakeReason::TimedOut) {
            woken += 1;
        }
    }
    woken
}

/// Earliest pending deadline (one-shot timer programming).
pub fn next_deadline() -> Option<u64> {
    SLEEPERS.lock().front().map(|s| s.deadline)
}

/// Earliest pending deadline owned by `cpu` (tickless idle, D77).
pub fn next_deadline_for(cpu: usize) -> Option<u64> {
    SLEEPERS
        .lock()
        .iter()
        .find(|s| s.owner == cpu)
        .map(|s| s.deadline)
}
