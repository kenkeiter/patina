//! ACPI platform facts beyond the CPU list (Phase 3), gathered once in
//! `early_main` on the boot page tables (the tables may sit in pages the
//! frame allocator would later hand out): the MADT I/O APICs and interrupt
//! source overrides (`ioapic.rs`), the FADT sleep registers (`power.rs`)
//! the DSDT's virtio-mmio devices and `_S5` value (`devices.rs`) and the
//! MCFG's PCI ECAM regions (`devices::pci_hosts`, D70).
//!
//! RSDP sources, first that works: `hvm_start_info.rsdp_paddr`, the RSDP
//! `smp::enumerate` found by the BIOS-area scan (QEMU microvm/pc/q35,
//! Firecracker >= 1.8 at 0xe0000), fw_cfg `etc/acpi/*` (table pointers are
//! offsets into the blob). Without ACPI (older Firecracker) every field
//! stays empty and the users fall back to defaults.

use super::acpi::{self, DsdtInfo, Fadt, IrqInfo, Mcfg, Mem};
use super::smp;
use crate::sync::SpinLock;

pub struct Facts {
    pub source: &'static str,
    pub irq: Option<IrqInfo>,
    pub fadt: Option<Fadt>,
    pub dsdt: Option<DsdtInfo>,
    /// PCI Express ECAM regions (D70).
    pub mcfg: Option<Mcfg>,
}

static FACTS: SpinLock<Facts> = SpinLock::new(Facts {
    source: "none",
    irq: None,
    fadt: None,
    dsdt: None,
    mcfg: None,
});

fn gather(mem: &impl Mem, rsdp: u64, source: &'static str) -> bool {
    if acpi::rsdp_root(mem, rsdp).is_none() {
        return false;
    }
    let fadt = acpi::fadt(mem, rsdp);
    let dsdt = fadt.and_then(|f| acpi::dsdt_scan(mem, f.dsdt));
    let irq = acpi::madt_irq(mem, rsdp);
    let mcfg = acpi::mcfg(mem, rsdp);
    *FACTS.lock() = Facts {
        source,
        irq,
        fadt,
        dsdt,
        mcfg,
    };
    true
}

/// Gather the facts (boot CPU, boot tables, after `smp::enumerate`).
pub(super) fn init(start_info_rsdp: usize) {
    let ok = (start_info_rsdp != 0
        && gather(&acpi::Phys, start_info_rsdp as u64, "start_info rsdp"))
        || (smp::rsdp_addr() != 0 && gather(&acpi::Phys, smp::rsdp_addr(), "bios-area rsdp"))
        || smp::fwcfg_acpi().is_some_and(|(blob, r)| gather(&blob, r, "fw_cfg acpi tables"));
    let f = FACTS.lock();
    if !ok {
        drop(f);
        crate::kprintln!("[patina] acpi: no tables (ioapic defaults, no dsdt)");
        return;
    }
    crate::kprintln!(
        "[patina] acpi: {}: {} ioapic(s), {} irq override(s), fadt {} (flags {:#x}, pm1a_cnt {:#x}, sleep_ctl {:#x}), dsdt {} bytes, {} virtio-mmio device(s), _S5 {:?}",
        f.source,
        f.irq.map_or(0, |i| i.n_ioapics),
        f.irq.map_or(0, |i| i.n_isos),
        if f.fadt.is_some() { "found" } else { "missing" },
        f.fadt.map_or(0, |x| x.flags),
        f.fadt.map_or(0, |x| x.pm1a_cnt),
        f.fadt.map_or(0, |x| x.sleep_control.address),
        f.dsdt.map_or(0, |d| d.len),
        f.dsdt.map_or(0, |d| d.n_virtio),
        f.dsdt.and_then(|d| d.s5_typ),
    );
    match f.mcfg {
        Some(m) if m.n > 0 => {
            for &(base, seg, s, e) in &m.entries[..m.n] {
                crate::kprintln!(
                    "[patina] acpi: mcfg @ {:#x}: ecam {:#x} segment {} bus {}-{}",
                    m.addr,
                    base,
                    seg,
                    s,
                    e
                );
            }
        }
        _ => crate::kprintln!("[patina] acpi: no mcfg (no pci ecam)"),
    }
}

/// Run `f` on the gathered facts.
pub fn with<R>(f: impl FnOnce(&Facts) -> R) -> R {
    f(&FACTS.lock())
}
