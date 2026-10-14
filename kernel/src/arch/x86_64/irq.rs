//! Local interrupt masking (RFLAGS.IF) and device interrupt registration.
//!
//! Device interrupts (Phase 3) are identified by GSI, routed through the
//! I/O APIC (`ioapic.rs`) to the boot CPU on a vector from
//! [`DEV_VECTOR_BASE`]..[`DEV_VECTOR_END`] allocated at [`register`]. The
//! handler runs with IRQs masked (interrupt gate), then the LAPIC gets its
//! EOI (`exceptions::handle_trap`), which for a level-triggered pin is also
//! the I/O APIC EOI broadcast: the handler must have quieted the device by
//! then.
//!
//! MSI / MSI-X (Phase 4, D70): [`alloc_msi`] takes a vector from the same
//! range and names it with the IRQ number [`MSI_IRQ_BASE`] + vector, which
//! [`configure`] ignores and [`register`] binds without touching the I/O
//! APIC. The message targets the boot CPU's LAPIC (fixed delivery, edge).

use core::arch::asm;

/// Saved RFLAGS value.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub struct Flags(u64);

const RFLAGS_IF: u64 = 1 << 9;

/// Mask interrupts; return the previous RFLAGS.
#[inline]
pub fn save_disable() -> Flags {
    let f: u64;
    // SAFETY: only changes the local interrupt flag. Not `nomem`: acts as a
    // compiler barrier like the lock it usually guards.
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) f, options(preserves_flags)) };
    Flags(f)
}

/// Restore the interrupt state returned by [`save_disable`].
#[inline]
pub fn restore(f: Flags) {
    if f.0 & RFLAGS_IF != 0 {
        // SAFETY: re-enables interrupts that were enabled before.
        unsafe { asm!("sti", options(nostack)) };
    }
}

/// Unmask interrupts on this CPU.
#[inline]
pub fn enable() {
    // SAFETY: callers enable IRQs only where the IDT and stacks are ready.
    unsafe { asm!("sti", options(nostack)) };
}

/// Whether interrupts are currently enabled on this CPU.
#[inline]
pub fn enabled() -> bool {
    let f: u64;
    // SAFETY: read-only.
    unsafe { asm!("pushfq", "pop {}", out(reg) f, options(nomem, preserves_flags)) };
    f & RFLAGS_IF != 0
}

/// Handler for a device interrupt; gets the GSI.
pub type Handler = fn(u32);

/// First and one-past-last vector used for device interrupts.
pub const DEV_VECTOR_BASE: u8 = 0x30;
pub const DEV_VECTOR_END: u8 = 0x80;
const NDEV: usize = (DEV_VECTOR_END - DEV_VECTOR_BASE) as usize;

#[derive(Clone, Copy)]
struct Slot {
    gsi: u32,
    handler: Handler,
}

static SLOTS: crate::sync::SpinLock<[Option<Slot>; NDEV]> =
    crate::sync::SpinLock::new([None; NDEV]);
/// Per-GSI trigger configuration set by [`configure`]: (gsi, level, active_low).
static CONFIG: crate::sync::SpinLock<[(u32, bool, bool); 32]> =
    crate::sync::SpinLock::new([(u32::MAX, false, false); 32]);

/// IRQ numbers at and above this name MSI vectors (`MSI_IRQ_BASE` +
/// vector), not GSIs.
pub const MSI_IRQ_BASE: u32 = 0x100;

fn noop(_irq: u32) {}

/// Reserve a device vector for an MSI/MSI-X message: returns its IRQ
/// number (for [`register`]) and the message address/data, physical
/// destination = the boot CPU's APIC id (xAPIC format, id < 256), fixed
/// delivery, edge. Until [`register`] binds a handler the vector is
/// ignored. `_requester` (the PCI requester id) is unused on x86.
pub fn alloc_msi(_requester: u32) -> Option<crate::drivers::pci::MsiMsg> {
    let mut s = SLOTS.lock();
    let i = s.iter().position(|e| e.is_none())?;
    let vector = DEV_VECTOR_BASE as u32 + i as u32;
    let irq = MSI_IRQ_BASE + vector;
    s[i] = Some(Slot {
        gsi: irq,
        handler: noop,
    });
    let dest = super::smp::apic_id(0) & 0xff;
    Some(crate::drivers::pci::MsiMsg {
        irq,
        addr: 0xfee0_0000 | ((dest as u64) << 12),
        data: vector,
    })
}

/// Set the trigger mode / polarity of `gsi` (from the DSDT `Interrupt`
/// descriptor or the cmdline default) before [`register`]. Without it a
/// GSI below 16 follows the MADT ISA overrides and others are edge,
/// active-high.
pub fn configure(gsi: u32, edge: bool, active_low: bool) {
    if gsi >= MSI_IRQ_BASE {
        return; // MSI: always edge
    }
    let mut c = CONFIG.lock();
    if let Some(e) = c.iter_mut().find(|e| e.0 == gsi || e.0 == u32::MAX) {
        *e = (gsi, !edge, active_low);
    }
}

/// Install `handler` for `gsi`, route it through the I/O APIC to the boot
/// CPU and unmask it. The handler runs with IRQs masked and must not block.
pub fn register(gsi: u32, handler: Handler) {
    if gsi >= MSI_IRQ_BASE {
        let mut s = SLOTS.lock();
        match s.iter_mut().flatten().find(|e| e.gsi == gsi) {
            Some(e) => e.handler = handler,
            None => crate::kprintln!("[patina] irq: msi irq {} was not allocated", gsi),
        }
        return;
    }
    let cfg = CONFIG.lock().iter().find(|e| e.0 == gsi).copied();
    let (level, active_low) = match cfg {
        Some((_, l, a)) => (l, a),
        None if gsi < 16 => {
            let (_, l, a) = super::ioapic::isa_to_gsi(gsi as u8);
            (l, a)
        }
        None => (false, false),
    };
    let vector = {
        let mut s = SLOTS.lock();
        let i = match s.iter().position(|e| e.is_some_and(|e| e.gsi == gsi)) {
            Some(i) => i,
            None => s
                .iter()
                .position(|e| e.is_none())
                .expect("irq::register: out of device vectors"),
        };
        s[i] = Some(Slot { gsi, handler });
        DEV_VECTOR_BASE + i as u8
    };
    if !super::ioapic::route(gsi, vector, level, active_low) {
        crate::kprintln!("[patina] irq: no i/o apic serves gsi {}", gsi);
    }
}

/// Device vector entry (from `handle_trap`, IRQs masked, before EOI).
pub(super) fn dispatch(vector: u8) {
    let i = (vector - DEV_VECTOR_BASE) as usize;
    let slot = SLOTS.lock().get(i).copied().flatten();
    match slot {
        Some(s) => (s.handler)(s.gsi),
        None => crate::kprintln!("[patina] irq: unexpected vector {:#x}", vector),
    }
}
