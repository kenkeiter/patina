//! Kernel console: `kprint!` / `kprintln!` over whichever driver arch code
//! registered with [`set`].
//!
//! Drivers implement [`Console::write_bytes`] and receive raw bytes; this
//! module translates `\n` into `\r\n`. All output is serialised by an IRQ-safe
//! spinlock so lines from different CPUs do not interleave mid-line.
//! [`Console::flush`] is called after every print (buffered drivers such as
//! the virtio `hvc0` port send then).
//!
//! `console=hvc0` (virtio-console, decision D61): the arch boot code keeps
//! its UART (if any) and calls [`start_early_ring`], which also records all
//! output in an 8 KiB ring. When the virtio-console comes up,
//! [`switch_to`] replays the ring into it and makes it the console, so
//! hvc0 sees the whole boot log even on a VMM without a UART.
//!
//! Panic path: [`panic_write_fmt`] tries the lock for a bounded time and then
//! force-unlocks it. The panic handler never returns to the code that held the
//! lock, so breaking mutual exclusion there only risks garbled output, never a
//! deadlock.

use core::cell::UnsafeCell;
use core::fmt::{self, Write};

use crate::sync::SpinLock;

/// A byte sink for kernel output (UART, virtio-console, ...).
pub trait Console: Send + Sync {
    /// Write raw bytes. Must not allocate and must not take the console lock.
    fn write_bytes(&self, bytes: &[u8]);
    /// Push out anything buffered (end of one print).
    fn flush(&self) {}
}

static CONSOLE: SpinLock<Option<&'static dyn Console>> = SpinLock::new(None);

/// Register (or replace) the active console driver.
pub fn set(console: &'static dyn Console) {
    *CONSOLE.lock() = Some(console);
}

/// True once a console driver has been registered.
pub fn is_set() -> bool {
    CONSOLE.lock().is_some()
}

const RING: usize = 8192;

/// The early console for `console=hvc0`: forwards to the UART (if any)
/// and records everything in a ring. Only touched under the CONSOLE lock.
struct EarlyRing {
    inner: UnsafeCell<RingState>,
}

struct RingState {
    uart: Option<&'static dyn Console>,
    buf: [u8; RING],
    /// Total bytes ever written (ring position = total % RING).
    total: usize,
}

// SAFETY: every access happens with the CONSOLE lock held (it is only
// reachable as the registered console, or from `switch_to` under the lock).
unsafe impl Sync for EarlyRing {}

static EARLY: EarlyRing = EarlyRing {
    inner: UnsafeCell::new(RingState {
        uart: None,
        buf: [0; RING],
        total: 0,
    }),
};

impl Console for EarlyRing {
    fn write_bytes(&self, bytes: &[u8]) {
        // SAFETY: see `unsafe impl Sync`.
        let s = unsafe { &mut *self.inner.get() };
        if let Some(u) = s.uart {
            u.write_bytes(bytes);
        }
        for &b in bytes {
            s.buf[s.total % RING] = b;
            s.total += 1;
        }
    }
}

/// `console=hvc0`: wrap the current console (UART or none) so output is
/// also kept for the virtio console that comes up later.
pub fn start_early_ring() {
    let mut g = CONSOLE.lock();
    // SAFETY: CONSOLE lock held.
    let s = unsafe { &mut *EARLY.inner.get() };
    s.uart = *g;
    *g = Some(&EARLY);
}

/// Make `console` the kernel console. If the early ring was active, its
/// contents are written to `console` first.
pub fn switch_to(console: &'static dyn Console) {
    let mut g = CONSOLE.lock();
    if let Some(cur) = *g
        && core::ptr::addr_eq(cur as *const dyn Console, &EARLY as *const EarlyRing)
    {
        // SAFETY: CONSOLE lock held.
        let s = unsafe { &mut *EARLY.inner.get() };
        let start = s.total.saturating_sub(RING);
        if start > 0 {
            console.write_bytes(b"[patina] console: (early output truncated)\r\n");
        }
        for i in start..s.total {
            let b = s.buf[i % RING];
            console.write_bytes(core::slice::from_ref(&b));
        }
        console.flush();
    }
    *g = Some(console);
}

struct Writer(&'static dyn Console);

impl Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_translated(self.0, s.as_bytes());
        Ok(())
    }
}

fn write_translated(c: &'static dyn Console, bytes: &[u8]) {
    let mut rest = bytes;
    while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
        c.write_bytes(&rest[..pos]);
        c.write_bytes(b"\r\n");
        rest = &rest[pos + 1..];
    }
    if !rest.is_empty() {
        c.write_bytes(rest);
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    let guard = CONSOLE.lock();
    if let Some(c) = *guard {
        let _ = Writer(c).write_fmt(args);
        c.flush();
    }
}

/// Write raw bytes (user `write(1/2, ...)` output), translating `\n` to
/// `\r\n` like `kprint!`. Bytes need not be UTF-8.
pub fn write_bytes(bytes: &[u8]) {
    let guard = CONSOLE.lock();
    if let Some(c) = *guard {
        write_translated(c, bytes);
        c.flush();
    }
}

/// Print from a context that must not deadlock on the console lock (panic,
/// fatal exception). Spins briefly, then force-unlocks.
pub fn panic_write_fmt(args: fmt::Arguments<'_>) {
    let mut guard = None;
    for _ in 0..1_000_000 {
        if let Some(g) = CONSOLE.try_lock() {
            guard = Some(g);
            break;
        }
        core::hint::spin_loop();
    }
    let guard = match guard {
        Some(g) => g,
        None => {
            // SAFETY: we are on a no-return path (see module docs).
            unsafe { CONSOLE.force_unlock() };
            CONSOLE.lock()
        }
    };
    if let Some(c) = *guard {
        let _ = Writer(c).write_fmt(args);
        c.flush();
    }
}

#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! kprintln {
    () => { $crate::console::_print(format_args!("\n")) };
    ($($arg:tt)*) => {
        $crate::console::_print(format_args!("{}\n", format_args!($($arg)*)))
    };
}
