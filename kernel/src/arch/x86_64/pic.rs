//! Legacy 8259 PICs: remapped and fully masked at boot, never used.
//!
//! Without this, a PIC left uninitialized by the VMM/firmware (vector base
//! 0 on `microvm`) delivers PIT IRQ0 as vector 0 through LAPIC LINT0
//! (ExtINT) as soon as IRQs are enabled, which looks like #DE. Remapping to
//! 0x20-0x2f first means even a spurious IRQ7/15 cannot alias an exception
//! vector. External IRQs will come through the IOAPIC (Phase 2).

use super::port::outb;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;

/// Vector base for PIC1 / PIC2 after remapping (gates stay not present).
pub const PIC1_BASE: u8 = 0x20;
pub const PIC2_BASE: u8 = 0x28;

/// Initialize both PICs (ICW1-4: edge, cascade on IRQ2, 8086 mode) with
/// vector bases 0x20 / 0x28, then mask every line. Harmless without PICs.
pub(super) fn disable() {
    // SAFETY: standard 8259 init sequence on the legacy PIC ports; with all
    // lines masked no interrupt can come from them.
    unsafe {
        outb(PIC1_CMD, 0x11);
        outb(PIC2_CMD, 0x11);
        outb(PIC1_DATA, PIC1_BASE);
        outb(PIC2_DATA, PIC2_BASE);
        outb(PIC1_DATA, 1 << 2);
        outb(PIC2_DATA, 2);
        outb(PIC1_DATA, 0x01);
        outb(PIC2_DATA, 0x01);
        outb(PIC1_DATA, 0xff);
        outb(PIC2_DATA, 0xff);
    }
}
