//! Kernel heap: page-sized slabs for small objects, whole frames for the
//! rest, both taken from the frame allocator on demand and given back to
//! it as soon as they are free (D3, D87).
//!
//! * Small allocations (up to [`SMALL_MAX`] bytes, alignment permitting)
//!   come from one of the size classes in `CLASS_SIZE`. Each class keeps
//!   4 KiB slabs: a 32-byte header at the start of the page, then equal
//!   slots filling the page's end, placed so that every slot has the
//!   class's alignment. A slab whose last object is freed goes back to the
//!   frame allocator, except for up to `KEEP_EMPTY` empty slabs per class,
//!   kept to avoid churn. Each class has its own IRQ-safe [`SpinLock`].
//! * Larger allocations take `ceil(size / 4 KiB)` contiguous frames
//!   ([`frame::alloc_pages`]: a buddy block with its tail handed back) and
//!   return them on free.
//!
//! The allocator keeps no metadata of its own: `dealloc` gets the layout,
//! which names the class (or the page count), and a slab's header is at
//! the start of the page holding the object. Lock order is class lock →
//! frames; the frame allocator never allocates from the heap. Every lock
//! masks interrupts, so allocating in interrupt context never deadlocks
//! against the interrupted holder, and nothing blocks.
//!
//! Init order: `mm::frame::init` must run before the first allocation.
//! Any allocation before that (or after RAM runs out) returns null, which
//! the `alloc` crate turns into a panic via the default alloc error handler.
//!
//! The heap has no cap of its own: it takes frames as it needs them, so
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
//!   frames plus free slab slots) stays above a low watermark (half of
//!   the D78 reserve), and fails with ENOMEM/ENOSPC/ENOBUFS otherwise.
//! * [`try_zeroed`] and `try_reserve*`: buffers sized by user input are
//!   allocated fallibly.
//!
//! Infallible allocations then only happen while the watermark holds.

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use patina_abi::errno::{ENOMEM, Errno};

use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, frame, phys_to_virt, virt_to_phys};
use crate::sync::SpinLock;

// ---------------------------------------------------------------------------
// Size classes.

/// Bytes at the start of a slab page reserved for its [`Slab`] header.
const HDR: usize = 32;
/// Slot sizes. Powers of two get natural alignment, the others 16 to 64.
const CLASS_SIZE: [usize; 13] = [16, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1344, 2032];
const NCLASSES: usize = CLASS_SIZE.len();
/// Largest small allocation.
pub const SMALL_MAX: usize = CLASS_SIZE[NCLASSES - 1];
/// Empty slabs a class keeps instead of freeing them.
const KEEP_EMPTY: usize = 2;

/// Slots per slab of class `c`.
const fn class_objs(c: usize) -> usize {
    (PAGE_SIZE - HDR) / CLASS_SIZE[c]
}

/// Offset of slot 0 in a slab of class `c` (the slots fill the page's end).
const fn class_offset(c: usize) -> usize {
    PAGE_SIZE - class_objs(c) * CLASS_SIZE[c]
}

/// Alignment of every slot of class `c`: the lowest set bit of both the
/// first slot's offset and the slot size.
const fn class_align(c: usize) -> usize {
    (class_offset(c) | CLASS_SIZE[c]).isolate_lowest_one()
}

/// `SIZE_TO_CLASS[size.div_ceil(16)]`: the smallest class holding `size`.
const SIZE_TO_CLASS: [u8; SMALL_MAX / 16 + 1] = {
    let mut t = [0u8; SMALL_MAX / 16 + 1];
    let mut i = 0;
    let mut c = 0;
    while i < t.len() {
        while CLASS_SIZE[c] < i * 16 {
            c += 1;
        }
        t[i] = c as u8;
        i += 1;
    }
    t
};

const _: () = {
    let mut c = 0;
    while c < NCLASSES {
        assert!(CLASS_SIZE[c].is_multiple_of(16) && class_objs(c) >= 2);
        assert!(class_offset(c) >= HDR && class_align(c) >= 16);
        c += 1;
    }
};

/// The class for `layout`, or `None` for the page path.
#[inline]
fn class_for(layout: &Layout) -> Option<usize> {
    let size = layout.size().max(1);
    if size > SMALL_MAX {
        return None;
    }
    let mut c = SIZE_TO_CLASS[size.div_ceil(16)] as usize;
    while class_align(c) < layout.align() {
        c += 1;
        if c == NCLASSES {
            return None;
        }
    }
    Some(c)
}

/// Pages and alignment order of a page-path allocation.
fn pages_for(layout: &Layout) -> (usize, usize) {
    let align_order = if layout.align() > PAGE_SIZE {
        frame::order_for(layout.align())
    } else {
        0
    };
    (layout.size().max(1).div_ceil(PAGE_SIZE), align_order)
}

// ---------------------------------------------------------------------------
// Slabs.

/// Header at the start of every slab page.
#[repr(C)]
struct Slab {
    next: *mut Slab,
    prev: *mut Slab,
    /// Free slots, linked through their first word.
    free: *mut usize,
    inuse: usize,
}

