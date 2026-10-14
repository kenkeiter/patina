//! PVH `hvm_start_info` (xen/include/public/arch-x86/hvm/start_info.h).
//!
//! Read through the direct map (the boot tables map the low 4 GiB at
//! `PHYS_OFFSET`; VMMs place start_info, the memory map, the module list and
//! the command line below 4 GiB). Everything needed later is copied into a
//! plain [`StartInfo`] value; the command line is copied into a static buffer
//! because the heap does not exist yet.

use core::cell::UnsafeCell;
use core::mem::size_of;

use super::PHYS_OFFSET;

pub const MAGIC: u32 = 0x336e_c578;

#[repr(C)]
#[derive(Clone, Copy)]
struct RawStartInfo {
    magic: u32,
    version: u32,
    flags: u32,
    nr_modules: u32,
    modlist_paddr: u64,
    cmdline_paddr: u64,
    rsdp_paddr: u64,
    // version >= 1
    memmap_paddr: u64,
    memmap_entries: u32,
    _reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawModlistEntry {
    paddr: u64,
    size: u64,
    cmdline_paddr: u64,
    _reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemmapEntry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
    _reserved: u32,
}

pub const MEMMAP_RAM: u32 = 1;
pub const MEMMAP_RESERVED: u32 = 2;
pub const MEMMAP_ACPI: u32 = 3;
pub const MEMMAP_NVS: u32 = 4;
pub const MEMMAP_UNUSABLE: u32 = 5;
pub const MEMMAP_DISABLED: u32 = 6;
pub const MEMMAP_PMEM: u32 = 7;

pub fn memmap_kind_name(kind: u32) -> &'static str {
    match kind {
        MEMMAP_RAM => "ram",
        MEMMAP_RESERVED => "reserved",
        MEMMAP_ACPI => "acpi",
        MEMMAP_NVS => "nvs",
        MEMMAP_UNUSABLE => "unusable",
        MEMMAP_DISABLED => "disabled",
        MEMMAP_PMEM => "pmem",
        _ => "?",
    }
}

pub const MAX_MEMMAP: usize = 64;
const CMDLINE_MAX: usize = 4096;

pub struct StartInfo {
    pub pa: usize,
    pub version: u32,
    pub flags: u32,
    pub nr_modules: u32,
    /// Physical range `(start, end)` of the module list itself.
    pub modlist: Option<(usize, usize)>,
    /// Module 0 (the initrd), `(start, end)`.
    pub initrd: Option<(usize, usize)>,
    pub cmdline_pa: usize,
    /// Length of the command line in guest memory including the NUL.
    pub cmdline_len: usize,
    pub cmdline: &'static str,
    /// True if the command line did not fit in the static buffer.
    pub cmdline_truncated: bool,
    pub rsdp: usize,
    pub memmap_pa: usize,
    pub memmap: [MemmapEntry; MAX_MEMMAP],
    pub memmap_len: usize,
    /// Entries reported by the VMM (may exceed `MAX_MEMMAP`).
    pub memmap_entries: usize,
}

impl StartInfo {
    pub fn memmap(&self) -> &[MemmapEntry] {
        &self.memmap[..self.memmap_len]
    }
}

