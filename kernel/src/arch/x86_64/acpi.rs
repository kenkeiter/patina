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

// ---------------------------------------------------------------------------
// Phase 3: interrupt routing (MADT I/O APIC + interrupt source overrides),
// the FADT (DSDT pointer, sleep registers) and a byte scan of the DSDT for
// virtio-mmio devices and the `_S5` sleep type.

/// Address and length of the table with signature `sig` in the RSDT/XSDT.
pub fn find_table(mem: &impl Mem, rsdp: u64, sig: &[u8; 4]) -> Option<(u64, usize)> {
    let (root, esz) = rsdp_root(mem, rsdp)?;
    let mut hdr = [0u8; 36];
    if !mem.read(root, &mut hdr) || !(&hdr[..4] == b"RSDT" || &hdr[..4] == b"XSDT") {
        return None;
    }
    let len = le32(&hdr[4..]) as usize;
    if !(36..=1 << 16).contains(&len) {
        return None;
    }
    for i in 0..(len - 36) / esz {
        let mut e = [0u8; 8];
        if !mem.read(root + 36 + (i * esz) as u64, &mut e[..esz]) {
            return None;
        }
        let t = if esz == 8 { le64(&e) } else { le32(&e) as u64 };
        let mut h = [0u8; 8];
        if mem.read(t, &mut h) && &h[..4] == sig {
            let tlen = le32(&h[4..]) as usize;
            if (36..=1 << 20).contains(&tlen) {
                return Some((t, tlen));
            }
        }
    }
    None
}

pub const MAX_IOAPICS: usize = 4;
pub const MAX_ISOS: usize = 16;

/// MADT type 1 / type 2 entries.
#[derive(Clone, Copy, Debug)]
pub struct IrqInfo {
    /// (I/O APIC id, MMIO address, GSI base).
    pub ioapics: [(u8, u32, u32); MAX_IOAPICS],
    pub n_ioapics: usize,
    /// Interrupt source overrides: (ISA irq, GSI, MPS INTI flags).
    pub isos: [(u8, u32, u16); MAX_ISOS],
    pub n_isos: usize,
}

impl IrqInfo {
    pub const fn new() -> Self {
        IrqInfo {
            ioapics: [(0, 0, 0); MAX_IOAPICS],
            n_ioapics: 0,
            isos: [(0, 0, 0); MAX_ISOS],
            n_isos: 0,
        }
    }
}

/// I/O APICs and interrupt source overrides from the MADT.
pub fn madt_irq(mem: &impl Mem, rsdp: u64) -> Option<IrqInfo> {
    let (madt, len) = find_table(mem, rsdp, b"APIC")?;
    let mut out = IrqInfo::new();
    let mut off = 44;
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
            // I/O APIC: id u8, reserved u8, address u32, GSI base u32.
            1 if elen >= 12
                && out.n_ioapics < MAX_IOAPICS
                && mem.read(madt + off as u64, &mut e[..12]) =>
            {
                out.ioapics[out.n_ioapics] = (e[2], le32(&e[4..]), le32(&e[8..]));
                out.n_ioapics += 1;
            }
            // Interrupt source override: bus u8 (0 = ISA), source u8, GSI
            // u32, flags u16.
            2 if elen >= 10
                && out.n_isos < MAX_ISOS
                && mem.read(madt + off as u64, &mut e[..10]) =>
            {
                out.isos[out.n_isos] = (e[3], le32(&e[4..]), u16::from_le_bytes([e[8], e[9]]));
                out.n_isos += 1;
            }
            _ => {}
        }
        off += elen;
    }
    Some(out)
}

/// ACPI Generic Address Structure.
#[derive(Clone, Copy, Debug, Default)]
pub struct Gas {
    /// 0 = system memory, 1 = system I/O.
    pub space: u8,
    pub bit_width: u8,
    pub address: u64,
}

fn gas(b: &[u8]) -> Gas {
    Gas {
        space: b[0],
        bit_width: b[1],
        address: le64(&b[4..]),
    }
}

/// What the kernel uses from the FADT.
#[derive(Clone, Copy, Debug, Default)]
pub struct Fadt {
    pub dsdt: u64,
    pub flags: u32,
    /// PM1a control block (I/O port), 0 if none.
    pub pm1a_cnt: u32,
    /// ACPI 5+ hardware-reduced sleep control register (address 0 = none).
    pub sleep_control: Gas,
}

/// FADT flag: hardware-reduced ACPI (no PM1 blocks; sleep through
/// SLEEP_CONTROL_REG).
pub const FADT_HW_REDUCED: u32 = 1 << 20;

/// The FADT ("FACP"): DSDT pointer (X_DSDT preferred), flags, PM1a_CNT
/// (or X_PM1a_CNT_BLK when it is an I/O port) and SLEEP_CONTROL_REG.
pub fn fadt(mem: &impl Mem, rsdp: u64) -> Option<Fadt> {
    let (addr, len) = find_table(mem, rsdp, b"FACP")?;
    let mut b = [0u8; 276];
    let n = len.min(b.len());
    if n < 116 || !mem.read(addr, &mut b[..n]) {
        return None;
    }
    let mut f = Fadt {
        dsdt: le32(&b[40..]) as u64,
        flags: le32(&b[112..]),
        pm1a_cnt: le32(&b[64..]),
        sleep_control: Gas::default(),
    };
    if n >= 148 {
        let x = le64(&b[140..]);
        if x != 0 {
            f.dsdt = x;
        }
    }
    if f.pm1a_cnt == 0 && n >= 184 {
        let g = gas(&b[172..]);
        if g.space == 1 && g.address <= 0xffff {
            f.pm1a_cnt = g.address as u32;
        }
    }
    if n >= 256 {
        f.sleep_control = gas(&b[244..]);
    }
    Some(f)
}

