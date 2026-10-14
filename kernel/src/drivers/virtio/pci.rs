//! virtio over PCI (virtio 1.2 §4.1), modern capabilities only (decision
//! D70).
//!
//! [`probe`] (called by `drivers::pci::init` for vendor 0x1af4, device
//! 0x1000-0x107f) walks the function's capabilities: the virtio vendor
//! capabilities (common / notify / ISR / device cfg, each `{bar, offset,
//! length}`) and MSI-X. Transitional devices (0x1000-0x103f, device type
//! from the subsystem id) also carry the modern capabilities; their legacy
//! I/O BAR is ignored. The BARs the capabilities name are assigned if
//! needed (`pci::BarAlloc`), mapped uncached, and memory decoding plus bus
//! mastering are enabled. A device whose host bridge (function 0 of the
//! first bus) has Apple's vendor id 0x106b is Virtualization.framework's:
//! [`Transport::apple_vz`] says so (and the discovery line `apple host`),
//! for the drivers' VZ workarounds (console.rs).
//!
//! Interrupts, in order of preference:
//! * MSI-X (x86_64 always; aarch64 through the GICv3 ITS): one message
//!   from `arch::irq::alloc_msi` written to table entry 0 and unmasked,
//!   MSI-X enabled, INTx disabled. Every queue and the config change use
//!   vector 0: `queue_msix_vector` is written in `queue_setup`,
//!   `msix_config` with `DRIVER_OK` (a device reset clears both), each
//!   read back (`0xffff` = rejected). The [`IrqLine`](super::IrqLine) the
//!   drivers get names the arch IRQ number the message was bound to, so
//!   `attach_irq` routes it like any other line.
//! * INTx (aarch64 without an ITS, or `patina.pci.irq=intx`): the
//!   interrupt pin through the DTB `interrupt-map` to a level-triggered
//!   GIC SPI; reading the ISR register in `ack_interrupt` deasserts it.
//! * Neither: the device is registered without an IRQ line (on x86_64,
//!   INTx would need the DSDT `_PRT`; not implemented).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU16, Ordering};

use super::{Device, INT_VRING, IrqLine, STATUS_DRIVER_OK, Transport};
use crate::drivers::pci::{self, BarAlloc, Bdf, Function, Host};
use crate::mm::PhysAddr;

pub const VENDOR: u16 = 0x1af4;
/// PCI vendor id of Apple's host bridge (Virtualization.framework: 0x106b:0x1a05).
const APPLE_VENDOR: u16 = 0x106b;

// virtio_pci_cap cfg_type.
const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_ISR: u8 = 3;
const CAP_DEVICE: u8 = 4;

// struct virtio_pci_common_cfg.
const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0c;
const MSIX_CONFIG: usize = 0x10;
const NUM_QUEUES: usize = 0x12;
const DEVICE_STATUS: usize = 0x14;
const CONFIG_GENERATION: usize = 0x15;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_MSIX_VECTOR: usize = 0x1a;
const QUEUE_ENABLE: usize = 0x1c;
const QUEUE_NOTIFY_OFF: usize = 0x1e;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;

/// No MSI-X vector / vector rejected.
pub const NO_VECTOR: u16 = 0xffff;

// MSI-X capability and table.
const MSIX_CTRL_ENABLE: u16 = 1 << 15;
const MSIX_CTRL_FUNC_MASK: u16 = 1 << 14;
const MSIX_ENTRY_CTRL_MASKED: u32 = 1;

/// A virtio capability: BAR index, offset in the BAR, length.
#[derive(Clone, Copy, Debug)]
struct Cap {
    bar: u8,
    off: u32,
    len: u32,
}

/// The modern virtio-pci register blocks of one function.
pub struct PciTransport {
    bdf: Bdf,
    segment: u16,
    device_type: u32,
    common: usize,
    notify: usize,
    notify_len: usize,
    notify_mult: u32,
    isr: usize,
    device: usize,
    device_len: usize,
    num_queues: u16,
    /// `queue_notify_off` per queue, read at `queue_setup`.
    notify_off: Vec<AtomicU16>,
    /// MSI-X vector 0 is routed: program it into the device.
    msix: bool,
    /// The host bridge is Apple's: the device is Virtualization.framework's.
    apple: bool,
}

