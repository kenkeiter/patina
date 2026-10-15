//! The seam between network drivers and the TCP/IP stack (design §9).
//!
//! A driver (virtio-net today) implements [`NetDevice`] and hands one
//! instance to [`register`] during `drivers::init`. The stack
//! (`net::stack`) [`take`]s it when it starts its interface task. Both
//! sides see only this trait, so a driver never depends on smoltcp and the
//! stack never depends on virtio.
//!
//! Contract:
//!
//! * `recv`/`send`/`flush` never block and may be called from several
//!   CPUs at once, with stack locks held (IRQs masked): the driver
//!   serializes them with its own lock (a leaf) and only touches its own
//!   rings.
//! * The driver signals [`NetDevice::event`] from its interrupt handler
//!   (and from anywhere else it learns of new receive completions or
//!   freed transmit slots). The stack's task waits on that `Event` with a
//!   deadline and re-polls both directions after every wake-up, so a
//!   missed edge costs at most one timer period.
//! * Frames are Ethernet frames without any transport header (the
//!   virtio-net header is stripped / added by the driver) and without FCS.
//!   Offloads are off: checksums are the stack's job.

use alloc::boxed::Box;

use crate::drivers::Event;
use crate::sync::SpinLock;

/// Largest Ethernet frame the stack hands a driver (MTU 1500 + 14).
pub const MAX_FRAME: usize = 1514;

pub trait NetDevice: Send + Sync {
    /// Driver name for logs (`"virtio-net"`).
    fn name(&self) -> &'static str;
    /// The device's MAC address.
    fn mac(&self) -> [u8; 6];
    /// Link MTU in bytes (IP payload, i.e. 1500 for Ethernet).
    fn mtu(&self) -> usize {
        1500
    }
    /// Copy the next received frame into `buf` and return its length; `None`
    /// when nothing is pending. A frame longer than `buf` is dropped (and
    /// the next one tried). Must also recycle the receive buffer to the
    /// device.
    fn recv(&self, buf: &mut [u8]) -> Option<usize>;
    /// Whether [`NetDevice::send`] would accept a frame right now.
    fn can_send(&self) -> bool;
    /// Queue `frame` for transmission; `false` when no transmit slot is free
    /// (the stack keeps the frame and retries after the next event).
    fn send(&self, frame: &[u8]) -> bool;
    /// Signalled by the driver on receive completions and freed transmit
    /// slots (from IRQ context). The stack's task waits on it.
    fn event(&self) -> &'static Event;
    /// The stack is done with a batch of `recv`/`send` calls: issue any
    /// deferred device notifications (doorbells) now. A driver that
    /// notifies eagerly in `recv`/`send` leaves this a no-op.
    fn flush(&self) {}
}

static PENDING: SpinLock<Option<Box<dyn NetDevice>>> = SpinLock::new(None);

/// Register the (single) network device. A second registration is ignored
/// with a log line.
pub fn register(dev: Box<dyn NetDevice>) {
    let mut p = PENDING.lock();
    if p.is_some() {
        drop(p);
        crate::kprintln!(
            "[patina] net: a second network device ({}) was ignored",
            dev.name()
        );
        return;
    }
    *p = Some(dev);
}

/// Claim the registered device (the stack calls this once).
pub fn take() -> Option<Box<dyn NetDevice>> {
    PENDING.lock().take()
}

/// Whether a device is registered and not yet taken.
pub fn present() -> bool {
    PENDING.lock().is_some()
}
