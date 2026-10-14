//! Kernel stacks with guard pages (design §4 "Stacks", decision D29).
//!
//! Every thread (and kernel self-test thread) gets a [`KSTACK_PAGES`]-page
//! kernel stack in a dedicated kernel VA window, [`KSTACK_BASE`], outside
//! the direct map (`PHYS_OFFSET` + RAM) and the image window
//! (`KERNEL_VA_BASE`). The window is carved into fixed slots of one
//! unmapped guard page followed by the stack pages, so an overflow runs
//! into a translation fault (aarch64 detects it with `AT` probes, see
//! D29; x86_64 takes a #PF → #DF on IST1).
//!
//! The window is in the kernel half on both arches: on aarch64 it is
//! translated by TTBR1, which every user address space shares; on x86_64
//! its PML4 slot (320) is one of the 256 upper slots that
//! `paging::populate_kernel_pml4` pre-populated at boot and that every user
//! PML4 copies, so mappings added here later are visible in all of them.
//!
//! Slots are allocated with a bump pointer; freed slots keep their frames
//! mapped and go on a free list for reuse (no kernel unmap or TLB
//! shootdown needed — the number of stacks is bounded by the peak thread
//! count, which Go keeps small).

use alloc::vec::Vec;

use super::KernelStack;
use crate::arch::paging::{self, MapFlags};
use crate::mm::{PAGE_SIZE, VirtAddr, frame};
use crate::sync::SpinLock;

/// Base of the kernel-stack window (PHYS_OFFSET + 32 TiB on both arches).
pub const KSTACK_BASE: usize = 0xffff_a000_0000_0000;
/// Size of the window: 1 TiB, i.e. ~29 million slots, never exhausted.
pub const KSTACK_WINDOW: usize = 1 << 40;
/// Stack pages per thread (32 KiB).
pub const KSTACK_PAGES: usize = 8;
/// Bytes per slot: one guard page + the stack.
const SLOT_SIZE: usize = (KSTACK_PAGES + 1) * PAGE_SIZE;

struct Slots {
    /// Next never-used slot index.
    next: usize,
    /// Freed slots (still mapped), by index.
    free: Vec<usize>,
}

static SLOTS: SpinLock<Slots> = SpinLock::new(Slots {
    next: 0,
    free: Vec::new(),
});

fn slot_stack(i: usize) -> KernelStack {
    let base = KSTACK_BASE + i * SLOT_SIZE + PAGE_SIZE;
    KernelStack {
        base,
        top: base + KSTACK_PAGES * PAGE_SIZE,
    }
}

/// Allocate a kernel stack; `None` when out of memory.
pub fn alloc() -> Option<KernelStack> {
    let i = {
        let mut s = SLOTS.lock();
        if let Some(i) = s.free.pop() {
            return Some(slot_stack(i));
        }
        let i = s.next;
        if (i + 1) * SLOT_SIZE > KSTACK_WINDOW {
            return None;
        }
        s.next += 1;
        i
    };
    // Map the stack pages (the guard page below stays unmapped). Done
    // outside the slot lock: the frame allocator and page tables have
    // their own locks.
    let st = slot_stack(i);
    let mut va = st.base;
    while va < st.top {
        let Some(pa) = frame::alloc_frames(0) else {
            // Leak the partially mapped slot: it is never handed out again
            // (`next` already moved past it). Out of memory is fatal for
            // the caller anyway.
            return None;
        };
        if let Err(e) = paging::map_range(
            VirtAddr(va),
            pa,
            PAGE_SIZE,
            MapFlags::READ | MapFlags::WRITE,
        ) {
            panic!("kstack: map {:#x}: {:?}", va, e);
        }
        va += PAGE_SIZE;
    }
    Some(st)
}

/// Return a stack to the pool. The caller must not be running on it.
pub fn free(st: KernelStack) {
    let off = st.base - PAGE_SIZE - KSTACK_BASE;
    debug_assert!(off.is_multiple_of(SLOT_SIZE));
    SLOTS.lock().free.push(off / SLOT_SIZE);
}

/// (slots ever allocated, slots on the free list) — diagnostics.
pub fn stats() -> (usize, usize) {
    let s = SLOTS.lock();
    (s.next, s.free.len())
}
