//! Split virtqueues (virtio 1.2 §2.7) and DMA memory.
//!
//! Memory comes from the frame allocator and is accessed through the
//! direct map; device ("bus") addresses are physical addresses (no IOMMU
//! on any target VMM). The ring always uses the legacy layout (descriptor
//! table and available ring from the start of a 4 KiB-aligned block, used
//! ring at the next 4 KiB boundary), which modern transports accept too,
//! so one queue type serves virtio-mmio v1 and v2 (and later virtio-pci).
//!
//! A `VirtQueue` is not locked itself: its owner keeps it in a `SpinLock`
//! (or owns it exclusively). It never blocks; completion is observed with
//! [`VirtQueue::pop_used`] (from an IRQ-woken thread or by polling).

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

use super::Transport;
use crate::mm::{PAGE_SIZE, PhysAddr, frame, phys_to_virt};

/// Physically contiguous, zeroed DMA memory (whole pages).
pub struct DmaBuf {
    pa: usize,
    len: usize,
    order: usize,
}

impl DmaBuf {
    pub fn new(len: usize) -> Option<DmaBuf> {
        let order = frame::order_for(len.max(1));
        let pa = frame::alloc_frames_zeroed(order)?;
        Some(DmaBuf {
            pa: pa.0,
            len: PAGE_SIZE << order,
            order,
        })
    }

    /// Bus (= physical) address of byte `off`.
    pub fn pa(&self, off: usize) -> u64 {
        debug_assert!(off <= self.len);
        (self.pa + off) as u64
    }

    /// Kernel virtual address of byte `off`.
    pub fn va(&self, off: usize) -> usize {
        debug_assert!(off <= self.len);
        phys_to_virt(PhysAddr(self.pa + off)).as_usize()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Copy `src` to offset `off`.
    pub fn write(&self, off: usize, src: &[u8]) {
        assert!(off + src.len() <= self.len, "DmaBuf::write out of range");
        // SAFETY: inside the buffer (checked), which we own; the device
        // only reads it after it is made available.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.va(off) as *mut u8, src.len()) };
    }

    /// Copy from offset `off` into `dst`.
    pub fn read(&self, off: usize, dst: &mut [u8]) {
        assert!(off + dst.len() <= self.len, "DmaBuf::read out of range");
        // SAFETY: inside the buffer (checked); the device finished writing
        // it (its used-ring entry was observed with acquire ordering).
        unsafe {
            core::ptr::copy_nonoverlapping(self.va(off) as *const u8, dst.as_mut_ptr(), dst.len())
        };
    }

    /// A slice view of `[off, off+len)`.
    ///
    /// # Safety
    /// The device must not be writing that range while the slice lives.
    pub unsafe fn slice(&self, off: usize, len: usize) -> &[u8] {
        assert!(off + len <= self.len, "DmaBuf::slice out of range");
        // SAFETY: in range; the caller guarantees no concurrent DMA.
        unsafe { core::slice::from_raw_parts(self.va(off) as *const u8, len) }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        frame::free_frames(PhysAddr(self.pa), self.order);
    }
}

/// One buffer of a descriptor chain.
#[derive(Clone, Copy, Debug)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (an "in" buffer from the driver's view).
    pub write: bool,
}

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Largest queue this driver sets up (keeps the ring within 8 KiB).
pub const MAX_QUEUE_SIZE: u16 = 128;

/// A split virtqueue.
pub struct VirtQueue {
    index: u16,
    size: u16,
    mem: DmaBuf,
    used_off: usize,
    free_head: u16,
    num_free: u16,
    /// Descriptors per chain, indexed by head (for freeing).
    chain: Vec<u16>,
    avail_idx: u16,
    last_used: u16,
}

// SAFETY: the ring memory is owned by the queue; all access goes through
// `&mut self` (or `&self` for read-only volatile loads).
unsafe impl Send for VirtQueue {}

impl VirtQueue {
    /// Allocate queue `index` with at most `want` entries (power of two,
    /// capped by the device and [`MAX_QUEUE_SIZE`]) and tell the device.
    /// `None` if the device has no such queue or memory ran out.
    pub fn new(t: &dyn Transport, index: u16, want: u16) -> Option<VirtQueue> {
        let max = t.queue_max(index);
        if max == 0 {
            return None;
        }
        let mut size = want.min(max).clamp(1, MAX_QUEUE_SIZE);
        // Power of two (required by legacy devices, harmless otherwise).
        while !size.is_power_of_two() {
            size &= size - 1;
        }
        let n = size as usize;
        let desc_bytes = 16 * n;
        let avail_bytes = 6 + 2 * n;
        let used_off = (desc_bytes + avail_bytes).next_multiple_of(PAGE_SIZE);
        let used_bytes = 6 + 8 * n;
        let mem = DmaBuf::new(used_off + used_bytes)?;
        let q = VirtQueue {
            index,
            size,
            used_off,
            free_head: 0,
            num_free: size,
            chain: vec![0; n],
            avail_idx: 0,
            last_used: 0,
            mem,
        };
        // Free list: each descriptor's `next` points at the following one.
        for i in 0..n {
            q.wr_desc(i as u16, 0, 0, 0, ((i + 1) % n) as u16);
        }
        t.queue_setup(
            index,
            size,
            q.mem.pa(0),
            q.mem.pa(desc_bytes),
            q.mem.pa(used_off),
        );
        Some(q)
    }

