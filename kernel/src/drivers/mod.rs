//! Device drivers (Phase 3): virtio over mmio, discovered through the
//! arch layer (`arch::devices::virtio_mmio`: DTB on aarch64; cmdline,
//! DSDT or a probe on x86_64), and virtio over PCI (Phase 4, D70:
//! `pci.rs` enumerates `arch::devices::pci_hosts`, `virtio/pci.rs` is the
//! transport).
//!
//! Boot order (`init`, from `kernel_main` before the self-tests and the
//! loader): discover and register every virtio transport, then bring up
//! virtio-rng (seeds `random`), virtio-console (multiport; the
//! `patina.ctl` port and the `hvc0` console), virtio-vsock and virtio-net
//! (registered with `net::device`). All device
//! setup polls with bounded waits: IRQs are still masked and a device
//! that never answers must not hang the boot. Interrupts are attached for
//! use once the scheduler runs.

pub mod pci;
pub mod virtio;

use alloc::sync::Arc;

use crate::sync::SpinLock;
use crate::task::wait::{self, WaitQueue};

/// A virtio-mmio region described by the platform.
#[derive(Clone, Copy, Debug)]
pub struct MmioDesc {
    pub pa: usize,
    pub size: usize,
    /// Arch IRQ number (GIC INTID / GSI).
    pub irq: Option<u32>,
    pub edge: bool,
    pub active_low: bool,
    pub source: &'static str,
}

/// Discover devices and start the drivers.
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    let found = crate::arch::devices::virtio_mmio(cmdline);
    let mut n = 0;
    for d in &found {
        let Some(t) = virtio::mmio::MmioTransport::probe(d.pa, d.size) else {
            continue;
        };
        n += 1;
        let dev = virtio::Device {
            transport: alloc::boxed::Box::new(t),
            irq: d.irq.map(|irq| virtio::IrqLine {
                irq,
                edge: d.edge,
                active_low: d.active_low,
            }),
        };
        virtio::register_transport(dev, d.source);
    }
    n += pci::init(cmdline);
    if n == 0 {
        crate::kprintln!(
            "[patina] virtio: no devices ({} mmio region(s) described, no virtio-pci)",
            found.len()
        );
    }
    virtio::rng::init();
    virtio::console::init(cmdline);
    virtio::vsock::init();
    // virtio-net registers itself with `net::device` for the stack.
    virtio::net::init();
    let left = virtio::unclaimed();
    if left > 0 {
        crate::kprintln!("[patina] virtio: {} device(s) left undriven", left);
    }
}

/// A wake-up flag plus a wait queue: an IRQ handler (or any context)
/// [`signal`](Event::signal)s, one kernel thread [`wait`](Event::wait)s.
pub struct Event {
    inner: SpinLock<(bool, WaitQueue)>,
}

impl Default for Event {
    fn default() -> Self {
        Self::new()
    }
}

impl Event {
    pub const fn new() -> Event {
        Event {
            inner: SpinLock::new((false, WaitQueue::new())),
        }
    }

    /// Set the flag and wake the waiter. Any context, IRQs masked or not.
    pub fn signal(&self) {
        let mut g = self.inner.lock();
        g.0 = true;
        g.1.wake_all();
    }

    /// Block the calling kernel thread until signalled or until `deadline`
    /// (monotonic ns). Clears the flag. Returns true if signalled.
    pub fn wait(&self, deadline: Option<u64>) -> bool {
        let g = self.inner.lock();
        if g.0 {
            drop(g);
            return self.take();
        }
        let _ = wait::block(g, |s| &mut s.1, 0, 0, deadline);
        self.take()
    }

    fn take(&self) -> bool {
        core::mem::replace(&mut self.inner.lock().0, false)
    }
}

/// Spawn a kernel thread (once the scheduler runs it); keeps the `Arc`.
pub fn spawn(f: fn()) -> Arc<crate::task::Thread> {
    crate::task::spawn_kernel_thread(f)
}
