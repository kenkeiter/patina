//! Kernel synchronisation primitives.
//!
//! [`SpinLock`] is an IRQ-safe spinlock: taking it disables interrupts on the
//! local CPU (via `arch::irq::save_disable`) and the guard restores the saved
//! interrupt state on drop. That makes it safe to take from interrupt context
//! without deadlocking against the interrupted holder on the same CPU.
//!
//! [`OnceCell`] is a minimal set-once cell for boot-time globals.

use core::cell::UnsafeCell;
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::arch::irq;

/// IRQ-safe test-and-test-and-set spinlock.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `data`.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

pub struct SpinLockGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
    irq: Option<irq::Flags>,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Disable local interrupts and spin until the lock is acquired.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let flags = irq::save_disable();
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return SpinLockGuard {
                    lock: self,
                    irq: Some(flags),
                };
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Try once to acquire the lock (interrupts disabled while held).
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let flags = irq::save_disable();
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinLockGuard {
                lock: self,
                irq: Some(flags),
            })
        } else {
            irq::restore(flags);
            None
        }
    }

    /// Forcibly release the lock regardless of who holds it.
    ///
    /// # Safety
    /// Only for paths that will never return to the holder (the panic handler
    /// printing its final message). Breaks mutual exclusion.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

impl<T: ?Sized> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock exclusively.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if let Some(f) = self.irq.take() {
            irq::restore(f);
        }
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SpinLock { .. }")
    }
}

const ONCE_EMPTY: u8 = 0;
const ONCE_BUSY: u8 = 1;
const ONCE_READY: u8 = 2;

/// A cell written at most once (typically during boot) and then read freely.
pub struct OnceCell<T> {
    state: AtomicU8,
    value: UnsafeCell<core::mem::MaybeUninit<T>>,
}

// SAFETY: the value is only written once, before `state` becomes READY
// (Release), and only read after observing READY (Acquire).
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}

impl<T> OnceCell<T> {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(ONCE_EMPTY),
            value: UnsafeCell::new(core::mem::MaybeUninit::uninit()),
        }
    }

    /// Store `value`. Returns a reference to the stored value, or gives the
    /// value back if the cell was already set.
    pub fn set(&self, value: T) -> Result<&T, T> {
        if self
            .state
            .compare_exchange(ONCE_EMPTY, ONCE_BUSY, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(value);
        }
        // SAFETY: we won the EMPTY->BUSY transition; nobody else touches `value`.
        unsafe { (*self.value.get()).write(value) };
        self.state.store(ONCE_READY, Ordering::Release);
        Ok(self.get().unwrap())
    }

    pub fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == ONCE_READY {
            // SAFETY: READY means the value was fully written and is never written again.
            Some(unsafe { (*self.value.get()).assume_init_ref() })
        } else {
            None
        }
    }
}
