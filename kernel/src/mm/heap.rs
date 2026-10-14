//! Kernel heap: `buddy_system_allocator::Heap` behind an IRQ-safe spinlock,
//! seeded from the frame allocator and grown on demand.
//!
//! Init order: `mm::frame::init` must run before [`init`]. Any allocation
//! before [`init`] (or after RAM runs out) returns null, which the `alloc`
//! crate turns into a panic via the default alloc error handler.
//!
//! The heap has no cap of its own: it grows from the frame allocator, so
//! it competes with user pages for the same frames. User-driven kernel
//! memory is therefore kept in check here (D87):
//!
//! * [`charge`]/[`uncharge`]: long-lived memory held for the program
//!   (socket and pipe buffers, queued AF_UNIX and netlink data, tmpfs file
//!   pages) counts against the program's commit limit (D78) next to its
//!   writable mappings, so the kernel reserve D78 sets aside stays for
//!   page tables, kernel stacks and kernel objects; [`KBuf`] is a charged
//!   byte buffer.
//! * [`reserve`]: a syscall that creates kernel objects (pipes, sockets,
//!   epolls, files, fds, threads) first checks that free memory (free
//!   frames plus free heap bytes) stays above a low watermark (half of
//!   the D78 reserve), and fails with ENOMEM/ENOSPC/ENOBUFS otherwise.
//! * [`try_zeroed`] and `try_reserve*`: buffers sized by user input are
//!   allocated fallibly.
//!
//! Infallible allocations then only happen while the watermark holds.

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};

use buddy_system_allocator::Heap;
use patina_abi::errno::{ENOMEM, Errno};

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
        // at least GROW_MIN_ORDER; when RAM is fragmented (user pages
        // everywhere), fall back to smaller blocks down to the request's
        // own order. Then retry once.
        if heap.stats_total_bytes() == 0 {
            return ptr::null_mut();
        }
        let need = layout.size().max(layout.align()).next_power_of_two();
        let min_order = frame::order_for(need);
        let mut order = min_order.max(GROW_MIN_ORDER);
        loop {
            if add_frames(&mut heap, order) {
                return match heap.alloc(layout) {
                    Ok(p) => p.as_ptr(),
                    Err(()) => ptr::null_mut(),
                };
            }
            if order <= min_order {
                return ptr::null_mut();
            }
            order -= 1;
        }
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

// ---------------------------------------------------------------------------
// User-driven kernel memory (D87).

/// Bytes of kernel memory held for the program ([`charge`]).
static CHARGED: AtomicUsize = AtomicUsize::new(0);
/// The program's committed pages and commit limit (D78), published by its
/// address space ([`publish_commit`]); no limit until the loader sets it.
static COMMITTED: AtomicUsize = AtomicUsize::new(0);
static COMMIT_LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The address space's commit numbers changed (`mm::vm`).
pub fn publish_commit(committed_pages: usize, limit_pages: usize) {
    COMMITTED.store(committed_pages, Ordering::Relaxed);
    COMMIT_LIMIT.store(limit_pages, Ordering::Relaxed);
}

/// Pages of kernel memory charged to the program (part of its commit).
pub fn charged_pages() -> usize {
    CHARGED.load(Ordering::Relaxed).div_ceil(PAGE_SIZE)
}

/// What a heap allocation of `n` bytes really takes (buddy blocks are
/// powers of two): the amount [`charge`] callers account for.
pub const fn alloc_size(n: usize) -> usize {
    if n == 0 { 0 } else { n.next_power_of_two() }
}

/// Free memory the kernel can still allocate from: free frames plus heap
/// bytes not in use.
fn available_bytes() -> usize {
    let (free, _) = frame::stats();
    let heap_free = {
        let h = HEAP.heap.lock();
        h.stats_total_bytes() - h.stats_alloc_actual()
    };
    free.saturating_mul(PAGE_SIZE).saturating_add(heap_free)
}

/// The low watermark: half of the D78 kernel reserve (RAM/32, 2–32 MiB).
fn watermark_bytes() -> usize {
    let (_, total) = frame::stats();
    (total / 32).clamp(512, 8192) * PAGE_SIZE
}

/// Before a syscall allocates about `bytes` of kernel memory on the
/// program's behalf (new pipes, sockets, epolls, fds, files, threads,
/// large transient buffers): ENOMEM unless free memory stays above the
/// low watermark afterwards. Callers map it to the syscall's own errno.
pub fn reserve(bytes: usize) -> Result<(), Errno> {
    if available_bytes() < watermark_bytes().saturating_add(bytes) {
        return Err(Errno(ENOMEM));
    }
    Ok(())
}

/// Charge `bytes` of long-lived kernel memory held for the program
/// against its commit limit (D78) and the watermark ([`reserve`]).
/// `false`: refused (the caller fails with ENOBUFS/ENOMEM/ENOSPC and
/// allocates nothing). Undo with [`uncharge`] when the memory is freed.
pub fn charge(bytes: usize) -> bool {
    if bytes == 0 {
        return true;
    }
    if reserve(bytes).is_err() {
        return false;
    }
    let held = CHARGED.fetch_add(bytes, Ordering::Relaxed) + bytes;
    let committed = COMMITTED.load(Ordering::Relaxed);
    if committed.saturating_add(held.div_ceil(PAGE_SIZE)) > COMMIT_LIMIT.load(Ordering::Relaxed) {
        CHARGED.fetch_sub(bytes, Ordering::Relaxed);
        return false;
    }
    true
}

/// Memory charged with [`charge`] was freed.
pub fn uncharge(bytes: usize) {
    if bytes > 0 {
        let prev = CHARGED.fetch_sub(bytes, Ordering::Relaxed);
        debug_assert!(prev >= bytes, "heap: uncharge below zero");
    }
}

/// [`charge`] in the signature the VFS page hook wants: `+pages` before
/// tmpfs file data takes heap pages (`false`: ENOSPC), `-pages` when they
/// are freed.
pub fn charge_pages(delta: i64) -> bool {
    let bytes = (delta.unsigned_abs() as usize).saturating_mul(PAGE_SIZE);
    if delta >= 0 {
        charge(bytes)
    } else {
        uncharge(bytes);
        true
    }
}

/// `n` zero bytes on the heap, allocated fallibly: ENOMEM instead of a
/// kernel panic when the heap cannot grow. For buffers whose size comes
/// from the program.
pub fn try_zeroed(n: usize) -> Result<Vec<u8>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Errno(ENOMEM))?;
    v.resize(n, 0);
    Ok(v)
}

/// A byte buffer charged to the program ([`charge`]) for as long as it
/// lives: queued socket data. Uncharged on drop.
pub struct KBuf {
    data: Vec<u8>,
    charged: usize,
}

impl KBuf {
    /// A charged copy of `src`; `None` if the charge is refused or the
    /// allocation fails.
    pub fn try_copy(src: &[u8]) -> Option<KBuf> {
        let charged = alloc_size(src.len());
        if !charge(charged) {
            return None;
        }
        let mut data = Vec::new();
        if data.try_reserve_exact(src.len()).is_err() {
            uncharge(charged);
            return None;
        }
        data.extend_from_slice(src);
        Some(KBuf { data, charged })
    }
}

impl core::ops::Deref for KBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.data
    }
}

impl Drop for KBuf {
    fn drop(&mut self) {
        uncharge(self.charged);
    }
}
