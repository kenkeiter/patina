//! Power off (decision D64).
//!
//! Tried in order, each falling through if the VMM ignores it:
//! 1. QEMU `isa-debug-exit` at port 0xf4, if present (QEMU exits with
//!    `(code << 1) | 1`; the exit code proper travels through the control
//!    channel / serial sentinel). Probed by reading the port: the device
//!    reads as 0, unclaimed port space as 0xff.
//! 2. ACPI S5 from the FADT (`platform.rs`):
//!    * hardware-reduced ACPI (FADT flag HW_REDUCED_ACPI) with a
//!      SLEEP_CONTROL_REG: write `SLP_TYP << 2 | SLP_EN` (S5 = 5 unless
//!      the DSDT's `_S5` says otherwise) there. This is Cloud
//!      Hypervisor's poweroff (0x34 to port 0x600);
//!    * else PM1a_CNT: `SLP_TYP << 10 | SLP_EN` with SLP_TYP from `_S5`
//!      (QEMU pc/q35: 0 at 0x604);
//!    * no ACPI tables: nothing here.
//! 3. i8042 CPU reset (port 0x64, 0xfe): how Firecracker (no ACPI sleep
//!    register) terminates a guest; QEMU with `-no-reboot` exits on it too.
//!    Never reached on Cloud Hypervisor, where it would REBOOT the VM: its
//!    S5 write in step 2 does not return.
//! 4. `cli; hlt` forever.
//!
//! One `[patina] poweroff: <method>` line is printed (after the exit
//! sentinel) for each method tried.

use super::port::{inb, outb, outw};

const DEBUG_EXIT_PORT: u16 = 0xf4;
const SLP_EN_PM1: u16 = 1 << 13;
const SLP_EN_SLEEP_CTL: u8 = 1 << 5;
const I8042_CMD: u16 = 0x64;
const I8042_RESET: u8 = 0xfe;
/// S5 sleep type when the DSDT `_S5` package could not be read.
const DEFAULT_S5_HW_REDUCED: u8 = 5;
const DEFAULT_S5_PM1: u8 = 0;

enum AcpiOff {
    SleepCtl { space: u8, addr: u64, typ: u8 },
    Pm1a { port: u16, typ: u8 },
}

fn acpi_method() -> Option<AcpiOff> {
    super::platform::with(|f| {
        let fadt = f.fadt?;
        let s5 = f.dsdt.and_then(|d| d.s5_typ);
        let sc = fadt.sleep_control;
        if fadt.flags & super::acpi::FADT_HW_REDUCED != 0 && sc.address != 0 {
            return Some(AcpiOff::SleepCtl {
                space: sc.space,
                addr: sc.address,
                typ: s5.unwrap_or(DEFAULT_S5_HW_REDUCED),
            });
        }
        if fadt.pm1a_cnt != 0 && fadt.pm1a_cnt <= 0xffff {
            return Some(AcpiOff::Pm1a {
                port: fadt.pm1a_cnt as u16,
                typ: s5.unwrap_or(DEFAULT_S5_PM1),
            });
        }
        None
    })
}

fn log(args: core::fmt::Arguments<'_>) {
    crate::console::panic_write_fmt(format_args!("[patina] poweroff: {}\n", args));
}

fn settle() {
    for _ in 0..100_000 {
        core::hint::spin_loop();
    }
}

pub fn poweroff(code: i32) -> ! {
    let _ = super::irq::save_disable();
    // SAFETY: well-known VMM poweroff ports / the FADT's own registers; we
    // never return.
    unsafe {
        if inb(DEBUG_EXIT_PORT) != 0xff {
            log(format_args!("isa-debug-exit"));
            outb(DEBUG_EXIT_PORT, code as u8);
            settle();
        }
        match acpi_method() {
            Some(AcpiOff::SleepCtl { space, addr, typ }) => {
                log(format_args!(
                    "acpi s5 via sleep control register ({} {:#x}, SLP_TYP {})",
                    if space == 1 { "io" } else { "mem" },
                    addr,
                    typ
                ));
                let v = ((typ & 7) << 2) | SLP_EN_SLEEP_CTL;
                if space == 1 && addr <= 0xffff {
                    outb(addr as u16, v);
                } else if space == 0 {
                    let va = super::paging::map_device(crate::mm::PhysAddr(addr as usize), 1);
                    (va.as_usize() as *mut u8).write_volatile(v);
                }
                settle();
            }
            Some(AcpiOff::Pm1a { port, typ }) => {
                log(format_args!(
                    "acpi s5 via pm1a_cnt (io {:#x}, SLP_TYP {})",
                    port, typ
                ));
                outw(port, ((typ as u16 & 7) << 10) | SLP_EN_PM1);
                settle();
            }
            None => {}
        }
        log(format_args!("i8042 reset"));
        outb(I8042_CMD, I8042_RESET);
    }
    super::cpu::halt()
}