#[derive(Debug)]
pub enum Error {
    /// start_info address is 0 or not below 4 GiB.
    BadAddress(usize),
    BadMagic(u32),
    /// A referenced structure lies outside the boot direct map (>= 4 GiB).
    Unreachable(&'static str, u64),
}

struct CmdlineBuf(UnsafeCell<[u8; CMDLINE_MAX]>);
// SAFETY: written once by the boot CPU before any other reader exists.
unsafe impl Sync for CmdlineBuf {}
static CMDLINE: CmdlineBuf = CmdlineBuf(UnsafeCell::new([0; CMDLINE_MAX]));

const LOW_4G: u64 = 1 << 32;

fn reachable(what: &'static str, pa: u64, len: u64) -> Result<usize, Error> {
    if pa.checked_add(len).is_none_or(|e| e > LOW_4G) {
        return Err(Error::Unreachable(what, pa));
    }
    Ok(PHYS_OFFSET + pa as usize)
}

/// Parse the start_info at physical address `pa`.
///
/// # Safety
/// Must run on the boot tables (direct map of the low 4 GiB present), once.
pub unsafe fn parse(pa: usize) -> Result<StartInfo, Error> {
    if pa == 0 || pa as u64 + size_of::<RawStartInfo>() as u64 > LOW_4G {
        return Err(Error::BadAddress(pa));
    }
    // SAFETY: inside the boot direct map; the VMM provided the structure.
    let raw = unsafe { ((PHYS_OFFSET + pa) as *const RawStartInfo).read_unaligned() };
    if raw.magic != MAGIC {
        return Err(Error::BadMagic(raw.magic));
    }
    let mut si = StartInfo {
        pa,
        version: raw.version,
        flags: raw.flags,
        nr_modules: raw.nr_modules,
        modlist: None,
        initrd: None,
        cmdline_pa: raw.cmdline_paddr as usize,
        cmdline_len: 0,
        cmdline: "",
        cmdline_truncated: false,
        rsdp: raw.rsdp_paddr as usize,
        memmap_pa: 0,
        memmap: [MemmapEntry {
            addr: 0,
            size: 0,
            kind: 0,
            _reserved: 0,
        }; MAX_MEMMAP],
        memmap_len: 0,
        memmap_entries: 0,
    };

    // ---- Modules: module 0 is the initrd.
    if raw.nr_modules > 0 && raw.modlist_paddr != 0 {
        let len = raw.nr_modules as u64 * size_of::<RawModlistEntry>() as u64;
        let va = reachable("module list", raw.modlist_paddr, len)?;
        si.modlist = Some((
            raw.modlist_paddr as usize,
            (raw.modlist_paddr + len) as usize,
        ));
        // SAFETY: inside the boot direct map.
        let m0 = unsafe { (va as *const RawModlistEntry).read_unaligned() };
        if m0.size > 0 {
            si.initrd = Some((m0.paddr as usize, (m0.paddr + m0.size) as usize));
        }
    }

    // ---- Command line (NUL-terminated).
    if raw.cmdline_paddr != 0 {
        let va = reachable("cmdline", raw.cmdline_paddr, 1)?;
        // SAFETY: single boot-time writer; no reader exists yet.
        let buf = unsafe { &mut *CMDLINE.0.get() };
        let mut n = 0;
        loop {
            if raw.cmdline_paddr + n as u64 >= LOW_4G {
                break;
            }
            // SAFETY: inside the boot direct map.
            let b = unsafe { ((va + n) as *const u8).read_volatile() };
            if b == 0 {
                si.cmdline_len = n + 1;
                break;
            }
            if n < CMDLINE_MAX {
                buf[n] = b;
            } else {
                si.cmdline_truncated = true;
            }
            n += 1;
            if n > 1 << 20 {
                break; // not NUL-terminated within 1 MiB: give up
            }
        }
        let used = n.min(CMDLINE_MAX);
        let bytes: &'static [u8] = &buf[..used];
        si.cmdline = match core::str::from_utf8(bytes) {
            Ok(s) => s,
            // SAFETY: valid_up_to() marks a valid UTF-8 prefix.
            Err(e) => unsafe { core::str::from_utf8_unchecked(&bytes[..e.valid_up_to()]) },
        };
    }

    // ---- Memory map (start_info version >= 1).
    if raw.version >= 1 && raw.memmap_paddr != 0 && raw.memmap_entries > 0 {
        let n = raw.memmap_entries as usize;
        let len = (n * size_of::<MemmapEntry>()) as u64;
        let va = reachable("memmap", raw.memmap_paddr, len)?;
        si.memmap_pa = raw.memmap_paddr as usize;
        si.memmap_entries = n;
        for i in 0..n.min(MAX_MEMMAP) {
            // SAFETY: inside the boot direct map.
            si.memmap[i] = unsafe { (va as *const MemmapEntry).add(i).read_unaligned() };
        }
        si.memmap_len = n.min(MAX_MEMMAP);
    }
    Ok(si)
}
