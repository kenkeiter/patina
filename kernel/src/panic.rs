//! Panic handler: emit the control-channel `panic` event, print the message
//! and location, emit the exit sentinel with code 255, power off. A panic while panicking (e.g. the console driver
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
        crate::task::sched::halt_this_cpu();
    }
    // Stop the other CPUs (this may be an AP; CPU 0 is stopped too) so
    // nothing else prints or runs user code; if another CPU is already
    // exiting, this one halts here and that one powers off.
    crate::task::sched::stop_other_cpus();
    crate::ctl::emit_panic(format_args!("{}", info.message()));
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