impl PciTransport {
    fn r8(&self, off: usize) -> u8 {
        // SAFETY: `common` maps the common configuration structure (its
        // capability length was checked >= 0x38 at probe), uncached.
        unsafe { ((self.common + off) as *const u8).read_volatile() }
    }
    fn r16(&self, off: usize) -> u16 {
        // SAFETY: as in `r8`.
        unsafe { ((self.common + off) as *const u16).read_volatile() }
    }
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: as in `r8`.
        unsafe { ((self.common + off) as *const u32).read_volatile() }
    }
    fn w8(&self, off: usize, v: u8) {
        // SAFETY: as in `r8`.
        unsafe { ((self.common + off) as *mut u8).write_volatile(v) }
    }
    fn w16(&self, off: usize, v: u16) {
        // SAFETY: as in `r8`.
        unsafe { ((self.common + off) as *mut u16).write_volatile(v) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as in `r8`.
        unsafe { ((self.common + off) as *mut u32).write_volatile(v) }
    }
    /// 64-bit fields as two 32-bit writes, low half first (virtio 1.2
    /// §4.1.3.1 allows it; some devices only take 32-bit accesses).
    fn w64(&self, off: usize, v: u64) {
        self.w32(off, v as u32);
        self.w32(off + 4, (v >> 32) as u32);
    }

    /// Program `vector` into a vector register (`msix_config` or the
    /// selected queue's `queue_msix_vector`); false if the device refused.
    fn set_vector(&self, reg: usize, vector: u16) -> bool {
        self.w16(reg, vector);
        self.r16(reg) == vector
    }
}

impl Transport for PciTransport {
    fn device_type(&self) -> u32 {
        self.device_type
    }

    fn describe(&self, out: &mut dyn Write) -> core::fmt::Result {
        write!(
            out,
            "pci {:04x}:{:02x}:{:02x}.{}",
            self.segment, self.bdf.bus, self.bdf.dev, self.bdf.func
        )
    }

    fn details(&self, out: &mut dyn Write) -> core::fmt::Result {
        write!(out, "modern virtio-pci, vendor {:#x}", VENDOR)?;
        if self.apple {
            write!(out, ", apple host")?;
        }
        Ok(())
    }

    fn apple_vz(&self) -> bool {
        self.apple
    }

    fn irq_kind(&self) -> Option<&'static str> {
        Some(if self.msix { "msi-x" } else { "intx" })
    }

    fn modern(&self) -> bool {
        true
    }

    fn status(&self) -> u32 {
        self.r8(DEVICE_STATUS) as u32
    }

    fn set_status(&self, s: u32) {
        if s & STATUS_DRIVER_OK != 0
            && self.msix
            && !self.set_vector(MSIX_CONFIG, 0)
        {
            crate::kprintln!("[patina] virtio-pci: device refused msix_config vector 0");
        }
        self.w8(DEVICE_STATUS, s as u8);
        if s == 0 {
            // Reset completes when the status reads back 0 (§4.1.4.3.2).
            super::poll_until(100_000_000, || self.r8(DEVICE_STATUS) == 0);
        }
    }

    fn device_features(&self) -> u64 {
        self.w32(DEVICE_FEATURE_SELECT, 0);
        let lo = self.r32(DEVICE_FEATURE) as u64;
        self.w32(DEVICE_FEATURE_SELECT, 1);
        let hi = self.r32(DEVICE_FEATURE) as u64;
        lo | (hi << 32)
    }

    fn set_driver_features(&self, f: u64) {
        self.w32(DRIVER_FEATURE_SELECT, 0);
        self.w32(DRIVER_FEATURE, f as u32);
        self.w32(DRIVER_FEATURE_SELECT, 1);
        self.w32(DRIVER_FEATURE, (f >> 32) as u32);
    }

    fn queue_max(&self, q: u16) -> u16 {
        if q >= self.num_queues {
            return 0;
        }
        self.w16(QUEUE_SELECT, q);
        if self.r16(QUEUE_ENABLE) != 0 {
            return 0; // already in use
        }
        self.r16(QUEUE_SIZE)
    }

    fn queue_setup(&self, q: u16, size: u16, desc: u64, avail: u64, used: u64) {
        self.w16(QUEUE_SELECT, q);
        self.w16(QUEUE_SIZE, size);
        if self.msix && !self.set_vector(QUEUE_MSIX_VECTOR, 0) {
            crate::kprintln!(
                "[patina] virtio-pci: device refused msi-x vector 0 for queue {}",
                q
            );
        }
        self.w64(QUEUE_DESC, desc);
        self.w64(QUEUE_DRIVER, avail);
        self.w64(QUEUE_DEVICE, used);
        if let Some(n) = self.notify_off.get(q as usize) {
            n.store(self.r16(QUEUE_NOTIFY_OFF), Ordering::Relaxed);
        }
        self.w16(QUEUE_ENABLE, 1);
    }

    fn notify(&self, q: u16) {
        let off = self
            .notify_off
            .get(q as usize)
            .map_or(0, |n| n.load(Ordering::Relaxed)) as usize
            * self.notify_mult as usize;
        if off + 2 > self.notify_len {
            return;
        }
        // SAFETY: inside the mapped notification structure (checked);
        // without VIRTIO_F_NOTIFICATION_DATA the doorbell value is the
        // queue index, written as 16 bits.
        unsafe { ((self.notify + off) as *mut u16).write_volatile(q) }
    }

    fn ack_interrupt(&self) -> u32 {
        // SAFETY: `isr` maps the 1-byte ISR status; reading it clears it
        // and deasserts INTx.
        let s = unsafe { (self.isr as *const u8).read_volatile() } as u32;
        if self.msix && s == 0 {
            // MSI-X vectors are exclusive to this device, and devices need
            // not set the queue bit with MSI-X (Cloud Hypervisor does not).
            return INT_VRING;
        }
        s
    }

    fn config_generation(&self) -> u32 {
        self.r8(CONFIG_GENERATION) as u32
    }

    fn config_read8(&self, off: usize) -> u8 {
        if off >= self.device_len {
            return 0;
        }
        // SAFETY: inside the mapped device-specific configuration (checked).
        unsafe { ((self.device + off) as *const u8).read_volatile() }
    }

    fn config_read32(&self, off: usize) -> u32 {
        if off + 4 > self.device_len {
            return 0;
        }
        // SAFETY: as in `config_read8`.
        unsafe { ((self.device + off) as *const u32).read_volatile() }
    }
}

