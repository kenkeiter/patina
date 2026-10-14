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
//! # Reclaim (D80)
//! A freed stack (its thread is dead and this CPU switched away from it,
//! `sched::finish_switch`) goes back on a small pool of still-mapped slots
//! ([`POOL_KEEP`]), which makes the common thread churn cheap. Beyond that
//! its pages are unmapped and the frames returned to the frame allocator,
//! so a burst of threads does not pin memory for good:
//! * aarch64: `paging::unmap_page` invalidates with `TLBI VAAE1IS`, which
//!   is complete on every CPU on return: frames and slot are reusable at
//!   once;
//! * x86_64: stack pages are mapped non-global (`MapFlags::LOCAL`), so the
//!   `mm::tlb` CR3-reload shootdown covers them: the frames go through a
//!   [`Shootdown`] (freed once every CPU flushed, D52), and the slot's VA
//!   is reused only once [`tlb::generation_reached`] for the generation
//!   of its unmap (a CPU with a stale entry for the VA would otherwise
//!   reach the old frame through the new stack).
//!
//! Page-table frames of the window are kept (bounded by the peak number
//! of stacks and reused).

use alloc::vec::Vec;

use super::KernelStack;
use crate::arch::paging::{self, MapFlags};
use crate::mm::tlb::{self, Shootdown};
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
/// Freed stacks kept mapped for reuse; more are unmapped and freed.
pub const POOL_KEEP: usize = 32;

struct Slots {
    /// Next never-used slot index.
    next: usize,
    /// Freed slots still mapped, by index.
    free: Vec<usize>,
    /// Unmapped slots (frames returned) and the shootdown generation of
    /// their unmap: reusable once every CPU reached it.
    unmapped: Vec<(usize, u64)>,
}

static SLOTS: SpinLock<Slots> = SpinLock::new(Slots {
    next: 0,
    free: Vec::new(),
    unmapped: Vec::new(),
});

/// Serializes kernel page-table changes in the window (`map_range` /
/// `unmap_page` walk and allocate intermediate tables without a lock of
/// their own). Never held together with `SLOTS`.
static KMAP: SpinLock<()> = SpinLock::new(());

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
        let reusable = s
            .unmapped
            .iter()
            .position(|&(_, g)| tlb::generation_reached(g));
        match reusable {
            Some(pos) => s.unmapped.swap_remove(pos).0,
            None => {
                let i = s.next;
                if (i + 1) * SLOT_SIZE > KSTACK_WINDOW {
                    return None;
                }
                s.next += 1;
                i
            }
        }
    };
    // Map the stack pages (the guard page below stays unmapped). Done
    // outside the slot lock: the frame allocator and page tables have
    // their own locks.
    let st = slot_stack(i);
    let mut va = st.base;
    while va < st.top {
        let Some(pa) = frame::alloc_frames(0) else {
            // Unmap and free what was mapped; the slot goes back as
            // unmapped (no CPU ever ran on it, so no stale translation).
            release_pages(st.base, va);
            SLOTS.lock().unmapped.push((i, 0));
            return None;
        };
        let _g = KMAP.lock();
        if let Err(e) = paging::map_range(
            VirtAddr(va),
            pa,
            PAGE_SIZE,
            MapFlags::READ | MapFlags::WRITE | MapFlags::LOCAL,
        ) {
            panic!("kstack: map {:#x}: {:?}", va, e);
        }
        va += PAGE_SIZE;
    }
    Some(st)
}

/// Unmap `[from, to)` of the window and hand the frames to a shootdown.
fn release_pages(from: usize, to: usize) {
    let mut sd = Shootdown::new();
    {
        let _g = KMAP.lock();
        let mut va = from;
        while va < to {
            if let Some(pa) = paging::unmap_page(VirtAddr(va)) {
                sd.free_frame(pa);
            }
            va += PAGE_SIZE;
        }
    }
    drop(sd);
}

/// Return a stack. The caller must not be running on it, and no CPU may
/// run on it any more (its thread is dead and switched away from).
pub fn free(st: KernelStack) {
    let off = st.base - PAGE_SIZE - KSTACK_BASE;
    debug_assert!(off.is_multiple_of(SLOT_SIZE));
    let i = off / SLOT_SIZE;
    {
        let mut s = SLOTS.lock();
        if s.free.len() < POOL_KEEP {
            s.free.push(i);
            return;
        }
    }
    release_pages(st.base, st.top);
    // Read after the shootdown was published (its drop bumped the
    // generation on x86_64 SMP).
    let g = tlb::generation();
    SLOTS.lock().unmapped.push((i, g));
}

/// (slots ever allocated, mapped free slots, unmapped slots) — diagnostics.
pub fn stats() -> (usize, usize, usize) {
    let s = SLOTS.lock();
    (s.next, s.free.len(), s.unmapped.len())
}
