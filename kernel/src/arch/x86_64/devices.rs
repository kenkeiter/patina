//! Device discovery for the arch-neutral drivers: virtio-mmio transports.
//!
//! Sources, merged in this order and de-duplicated by base address:
//! 1. `virtio_mmio.device=<size>@<base>:<gsi>[:<id>]` kernel parameters
//!    (QEMU microvm with `acpi=off`, Firecracker: one per device, GSIs
//!    from 5). Edge-triggered, active-high, as Linux treats them.
//! 2. The DSDT's `LNRO0005` devices (`platform.rs` byte scan): QEMU
//!    microvm with ACPI (the CLI's default) and Firecracker >= 1.8, with
//!    the trigger/polarity of their `Interrupt` descriptor.
//! 3. Only if both found nothing and there is no DSDT at all (pc/q35
//!    have one and something else may live in that window): probing QEMU microvm's window,
//!    0xFEB00000 + 512 * n for n < 32, for the virtio magic. The GSI is a
//!    guess: microvm without the second I/O APIC numbers slot n as GSI
//!    5 + n (as seen with `acpi=off`: slot 7 at 0xfeb00e00 = GSI 12).
//!
//! virtio-pci (Cloud Hypervisor, QEMU q35) is not discovered here; that
//! is Phase 4/5 (ECAM from the MCFG) and plugs into
//! `drivers::virtio::register_transport` the same way.

use alloc::vec::Vec;

use super::platform;
use crate::drivers::MmioDesc;
use crate::mm::PhysAddr;

const PROBE_BASE: usize = 0xfeb0_0000;
const PROBE_STRIDE: usize = 0x200;
const PROBE_SLOTS: usize = 32;
const PROBE_GSI_BASE: u32 = 5;
const VIRTIO_MAGIC: u32 = 0x7472_6976;

/// virtio-mmio transports the platform describes (see the module docs).
pub fn virtio_mmio(cmdline: &patina_cmdline::Cmdline) -> Vec<MmioDesc> {
    let mut out: Vec<MmioDesc> = Vec::new();
    for d in &cmdline.virtio_mmio {
        if out.iter().any(|o| o.pa as u64 == d.base) {
            continue;
        }
        out.push(MmioDesc {
            pa: d.base as usize,
            size: d.size.unwrap_or(0x200) as usize,
            irq: Some(d.irq),
            edge: true,
            active_low: false,
            source: "cmdline",
        });
    }
    let have_dsdt = platform::with(|f| {
        if let Some(ds) = f.dsdt {
            for v in &ds.virtio[..ds.n_virtio] {
                if out.iter().any(|o| o.pa as u64 == v.base) {
                    continue;
                }
                out.push(MmioDesc {
                    pa: v.base as usize,
                    size: v.size as usize,
                    irq: Some(v.gsi),
                    edge: v.edge,
                    active_low: v.active_low,
                    source: "acpi dsdt",
                });
            }
        }
        f.dsdt.is_some()
    });
    if out.is_empty() && !have_dsdt {
        let va = crate::arch::paging::map_device(PhysAddr(PROBE_BASE), PROBE_STRIDE * PROBE_SLOTS)
            .as_usize();
        for n in 0..PROBE_SLOTS {
            // SAFETY: inside the uncached mapping just made; a missing
            // device reads as 0 or all-ones, never faults (QEMU
            // unassigned-memory semantics; real hardware has nothing here).
            let magic = unsafe { ((va + n * PROBE_STRIDE) as *const u32).read_volatile() };
            if magic == VIRTIO_MAGIC {
                out.push(MmioDesc {
                    pa: PROBE_BASE + n * PROBE_STRIDE,
                    size: PROBE_STRIDE,
                    irq: Some(PROBE_GSI_BASE + n as u32),
                    edge: true,
                    active_low: false,
                    source: "probe, irq guessed",
                });
            }
        }
    }
    out
}