/// How interrupts are preferred (`patina.pci.irq=intx|msix`, default:
/// MSI-X when the platform can route it, else INTx).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrqPref {
    Auto,
    Intx,
}

/// Set up virtio-pci function `f` and register it. False if skipped.
pub fn probe(h: &Host, f: &mut Function, alloc: &mut BarAlloc, pref: IrqPref) -> bool {
    let bdf = f.bdf;
    let loc = |out: &mut alloc::string::String| {
        let _ = write!(
            out,
            "{:04x}:{:02x}:{:02x}.{}",
            h.desc.segment, bdf.bus, bdf.dev, bdf.func
        );
    };
    let mut name = alloc::string::String::new();
    loc(&mut name);
    let device_type = if f.device >= 0x1040 {
        (f.device - 0x1040) as u32
    } else {
        h.read16(bdf, pci::CFG_SUBSYS_ID) as u32
    };

    let (mut common, mut notify, mut isr, mut device) = (None, None, None, None);
    let mut notify_mult = 0;
    let mut msix_cap = None;
    for (id, p) in pci::capabilities(h, bdf) {
        match id {
            pci::CAP_VENDOR => {
                let cap = Cap {
                    bar: h.read8(bdf, p + 4),
                    off: h.read32(bdf, p + 8),
                    len: h.read32(bdf, p + 12),
                };
                if cap.bar > 5 || f.bars[cap.bar as usize].is_none() {
                    continue;
                }
                let slot = match h.read8(bdf, p + 3) {
                    CAP_COMMON => &mut common,
                    CAP_NOTIFY => {
                        if notify.is_none() {
                            notify_mult = h.read32(bdf, p + 16);
                        }
                        &mut notify
                    }
                    CAP_ISR => &mut isr,
                    CAP_DEVICE => &mut device,
                    _ => continue,
                };
                if slot.is_none() {
                    *slot = Some(cap);
                }
            }
            pci::CAP_MSIX => msix_cap = Some(p),
            _ => {}
        }
    }
    let (Some(common), Some(notify), Some(isr)) = (common, notify, isr) else {
        crate::kprintln!(
            "[patina] virtio: pci {} device-id {} has no modern virtio capabilities (legacy-only virtio-pci is not supported), skipped",
            name,
            device_type
        );
        return false;
    };
    if common.len < 0x38 {
        crate::kprintln!("[patina] virtio: pci {} common cfg too short, skipped", name);
        return false;
    }

    // MSI-X table location: (BIR, offset, entries).
    let msix_table = msix_cap.map(|p| {
        let t = h.read32(bdf, p + 4);
        ((t & 7) as u8, t & !7, (h.read16(bdf, p + 2) & 0x7ff) + 1)
    });

    // BARs we need: the capabilities' and the MSI-X table's.
    let mut need = [false; 6];
    for c in [Some(common), Some(notify), Some(isr), device]
        .into_iter()
        .flatten()
    {
        need[c.bar as usize] = true;
    }
    if let Some((bir, _, _)) = msix_table
        && (bir as usize) < 6
        && f.bars[bir as usize].is_some()
    {
        need[bir as usize] = true;
    }
    let mut va = [0usize; 6];
    for i in 0..6 {
        if !need[i] {
            continue;
        }
        if !alloc.assign(h, f, i) {
            crate::kprintln!(
                "[patina] virtio: pci {} BAR{} unassigned and no free window, skipped",
                name,
                i
            );
            return false;
        }
        let b = f.bars[i].expect("assigned BAR");
        let cpu = pci::bus_to_cpu(h, b.addr);
        va[i] = crate::arch::paging::map_device(PhysAddr(cpu as usize), b.size as usize).as_usize();
    }
    let within = |c: Cap| {
        f.bars[c.bar as usize].is_some_and(|b| c.off as u64 + c.len as u64 <= b.size)
    };
    if ![Some(common), Some(notify), Some(isr), device]
        .into_iter()
        .flatten()
        .all(within)
    {
        crate::kprintln!("[patina] virtio: pci {} capability outside its BAR, skipped", name);
        return false;
    }
    let at = |c: Cap| va[c.bar as usize] + c.off as usize;

    let mut cmd = h.read16(bdf, pci::CFG_COMMAND) | pci::CMD_MEMORY | pci::CMD_BUS_MASTER;
    h.write16(bdf, pci::CFG_COMMAND, cmd);

    // Interrupts.
    let mut irq = None;
    let mut msix = false;
    if pref == IrqPref::Auto
        && let (Some(p), Some((bir, toff, n))) = (msix_cap, msix_table)
        && (bir as usize) < 6
        && va[bir as usize] != 0
        && f.bars[bir as usize].is_some_and(|b| toff as u64 + 16 * n as u64 <= b.size)
        && let Some(m) = crate::arch::irq::alloc_msi(bdf.rid().wrapping_add(h.desc.msi_devid_offset))
    {
        let ctrl = h.read16(bdf, p + 2);
        // Enable with the function masked while the entry is written.
        h.write16(bdf, p + 2, ctrl | MSIX_CTRL_ENABLE | MSIX_CTRL_FUNC_MASK);
        let e = va[bir as usize] + toff as usize;
        // SAFETY: MSI-X table entry 0 (16 bytes) inside the mapped BAR
        // (bounds checked above): message address lo/hi, data, control.
        unsafe {
            (e as *mut u32).write_volatile(m.addr as u32);
            ((e + 4) as *mut u32).write_volatile((m.addr >> 32) as u32);
            ((e + 8) as *mut u32).write_volatile(m.data);
            let c = ((e + 12) as *const u32).read_volatile();
            ((e + 12) as *mut u32).write_volatile(c & !MSIX_ENTRY_CTRL_MASKED);
        }
        h.write16(bdf, p + 2, (ctrl | MSIX_CTRL_ENABLE) & !MSIX_CTRL_FUNC_MASK);
        cmd |= pci::CMD_INTX_DISABLE;
        h.write16(bdf, pci::CFG_COMMAND, cmd);
        msix = true;
        irq = Some(IrqLine {
            irq: m.irq,
            edge: true,
            active_low: false,
        });
    }
    if irq.is_none() {
        let pin = h.read8(bdf, pci::CFG_INT_PIN);
        if (1..=4).contains(&pin)
            && let Some(e) = h.desc.intx_lookup(bdf, pin)
        {
            irq = Some(IrqLine {
                irq: e.irq,
                edge: e.edge,
                active_low: false,
            });
        }
    }
    if irq.is_none() {
        crate::kprintln!(
            "[patina] virtio: pci {} device-id {}: no usable interrupt (msi-x {}, intx pin {}); registered without one",
            name,
            device_type,
            if msix_cap.is_some() { "present but not routable" } else { "absent" },
            h.read8(bdf, pci::CFG_INT_PIN)
        );
    }

    let t = PciTransport {
        bdf,
        segment: h.desc.segment,
        device_type,
        common: at(common),
        notify: at(notify),
        notify_len: notify.len as usize,
        notify_mult,
        isr: at(isr),
        device: device.map_or(0, at),
        device_len: device.map_or(0, |d| d.len as usize),
        num_queues: 0,
        notify_off: Vec::new(),
        msix,
        apple: h.read16(
            Bdf {
                bus: h.desc.bus_start,
                dev: 0,
                func: 0,
            },
            pci::CFG_VENDOR,
        ) == APPLE_VENDOR,
    };
    let nq = t.r16(NUM_QUEUES);
    let t = PciTransport {
        num_queues: nq,
        notify_off: (0..nq).map(|_| AtomicU16::new(0)).collect(),
        ..t
    };
    super::register_transport(
        Device {
            transport: Box::new(t),
            irq,
        },
        h.desc.source,
    );
    true
}
