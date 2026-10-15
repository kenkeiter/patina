//! Physical frame allocator: a binary buddy allocator over the direct map.
//!
//! * Blocks of `2^order` frames, `order` in `0..=MAX_ORDER`, naturally aligned
//!   in physical address space.
//! * Free blocks are kept in per-order intrusive doubly linked lists whose
//!   nodes live in the first bytes of the free block itself (reached through
//!   the direct map), so no heap is needed.
//! * A byte per frame (`meta`) records whether the frame is the head of a free
//!   block and of which order, which makes the buddy check on free O(1). The
//!   metadata array is carved out of the first usable region big enough.
//!
//! The global allocator sits behind an IRQ-safe [`SpinLock`].

use crate::bootinfo::MemoryMap;
use crate::mm::{PAGE_SHIFT, PAGE_SIZE, PhysAddr, page_align_up, phys_to_virt};
use crate::sync::SpinLock;

/// Largest block: 2^18 frames = 1 GiB.
pub const MAX_ORDER: usize = 18;

const NIL: usize = usize::MAX;
/// meta value for "not the head of a free block".
const META_NONE: u8 = 0;

#[repr(C)]
struct FreeNode {
    next: usize, // physical address or NIL
    prev: usize,
}

pub struct BuddyAllocator {
    base: usize, // physical address of frame index 0 (MAX_ORDER-aligned)
    nframes: usize,
    meta: *mut u8,
    heads: [usize; MAX_ORDER + 1],
    free_frames: usize,
    total_frames: usize,
}

// SAFETY: the raw pointer refers to direct-map memory owned by the allocator;
// access is serialised by the global SpinLock.
unsafe impl Send for BuddyAllocator {}

impl BuddyAllocator {
    const fn empty() -> Self {
        Self {
            base: 0,
            nframes: 0,
            meta: core::ptr::null_mut(),
            heads: [NIL; MAX_ORDER + 1],
            free_frames: 0,
            total_frames: 0,
        }
    }

    fn node(pa: usize) -> *mut FreeNode {
        phys_to_virt(PhysAddr(pa)).as_mut_ptr()
    }

    fn idx(&self, pa: usize) -> usize {
        (pa - self.base) >> PAGE_SHIFT
    }

    fn meta_get(&self, pa: usize) -> u8 {
        // SAFETY: callers only pass in-range addresses.
        unsafe { *self.meta.add(self.idx(pa)) }
    }

    fn meta_set(&mut self, pa: usize, v: u8) {
        let i = self.idx(pa);
        // SAFETY: in range.
        unsafe { *self.meta.add(i) = v };
    }

    fn in_range(&self, pa: usize) -> bool {
        pa >= self.base && (pa - self.base) >> PAGE_SHIFT < self.nframes
    }

    fn push(&mut self, pa: usize, order: usize) {
        let head = self.heads[order];
        // SAFETY: `pa` is a free block owned by the allocator, mapped via the direct map.
        unsafe {
            Self::node(pa).write(FreeNode {
                next: head,
                prev: NIL,
            });
            if head != NIL {
                (*Self::node(head)).prev = pa;
            }
        }
        self.heads[order] = pa;
        self.meta_set(pa, order as u8 + 1);
    }

    fn unlink(&mut self, pa: usize, order: usize) {
        // SAFETY: `pa` is on list `order`.
        unsafe {
            let n = Self::node(pa).read();
            if n.prev != NIL {
                (*Self::node(n.prev)).next = n.next;
            } else {
                self.heads[order] = n.next;
            }
            if n.next != NIL {
                (*Self::node(n.next)).prev = n.prev;
            }
        }
        self.meta_set(pa, META_NONE);
    }

    fn alloc(&mut self, order: usize) -> Option<PhysAddr> {
        if order > MAX_ORDER {
            return None;
        }
        let k = (order..=MAX_ORDER).find(|&k| self.heads[k] != NIL)?;
        let pa = self.heads[k];
        self.unlink(pa, k);
        // Split down, returning upper halves to the free lists.
        let mut cur = k;
        while cur > order {
            cur -= 1;
            self.push(pa + (PAGE_SIZE << cur), cur);
        }
        self.free_frames -= 1 << order;
        Some(PhysAddr(pa))
    }

    fn free(&mut self, pa: usize, order: usize) {
        let mut pa = pa;
        let mut order = order;
        self.free_frames += 1 << order;
        while order < MAX_ORDER {
            let buddy = pa ^ (PAGE_SIZE << order);
            if !self.in_range(buddy) || self.meta_get(buddy) != order as u8 + 1 {
                break;
            }
            self.unlink(buddy, order);
            pa = pa.min(buddy);
            order += 1;
        }
        self.push(pa, order);
    }

