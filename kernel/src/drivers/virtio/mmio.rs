//! virtio-mmio transport (virtio 1.2 §4.2), legacy (version 1) and modern
//! (version 2) register layouts.

use super::Transport;
use crate::mm::PhysAddr;

pub const MAGIC: u32 = 0x7472_6976; // "virt"

const MAGIC_VALUE: usize = 0x000;
const VERSION: usize = 0x004;
const DEVICE_ID: usize = 0x008;
const VENDOR_ID: usize = 0x00c;
const DEVICE_FEATURES: usize = 0x010;
const DEVICE_FEATURES_SEL: usize = 0x014;
const DRIVER_FEATURES: usize = 0x020;
const DRIVER_FEATURES_SEL: usize = 0x024;
const GUEST_PAGE_SIZE: usize = 0x028; // legacy
const QUEUE_SEL: usize = 0x030;
const QUEUE_NUM_MAX: usize = 0x034;
const QUEUE_NUM: usize = 0x038;
const QUEUE_ALIGN: usize = 0x03c; // legacy
const QUEUE_PFN: usize = 0x040; // legacy
const QUEUE_READY: usize = 0x044;
const QUEUE_NOTIFY: usize = 0x050;
const INTERRUPT_STATUS: usize = 0x060;
const INTERRUPT_ACK: usize = 0x064;
const STATUS: usize = 0x070;
const QUEUE_DESC_LOW: usize = 0x080;
const QUEUE_DESC_HIGH: usize = 0x084;
const QUEUE_DRIVER_LOW: usize = 0x090;
const QUEUE_DRIVER_HIGH: usize = 0x094;
const QUEUE_DEVICE_LOW: usize = 0x0a0;
const QUEUE_DEVICE_HIGH: usize = 0x0a4;
const CONFIG_GENERATION: usize = 0x0fc;
const CONFIG: usize = 0x100;

/// A probed virtio-mmio register block.
pub struct MmioTransport {
    pa: usize,
    va: usize,
    version: u32,
    device_id: u32,
    vendor: u32,
}

impl MmioTransport {
    /// Map `[pa, pa+size)` and check for a virtio device. `None` if there
    /// is no virtio magic, an unknown version, or no device behind the
    /// transport (device id 0: QEMU `virt` exposes 32 empty transports).
    pub fn probe(pa: usize, size: usize) -> Option<MmioTransport> {
        let va = crate::arch::paging::map_device(PhysAddr(pa), size.max(0x200)).as_usize();
        let t = MmioTransport {
            pa,
            va,
            version: 0,
            device_id: 0,
            vendor: 0,
        };
        if t.r32(MAGIC_VALUE) != MAGIC {
            return None;
        }
        let version = t.r32(VERSION);
        let device_id = t.r32(DEVICE_ID);
        if !(version == 1 || version == 2) || device_id == 0 {
            return None;
        }
        Some(MmioTransport {
            version,
            device_id,
            vendor: t.r32(VENDOR_ID),
            ..t
        })
    }

    pub fn pa(&self) -> usize {
        self.pa
    }

    pub fn vendor(&self) -> u32 {
        self.vendor
    }

    fn r32(&self, off: usize) -> u32 {
        // SAFETY: `va` maps the device's register block (>= 0x200 bytes),
        // uncached; `off` is a register offset inside it.
        unsafe { ((self.va + off) as *const u32).read_volatile() }
    }

    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as in `r32`.
        unsafe { ((self.va + off) as *mut u32).write_volatile(v) }
    }
}

impl Transport for MmioTransport {
    fn device_type(&self) -> u32 {
        self.device_id
    }

    fn describe(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        write!(out, "mmio @ {:#x}", self.pa)
    }

    fn details(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        write!(
            out,
            "virtio-mmio v{}{}, vendor {:#x}",
            self.version,
            if self.version == 1 { " (legacy)" } else { "" },
            self.vendor
        )
    }

    fn modern(&self) -> bool {
        self.version == 2
    }

    fn status(&self) -> u32 {
        self.r32(STATUS)
    }

    fn set_status(&self, s: u32) {
        self.w32(STATUS, s);
        if s == 0 && self.version == 1 {
            // Legacy: the guest page size must be set before any queue.
            self.w32(GUEST_PAGE_SIZE, crate::mm::PAGE_SIZE as u32);
        }
    }

    fn device_features(&self) -> u64 {
        self.w32(DEVICE_FEATURES_SEL, 0);
        let lo = self.r32(DEVICE_FEATURES) as u64;
        self.w32(DEVICE_FEATURES_SEL, 1);
        let hi = self.r32(DEVICE_FEATURES) as u64;
        lo | (hi << 32)
    }

    fn set_driver_features(&self, f: u64) {
        self.w32(DRIVER_FEATURES_SEL, 0);
        self.w32(DRIVER_FEATURES, f as u32);
        self.w32(DRIVER_FEATURES_SEL, 1);
        self.w32(DRIVER_FEATURES, (f >> 32) as u32);
    }

    fn queue_max(&self, q: u16) -> u16 {
        self.w32(QUEUE_SEL, q as u32);
        if self.version == 2 && self.r32(QUEUE_READY) != 0 {
            return 0; // already in use
        }
        if self.version == 1 && self.r32(QUEUE_PFN) != 0 {
            return 0;
        }
        self.r32(QUEUE_NUM_MAX).min(u16::MAX as u32) as u16
    }

    fn queue_setup(&self, q: u16, size: u16, desc: u64, avail: u64, used: u64) {
        self.w32(QUEUE_SEL, q as u32);
        self.w32(QUEUE_NUM, size as u32);
        if self.version == 1 {
            // Legacy layout: avail follows the descriptors, used at the
            // next `QueueAlign` boundary (VirtQueue guarantees both).
            debug_assert_eq!(avail, desc + 16 * size as u64);
            let _ = used;
            self.w32(QUEUE_ALIGN, crate::mm::PAGE_SIZE as u32);
            self.w32(QUEUE_PFN, (desc >> crate::mm::PAGE_SHIFT) as u32);
        } else {
            self.w32(QUEUE_DESC_LOW, desc as u32);
            self.w32(QUEUE_DESC_HIGH, (desc >> 32) as u32);
            self.w32(QUEUE_DRIVER_LOW, avail as u32);
            self.w32(QUEUE_DRIVER_HIGH, (avail >> 32) as u32);
            self.w32(QUEUE_DEVICE_LOW, used as u32);
            self.w32(QUEUE_DEVICE_HIGH, (used >> 32) as u32);
            self.w32(QUEUE_READY, 1);
        }
    }

    fn notify(&self, q: u16) {
        self.w32(QUEUE_NOTIFY, q as u32);
    }

    fn ack_interrupt(&self) -> u32 {
        let s = self.r32(INTERRUPT_STATUS);
        if s != 0 {
            self.w32(INTERRUPT_ACK, s);
        }
        s
    }

    fn config_generation(&self) -> u32 {
        if self.version == 2 {
            self.r32(CONFIG_GENERATION)
        } else {
            0
        }
    }

    fn config_read8(&self, off: usize) -> u8 {
        // SAFETY: device-specific config space follows the registers at
        // 0x100 inside the mapped block; byte access is allowed.
        unsafe { ((self.va + CONFIG + off) as *const u8).read_volatile() }
    }

    fn config_read32(&self, off: usize) -> u32 {
        self.r32(CONFIG + off)
    }
}
