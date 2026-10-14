//! Pure, host-testable pieces of the kernel's file layer (design §8):
//!
//! * [`FdSlots`]: the fd-number allocation policy of a file-descriptor
//!   table (lowest free number at or above a minimum, below a limit);
//! * [`ByteRing`]: the fixed-capacity byte ring behind a pipe;
//! * [`EventCounter`]: `eventfd` counter semantics.
//!
//! The kernel (`kernel/src/fs/`) wraps these in locks and wait queues.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Fd slots.

/// A sparse array of `T` indexed by fd number.
#[derive(Debug)]
pub struct FdSlots<T> {
    slots: Vec<Option<T>>,
    used: usize,
}

impl<T> Default for FdSlots<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FdSlots<T> {
    pub const fn new() -> Self {
        FdSlots {
            slots: Vec::new(),
            used: 0,
        }
    }

    /// Number of occupied slots.
    pub fn len(&self) -> usize {
        self.used
    }

    pub fn is_empty(&self) -> bool {
        self.used == 0
    }

    pub fn get(&self, fd: usize) -> Option<&T> {
        self.slots.get(fd).and_then(Option::as_ref)
    }

    pub fn get_mut(&mut self, fd: usize) -> Option<&mut T> {
        self.slots.get_mut(fd).and_then(Option::as_mut)
    }

    /// Lowest free fd `>= min` and `< limit`, if any.
    pub fn lowest_free(&self, min: usize, limit: usize) -> Option<usize> {
        let mut fd = min;
        while fd < limit {
            match self.slots.get(fd) {
                Some(Some(_)) => fd += 1,
                _ => return Some(fd),
            }
        }
        None
    }

    /// Put `v` at the lowest free fd `>= min` and `< limit`. Gives `v`
    /// back if there is none (`EMFILE`).
    pub fn alloc(&mut self, v: T, min: usize, limit: usize) -> Result<usize, T> {
        match self.lowest_free(min, limit) {
            Some(fd) => {
                let old = self.install(fd, v);
                debug_assert!(old.is_none());
                Ok(fd)
            }
            None => Err(v),
        }
    }

    /// Put `v` at `fd` (growing the table), returning what was there
    /// (`dup2`/`dup3`). The caller checks `fd` against the limit.
    pub fn install(&mut self, fd: usize, v: T) -> Option<T> {
        if fd >= self.slots.len() {
            self.slots.resize_with(fd + 1, || None);
        }
        let old = self.slots[fd].replace(v);
        if old.is_none() {
            self.used += 1;
        }
        old
    }

    /// Empty slot `fd`, returning its value.
    pub fn take(&mut self, fd: usize) -> Option<T> {
        let v = self.slots.get_mut(fd)?.take();
        if v.is_some() {
            self.used -= 1;
            while self.slots.last().is_some_and(Option::is_none) {
                self.slots.pop();
            }
        }
        v
    }

