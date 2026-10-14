//! 16550 UART over port I/O (`ttyS0` = 0x3f8 on QEMU, Firecracker and
//! Cloud Hypervisor). Output only, polled, no interrupts.

use super::port::{inb, outb};
use crate::console::Console;
use crate::sync::OnceCell;

const THR: u16 = 0; // DLAB=0: transmit holding register; DLAB=1: divisor low
const IER: u16 = 1; // DLAB=0: interrupt enable; DLAB=1: divisor high
const FCR: u16 = 2;
const LCR: u16 = 3;
const MCR: u16 = 4;
const LSR: u16 = 5;
const SCR: u16 = 7;
const LSR_THRE: u8 = 1 << 5;

/// Legacy COM port bases for `ttyS0`..`ttyS3`.
pub const COM_PORTS: [u16; 4] = [0x3f8, 0x2f8, 0x3e8, 0x2e8];

pub struct Uart16550 {
    base: u16,
}

impl Uart16550 {
    /// Program 115200 8N1, FIFOs on, interrupts off.
    ///
    /// # Safety
    /// `base` must be the I/O base of a 16550-compatible UART (or unused
    /// port space, where the writes are ignored).
    pub unsafe fn new(base: u16) -> Self {
        // SAFETY: caller guarantees the port range.
        unsafe {
            outb(base + IER, 0);
            outb(base + LCR, 0x80); // DLAB
            outb(base + THR, 1); // divisor 1 = 115200
            outb(base + IER, 0);
            outb(base + LCR, 0x03); // 8N1, DLAB off
            outb(base + FCR, 0xc7); // enable + clear FIFOs, 14-byte threshold
            outb(base + MCR, 0x0b); // DTR | RTS | OUT2
        }
        Self { base }
    }

    /// Scratch-register probe: true if something answers at `base`.
    pub fn present(base: u16) -> bool {
        // SAFETY: SCR has no side effects on a 16550.
        unsafe {
            outb(base + SCR, 0x5a);
            inb(base + SCR) == 0x5a
        }
    }

    pub fn base(&self) -> u16 {
        self.base
    }
}

impl Console for Uart16550 {
    fn write_bytes(&self, bytes: &[u8]) {
        for &b in bytes {
            // Bounded wait: a VMM that never sets THRE must not hang the kernel.
            let mut spins = 0u32;
            // SAFETY: registers of the UART this driver was created for.
            while unsafe { inb(self.base + LSR) } & LSR_THRE == 0 && spins < 1_000_000 {
                spins += 1;
                core::hint::spin_loop();
            }
            // SAFETY: as above.
            unsafe { outb(self.base + THR, b) };
        }
    }
}

static UART: OnceCell<Uart16550> = OnceCell::new();

/// Install `uart` as the kernel console.
pub(super) fn install(uart: Uart16550) {
    if let Ok(u) = UART.set(uart) {
        crate::console::set(u);
    }
}
