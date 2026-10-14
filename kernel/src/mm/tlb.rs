//! TLB shootdown for the user address space: deferred frame reclaim with
//! flush generations (RCU-like, no synchronous waiting).
//!
//! # Problem
//! On x86_64 `UserTables::unmap`/`protect` only `invlpg` the executing CPU;
//! another CPU running a thread of the same (single) address space can keep
//! a stale translation and write through it after the frame went back to
//! the allocator and got reused: silent corruption. aarch64's `TLBI ...IS`
//! is broadcast and complete when `unmap` returns
//! (`arch::uspace::NEEDS_SHOOTDOWN == false`), so there frames are freed at
//! once.
//!
//! # Scheme
//! * [`FLUSH_GEN`]: global generation, bumped by every operation that frees
//!   frames that were mapped in the user tables.
//! * [`CPU_GEN`]`[c]`: the highest generation CPU `c` is known to have
//!   flushed its whole user TLB for. A CPU reaches `g` by reading
//!   `FLUSH_GEN` (= g) *before* a full local flush and storing g after it:
//!   every batch `<= g` cleared its PTEs before bumping, so after the flush
//!   none of their translations can be cached any more.
//! * [`Shootdown`] collects one vm operation's freed frames (and whether any
//!   translation was reduced while keeping the frame: mprotect, mremap
//!   move). When it is dropped at the end of the operation it, once:
//!   bumps `FLUSH_GEN` → g, queues `(g, frames)` on `DEFERRED`, flushes the
//!   local TLB and sets its own `CPU_GEN` to g, sends `IpiKind::TlbFlush`
//!   to every other CPU and returns without waiting. Permission-only
//!   changes just send the IPI (the frame stays owned; nothing to defer).
//!   One IPI per syscall, not per page: a full flush covers every page.
//! * [`reclaim`] frees each queued batch whose generation every online
//!   CPU has reached (`min CPU_GEN[c] >= g`). It runs at the end of every
//!   vm operation (via `Shootdown`'s drop), from `sched::tick` on every
//!   CPU, and before a user page fill reports ENOMEM.
//!
//! The receiving side ([`ipi_flush`]) runs from the IPI with IRQs masked.
//! Nobody waits for acks while holding a lock, so a CPU spinning on the
//! address-space lock with IRQs masked (futex bucket → space lock, signal
//! delivery) only *delays* reclaim of the batch; it never deadlocks and
//! never corrupts (the frames stay allocated meanwhile).
//!
//! Uniprocessor fast path: with one CPU online (or an arch without
//! `NEEDS_SHOOTDOWN`) `Shootdown` frees frames immediately and sends
//! nothing — exactly the Phase 1 behaviour.
//!
//! Limits: a CPU that stopped (`sched::stop_other_cpus`, exit/panic) never
//! advances its `CPU_GEN`, so batches queued after that stay pending; the
//! VM is going down then. The pending list is unbounded (only bounded by
//! how long some CPU keeps IRQs masked; in practice one tick).
//!
//! Lock order: address space → `DEFERRED` → heap / frame allocator. The
//! tick takes `DEFERRED` (try_lock) then the frame allocator.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::arch::cpu::{self, MAX_CPUS};
use crate::arch::uspace::{NEEDS_SHOOTDOWN, flush_tlb_local};
use crate::arch::{irq, smp};
use crate::mm::{PhysAddr, frame};
use crate::sync::SpinLock;
use crate::trap::IpiKind;