    /// Occupied slots in fd order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|v| (i, v)))
    }

    /// Remove every slot for which `pred` holds (close-on-exec), returning
    /// the removed values.
    pub fn take_matching(&mut self, mut pred: impl FnMut(&T) -> bool) -> Vec<T> {
        let mut out = Vec::new();
        let mut fd = 0;
        while fd < self.slots.len() {
            if self.slots[fd].as_ref().is_some_and(&mut pred) {
                out.extend(self.take(fd));
            }
            fd += 1;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Byte ring.

/// A byte FIFO of fixed capacity. The storage is allocated on the first
/// push, so an idle pipe costs no buffer.
#[derive(Debug)]
pub struct ByteRing {
    buf: Vec<u8>,
    cap: usize,
    head: usize,
    len: usize,
}

impl ByteRing {
    pub const fn new(cap: usize) -> ByteRing {
        ByteRing {
            buf: Vec::new(),
            cap,
            head: 0,
            len: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Free space in bytes.
    pub fn space(&self) -> usize {
        self.cap - self.len
    }

    /// Append as much of `data` as fits; returns the number of bytes taken.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.space());
        if n == 0 {
            return 0;
        }
        if self.buf.is_empty() {
            self.buf.resize(self.cap, 0);
        }
        let tail = (self.head + self.len) % self.cap;
        let first = n.min(self.cap - tail);
        self.buf[tail..tail + first].copy_from_slice(&data[..first]);
        self.buf[..n - first].copy_from_slice(&data[first..n]);
        self.len += n;
        n
    }

    /// Remove up to `out.len()` bytes into `out`; returns the count.
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        if n == 0 {
            return 0;
        }
        let first = n.min(self.cap - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % self.cap;
        self.len -= n;
        if self.len == 0 {
            self.head = 0;
        }
        n
    }
}

// ---------------------------------------------------------------------------
// eventfd counter.

/// Why an eventfd operation cannot complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventError {
    /// `EAGAIN` (or block): counter is 0 (read) / would overflow (write).
    WouldBlock,
    /// `EINVAL`: write of `u64::MAX`.
    Invalid,
}

/// The eventfd counter (`fs/eventfd.c`): values up to `u64::MAX - 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventCounter {
    pub count: u64,
    pub semaphore: bool,
}

impl EventCounter {
    pub const MAX: u64 = u64::MAX - 1;

    pub const fn new(init: u64, semaphore: bool) -> EventCounter {
        EventCounter {
            count: init,
            semaphore,
        }
    }

    /// A read: the counter (or 1 in semaphore mode), decrementing it.
    pub fn read(&mut self) -> Result<u64, EventError> {
        if self.count == 0 {
            return Err(EventError::WouldBlock);
        }
        let v = if self.semaphore { 1 } else { self.count };
        self.count -= v;
        Ok(v)
    }

    /// A write of `v`.
    pub fn write(&mut self, v: u64) -> Result<(), EventError> {
        if v == u64::MAX {
            return Err(EventError::Invalid);
        }
        if Self::MAX - self.count < v {
            return Err(EventError::WouldBlock);
        }
        self.count += v;
        Ok(())
    }

    pub fn readable(&self) -> bool {
        self.count > 0
    }

    /// Linux reports `EPOLLOUT` while a write of 1 would not block.
    pub fn writable(&self) -> bool {
        self.count < Self::MAX
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use alloc::vec;

    #[test]
    fn fd_lowest_free() {
        let mut t = FdSlots::new();
        assert_eq!(t.alloc('a', 0, 4), Ok(0));
        assert_eq!(t.alloc('b', 0, 4), Ok(1));
        assert_eq!(t.alloc('c', 0, 4), Ok(2));
        assert_eq!(t.take(1), Some('b'));
        assert_eq!(t.alloc('d', 0, 4), Ok(1));
        assert_eq!(t.alloc('e', 0, 4), Ok(3));
        assert_eq!(t.alloc('f', 0, 4), Err('f'));
        assert_eq!(t.len(), 4);
        assert_eq!(t.take(3), Some('e'));
        assert_eq!(t.take(3), None);
        assert_eq!(t.alloc('g', 10, 20), Ok(10));
        assert_eq!(t.alloc('h', 2, 20), Ok(3));
        assert_eq!(t.get(10), Some(&'g'));
        assert_eq!(t.get(11), None);
        assert_eq!(t.alloc('i', 30, 20), Err('i'));
    }

    #[test]
    fn fd_install_take_matching() {
        let mut t = FdSlots::new();
        assert_eq!(t.install(5, 1), None);
        assert_eq!(t.install(5, 2), Some(1));
        t.install(0, 3);
        t.install(7, 4);
        assert_eq!(t.len(), 3);
        let gone = t.take_matching(|v| *v % 2 == 0);
        assert_eq!(gone, vec![2, 4]);
        assert_eq!(t.iter().collect::<std::vec::Vec<_>>(), vec![(0, &3)]);
        assert_eq!(t.take(0), Some(3));
        assert!(t.is_empty());
    }

    #[test]
    fn ring_wraps() {
        let mut r = ByteRing::new(8);
        assert_eq!(r.space(), 8);
        assert_eq!(r.push(b"abcdef"), 6);
        let mut out = [0u8; 4];
        assert_eq!(r.pop(&mut out), 4);
        assert_eq!(&out, b"abcd");
        assert_eq!(r.push(b"123456789"), 6);
        assert_eq!(r.len(), 8);
        assert_eq!(r.push(b"x"), 0);
        let mut out = [0u8; 16];
        assert_eq!(r.pop(&mut out), 8);
        assert_eq!(&out[..8], b"ef123456");
        assert!(r.is_empty());
        assert_eq!(r.pop(&mut out), 0);
    }

    #[test]
    fn ring_many_rounds() {
        let mut r = ByteRing::new(7);
        let mut next = 0u8;
        let mut expect = 0u8;
        for round in 0..200 {
            let n = round % 5 + 1;
            let data: std::vec::Vec<u8> = (0..n).map(|i| next.wrapping_add(i as u8)).collect();
            let k = r.push(&data);
            next = next.wrapping_add(k as u8);
            let mut out = [0u8; 3];
            let m = r.pop(&mut out[..round % 3 + 1]);
            for b in &out[..m] {
                assert_eq!(*b, expect);
                expect = expect.wrapping_add(1);
            }
        }
    }

    #[test]
    fn eventfd_counter() {
        let mut c = EventCounter::new(0, false);
        assert_eq!(c.read(), Err(EventError::WouldBlock));
        c.write(3).unwrap();
        c.write(4).unwrap();
        assert_eq!(c.read(), Ok(7));
        assert!(!c.readable());
        assert_eq!(c.write(u64::MAX), Err(EventError::Invalid));
        c.write(EventCounter::MAX).unwrap();
        assert!(!c.writable());
        assert_eq!(c.write(1), Err(EventError::WouldBlock));
        c.write(0).unwrap();
        let mut s = EventCounter::new(2, true);
        assert_eq!(s.read(), Ok(1));
        assert_eq!(s.read(), Ok(1));
        assert_eq!(s.read(), Err(EventError::WouldBlock));
    }
}