const _: () = assert!(core::mem::size_of::<Slab>() <= HDR);

/// The slabs of one class that have a free slot: partly used ones at the
/// front, empty ones at the back (the last to be used again, so the
/// partly used ones fill up and empty ones can go back to the frames).
struct ClassList {
    head: *mut Slab,
    tail: *mut Slab,
    /// Empty slabs on the list.
    empty: usize,
}

// SAFETY: the raw pointers name slab pages owned by the heap; every access
// is serialised by the class's SpinLock.
unsafe impl Send for ClassList {}

impl ClassList {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            empty: 0,
        }
    }

    /// # Safety
    /// `s` is a live slab of this class, not on the list.
    unsafe fn push_front(&mut self, s: *mut Slab) {
        // SAFETY: caller contract; list members are live slab headers.
        unsafe {
            (*s).prev = ptr::null_mut();
            (*s).next = self.head;
            if self.head.is_null() {
                self.tail = s;
            } else {
                (*self.head).prev = s;
            }
        }
        self.head = s;
    }

    /// # Safety
    /// `s` is a live slab of this class, not on the list.
    unsafe fn push_back(&mut self, s: *mut Slab) {
        // SAFETY: as above.
        unsafe {
            (*s).next = ptr::null_mut();
            (*s).prev = self.tail;
            if self.tail.is_null() {
                self.head = s;
            } else {
                (*self.tail).next = s;
            }
        }
        self.tail = s;
    }

    /// # Safety
    /// `s` is on the list.
    unsafe fn unlink(&mut self, s: *mut Slab) {
        // SAFETY: as above.
        unsafe {
            let (n, p) = ((*s).next, (*s).prev);
            if p.is_null() {
                self.head = n;
            } else {
                (*p).next = n;
            }
            if n.is_null() {
                self.tail = p;
            } else {
                (*n).prev = p;
            }
        }
    }
}

static CLASSES: [SpinLock<ClassList>; NCLASSES] =
    [const { SpinLock::new(ClassList::new()) }; NCLASSES];

/// Frames held by slabs, and by page-path allocations.
static SLAB_PAGES: AtomicUsize = AtomicUsize::new(0);
static LARGE_PAGES: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested; bytes of slots and pages handed out; bytes of slots
/// handed out.
static USER_BYTES: AtomicUsize = AtomicUsize::new(0);
static ACTUAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static SLOT_BYTES: AtomicUsize = AtomicUsize::new(0);

/// A new slab for class `c`, every slot on its free list.
fn new_slab(c: usize) -> Option<*mut Slab> {
    let pa = frame::alloc_frames(0)?;
    SLAB_PAGES.fetch_add(1, Ordering::Relaxed);
    let base = phys_to_virt(pa).as_usize();
    let size = CLASS_SIZE[c];
    let mut free: *mut usize = ptr::null_mut();
    // Linked back to front, so allocation walks the page upwards.
    let mut off = PAGE_SIZE - size;
    loop {
        let slot = (base + off) as *mut usize;
        // SAFETY: a slot inside the freshly allocated, direct-mapped page.
        unsafe { slot.write(free as usize) };
        free = slot;
        if off == class_offset(c) {
            break;
        }
        off -= size;
    }
    let s = base as *mut Slab;
    // SAFETY: the page's header area.
    unsafe {
        s.write(Slab {
            next: ptr::null_mut(),
            prev: ptr::null_mut(),
            free,
            inuse: 0,
        })
    };
    Some(s)
}

fn free_slab(s: *mut Slab) {
    let pa = virt_to_phys(VirtAddr(s as usize)).expect("heap: slab outside the direct map");
    frame::free_frames(pa, 0);
    SLAB_PAGES.fetch_sub(1, Ordering::Relaxed);
}

fn slab_alloc(c: usize) -> *mut u8 {
    let mut l = CLASSES[c].lock();
    if l.head.is_null() {
        let Some(s) = new_slab(c) else {
            return ptr::null_mut();
        };
        // SAFETY: a new slab, on no list.
        unsafe { l.push_front(s) };
        l.empty += 1;
    }
    let s = l.head;
    // SAFETY: list members are live slabs of this class with a free slot.
    unsafe {
        let obj = (*s).free;
        (*s).free = obj.read() as *mut usize;
        if (*s).inuse == 0 {
            l.empty -= 1;
        }
        (*s).inuse += 1;
        if (*s).free.is_null() {
            l.unlink(s);
        }
        obj as *mut u8
    }
}

/// # Safety
/// `p` came from `slab_alloc(c)` and is not used any more.
unsafe fn slab_free(p: *mut u8, c: usize) {
    let s = (p as usize & !(PAGE_SIZE - 1)) as *mut Slab;
    let mut l = CLASSES[c].lock();
    // SAFETY: `s` is the live slab holding `p` (caller contract).
    unsafe {
        let was_full = (*s).free.is_null();
        (p as *mut usize).write((*s).free as usize);
        (*s).free = p as *mut usize;
        (*s).inuse -= 1;
        if (*s).inuse == 0 {
            if !was_full {
                l.unlink(s);
            }
            if l.empty >= KEEP_EMPTY {
                drop(l);
                free_slab(s);
            } else {
                l.empty += 1;
                l.push_back(s);
            }
        } else if was_full {
            l.push_front(s);
        }
    }
}

