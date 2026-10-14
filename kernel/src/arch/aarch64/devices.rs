//! Device discovery for the arch-neutral drivers: virtio-mmio transports
//! from the DTB (`compatible = "virtio,mmio"`), recorded at boot.

use alloc::vec::Vec;

use super::dtb::{MAX_VIRTIO, Platform, VirtioDesc};
use crate::drivers::MmioDesc;
use crate::sync::SpinLock;

static VIRTIO: SpinLock<([Option<VirtioDesc>; MAX_VIRTIO], bool)> =
    SpinLock::new(([None; MAX_VIRTIO], false));

/// Record the DTB's virtio-mmio nodes (boot, before the heap).
pub(super) fn record(p: &Platform) {
    let mut v = VIRTIO.lock();
    v.0 = p.virtio;
    v.1 = true;
}

/// virtio-mmio transports the platform describes (DTB order). The kernel
/// command line is not consulted on aarch64: every VMM we target (QEMU
/// `virt`, Firecracker, Apple VZ) lists them in the DTB.
pub fn virtio_mmio(_cmdline: &patina_cmdline::Cmdline) -> Vec<MmioDesc> {
    let v = VIRTIO.lock();
    v.0.iter()
        .flatten()
        .map(|d| MmioDesc {
            pa: d.base,
            size: d.size,
            irq: Some(d.intid),
            edge: d.edge,
            active_low: false,
            source: "dtb",
        })
        .collect()
}
