//! Platform discovery from the flattened device tree passed in x0.
//!
//! Extracts everything Phase 0 needs into a plain [`Platform`] value so the
//! rest of boot does not depend on the `fdt` crate's types.

use fdt::Fdt;
use fdt::node::FdtNode;

use super::power::Conduit;

pub const MAX_RANGES: usize = 32;
pub const MAX_UARTS: usize = 8;

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
    }
    Ok(p)
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
