//! Just enough ACPI to enumerate CPUs: find the RSDP, walk the RSDT/XSDT
//! to the MADT ("APIC") and collect the processor Local APIC (type 0) and
//! Local x2APIC (type 9) entries. Also the MP-table (Intel MultiProcessor
//! Spec 1.4) processor entries, the fallback for VMMs without ACPI (older
//! Firecracker).
//!
//! Tables are read through a [`Mem`]: guest physical memory (the boot
//! direct map of the low 4 GiB, so this runs before the final page tables
//! and before the frame allocator can hand out the pages the tables live
//! in), or a blob fetched from fw_cfg, where table "addresses" are the
//! not-yet-relocated offsets into `etc/acpi/tables` and the checksums are
//! still unset (the firmware's table loader fills both in).

use super::PHYS_OFFSET;
use super::cpu::MAX_CPUS;

/// Source of table bytes.
pub trait Mem {
    /// Copy `buf.len()` bytes at `addr`; false if out of range.
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;
    /// Whether checksums are meaningful.
    fn checksums(&self) -> bool;
}

/// Guest physical memory below 4 GiB through the direct map. Only valid on
/// the boot page tables (which map all of the low 4 GiB at `PHYS_OFFSET`).
pub struct Phys;

impl Mem for Phys {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        if addr == 0
            || addr
                .checked_add(buf.len() as u64)
                .is_none_or(|e| e > 1 << 32)
        {
            return false;
        }
        for (i, b) in buf.iter_mut().enumerate() {
            // SAFETY: inside the boot direct map of the low 4 GiB.
            *b = unsafe { ((PHYS_OFFSET + addr as usize + i) as *const u8).read_volatile() };
        }
        true
    }
    fn checksums(&self) -> bool {
        true
    }
}

/// An in-memory copy of fw_cfg's `etc/acpi/tables`.
pub struct Blob<'a>(pub &'a [u8]);

