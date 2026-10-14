//! UART drivers for aarch64 VMMs: ARM PL011 (QEMU `virt`) and the 16550
//! family over MMIO (Firecracker / Cloud Hypervisor arm64: `ns16550a`).
//!
//! Output only, polled. No interrupts, no baud-rate programming (every
//! supported VMM ignores the divisor).

use core::ptr::{read_volatile, write_volatile};

use crate::console::Console;
use crate::sync::OnceCell;

/// ARM PrimeCell UART (PL011).
pub struct Pl011 {
    base: usize,
}

const PL011_DR: usize = 0x00;
const PL011_FR: usize = 0x18;
const PL011_CR: usize = 0x30;
const PL011_FR_TXFF: u32 = 1 << 5;
const PL011_CR_UARTEN: u32 = 1 << 0;
const PL011_CR_TXE: u32 = 1 << 8;
const PL011_CR_RXE: u32 = 1 << 9;

impl Pl011 {
    /// # Safety
    /// `base` must be the virtual address of a mapped PL011 register block.
    pub unsafe fn new(base: usize) -> Self {
        let u = Self { base };
        let cr = u.read(PL011_CR);
        let want = PL011_CR_UARTEN | PL011_CR_TXE | PL011_CR_RXE;
        if cr & want != want {
            u.write(PL011_CR, cr | want);
        }
        u
    }

    fn read(&self, off: usize) -> u32 {
        // SAFETY: MMIO register inside the mapped block.
        unsafe { read_volatile((self.base + off) as *const u32) }
    }

    fn write(&self, off: usize, v: u32) {
        // SAFETY: MMIO register inside the mapped block.
        unsafe { write_volatile((self.base + off) as *mut u32, v) }
    }
}

impl Console for Pl011 {
    fn write_bytes(&self, bytes: &[u8]) {
        for &b in bytes {
            while self.read(PL011_FR) & PL011_FR_TXFF != 0 {
                core::hint::spin_loop();
            }
            self.write(PL011_DR, b as u32);
        }
    }
}

/// 16550-compatible UART with MMIO registers at `base + (reg << shift)`,
/// accessed `width` bytes wide (DT `reg-shift` / `reg-io-width`).
pub struct Ns16550 {
    base: usize,
    shift: u32,
    width: u32,
}

const UART_THR: usize = 0;
const UART_IER: usize = 1;
const UART_FCR: usize = 2;
const UART_LCR: usize = 3;
const UART_MCR: usize = 4;
const UART_LSR: usize = 5;
const UART_LSR_THRE: u32 = 1 << 5;

impl Ns16550 {
    /// # Safety
    /// `base` must be the virtual address of the mapped register block,
    /// large enough for 8 registers at the given shift.
    pub unsafe fn new(base: usize, shift: u32, width: u32) -> Self {
        let u = Self { base, shift, width };
        u.write(UART_IER, 0); // no interrupts
        u.write(UART_LCR, 0x03); // 8N1, DLAB=0
        u.write(UART_FCR, 0x07); // enable + clear FIFOs
        u.write(UART_MCR, 0x03); // DTR | RTS
        u
    }

    fn addr(&self, reg: usize) -> usize {
        self.base + (reg << self.shift)
    }

    fn read(&self, reg: usize) -> u32 {
        // SAFETY: MMIO register inside the mapped block.
        unsafe {
            match self.width {
                4 => read_volatile(self.addr(reg) as *const u32),
                2 => read_volatile(self.addr(reg) as *const u16) as u32,
                _ => read_volatile(self.addr(reg) as *const u8) as u32,
            }
        }
    }

    fn write(&self, reg: usize, v: u32) {
        // SAFETY: MMIO register inside the mapped block.
        unsafe {
            match self.width {
                4 => write_volatile(self.addr(reg) as *mut u32, v),
                2 => write_volatile(self.addr(reg) as *mut u16, v as u16),
                _ => write_volatile(self.addr(reg) as *mut u8, v as u8),
            }
        }
    }
}

impl Console for Ns16550 {
    fn write_bytes(&self, bytes: &[u8]) {
        for &b in bytes {
            // Bounded wait: a VMM that never sets THRE must not hang the kernel.
            let mut spins = 0u32;
            while self.read(UART_LSR) & UART_LSR_THRE == 0 && spins < 1_000_000 {
                spins += 1;
                core::hint::spin_loop();
            }
            self.write(UART_THR, b as u32);
        }
    }
}

/// The boot console driver, one of the above.
pub enum Uart {
    Pl011(Pl011),
    Ns16550(Ns16550),
}

impl Console for Uart {
    fn write_bytes(&self, bytes: &[u8]) {
        match self {
            Uart::Pl011(u) => u.write_bytes(bytes),
            Uart::Ns16550(u) => u.write_bytes(bytes),
        }
    }
}

static UART: OnceCell<Uart> = OnceCell::new();

/// Install `uart` as the kernel console.
pub(super) fn install(uart: Uart) {
    if let Ok(u) = UART.set(uart) {
        crate::console::set(u);
    }
}