/// A virtio-mmio device declared in the DSDT (`_HID` "LNRO0005").
#[derive(Clone, Copy, Debug, Default)]
pub struct DsdtVirtio {
    pub base: u64,
    pub size: u32,
    pub gsi: u32,
    pub edge: bool,
    pub active_low: bool,
}

pub const MAX_DSDT_VIRTIO: usize = 32;

/// What the DSDT byte scan found.
#[derive(Clone, Copy, Debug)]
pub struct DsdtInfo {
    pub virtio: [DsdtVirtio; MAX_DSDT_VIRTIO],
    pub n_virtio: usize,
    /// SLP_TYPa of `\_S5`, if the package was readable.
    pub s5_typ: Option<u8>,
    pub len: usize,
}

/// Scan the DSDT at `addr` for `LNRO0005` devices and `_S5_`.
///
/// Not an AML interpreter: QEMU and Firecracker emit each virtio-mmio
/// device as `Device(VRxx) { _HID "LNRO0005", _UID n, _CCA, _CRS
/// ResourceTemplate { Memory32Fixed(base, size), Interrupt(..){gsi} } }`,
/// so after each `LNRO0005` string the next Memory32Fixed descriptor
/// (`86 09 00`) and Extended Interrupt descriptor (`89 len 00 flags n
/// gsi`) before the next `LNRO` are taken. `_S5_` is `08 '_S5_' 12
/// pkglen n (0A x | 00 | 01)`.
pub fn dsdt_scan(mem: &impl Mem, addr: u64) -> Option<DsdtInfo> {
    let mut h = [0u8; 8];
    if !mem.read(addr, &mut h) || &h[..4] != b"DSDT" {
        return None;
    }
    let len = (le32(&h[4..]) as usize).min(1 << 20);
    let mut out = DsdtInfo {
        virtio: [DsdtVirtio::default(); MAX_DSDT_VIRTIO],
        n_virtio: 0,
        s5_typ: None,
        len,
    };
    // Windows of CHUNK bytes advanced by CHUNK, each read with OVER extra
    // bytes so a match near a window's end sees its descriptors.
    const CHUNK: usize = 4096;
    const OVER: usize = 256;
    let mut buf = [0u8; CHUNK + OVER];
    let mut pos = 36;
    while pos < len {
        let n = (len - pos).min(buf.len());
        if !mem.read(addr + pos as u64, &mut buf[..n]) {
            break;
        }
        let w = &buf[..n];
        let limit = CHUNK.min(n);
        for i in 0..limit {
            if w[i..].starts_with(b"LNRO0005") {
                if out.n_virtio < MAX_DSDT_VIRTIO
                    && let Some(d) = parse_crs(&w[i..(i + 192).min(n)])
                    && !out.virtio[..out.n_virtio].iter().any(|v| v.base == d.base)
                {
                    out.virtio[out.n_virtio] = d;
                    out.n_virtio += 1;
                }
            } else if out.s5_typ.is_none() && w[i..].starts_with(b"_S5_") {
                out.s5_typ = parse_s5(&w[i + 4..(i + 16).min(n)]);
            }
        }
        pos += CHUNK;
    }
    Some(out)
}

fn parse_crs(w: &[u8]) -> Option<DsdtVirtio> {
    let mut d = DsdtVirtio::default();
    let (mut mem, mut irq) = (false, false);
    let mut i = 8;
    while i + 3 <= w.len() && !(mem && irq) {
        if w[i..].starts_with(b"LNRO") {
            break; // the next device
        }
        if !mem && w[i] == 0x86 && w[i + 1] == 0x09 && w[i + 2] == 0x00 && i + 12 <= w.len() {
            d.base = le32(&w[i + 4..]) as u64;
            d.size = le32(&w[i + 8..]);
            mem = true;
            i += 12;
            continue;
        }
        if !irq && w[i] == 0x89 && w[i + 2] == 0x00 && i + 9 <= w.len() && w[i + 4] >= 1 {
            let flags = w[i + 3];
            d.edge = flags & 0x2 != 0;
            d.active_low = flags & 0x4 != 0;
            d.gsi = le32(&w[i + 5..]);
            irq = true;
            i += 9;
            continue;
        }
        i += 1;
    }
    (mem && irq && d.base != 0).then_some(d)
}

fn parse_s5(w: &[u8]) -> Option<u8> {
    // PackageOp, PkgLength (1-4 bytes), NumElements, first element.
    if *w.first()? != 0x12 {
        return None;
    }
    let lead = *w.get(1)?;
    let i = 1 + 1 + (lead >> 6) as usize + 1;
    match *w.get(i)? {
        0x0a => w.get(i + 1).copied(),
        0x00 => Some(0),
        0x01 => Some(1),
        _ => None,
    }
}