    pub fn index(&self) -> u16 {
        self.index
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    pub fn num_free(&self) -> u16 {
        self.num_free
    }

    /// TEMP: (avail_idx, last_used, device used idx, desc pa).
    pub fn debug_state(&self) -> (u16, u16, u16, u64) {
        (self.avail_idx, self.last_used, self.used_idx(), self.mem.pa(0))
    }

    fn wr_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let p = self.mem.va(16 * i as usize);
        // SAFETY: descriptor `i` lies in the ring memory we own.
        unsafe {
            (p as *mut u64).write_volatile(addr);
            ((p + 8) as *mut u32).write_volatile(len);
            ((p + 12) as *mut u16).write_volatile(flags);
            ((p + 14) as *mut u16).write_volatile(next);
        }
    }

    fn desc_next(&self, i: u16) -> u16 {
        // SAFETY: as in `wr_desc`.
        unsafe { ((self.mem.va(16 * i as usize) + 14) as *const u16).read_volatile() }
    }

    fn avail_va(&self, off: usize) -> usize {
        self.mem.va(16 * self.size as usize + off)
    }

    fn used_va(&self, off: usize) -> usize {
        self.mem.va(self.used_off + off)
    }

    /// Ask the device not to interrupt for used buffers on this queue
    /// (a hint; for queues the driver polls).
    pub fn set_no_interrupt(&mut self, on: bool) {
        // SAFETY: the avail ring's flags word.
        unsafe {
            (self.avail_va(0) as *mut u16).write_volatile(if on { AVAIL_F_NO_INTERRUPT } else { 0 })
        };
    }

    /// Make the chain `bufs` available; returns its head (the token
    /// [`pop_used`](Self::pop_used) reports), or `None` if the queue is
    /// full. The device is not notified: call [`kick`](Self::kick).
    pub fn add(&mut self, bufs: &[Buf]) -> Option<u16> {
        let n = bufs.len() as u16;
        if n == 0 || n > self.num_free {
            return None;
        }
        let head = self.free_head;
        let mut i = head;
        for (k, b) in bufs.iter().enumerate() {
            let next = self.desc_next(i);
            let last = k + 1 == bufs.len();
            let mut flags = if b.write { DESC_F_WRITE } else { 0 };
            if !last {
                flags |= DESC_F_NEXT;
            }
            self.wr_desc(i, b.addr, b.len, flags, next);
            if last {
                self.free_head = next;
            } else {
                i = next;
            }
        }
        self.num_free -= n;
        self.chain[head as usize] = n;
        let slot = (self.avail_idx % self.size) as usize;
        // SAFETY: ring slot and index word of the avail ring we own. The
        // release fence orders the descriptor writes before the index
        // update the device polls.
        unsafe {
            (self.avail_va(4 + 2 * slot) as *mut u16).write_volatile(head);
            fence(Ordering::Release);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            (self.avail_va(2) as *mut u16).write_volatile(self.avail_idx);
        }
        Some(head)
    }

    /// Notify the device that buffers are available.
    pub fn kick(&self, t: &dyn Transport) {
        crate::arch::cpu::io_barrier();
        t.notify(self.index);
    }

    fn used_idx(&self) -> u16 {
        // SAFETY: the used ring's index word (written by the device).
        let v = unsafe { (self.used_va(2) as *const u16).read_volatile() };
        fence(Ordering::Acquire);
        v
    }

    /// Whether the device has returned buffers not yet popped.
    pub fn has_used(&self) -> bool {
        self.used_idx() != self.last_used
    }

    /// Next completed chain: (head token, bytes written by the device).
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        if self.used_idx() == self.last_used {
            return None;
        }
        let slot = (self.last_used % self.size) as usize;
        // SAFETY: a used-ring element (id u32, len u32) the device wrote
        // before publishing the index we observed (acquire above).
        let (id, len) = unsafe {
            let p = self.used_va(4 + 8 * slot);
            (
                (p as *const u32).read_volatile(),
                ((p + 4) as *const u32).read_volatile(),
            )
        };
        self.last_used = self.last_used.wrapping_add(1);
        let head = id as u16;
        if head >= self.size {
            return None; // device bug; drop it
        }
        // Return the chain to the free list.
        let n = self.chain[head as usize];
        let mut last = head;
        for _ in 1..n {
            last = self.desc_next(last);
        }
        let flags = 0;
        let (addr, dlen) = (0, 0);
        self.wr_desc(last, addr, dlen, flags, self.free_head);
        self.free_head = head;
        self.num_free += n;
        Some((head, len))
    }
}
