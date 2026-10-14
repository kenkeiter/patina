//! GICv2m MSI frame (`arm,gic-v2m-frame`, Phase 4, D70): MSIs as SPIs.
//!
//! A device write of SPI INTID `n` to `MSI_SETSPI_NS` (frame + 0x40) sets
//! SPI `n` pending (edge). The frame's SPI range comes from `MSI_TYPER`
//! (base [25:16], count [9:0]) unless the DTB overrides it
//! (`arm,msi-base-spi` / `arm,msi-num-spis`, as Cloud Hypervisor's MSHV
//! backend does). QEMU `virt` under hvf has a v2m frame instead of an ITS;
//! Cloud Hypervisor on MSHV too.

use super::paging::map_device;
use crate::mm::PhysAddr;
use crate::sync::SpinLock;

const MSI_TYPER: usize = 0x008;
const MSI_SETSPI_NS: usize = 0x040;

/// DTB description: frame `(base, size)` and the optional SPI override
/// `(first INTID, count)`.
#[derive(Clone, Copy, Debug)]
pub struct V2mDesc {
    pub base: usize,
    pub size: usize,
    pub spis: Option<(u32, u32)>,
}

struct State {
    desc: Option<V2mDesc>,
    /// (next INTID, end INTID) once the frame was read.
    range: Option<(u32, u32)>,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    desc: None,
    range: None,
});

pub(super) fn set_desc(d: Option<V2mDesc>) {
    STATE.lock().desc = d;
}

/// Take the next SPI of the frame: (INTID, MSI message). `None` without a
/// frame or when its SPIs are used up.
pub fn alloc() -> Option<crate::drivers::pci::MsiMsg> {
    let mut s = STATE.lock();
    let d = s.desc?;
    if s.range.is_none() {
        let (first, n) = match d.spis {
            Some(r) => r,
            None => {
                let va = map_device(PhysAddr(d.base), d.size.max(0x1000)).as_usize();
                // SAFETY: MSI_TYPER of the v2m frame just mapped (uncached).
                let t = unsafe { ((va + MSI_TYPER) as *const u32).read_volatile() };
                ((t >> 16) & 0x3ff, t & 0x3ff)
            }
        };
        crate::kprintln!(
            "[patina] gic: v2m msi frame @ {:#x}, spis {}-{}",
            d.base,
            first,
            (first + n).saturating_sub(1)
        );
        s.range = Some((first, first + n));
    }
    let (next, end) = s.range?;
    if next >= end || next < 32 {
        return None;
    }
    s.range = Some((next + 1, end));
    Some(crate::drivers::pci::MsiMsg {
        irq: next,
        addr: (d.base + MSI_SETSPI_NS) as u64,
        data: next,
    })
}
