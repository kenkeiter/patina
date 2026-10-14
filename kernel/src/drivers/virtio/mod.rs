//! virtio core: the [`Transport`] abstraction, the device registry, IRQ
//! plumbing and the drivers (rng, console, vsock). Decision D58.
//!
//! * Transports implement [`Transport`] (`mmio.rs`; `pci.rs`, D70) and
//!   are added with [`register_transport`]. Drivers only
//!   see the trait plus [`queue::VirtQueue`], so they are transport-agnostic.
//! * Discovery (`drivers::init`) probes every platform-described
//!   transport, prints one `[patina] virtio: ...` line per device and
//!   registers it here. Drivers [`take`] devices by type; unclaimed ones
//!   (e.g. net until Phase 4) stay listed and undriven.
//! * Interrupts: [`attach_irq`] routes a device's interrupt line to a
//!   driver callback through `arch::irq::{configure, register}`. The
//!   callback runs with IRQs masked: it reads and acknowledges the
//!   transport's interrupt status ([`Transport::ack_interrupt`]) and wakes
//!   the driver's thread; queue processing happens in thread context, which
//!   always re-polls the queues after waking, so an edge-triggered line
//!   never loses a completion.

pub mod console;
pub mod mmio;
pub mod net;
pub mod pci;
pub mod queue;
pub mod rng;
pub mod vsock;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::sync::SpinLock;

/// Device types (virtio 1.2 §5).
pub const DEVICE_NET: u32 = 1;
pub const DEVICE_BLOCK: u32 = 2;
pub const DEVICE_CONSOLE: u32 = 3;
pub const DEVICE_RNG: u32 = 4;
pub const DEVICE_VSOCK: u32 = 19;

pub fn device_name(id: u32) -> &'static str {
    match id {
        DEVICE_NET => "net",
        DEVICE_BLOCK => "block",
        DEVICE_CONSOLE => "console",
        DEVICE_RNG => "rng",
        DEVICE_VSOCK => "vsock",
        _ => "unknown",
    }
}

// Device status bits.
pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_FAILED: u32 = 128;

/// VIRTIO_F_VERSION_1.
pub const F_VERSION_1: u64 = 1 << 32;

/// Interrupt status bits ([`Transport::ack_interrupt`]).
pub const INT_VRING: u32 = 1;
pub const INT_CONFIG: u32 = 2;

/// A virtio transport: the register-level interface a driver needs.
/// Implementations must be usable with IRQs masked and from any CPU
/// (plain MMIO / port accesses, no locks).
pub trait Transport: Send + Sync {
    /// virtio device type (1 = net, 3 = console, 4 = rng, 19 = vsock...).
    fn device_type(&self) -> u32;
    /// Location for logs (`mmio @ 0x...`).
    fn describe(&self, out: &mut dyn Write) -> core::fmt::Result;
    /// Transport details for logs (version, vendor).
    fn details(&self, out: &mut dyn Write) -> core::fmt::Result;
    /// Modern (virtio 1.x) device: `VIRTIO_F_VERSION_1` must be accepted
    /// and `FEATURES_OK` checked.
    fn modern(&self) -> bool;
    fn status(&self) -> u32;
    /// Writing 0 resets the device.
    fn set_status(&self, s: u32);
    fn device_features(&self) -> u64;
    fn set_driver_features(&self, f: u64);
    /// Maximum size of queue `q` (0: no such queue or already set up).
    fn queue_max(&self, q: u16) -> u16;
    /// Tell the device where queue `q` lives and enable it.
    fn queue_setup(&self, q: u16, size: u16, desc: u64, avail: u64, used: u64);
    fn notify(&self, q: u16);
    /// Read and acknowledge the interrupt status ([`INT_VRING`],
    /// [`INT_CONFIG`]); 0 if the device did not interrupt.
    fn ack_interrupt(&self) -> u32;
    /// Interrupt kind for the discovery line (`msi-x`, `intx`); `None`:
    /// say `edge`/`level` from the [`IrqLine`] (virtio-mmio).
    fn irq_kind(&self) -> Option<&'static str> {
        None
    }
    fn config_generation(&self) -> u32;
    fn config_read8(&self, off: usize) -> u8;
    fn config_read32(&self, off: usize) -> u32;
    fn config_read64(&self, off: usize) -> u64 {
        loop {
            let g = self.config_generation();
            let v = self.config_read32(off) as u64 | ((self.config_read32(off + 4) as u64) << 32);
            if g == self.config_generation() {
                return v;
            }
        }
    }
    fn config_read16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.config_read8(off), self.config_read8(off + 1)])
    }
}

