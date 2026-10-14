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
//! PCI host bridges ([`pci_hosts`], D70): one per ACPI MCFG entry (QEMU
//! q35: ECAM at 0xb0000000, buses 0-255; Cloud Hypervisor: 0xe8000000,
//! bus 0 per segment). Without an MCFG but with a PC-style platform
//! (QEMU `pc`: a DSDT and the legacy I/O APIC), bus 0 through the
//! 0xCF8/0xCFC port mechanism. Firmware (SeaBIOS) or the VMM assigns the
//! BARs, so no memory windows are described.

use alloc::vec::Vec;

use super::platform;
use crate::drivers::MmioDesc;
use crate::drivers::pci::PciHostDesc;
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

/// PCI host bridges the platform describes (see the module docs).
pub fn pci_hosts() -> Vec<PciHostDesc> {
    let (mcfg, legacy_pc) = platform::with(|f| {
        (
            f.mcfg,
            f.fadt
                .is_some_and(|x| x.flags & super::acpi::FADT_HW_REDUCED == 0),
        )
    });
    let mut out = Vec::new();
    if let Some(m) = mcfg {
        for &(base, segment, start, end) in &m.entries[..m.n] {
            let mut d = PciHostDesc::empty("ecam acpi mcfg");
            // The MCFG base addresses bus 0; ours addresses `bus_start`.
            d.ecam = base + ((start as u64) << 20);
            d.segment = segment;
            d.bus_start = start;
            d.bus_end = end;
            out.push(d);
        }
    } else if legacy_pc && port_cf8_present() {
        let mut d = PciHostDesc::empty("port 0xcf8 config");
        d.bus_end = 0xff;
        out.push(d);
    }
    out
}

/// Configuration mechanism #1 answers: CONFIG_ADDRESS reads back the
/// enable bit written to it (the classic probe).
fn port_cf8_present() -> bool {
    use super::port::{inl, outl};
    // SAFETY: 0xCF8 is CONFIG_ADDRESS on PC-compatible platforms (checked
    // by the caller: a full, non-hardware-reduced FADT); writing it only
    // selects a register. The old value is restored.
    unsafe {
        let old = inl(0xcf8);
        outl(0xcf8, 0x8000_0000);
        let v = inl(0xcf8);
        outl(0xcf8, old);
        v == 0x8000_0000
    }
}
