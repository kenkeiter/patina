//! Kernel heap: `buddy_system_allocator::Heap` behind an IRQ-safe spinlock,
//! seeded from the frame allocator and grown on demand.
//!
//! Init order: `mm::frame::init` must run before [`init`]. Any allocation
//! before [`init`] (or after RAM runs out) returns null, which the `alloc`
//! crate turns into a panic via the default alloc error handler.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};

use buddy_system_allocator::Heap;

use crate::mm::{PAGE_SIZE, frame, phys_to_virt};
use crate::sync::SpinLock;

/// Initial heap size: 8 MiB (order-11 frame block).
const INITIAL_ORDER: usize = 11;
/// Grow in chunks of at least 2 MiB.
const GROW_MIN_ORDER: usize = 9;

/// Heap with 32 size classes (up to 2 GiB blocks).
pub struct KernelHeap {
    heap: SpinLock<Heap<32>>,
}

impl KernelHeap {
    const fn new() -> Self {
        Self {
            heap: SpinLock::new(Heap::empty()),
        }
    }
}

fn add_frames(heap: &mut Heap<32>, order: usize) -> bool {
    match frame::alloc_frames(order) {
        Some(pa) => {
            let start = phys_to_virt(pa).as_usize();
            // SAFETY: freshly allocated frames, exclusively owned by the heap from now on.
            unsafe { heap.add_to_heap(start, start + (PAGE_SIZE << order)) };
            true
        }
        None => false,
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = self.heap.lock();
        if let Ok(p) = heap.alloc(layout) {
            return p.as_ptr();
        }
        // Grow: enough frames for the request rounded to a power of two,
        // at least GROW_MIN_ORDER, then retry once.
        let need = layout.size().max(layout.align()).next_power_of_two();
        let order = frame::order_for(need).max(GROW_MIN_ORDER);
        if heap.stats_total_bytes() > 0
            && add_frames(&mut heap, order)
            && let Ok(p) = heap.alloc(layout)
        {
            return p.as_ptr();
        }
        ptr::null_mut()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(p) = NonNull::new(ptr) {
            // SAFETY: GlobalAlloc contract: ptr/layout come from `alloc` above.
            unsafe { self.heap.lock().dealloc(p, layout) };
        }
    }
}

#[global_allocator]
static HEAP: KernelHeap = KernelHeap::new();

/// Seed the heap from the frame allocator. Panics if RAM is too small.
pub fn init() {
    let mut heap = HEAP.heap.lock();
    // Shrink the initial chunk on tiny VMs.
    let (free, _) = frame::stats();
    let mut order = INITIAL_ORDER;
    while order > GROW_MIN_ORDER && (1usize << order) > free / 4 {
        order -= 1;
    }
    assert!(
        add_frames(&mut heap, order),
        "heap: cannot allocate initial frames"
    );
}

/// `(user_bytes, allocated_bytes, total_bytes)` of the heap.
pub fn stats() -> (usize, usize, usize) {
    let h = HEAP.heap.lock();
    (
        h.stats_alloc_user(),
        h.stats_alloc_actual(),
        h.stats_total_bytes(),
    )
}