// ---------------------------------------------------------------------------
// The global allocator.

pub struct KernelHeap;

fn large_pa(p: *mut u8) -> PhysAddr {
    virt_to_phys(VirtAddr(p as usize)).expect("heap: block outside the direct map")
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (p, actual) = match class_for(&layout) {
            Some(c) => {
                let p = slab_alloc(c);
                if !p.is_null() {
                    SLOT_BYTES.fetch_add(CLASS_SIZE[c], Ordering::Relaxed);
                }
                (p, CLASS_SIZE[c])
            }
            None => {
                let (pages, align_order) = pages_for(&layout);
                match frame::alloc_pages(pages, align_order) {
                    Some(pa) => {
                        LARGE_PAGES.fetch_add(pages, Ordering::Relaxed);
                        (phys_to_virt(pa).as_mut_ptr(), pages * PAGE_SIZE)
                    }
                    None => (ptr::null_mut(), 0),
                }
            }
        };
        if !p.is_null() {
            USER_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            ACTUAL_BYTES.fetch_add(actual, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if p.is_null() {
            return;
        }
        USER_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        match class_for(&layout) {
            Some(c) => {
                ACTUAL_BYTES.fetch_sub(CLASS_SIZE[c], Ordering::Relaxed);
                SLOT_BYTES.fetch_sub(CLASS_SIZE[c], Ordering::Relaxed);
                // SAFETY: GlobalAlloc contract: `p`/`layout` come from `alloc`.
                unsafe { slab_free(p, c) };
            }
            None => {
                let (pages, _) = pages_for(&layout);
                LARGE_PAGES.fetch_sub(pages, Ordering::Relaxed);
                ACTUAL_BYTES.fetch_sub(pages * PAGE_SIZE, Ordering::Relaxed);
                frame::free_pages(large_pa(p), pages);
            }
        }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc contract: `new_size` with `layout.align()` is
        // a valid layout.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let in_place = match (class_for(&layout), class_for(&new)) {
            // Same slot class: nothing moves.
            (Some(a), Some(b)) => a == b,
            // Page path, not growing: keep the head, free the tail.
            (None, None) => {
                let (old_pages, _) = pages_for(&layout);
                let (new_pages, _) = pages_for(&new);
                if new_pages <= old_pages {
                    let tail = old_pages - new_pages;
                    if tail > 0 {
                        frame::free_pages(PhysAddr(large_pa(p).0 + new_pages * PAGE_SIZE), tail);
                        LARGE_PAGES.fetch_sub(tail, Ordering::Relaxed);
                        ACTUAL_BYTES.fetch_sub(tail * PAGE_SIZE, Ordering::Relaxed);
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if in_place {
            USER_BYTES.fetch_add(new_size, Ordering::Relaxed);
            USER_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            return p;
        }
        // SAFETY: `new` is a valid, non-zero-size layout.
        let q = unsafe { self.alloc(new) };
        if !q.is_null() {
            // SAFETY: both blocks are live and at least this long; `p` is
            // not used after.
            unsafe {
                ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
                self.dealloc(p, layout);
            }
        }
        q
    }
}

#[global_allocator]
static HEAP: KernelHeap = KernelHeap;

/// Boot hook, after `mm::frame::init`. The heap takes frames on demand,
/// so there is nothing to seed; this only checks the init order.
pub fn init() {
    assert!(frame::is_initialized(), "heap: frame allocator not initialised");
}

/// `(user_bytes, allocated_bytes, total_bytes)` of the heap: bytes
/// requested, bytes of slots and pages handed out, and bytes of frames
/// the heap holds (slab pages, kept empty ones included, and page-path
/// blocks).
pub fn stats() -> (usize, usize, usize) {
    (
        USER_BYTES.load(Ordering::Relaxed),
        ACTUAL_BYTES.load(Ordering::Relaxed),
        (SLAB_PAGES.load(Ordering::Relaxed) + LARGE_PAGES.load(Ordering::Relaxed)) * PAGE_SIZE,
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

/// What a heap allocation of `n` bytes (alignment at most 16) really
/// takes: its slot, or whole pages. The amount [`charge`] callers account
/// for.
pub const fn alloc_size(n: usize) -> usize {
    if n == 0 {
        0
    } else if n <= SMALL_MAX {
        CLASS_SIZE[SIZE_TO_CLASS[n.div_ceil(16)] as usize]
    } else {
        n.div_ceil(PAGE_SIZE) * PAGE_SIZE
    }
}

/// Free memory the kernel can still allocate from: free frames plus free
/// slots in slab pages.
fn available_bytes() -> usize {
    let (free, _) = frame::stats();
    let slab_free = (SLAB_PAGES.load(Ordering::Relaxed) * PAGE_SIZE)
        .saturating_sub(SLOT_BYTES.load(Ordering::Relaxed));
    free.saturating_mul(PAGE_SIZE).saturating_add(slab_free)
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