impl Mem for Blob<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let src = usize::try_from(addr)
            .ok()
            .and_then(|a| Some(a..a.checked_add(buf.len())?))
            .and_then(|r| self.0.get(r));
        match src {
            Some(s) => {
                buf.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn checksums(&self) -> bool {
        false
    }
}

/// Enumerated CPUs: APIC ids of the enabled processors, in table order.
pub struct CpuList {
    pub ids: [u32; MAX_CPUS],
    pub n: usize,
    /// Entries marked disabled / not present (not started).
    pub disabled: usize,
    /// Enabled entries beyond `MAX_CPUS` (dropped).
    pub dropped: usize,
}

impl CpuList {
    pub const fn new() -> Self {
        Self {
            ids: [0; MAX_CPUS],
            n: 0,
            disabled: 0,
            dropped: 0,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    fn push(&mut self, id: u32, enabled: bool) {
        if !enabled {
            self.disabled += 1;
            return;
        }
        if self.ids[..self.n].contains(&id) {
            return; // the same CPU listed as both type 0 and type 9
        }
        if self.n == MAX_CPUS {
            self.dropped += 1;
            return;
        }
        self.ids[self.n] = id;
        self.n += 1;
    }
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

/// Checksum of `len` bytes at `addr` (streamed); None if unreadable.
fn sum_range(mem: &impl Mem, addr: u64, len: usize) -> Option<u8> {
    let mut s = 0u8;
    let mut off = 0;
    let mut chunk = [0u8; 64];
    while off < len {
        let k = (len - off).min(chunk.len());
        if !mem.read(addr + off as u64, &mut chunk[..k]) {
            return None;
        }
        s = s.wrapping_add(sum(&chunk[..k]));
        off += k;
    }
    Some(s)
}

/// Validate an RSDP at `addr`: (address of the RSDT or XSDT, entry size).
pub fn rsdp_root(mem: &impl Mem, addr: u64) -> Option<(u64, usize)> {
    let mut b = [0u8; 36];
    if !mem.read(addr, &mut b[..20]) || &b[..8] != b"RSD PTR " {
        return None;
    }
    if mem.checksums() && sum(&b[..20]) != 0 {
        return None;
    }
    let rev = b[15];
    if rev >= 2 && mem.read(addr, &mut b) {
        let xsdt = le64(&b[24..]);
        if xsdt != 0 && (!mem.checksums() || sum(&b) == 0) {
            return Some((xsdt, 8));
        }
    }
    let rsdt = le32(&b[16..]) as u64;
    (rsdt != 0).then_some((rsdt, 4))
}

/// Scan `[start, end)` (16-byte steps) for a valid RSDP.
pub fn scan_rsdp(start: u64, end: u64) -> Option<u64> {
    let mut a = start;
    while a + 20 <= end {
        let mut sig = [0u8; 8];
        if Phys.read(a, &mut sig) && &sig == b"RSD PTR " && rsdp_root(&Phys, a).is_some() {
            return Some(a);
        }
        a += 16;
    }
    None
}

/// Find the MADT through the RSDP at `rsdp` and add its CPUs to `out`.
/// Returns the MADT's address.
pub fn madt_cpus(mem: &impl Mem, rsdp: u64, out: &mut CpuList) -> Option<u64> {
    let (root, esz) = rsdp_root(mem, rsdp)?;
    let mut hdr = [0u8; 36];
    if !mem.read(root, &mut hdr) || !(&hdr[..4] == b"RSDT" || &hdr[..4] == b"XSDT") {
        return None;
    }
    let len = le32(&hdr[4..]) as usize;
    if !(36..=1 << 16).contains(&len) {
        return None;
    }
    let mut madt = None;
    for i in 0..(len - 36) / esz {
        let mut e = [0u8; 8];
        if !mem.read(root + 36 + (i * esz) as u64, &mut e[..esz]) {
            return None;
        }
        let t = if esz == 8 { le64(&e) } else { le32(&e) as u64 };
        let mut sig = [0u8; 4];
        if mem.read(t, &mut sig) && &sig == b"APIC" {
            madt = Some(t);
            break;
        }
    }
    let madt = madt?;
    if !mem.read(madt, &mut hdr) {
        return None;
    }
    let len = le32(&hdr[4..]) as usize;
    if !(44..=1 << 20).contains(&len) {
        return None;
    }
    if mem.checksums() && sum_range(mem, madt, len)? != 0 {
        return None;
    }
    // Entries follow the 36-byte header + LAPIC address + flags.
    let mut off = 44;
    let before = out.n;
    while off + 2 <= len {
        let mut e = [0u8; 16];
        if !mem.read(madt + off as u64, &mut e[..2]) {
            break;
        }
        let (ty, elen) = (e[0], e[1] as usize);
        if elen < 2 || off + elen > len {
            break;
        }
        match ty {
            // Processor Local APIC: uid u8, apic id u8, flags u32 (bit 0
            // enabled; bit 1 online-capable = hot-pluggable, not started).
            0 if elen >= 8 && mem.read(madt + off as u64, &mut e[..8]) => {
                out.push(e[3] as u32, le32(&e[4..]) & 1 != 0);
            }
            // Processor Local x2APIC: reserved u16, x2apic id u32, flags u32, uid u32.
            9 if elen >= 16 && mem.read(madt + off as u64, &mut e[..16]) => {
                out.push(le32(&e[4..]), le32(&e[8..]) & 1 != 0);
            }
            _ => {}
        }
        off += elen;
    }
    (out.n > before).then_some(madt)
}

/// Scan `[start, end)` for an MP floating pointer ("_MP_"); returns the
/// physical address of its configuration table ("PCMP").
pub fn scan_mp(start: u64, end: u64) -> Option<(u64, u64)> {
    let mut a = start;
    while a + 16 <= end {
        let mut b = [0u8; 16];
        if Phys.read(a, &mut b) && &b[..4] == b"_MP_" && b[8] == 1 && sum(&b) == 0 {
            let cfg = le32(&b[4..]) as u64;
            if cfg != 0 {
                return Some((a, cfg));
            }
        }
        a += 16;
    }
    None
}

/// Add the processor entries of the MP configuration table at `cfg`.
pub fn mp_cpus(cfg: u64, out: &mut CpuList) -> bool {
    let mut h = [0u8; 44];
    if !Phys.read(cfg, &mut h) || &h[..4] != b"PCMP" {
        return false;
    }
    let len = u16::from_le_bytes([h[4], h[5]]) as usize;
    let count = u16::from_le_bytes([h[34], h[35]]) as usize;
    if len < 44 || sum_range(&Phys, cfg, len) != Some(0) {
        return false;
    }
    let before = out.n;
    let mut off = 44;
    for _ in 0..count {
        let mut e = [0u8; 20];
        if off >= len || !Phys.read(cfg + off as u64, &mut e[..1]) {
            break;
        }
        if e[0] == 0 {
            // Processor: type, lapic id, lapic version, flags (bit 0 EN, bit 1 BP), ...
            if !Phys.read(cfg + off as u64, &mut e) {
                break;
            }
            out.push(e[1] as u32, e[3] & 1 != 0);
            off += 20;
        } else if e[0] <= 4 {
            off += 8;
        } else {
            break; // unknown entry type: length unknown
        }
    }
    out.n > before
}
