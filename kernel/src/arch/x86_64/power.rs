//! Power off.
//!
//! Tried in order, each falling through if the VMM ignores it:
//! 1. QEMU `isa-debug-exit` at port 0xf4 (QEMU exits with `(code << 1) | 1`;
//!    the exit code proper travels through the serial sentinel). Probed by
//!    reading the port: the device reads as 0, unclaimed port space as 0xff.
//! 2. ACPI PM1a_CNT S5 at port 0x604 (QEMU `pc`/`q35` PM block; no table
//!    parsing). Cloud Hypervisor's ACPI shutdown needs FADT parsing: not done.
//! 3. i8042 CPU reset (port 0x64, 0xfe): how Firecracker terminates a guest;
//!    QEMU with `-no-reboot` exits on it too.
//! 4. `cli; hlt` forever.

use super::port::{inb, outb, outw};

const DEBUG_EXIT_PORT: u16 = 0xf4;
const ACPI_PM1A_CNT: u16 = 0x604;
const ACPI_SLP_EN_S5: u16 = 0x2000; // SLP_EN | SLP_TYP=0 (QEMU's S5 value)
const I8042_CMD: u16 = 0x64;
const I8042_RESET: u8 = 0xfe;

pub fn poweroff(code: i32) -> ! {
    let _ = super::irq::save_disable();
    // SAFETY: well-known VMM poweroff ports; we never return.
    unsafe {
        if inb(DEBUG_EXIT_PORT) != 0xff {
            outb(DEBUG_EXIT_PORT, code as u8);
        }
        outw(ACPI_PM1A_CNT, ACPI_SLP_EN_S5);
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
        outb(I8042_CMD, I8042_RESET);
    }
    super::cpu::halt()
}
