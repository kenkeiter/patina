//! Panic handler: print the message and location, emit the exit sentinel with
//! code 255, power off. A panic while panicking (e.g. the console driver
//! faults) halts immediately instead of recursing.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;
use crate::console::panic_write_fmt;

static PANICKING: AtomicBool = AtomicBool::new(false);

/// Exit code reported for a kernel panic (design §10.2).
pub const PANIC_EXIT_CODE: i32 = 255;

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    // Mask interrupts first; we never return.
    let _ = arch::irq::save_disable();
    if PANICKING.swap(true, Ordering::SeqCst) {
        arch::cpu::halt();
    }
    match info.location() {
        Some(loc) => panic_write_fmt(format_args!(
            "[patina] panic: {} at {}:{}\n",
            info.message(),
            loc.file(),
            loc.line()
        )),
        None => panic_write_fmt(format_args!("[patina] panic: {}\n", info.message())),
    }
    panic_write_fmt(format_args!("\n[patina] exit code {}\n", PANIC_EXIT_CODE));
    arch::power::poweroff(PANIC_EXIT_CODE)
}
