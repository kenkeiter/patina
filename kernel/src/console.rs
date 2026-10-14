//! Kernel console: `kprint!` / `kprintln!` over whichever driver arch code
//! registered with [`set`].
//!
//! Drivers implement [`Console::write_bytes`] and receive raw bytes; this
//! module translates `\n` into `\r\n`. All output is serialised by an IRQ-safe
//! spinlock so lines from different CPUs do not interleave mid-line.
//!
//! Panic path: [`panic_write_fmt`] tries the lock for a bounded time and then
//! force-unlocks it. The panic handler never returns to the code that held the
//! lock, so breaking mutual exclusion there only risks garbled output, never a
//! deadlock.

use core::fmt::{self, Write};

use crate::sync::SpinLock;

/// A byte sink for kernel output (UART, virtio-console, ...).
pub trait Console: Send + Sync {
    /// Write raw bytes. Must not allocate and must not take the console lock.
    fn write_bytes(&self, bytes: &[u8]);
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

struct Writer(&'static dyn Console);

impl Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut rest = s.as_bytes();
        while let Some(pos) = rest.iter().position(|&b| b == b'\n') {
            self.0.write_bytes(&rest[..pos]);
            self.0.write_bytes(b"\r\n");
            rest = &rest[pos + 1..];
        }
        if !rest.is_empty() {
            self.0.write_bytes(rest);
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    let guard = CONSOLE.lock();
    if let Some(c) = *guard {
        let _ = Writer(c).write_fmt(args);
    }
}

/// Write raw bytes (user `write(1/2, ...)` output), translating `\n` to
/// `\r\n` like `kprint!`. Bytes need not be UTF-8.
pub fn write_bytes(bytes: &[u8]) {
    let guard = CONSOLE.lock();
    if let Some(c) = *guard {
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