/// Global shootdown generation (bumped by every deferred free).
static FLUSH_GEN: AtomicU64 = AtomicU64::new(0);
/// Per-CPU: highest generation this CPU flushed its user TLB for.
static CPU_GEN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Freed-but-maybe-still-cached frames, by generation.
static DEFERRED: SpinLock<VecDeque<(u64, Vec<PhysAddr>)>> = SpinLock::new(VecDeque::new());
/// Frames in `DEFERRED` (fast emptiness check, diagnostics).
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Whether frames unmapped now may be freed at once (no other CPU can hold
/// a stale translation).
fn immediate() -> bool {
    !NEEDS_SHOOTDOWN || crate::task::sched::online_cpus() <= 1
}

/// Frames waiting for other CPUs' TLB flushes (diagnostics).
pub fn pending_frames() -> usize {
    PENDING.load(Ordering::Relaxed)
}

/// Bring this CPU to the current generation: read it, flush, record.
/// IRQs must be masked (stable `cpu::id()`).
fn flush_and_record() {
    let g = FLUSH_GEN.load(Ordering::SeqCst);
    flush_tlb_local();
    CPU_GEN[cpu::id()].fetch_max(g, Ordering::SeqCst);
}

/// `IpiKind::TlbFlush` arrived (IRQs masked): full local user-TLB flush,
/// then record the generation observed *before* it.
pub fn ipi_flush() {
    flush_and_record();
}

/// Lowest generation reached by any online CPU.
fn min_gen() -> u64 {
    let mut m = crate::task::sched::online_mask();
    let mut min = u64::MAX;
    while m != 0 {
        let c = m.trailing_zeros() as usize;
        m &= m - 1;
        if c < MAX_CPUS {
            min = min.min(CPU_GEN[c].load(Ordering::SeqCst));
        }
    }
    min
}

/// Free every deferred batch that no CPU can reach through a stale
/// translation any more. Cheap when nothing is pending; never blocks (a
/// contended `DEFERRED` lock skips the round). Any context that may take
/// the frame allocator lock (not under the heap or frame lock).
pub fn reclaim() {
    if PENDING.load(Ordering::Acquire) == 0 {
        return;
    }
    let flags = irq::save_disable();
    if CPU_GEN[cpu::id()].load(Ordering::SeqCst) < FLUSH_GEN.load(Ordering::SeqCst) {
        // E.g. a CPU that came online after a batch was queued.
        flush_and_record();
    }
    irq::restore(flags);
    let min = min_gen();
    loop {
        let batch = {
            let Some(mut d) = DEFERRED.try_lock() else {
                return;
            };
            match d.iter().position(|(g, _)| *g <= min) {
                Some(i) => d.swap_remove_back(i),
                None => None,
            }
        };
        let Some((_, frames)) = batch else {
            return;
        };
        PENDING.fetch_sub(frames.len(), Ordering::AcqRel);
        for pa in frames {
            frame::free_frames(pa, 0);
        }
    }
}

/// One vm operation's TLB work: frames unmapped from the user tables (to
/// free once no CPU caches them) and whether a kept translation was
/// reduced (permissions, or moved by mremap). Dropping it does the
/// shootdown (at most one IPI per CPU) and a [`reclaim`] round.
pub struct Shootdown {
    immediate: bool,
    frames: Vec<PhysAddr>,
    flush: bool,
}

impl Shootdown {
    /// Start collecting. Call with the address-space lock held (the
    /// uniprocessor decision must not change under the operation).
    pub fn new() -> Shootdown {
        Shootdown {
            immediate: immediate(),
            frames: Vec::new(),
            flush: false,
        }
    }

    /// `pa` (one 4 KiB frame) was just unmapped from the live user tables:
    /// free it once no CPU can still reach it.
    pub fn free_frame(&mut self, pa: PhysAddr) {
        if crate::vdso::is_vvar_frame(pa) {
            // Kernel-owned and shared (D75): only the mapping goes away.
            return;
        }
        if self.immediate {
            frame::free_frames(pa, 0);
        } else {
            self.frames.push(pa);
        }
    }

    /// A translation was reduced (fewer permissions, or removed while the
    /// frame lives on elsewhere): other CPUs must drop their copies.
    pub fn flush(&mut self) {
        self.flush = true;
    }
}

impl Default for Shootdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Shootdown {
    fn drop(&mut self) {
        if !self.immediate && (self.flush || !self.frames.is_empty()) {
            let flags = irq::save_disable();
            if !self.frames.is_empty() {
                // PTEs are already cleared: publish them under a new
                // generation, then flush here and record it.
                let g = FLUSH_GEN.fetch_add(1, Ordering::SeqCst) + 1;
                let frames = core::mem::take(&mut self.frames);
                PENDING.fetch_add(frames.len(), Ordering::AcqRel);
                DEFERRED.lock().push_back((g, frames));
                flush_tlb_local();
                CPU_GEN[cpu::id()].fetch_max(g, Ordering::SeqCst);
            }
            // Never waits for the targets (see the module docs).
            smp::send_ipi_others(IpiKind::TlbFlush);
            irq::restore(flags);
        }
        reclaim();
    }
}
