//! Platform discovery from the flattened device tree passed in x0.
//!
//! Extracts everything Phase 0 needs into a plain [`Platform`] value so the
//! rest of boot does not depend on the `fdt` crate's types.

use fdt::Fdt;
use fdt::node::FdtNode;

use super::power::Conduit;

pub const MAX_RANGES: usize = 32;
pub const MAX_UARTS: usize = 8;
/// virtio-mmio nodes recorded (QEMU `virt` has 32 transports).
pub const MAX_VIRTIO: usize = 64;

/// A `compatible = "virtio,mmio"` node.
#[derive(Clone, Copy, Debug)]
pub struct VirtioDesc {
    pub base: usize,
    pub size: usize,
    /// GIC INTID (SPI n = 32 + n, PPI n = 16 + n).
    pub intid: u32,
    /// `interrupts` flags say edge-triggered (IRQ_TYPE_EDGE_*).
    pub edge: bool,
}

/// Fixed-capacity list of physical ranges `(start, end)`.
pub struct Ranges {
    items: [(usize, usize); MAX_RANGES],
    len: usize,
}

impl Ranges {
    const fn new() -> Self {
        Self {
            items: [(0, 0); MAX_RANGES],
            len: 0,
        }
    }
    fn push(&mut self, start: usize, end: usize) {
        if self.len < MAX_RANGES && end > start {
            self.items[self.len] = (start, end);
            self.len += 1;
        }
    }
    pub fn as_slice(&self) -> &[(usize, usize)] {
        &self.items[..self.len]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UartKind {
    Pl011,
    Ns16550,
}

#[derive(Clone, Copy, Debug)]
pub struct UartDesc {
    pub kind: UartKind,
    pub compatible: &'static str,
    pub base: usize,
    pub size: usize,
    pub reg_shift: u32,
    pub reg_io_width: u32,
}

pub struct Platform {
    pub dtb_size: usize,
    pub ram: Ranges,
    /// `/memreserve/` entries and `/reserved-memory` children.
    pub reserved: Ranges,
    pub bootargs: &'static str,
    pub initrd: Option<(usize, usize)>,
    /// UART named by `/chosen/stdout-path`.
    pub stdout_uart: Option<UartDesc>,
    /// Every enabled UART we have a driver for, in DT order.
    pub uarts: [Option<UartDesc>; MAX_UARTS],
    pub psci: Option<Conduit>,
    /// Interrupt controller (`arm,gic-v3` or a GICv2 compatible).
    pub gic: Option<GicDesc>,
    /// INTID of the EL1 virtual timer (`arm,armv8-timer` `interrupts`, 3rd
    /// entry), if the DTB says.
    pub timer_virt_intid: Option<u32>,
    /// PSCI CPU_ON function id (`/psci`: fixed 0xC400_0003 for PSCI 0.2+,
    /// else the `cpu_on` property of a PSCI 0.1 node).
    pub psci_cpu_on: Option<u32>,
    /// MPIDR affinity values (`reg`) of the `/cpus/cpu@N` nodes that are
    /// enabled and use `enable-method = "psci"` (or name none), in DT order.
    pub cpus: [u64; MAX_DT_CPUS],
    pub ncpus: usize,
    /// virtio-mmio transports, in DT order.
    pub virtio: [Option<VirtioDesc>; MAX_VIRTIO],
}

/// CPU nodes recorded from the DTB (more are counted but ignored).
pub const MAX_DT_CPUS: usize = 64;
/// PSCI 0.2+ CPU_ON, SMC64 calling convention.
pub const PSCI_0_2_CPU_ON_64: u32 = 0xC400_0003;

/// GIC description from the DTB.
#[derive(Clone, Copy, Debug)]
pub struct GicDesc {
    /// 2 or 3.
    pub version: u32,
    /// Distributor (GICD) `(base, size)`.
    pub dist: (usize, usize),
    /// GICv3: redistributor region (all CPUs' GICR frames). GICv2: CPU
    /// interface (GICC).
    pub cpu: (usize, usize),
}

#[derive(Debug)]
pub enum DtbError {
    Parse(fdt::FdtError),
    NoMemory,
}

/// Parse the DTB at virtual address `va`.
///
/// # Safety
/// `va` must point at a readable DTB that stays mapped and unmodified for the
/// rest of the kernel's life (it is reserved in the memory map).
pub unsafe fn parse(va: usize) -> Result<Platform, DtbError> {
    // SAFETY: caller guarantees the mapping; from_ptr bounds reads by totalsize.
    let fdt: Fdt<'static> = unsafe { Fdt::from_ptr(va as *const u8) }.map_err(DtbError::Parse)?;

    let mut p = Platform {
        dtb_size: fdt.total_size(),
        ram: Ranges::new(),
        reserved: Ranges::new(),
        bootargs: "",
        initrd: None,
        stdout_uart: None,
        uarts: [None; MAX_UARTS],
        psci: None,
        gic: None,
        timer_virt_intid: None,
        psci_cpu_on: None,
        cpus: [0; MAX_DT_CPUS],
        ncpus: 0,
        virtio: [None; MAX_VIRTIO],
    };

    for node in fdt.all_nodes() {
        let is_memory = node.property("device_type").and_then(|p| p.as_str()) == Some("memory");
        if is_memory
            && !is_disabled(&node)
            && let Some(regs) = node.reg()
        {
            for r in regs {
                let s = r.starting_address as usize;
                p.ram.push(s, s + r.size.unwrap_or(0));
            }
        }
    }
    if p.ram.len == 0 {
        return Err(DtbError::NoMemory);
    }

    for r in fdt.memory_reservations() {
        let s = r.address() as usize;
        p.reserved.push(s, s + r.size());
    }
    if let Some(rm) = fdt.find_node("/reserved-memory") {
        for child in rm.children() {
            if let Some(regs) = child.reg() {
                for r in regs {
                    let s = r.starting_address as usize;
                    p.reserved.push(s, s + r.size.unwrap_or(0));
                }
            }
        }
    }

    let chosen = fdt.find_node("/chosen");
    if let Some(chosen) = chosen {
        p.bootargs = chosen
            .property("bootargs")
            .map(|pr| cstr(pr.value))
            .unwrap_or("");
        let start = chosen
            .property("linux,initrd-start")
            .and_then(|pr| be_uint(pr.value));
        let end = chosen
            .property("linux,initrd-end")
            .and_then(|pr| be_uint(pr.value));
        if let (Some(s), Some(e)) = (start, end)
            && e > s
        {
            p.initrd = Some((s, e));
        }
        // stdout-path = "<path-or-alias>[:options]"
        if let Some(path) = chosen.property("stdout-path").map(|pr| cstr(pr.value)) {
            let path = path.split(':').next().unwrap_or("");
            if !path.is_empty()
                && let Some(n) = fdt.find_node(path)
            {
                p.stdout_uart = uart_from_node(&n);
            }
        }
    }
    for (slot, u) in p.uarts.iter_mut().zip(
        fdt.all_nodes()
            .filter(|n| !is_disabled(n))
            .filter_map(|n| uart_from_node(&n)),
    ) {
        *slot = Some(u);
    }

    if let Some(psci) = fdt.find_node("/psci") {
        p.psci = match psci.property("method").map(|pr| cstr(pr.value)) {
            Some("smc") => Some(Conduit::Smc),
            Some("hvc") => Some(Conduit::Hvc),
            _ => None,
        };
        let v02 = psci
            .compatible()
            .map(|c| c.all().any(|c| c == "arm,psci-0.2" || c == "arm,psci-1.0"))
            .unwrap_or(false);
        p.psci_cpu_on = if v02 {
            Some(PSCI_0_2_CPU_ON_64)
        } else {
            psci.property("cpu_on")
                .and_then(|pr| be_uint(pr.value))
                .map(|v| v as u32)
                .or(Some(PSCI_0_2_CPU_ON_64))
        };
    }
    parse_cpus(&fdt, &mut p);
    let mut nv = 0;
    for n in fdt.all_nodes() {
        if nv < MAX_VIRTIO
            && !is_disabled(&n)
            && n.compatible()
                .is_some_and(|c| c.all().any(|c| c == "virtio,mmio"))
            && let Some(v) = virtio_from_node(&n)
        {
            p.virtio[nv] = Some(v);
            nv += 1;
        }
    }
    p.gic = gic_from_fdt(&fdt);
    p.timer_virt_intid = fdt
        .find_compatible(&["arm,armv8-timer", "arm,armv7-timer"])
        .and_then(|n| n.property("interrupts"))
        .and_then(|pr| {
            // Four <type number flags> triples: secure phys, non-secure phys,
            // virtual, hypervisor. PPI n is INTID 16 + n.
            let cell = |i: usize| {
                pr.value
                    .get(i * 4..i * 4 + 4)
                    .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            };
            match (cell(6)?, cell(7)?) {
                (1, n) => Some(16 + n),
                _ => None,
            }
        });
    Ok(p)
}

/// `/cpus/cpu@N`: `reg` is the MPIDR affinity in `#address-cells` (1 or 2)
/// cells of `/cpus`. Disabled CPUs and ones with a non-PSCI enable-method
/// (spin-table) are skipped.
fn parse_cpus(fdt: &Fdt<'static>, p: &mut Platform) {
    let Some(cpus) = fdt.find_node("/cpus") else {
        return;
    };
    let cells = cpus
        .property("#address-cells")
        .and_then(|pr| be_uint(pr.value))
        .unwrap_or(1);
    for n in cpus.children() {
        let is_cpu = n.property("device_type").map(|pr| cstr(pr.value)) == Some("cpu")
            || n.name.starts_with("cpu@");
        if !is_cpu || is_disabled(&n) {
            continue;
        }
        if let Some(m) = n.property("enable-method").map(|pr| cstr(pr.value))
            && m != "psci"
        {
            continue;
        }
        let Some(reg) = n.property("reg").map(|pr| pr.value) else {
            continue;
        };
        let mpidr = match (cells, reg.len()) {
            (1, l) if l >= 4 => u32::from_be_bytes([reg[0], reg[1], reg[2], reg[3]]) as u64,
            (2, l) if l >= 8 => u64::from_be_bytes(reg[..8].try_into().unwrap_or([0; 8])),
            _ => continue,
        };
        if p.ncpus < MAX_DT_CPUS {
            p.cpus[p.ncpus] = mpidr;
            p.ncpus += 1;
        }
    }
}

fn gic_from_fdt(fdt: &Fdt<'static>) -> Option<GicDesc> {
    let (node, version) = match fdt.find_compatible(&["arm,gic-v3"]) {
        Some(n) => (n, 3),
        None => (
            fdt.find_compatible(&[
                "arm,gic-400",
                "arm,cortex-a15-gic",
                "arm,cortex-a9-gic",
                "arm,cortex-a7-gic",
            ])?,
            2,
        ),
    };
    let mut regs = node.reg()?;
    let d = regs.next()?;
    let c = regs.next()?;
    Some(GicDesc {
        version,
        dist: (d.starting_address as usize, d.size.unwrap_or(0x10000)),
        cpu: (c.starting_address as usize, c.size.unwrap_or(0x20000)),
    })
}

/// `reg` + the first `interrupts` triple (GIC binding: `<type num flags>`,
/// type 0 = SPI, 1 = PPI; flags[3:0] 1/2 = edge, 4/8 = level).
fn virtio_from_node(n: &FdtNode<'_, 'static>) -> Option<VirtioDesc> {
    let reg = n.reg()?.next()?;
    let irq = n.property("interrupts")?.value;
    let cell = |i: usize| {
        irq.get(i * 4..i * 4 + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (ty, num, flags) = (cell(0)?, cell(1)?, cell(2).unwrap_or(4));
    let intid = match ty {
        0 => 32 + num,
        1 => 16 + num,
        _ => return None,
    };
    Some(VirtioDesc {
        base: reg.starting_address as usize,
        size: reg.size.unwrap_or(0x200),
        intid,
        edge: flags & 0x3 != 0,
    })
}

fn is_disabled(n: &FdtNode<'_, '_>) -> bool {
    matches!(n.property("status").map(|p| cstr(p.value)), Some(s) if s != "okay" && s != "ok")
}

fn uart_from_node(n: &FdtNode<'_, 'static>) -> Option<UartDesc> {
    let compat = n.compatible()?;
    let (kind, compatible) = compat.all().find_map(|c| match c {
        "arm,pl011" => Some((UartKind::Pl011, c)),
        "ns16550a" | "ns16550" | "snps,dw-apb-uart" => Some((UartKind::Ns16550, c)),
        _ => None,
    })?;
    let reg = n.reg()?.next()?;
    let u32_prop = |name: &str| {
        n.property(name)
            .and_then(|p| be_uint(p.value))
            .map(|v| v as u32)
    };
    Some(UartDesc {
        kind,
        compatible,
        base: reg.starting_address as usize,
        size: reg.size.unwrap_or(0x1000),
        reg_shift: u32_prop("reg-shift").unwrap_or(0),
        reg_io_width: u32_prop("reg-io-width").unwrap_or(1),
    })
}

/// Property value as a string, without the trailing NUL(s). Empty on bad UTF-8.
fn cstr(v: &[u8]) -> &str {
    let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
    core::str::from_utf8(&v[..end]).unwrap_or("")
}

/// Big-endian 1- or 2-cell integer property.
fn be_uint(v: &[u8]) -> Option<usize> {
    match v.len() {
        4 => Some(u32::from_be_bytes(v.try_into().ok()?) as usize),
        8 => Some(u64::from_be_bytes(v.try_into().ok()?) as usize),
        _ => None,
    }
}