/// How a device's interrupt is wired (arch IRQ number: GIC INTID on
/// aarch64, GSI on x86_64).
#[derive(Clone, Copy, Debug)]
pub struct IrqLine {
    pub irq: u32,
    pub edge: bool,
    pub active_low: bool,
}

/// A discovered device.
pub struct Device {
    pub transport: Box<dyn Transport>,
    pub irq: Option<IrqLine>,
}

static DEVICES: SpinLock<Vec<Device>> = SpinLock::new(Vec::new());

/// Add a discovered device (`source`: how it was found) and print its
/// `[patina] virtio: <location> irq N device-id K (<name>)` line.
pub fn register_transport(dev: Device, source: &str) {
    let mut loc = String::new();
    let _ = dev.transport.describe(&mut loc);
    let mut det = String::new();
    let _ = dev.transport.details(&mut det);
    let id = dev.transport.device_type();
    crate::kprintln!(
        "[patina] virtio: {} irq {} device-id {} ({}) [{}, {}, {} irq]",
        loc,
        dev.irq.map_or(-1, |i| i.irq as i64),
        id,
        device_name(id),
        source,
        det,
        match (dev.irq, dev.transport.irq_kind()) {
            (Some(_), Some(k)) => k,
            (Some(l), None) if l.edge => "edge",
            (Some(_), None) => "level",
            (None, _) => "no",
        }
    );
    DEVICES.lock().push(dev);
}

/// Claim the first unclaimed device of type `device_type`.
pub fn take(device_type: u32) -> Option<Device> {
    let mut d = DEVICES.lock();
    let i = d
        .iter()
        .position(|x| x.transport.device_type() == device_type)?;
    Some(d.remove(i))
}

/// Number of devices still unclaimed.
pub fn unclaimed() -> usize {
    DEVICES.lock().len()
}

/// Reset the device and negotiate features: the driver accepts
/// `wanted & offered` (plus `VIRTIO_F_VERSION_1` on modern devices).
/// Leaves the device in ACKNOWLEDGE | DRIVER | FEATURES_OK; the caller sets
/// up its queues and then calls [`driver_ok`]. Returns the accepted set.
pub fn negotiate(t: &dyn Transport, wanted: u64) -> Result<u64, &'static str> {
    t.set_status(0);
    t.set_status(STATUS_ACKNOWLEDGE);
    t.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    let offered = t.device_features();
    let mut accept = offered & wanted;
    if t.modern() {
        if offered & F_VERSION_1 == 0 {
            t.set_status(STATUS_FAILED);
            return Err("modern transport without VIRTIO_F_VERSION_1");
        }
        accept |= F_VERSION_1;
    } else {
        accept &= 0xffff_ffff;
    }
    t.set_driver_features(accept);
    let s = STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK;
    t.set_status(s);
    if t.modern() && t.status() & STATUS_FEATURES_OK == 0 {
        t.set_status(STATUS_FAILED);
        return Err("device rejected the feature set");
    }
    Ok(accept)
}

/// Queues are set up: the driver is live.
pub fn driver_ok(t: &dyn Transport) {
    t.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
}

/// Driver callback for a device interrupt (IRQs masked, must not block).
pub type IrqCallback = fn();

static ROUTES: SpinLock<[Option<(u32, IrqCallback)>; 16]> = SpinLock::new([None; 16]);

fn dispatch(irq: u32) {
    let routes = *ROUTES.lock();
    for (i, cb) in routes.iter().flatten() {
        if *i == irq {
            cb();
        }
    }
}

/// Route `line` to `cb` (shared lines call every callback on them).
pub fn attach_irq(line: IrqLine, cb: IrqCallback) {
    {
        let mut r = ROUTES.lock();
        let Some(slot) = r.iter_mut().find(|s| s.is_none()) else {
            drop(r);
            crate::kprintln!(
                "[patina] virtio: too many irq routes, irq {} not attached",
                line.irq
            );
            return;
        };
        *slot = Some((line.irq, cb));
    }
    crate::arch::irq::configure(line.irq, line.edge, line.active_low);
    crate::arch::irq::register(line.irq, dispatch);
}

/// Spin until `done()` or `ns` nanoseconds have passed (boot-time polling
/// with IRQs possibly masked). Returns whether `done()` became true.
pub fn poll_until(ns: u64, mut done: impl FnMut() -> bool) -> bool {
    let t0 = crate::arch::timer::now_ns();
    loop {
        if done() {
            return true;
        }
        if crate::arch::timer::now_ns().saturating_sub(t0) >= ns {
            return done();
        }
        core::hint::spin_loop();
    }
}
