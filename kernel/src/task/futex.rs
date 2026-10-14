//! Futex wait queues (design §5): a hash table of buckets keyed by the user
//! address. There is one address space per VM, so the user virtual address
//! is the whole key for both private and shared futexes.
//!
//! [`wait`] reads the futex word through uaccess (which faults the page in)
//! while holding the bucket lock and queues the thread under the same
//! lock; [`wake`] takes the bucket lock, so a wake that follows the user's
//! store to the word cannot be lost (see `wait.rs`).
//!
//! Lock order: bucket → address space (`with_space`) → frame allocator;
//! bucket → sleep list → runqueue.

use patina_abi::errno::{EAGAIN, ETIMEDOUT, Errno};

use super::WakeReason;
use super::wait::{self, WaitQueue};
use crate::sync::SpinLock;

const NBUCKETS: usize = 64;

static BUCKETS: [SpinLock<WaitQueue>; NBUCKETS] =
    [const { SpinLock::new(WaitQueue::new()) }; NBUCKETS];

fn bucket(uaddr: usize) -> &'static SpinLock<WaitQueue> {
    // Fibonacci hashing of the word index.
    let h = ((uaddr >> 2) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 58;
    &BUCKETS[h as usize % NBUCKETS]
}

fn id(q: &mut WaitQueue) -> &mut WaitQueue {
    q
}

/// `FUTEX_WAIT(_BITSET)`: if the u32 at `uaddr` (4-aligned, checked by the
/// caller) equals `val`, sleep until woken by a [`wake`] whose bitset
/// intersects `bits`, or until `deadline` (monotonic ns) → `ETIMEDOUT`.
/// `EAGAIN` if the value differs, `EFAULT` if unreadable. The wait is
/// interruptible: a signal ends it with the internal `ERESTARTSYS` (the
/// caller turns it into the right restart code).
pub fn wait(uaddr: usize, val: u32, bits: u32, deadline: Option<u64>) -> Result<(), Errno> {
    let b = bucket(uaddr).lock();
    let cur: u32 = crate::syscall::with_space(|s| s.read_val(uaddr))?;
    if cur != val {
        return Err(Errno(EAGAIN));
    }
    if deadline.is_some_and(|d| crate::arch::timer::now_ns() >= d) {
        return Err(Errno(ETIMEDOUT));
    }
    match wait::block_interruptible(b, id, uaddr, bits, deadline) {
        WakeReason::Woken => Ok(()),
        WakeReason::TimedOut => Err(Errno(ETIMEDOUT)),
        WakeReason::Interrupted => Err(Errno(super::signal::ERESTARTSYS)),
    }
}

/// `FUTEX_WAKE(_BITSET)`: wake up to `n` waiters on `uaddr` whose bitset
/// intersects `bits`. Returns the number woken.
pub fn wake(uaddr: usize, n: usize, bits: u32) -> usize {
    bucket(uaddr)
        .lock()
        .wake_matching(n, |w| w.key == uaddr && w.bits & bits != 0)
}