    /// Free `[start, end)` (page aligned, allocated) greedily in maximal
    /// naturally aligned blocks; returns the number of frames.
    fn free_range(&mut self, start: usize, end: usize) -> usize {
        let mut cur = start;
        while cur < end {
            let mut order = (cur >> PAGE_SHIFT).trailing_zeros().min(MAX_ORDER as u32) as usize;
            while order > 0 && cur + (PAGE_SIZE << order) > end {
                order -= 1;
            }
            self.free(cur, order);
            cur += PAGE_SIZE << order;
        }
        (end - start) >> PAGE_SHIFT
    }

    /// Add `[start, end)` (page aligned) as free memory.
    fn add_range(&mut self, start: usize, end: usize) {
        self.total_frames += self.free_range(start, end);
    }
}

static FRAMES: SpinLock<BuddyAllocator> = SpinLock::new(BuddyAllocator::empty());

/// Initialise the global frame allocator from the `Usable` regions of `map`.
/// Must be called once, after the direct map covers all RAM.
pub fn init(map: &MemoryMap) {
    let mut a = FRAMES.lock();
    assert!(a.meta.is_null(), "frame allocator initialised twice");

    let lo = map
        .usable()
        .map(|r| r.start.0)
        .min()
        .expect("no usable memory");
    let hi = map.usable().map(|r| r.end.0).max().unwrap();
    let base = lo & !((PAGE_SIZE << MAX_ORDER) - 1);
    let nframes = (hi - base) >> PAGE_SHIFT;
    let meta_bytes = page_align_up(nframes);

    // Carve the metadata array from the first usable region that fits.
    let meta_region = map
        .usable()
        .find(|r| r.len() >= meta_bytes + PAGE_SIZE)
        .expect("no room for frame metadata");
    let meta_pa = meta_region.start.0;
    let meta = phys_to_virt(PhysAddr(meta_pa)).as_mut_ptr::<u8>();
    // SAFETY: freshly carved, direct-mapped, usable RAM.
    unsafe { core::ptr::write_bytes(meta, META_NONE, meta_bytes) };

    a.base = base;
    a.nframes = nframes;
    a.meta = meta;

    for r in map.usable() {
        let (mut s, e) = (r.start.0, r.end.0);
        if s == meta_pa {
            s += meta_bytes;
        }
        a.add_range(s, e);
    }
}

/// Allocate `2^order` contiguous frames. Contents are not zeroed.
pub fn alloc_frames(order: usize) -> Option<PhysAddr> {
    FRAMES.lock().alloc(order)
}

/// Allocate `2^order` zeroed frames.
pub fn alloc_frames_zeroed(order: usize) -> Option<PhysAddr> {
    let pa = alloc_frames(order)?;
    // SAFETY: freshly allocated, direct-mapped RAM.
    unsafe { core::ptr::write_bytes(phys_to_virt(pa).as_mut_ptr::<u8>(), 0, PAGE_SIZE << order) };
    Some(pa)
}

/// Return a block obtained from [`alloc_frames`] with the same `order`.
pub fn free_frames(pa: PhysAddr, order: usize) {
    assert!(
        pa.0 & ((PAGE_SIZE << order) - 1) == 0,
        "free_frames: misaligned block"
    );
    let mut a = FRAMES.lock();
    assert!(a.in_range(pa.0), "free_frames: address outside managed RAM");
    a.free(pa.0, order);
}

/// Allocate `npages` contiguous frames whose start is aligned to
/// `PAGE_SIZE << align_order`: a block of the smallest order that holds
/// both, with the frames past `npages` handed straight back. Release with
/// [`free_pages`] (any page-aligned sub-range may be released on its own).
pub fn alloc_pages(npages: usize, align_order: usize) -> Option<PhysAddr> {
    let order = order_for(npages.max(1) * PAGE_SIZE).max(align_order);
    let mut a = FRAMES.lock();
    let pa = a.alloc(order)?;
    a.free_range(pa.0 + npages * PAGE_SIZE, pa.0 + (PAGE_SIZE << order));
    Some(pa)
}

/// Release `npages` frames at `pa` obtained from [`alloc_pages`] (or any
/// page-aligned run of frames the caller owns).
pub fn free_pages(pa: PhysAddr, npages: usize) {
    let mut a = FRAMES.lock();
    assert!(
        pa.is_page_aligned()
            && a.in_range(pa.0)
            && a.in_range(pa.0 + (npages.max(1) - 1) * PAGE_SIZE),
        "free_pages: bad range"
    );
    a.free_range(pa.0, pa.0 + npages * PAGE_SIZE);
}

/// `(free_frames, total_frames)` managed by the allocator.
pub fn stats() -> (usize, usize) {
    let a = FRAMES.lock();
    (a.free_frames, a.total_frames)
}

/// True once [`init`] has run.
pub fn is_initialized() -> bool {
    !FRAMES.lock().meta.is_null()
}

/// Smallest order whose block holds `bytes`.
pub const fn order_for(bytes: usize) -> usize {
    let frames = bytes.div_ceil(PAGE_SIZE);
    if frames <= 1 {
        0
    } else {
        (usize::BITS - (frames - 1).leading_zeros()) as usize
    }
}
