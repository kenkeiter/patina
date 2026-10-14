//! The sleep list: blocked threads with a deadline, ordered by deadline and
//! expired from the timer tick (10 ms granularity at 100 Hz; a deadline
//! is honoured at the first tick at or after it, never early).
//!
//! Entries quote the thread's block sequence; an entry whose thread was
//! woken another way is removed by [`cancel`] (or would fail the sequence
//! check in `sched::wake`).
//!
//! Phase 2: one list per CPU (or a timer wheel), and a one-shot timer
//! programmed for the earliest deadline instead of the periodic tick, so
//! sub-tick sleeps (`usleep(20)` in Go's sysmon) are accurate and idle
//! CPUs can stop ticking. `timer_create`/`setitimer` hang off the same
//! structure.

use alloc::collections::VecDeque;
use alloc::sync::Arc;

use super::{Thread, WakeReason, sched};
use crate::sync::SpinLock;

struct Sleeper {
    deadline: u64,
    thread: Arc<Thread>,
    seq: u64,
}

/// Sorted by `deadline`, earliest first (FIFO among equal deadlines).
static SLEEPERS: SpinLock<VecDeque<Sleeper>> = SpinLock::new(VecDeque::new());

/// Add `thread` (blocked in block `seq`) with `deadline` (monotonic ns).
pub fn add(deadline: u64, thread: Arc<Thread>, seq: u64) {
    let mut l = SLEEPERS.lock();
    let pos = l.partition_point(|s| s.deadline <= deadline);
    l.insert(
        pos,
        Sleeper {
            deadline,
            thread,
            seq,
        },
    );
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

/// Earliest pending deadline (Phase 2: one-shot timer programming).
pub fn next_deadline() -> Option<u64> {
    SLEEPERS.lock().front().map(|s| s.deadline)
}
