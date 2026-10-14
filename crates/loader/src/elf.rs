//! Static ELF64 executable parser for Go binaries (design §4 "ELF loading").
//!
//! Accepts only what a `CGO_ENABLED=0` `GOOS=linux` Go build produces: a
//! little-endian ELF64 `ET_EXEC` for the expected machine with no
//! `PT_INTERP`/`PT_DYNAMIC`. Everything is bounds-checked; malformed input
//! yields an [`Error`], never a panic.
//!
//! Go facts: amd64 binaries link at `0x400000`, arm64 at `0x10000` with
//! `p_align = 0x10000`. The loader works at 4 KiB granularity, so it only
//! requires `p_vaddr ≡ p_offset (mod 4096)`, not modulo `p_align`.

use alloc::vec::Vec;
use core::fmt;

use crate::{page_down, page_up, PAGE_SIZE, USER_VA_END};

const EHDR_SIZE: usize = 64;
/// `sizeof(Elf64_Phdr)`.
pub const PHDR_SIZE: u16 = 56;

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PT_GNU_STACK: u32 = 0x6474_e551;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// Target machine the binary must be built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Machine {
    X86_64,
    Aarch64,
}

impl Machine {
    /// The `e_machine` value (`EM_X86_64` = 62, `EM_AARCH64` = 183).
    pub const fn e_machine(self) -> u16 {
        match self {
            Machine::X86_64 => EM_X86_64,
            Machine::Aarch64 => EM_AARCH64,
        }
    }

    /// Go's `GOARCH` name for this machine.
    pub const fn goarch(self) -> &'static str {
        match self {
            Machine::X86_64 => "amd64",
            Machine::Aarch64 => "arm64",
        }
    }
}

impl fmt::Display for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Machine::X86_64 => "x86_64",
            Machine::Aarch64 => "aarch64",
        })
    }
}

/// Why a binary was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No `\x7fELF` magic.
    NotElf,
    /// `EI_CLASS` is not `ELFCLASS64`.
    Not64Bit,
    /// `EI_DATA` is not `ELFDATA2LSB`.
    NotLittleEndian,
    /// Built for another architecture (`found` is the raw `e_machine`).
    WrongMachine { expected: Machine, found: u16 },
    /// `ET_DYN`: a position-independent (or shared-object) binary.
    Pie,
    /// `e_type` is neither `ET_EXEC` nor `ET_DYN` (e.g. a relocatable object).
    NotExecutable(u16),
    /// `PT_INTERP` or `PT_DYNAMIC` present: dynamically linked.
    Dynamic,
    /// The file ends before a header, the program header table or a
    /// segment's file bytes.
    Truncated,
    /// A malformed ELF header field.
    BadHeader(&'static str),
    /// A malformed program header (`index` into the program header table).
    BadSegment { index: usize, reason: &'static str },
    /// `PT_LOAD` segments are not sorted by `p_vaddr`.
    Unsorted { index: usize },
    /// A `PT_LOAD` segment shares a 4 KiB page with the previous one.
    Overlap { index: usize },
    /// A segment reaches beyond the user address range.
    OutOfRange { index: usize },
    /// No `PT_LOAD` segments.
    NoLoadSegments,
    /// `e_entry` is not inside an executable `PT_LOAD` segment.
    BadEntry(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotElf => f.write_str("not an ELF file (bad magic)"),
            Error::Not64Bit => f.write_str("not a 64-bit ELF (ELFCLASS64 required)"),
            Error::NotLittleEndian => f.write_str("not a little-endian ELF (ELFDATA2LSB required)"),
            Error::WrongMachine { expected, found } => {
                let name = match *found {
                    EM_X86_64 => "x86_64",
                    EM_AARCH64 => "aarch64",
                    _ => "unknown",
                };
                write!(
                    f,
                    "ELF built for the wrong architecture: expected {expected} (GOARCH={}), found e_machine {found} ({name})",
                    expected.goarch()
                )
            }
            Error::Pie => f.write_str(
                "PIE/ET_DYN binaries are not supported: build with CGO_ENABLED=0 and no -buildmode=pie",
            ),
            Error::NotExecutable(t) => write!(f, "ELF is not an executable (e_type {t})"),
            Error::Dynamic => f.write_str(
                "dynamically linked ELF (PT_INTERP/PT_DYNAMIC present): build a static binary with CGO_ENABLED=0",
            ),
            Error::Truncated => f.write_str("ELF file is truncated"),
            Error::BadHeader(r) => write!(f, "malformed ELF header: {r}"),
            Error::BadSegment { index, reason } => {
                write!(f, "malformed program header {index}: {reason}")
            }
            Error::Unsorted { index } => {
                write!(f, "PT_LOAD segment (phdr {index}) is not sorted by address")
            }
            Error::Overlap { index } => {
                write!(f, "PT_LOAD segment (phdr {index}) overlaps the previous one")
            }
            Error::OutOfRange { index } => {
                write!(f, "segment (phdr {index}) lies outside the user address range")
            }
            Error::NoLoadSegments => f.write_str("ELF has no PT_LOAD segments"),
            Error::BadEntry(e) => {
                write!(f, "entry point {e:#x} is not inside an executable segment")
            }
        }
    }
}

impl core::error::Error for Error {}

/// Segment permissions (`PF_R`/`PF_W`/`PF_X`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Flags {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Flags {
    pub const fn from_bits(p_flags: u32) -> Flags {
        Flags {
            read: p_flags & PF_R != 0,
            write: p_flags & PF_W != 0,
            exec: p_flags & PF_X != 0,
        }
    }
}

impl fmt::Display for Flags {
    /// `R-X`-style, like `readelf`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = |b: bool, ch: char| if b { ch } else { '-' };
        write!(
            f,
            "{}{}{}",
            c(self.read, 'R'),
            c(self.write, 'W'),
            c(self.exec, 'X')
        )
    }
}

/// One validated `PT_LOAD` segment.
///
/// To load: map [`Segment::page_range`] with `flags`, copy the `filesz`
/// bytes of [`Image::data`] to `vaddr`, and zero everything else in the
/// range (the `.bss` tail). Linux instead maps whole file pages
/// ([`Segment::file_page_range`]), so the bytes before `vaddr` in the first
/// page hold file contents there; Go never reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub filesz: u64,
    pub offset: u64,
    pub flags: Flags,
    pub align: u64,
}

impl Segment {
    /// Page-rounded virtual range `[start, end)` this segment occupies.
    pub fn page_range(&self) -> (u64, u64) {
        // Overflow ruled out by validation.
        let end = page_up(self.vaddr + self.memsz).unwrap_or(u64::MAX);
        (page_down(self.vaddr), end)
    }

    /// The file range Linux would `mmap` for this segment:
    /// `[page_down(offset), offset + filesz)`, mapped at `page_down(vaddr)`.
    pub fn file_page_range(&self) -> (u64, u64) {
        (page_down(self.offset), self.offset + self.filesz)
    }
}

/// A validated static executable.
#[derive(Clone, Debug)]
pub struct Image<'a> {
    data: &'a [u8],
    /// `e_entry`.
    pub entry: u64,
    /// User virtual address of the program header table (the `AT_PHDR`
    /// value): `PT_PHDR.p_vaddr` if present, else computed from the
    /// `PT_LOAD` that contains `e_phoff` (`p_vaddr + e_phoff - p_offset`),
    /// else 0 (as Linux does when the table is not mapped).
    pub phdr_vaddr: u64,
    /// `e_phentsize` (`AT_PHENT`; always 56).
    pub phent: u16,
    /// `e_phnum` (`AT_PHNUM`).
    pub phnum: u16,
    loads: Vec<Segment>,
    stack_exec: bool,
}

impl<'a> Image<'a> {
    /// The `PT_LOAD` segments, sorted by `vaddr`.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.loads.iter().copied()
    }

    /// The `filesz` bytes of `seg` in the file.
    pub fn data(&self, seg: &Segment) -> &'a [u8] {
        let start = seg.offset as usize;
        start
            .checked_add(seg.filesz as usize)
            .and_then(|end| self.data.get(start..end))
            .unwrap_or(&[])
    }

    /// The whole file.
    pub fn file(&self) -> &'a [u8] {
        self.data
    }

    /// Whether `PT_GNU_STACK` asks for an executable stack (false when
    /// `PT_GNU_STACK` is present without `PF_X`, which is what Go emits).
    /// Absent `PT_GNU_STACK` reports `true` (the historical Linux default);
    /// patina never maps an executable stack either way.
    pub fn stack_executable(&self) -> bool {
        self.stack_exec
    }

    /// Page-rounded end of the highest `PT_LOAD` (the initial program
    /// break for a future `brk`).
    pub fn highest_mapped_vaddr(&self) -> u64 {
        self.loads.last().map(|s| s.page_range().1).unwrap_or(0)
    }
}

fn u16_at(d: &[u8], off: usize) -> Option<u16> {
    let b = d.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(d: &[u8], off: usize) -> Option<u32> {
    let b = d.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(d: &[u8], off: usize) -> Option<u64> {
    let b = d.get(off..off.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    Some(u64::from_le_bytes(a))
}

#[derive(Clone, Copy)]
struct Phdr {
    ty: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

fn read_phdr(d: &[u8], off: usize) -> Option<Phdr> {
    Some(Phdr {
        ty: u32_at(d, off)?,
        flags: u32_at(d, off + 4)?,
        offset: u64_at(d, off + 8)?,
        vaddr: u64_at(d, off + 16)?,
        filesz: u64_at(d, off + 32)?,
        memsz: u64_at(d, off + 40)?,
        align: u64_at(d, off + 48)?,
    })
}

/// Parse and validate a static ELF64 executable for `machine`.
pub fn parse(data: &[u8], machine: Machine) -> Result<Image<'_>, Error> {
    if data.get(..4) != Some(b"\x7fELF".as_slice()) {
        return Err(Error::NotElf);
    }
    let ident = data.get(..16).ok_or(Error::Truncated)?;
    if ident[4] != ELFCLASS64 {
        return Err(Error::Not64Bit);
    }
    if ident[5] != ELFDATA2LSB {
        return Err(Error::NotLittleEndian);
    }
    if ident[6] != EV_CURRENT {
        return Err(Error::BadHeader("EI_VERSION is not EV_CURRENT"));
    }
    if data.len() < EHDR_SIZE {
        return Err(Error::Truncated);
    }
    let rd16 = |o| u16_at(data, o).ok_or(Error::Truncated);
    let rd64 = |o| u64_at(data, o).ok_or(Error::Truncated);

    match rd16(16)? {
        ET_EXEC => {}
        ET_DYN => return Err(Error::Pie),
        t => return Err(Error::NotExecutable(t)),
    }
    let found = rd16(18)?;
    if found != machine.e_machine() {
        return Err(Error::WrongMachine {
            expected: machine,
            found,
        });
    }
    let entry = rd64(24)?;
    let phoff = rd64(32)?;
    let phent = rd16(54)?;
    let phnum = rd16(56)?;
    if phent != PHDR_SIZE {
        return Err(Error::BadHeader("e_phentsize is not 56"));
    }
    if phnum == 0 {
        return Err(Error::BadHeader("no program headers"));
    }
    if phnum == 0xffff {
        return Err(Error::BadHeader("extended program header count (PN_XNUM)"));
    }
    let table_len = u64::from(phnum) * u64::from(PHDR_SIZE);
    let table_end = phoff.checked_add(table_len).ok_or(Error::Truncated)?;
    if table_end > data.len() as u64 {
        return Err(Error::Truncated);
    }
    let phoff_us = phoff as usize;

    let mut loads: Vec<Segment> = Vec::new();
    let mut pt_phdr: Option<u64> = None;
    let mut stack_exec = true;
    for index in 0..usize::from(phnum) {
        let ph =
            read_phdr(data, phoff_us + index * usize::from(PHDR_SIZE)).ok_or(Error::Truncated)?;
        let bad = |reason| Error::BadSegment { index, reason };
        match ph.ty {
            PT_INTERP | PT_DYNAMIC => return Err(Error::Dynamic),
            PT_PHDR => {
                if pt_phdr.is_some() {
                    return Err(bad("more than one PT_PHDR"));
                }
                pt_phdr = Some(ph.vaddr);
            }
            PT_GNU_STACK => stack_exec = ph.flags & PF_X != 0,
            PT_LOAD => {
                if ph.memsz == 0 {
                    return Err(bad("empty PT_LOAD (p_memsz is 0)"));
                }
                if ph.filesz > ph.memsz {
                    return Err(bad("p_filesz > p_memsz"));
                }
                let file_end = ph.offset.checked_add(ph.filesz).ok_or(Error::Truncated)?;
                if file_end > data.len() as u64 {
                    return Err(Error::Truncated);
                }
                if ph.vaddr % PAGE_SIZE != ph.offset % PAGE_SIZE {
                    return Err(bad("p_vaddr and p_offset are not congruent modulo 4096"));
                }
                let end = ph
                    .vaddr
                    .checked_add(ph.memsz)
                    .and_then(page_up)
                    .ok_or(Error::OutOfRange { index })?;
                if end > USER_VA_END {
                    return Err(Error::OutOfRange { index });
                }
                let seg = Segment {
                    vaddr: ph.vaddr,
                    memsz: ph.memsz,
                    filesz: ph.filesz,
                    offset: ph.offset,
                    flags: Flags::from_bits(ph.flags),
                    align: ph.align,
                };
                if let Some(prev) = loads.last() {
                    if seg.vaddr <= prev.vaddr {
                        return Err(Error::Unsorted { index });
                    }
                    if seg.page_range().0 < prev.page_range().1 {
                        return Err(Error::Overlap { index });
                    }
                }
                loads.push(seg);
            }
            _ => {}
        }
    }
    if loads.is_empty() {
        return Err(Error::NoLoadSegments);
    }
    if !loads
        .iter()
        .any(|s| s.flags.exec && entry >= s.vaddr && entry - s.vaddr < s.memsz)
    {
        return Err(Error::BadEntry(entry));
    }
    let phdr_vaddr = match pt_phdr {
        Some(v) => v,
        None => loads
            .iter()
            .find(|s| s.offset <= phoff && phoff - s.offset < s.filesz)
            .map(|s| s.vaddr + (phoff - s.offset))
            .unwrap_or(0),
    };

    Ok(Image {
        data,
        entry,
        phdr_vaddr,
        phent,
        phnum,
        loads,
        stack_exec,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    #[derive(Clone, Copy)]
    struct Ph {
        ty: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        filesz: u64,
        memsz: u64,
        align: u64,
    }

    fn load(flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> Ph {
        Ph {
            ty: PT_LOAD,
            flags,
            offset,
            vaddr,
            filesz,
            memsz,
            align: 0x1000,
        }
    }

    fn other(ty: u32, flags: u32, offset: u64, vaddr: u64) -> Ph {
        Ph {
            ty,
            flags,
            offset,
            vaddr,
            filesz: 0,
            memsz: 0,
            align: 8,
        }
    }

    const BASE: u64 = 0x40_0000;

    /// Build a synthetic ELF: header at 0, phdrs at `phoff`, `len` bytes.
    fn build(e_type: u16, machine: u16, entry: u64, phoff: u64, phs: &[Ph], len: usize) -> Vec<u8> {
        let mut d = vec![0u8; len];
        d[..4].copy_from_slice(b"\x7fELF");
        d[4] = ELFCLASS64;
        d[5] = ELFDATA2LSB;
        d[6] = EV_CURRENT;
        d[16..18].copy_from_slice(&e_type.to_le_bytes());
        d[18..20].copy_from_slice(&machine.to_le_bytes());
        d[20..24].copy_from_slice(&1u32.to_le_bytes());
        d[24..32].copy_from_slice(&entry.to_le_bytes());
        d[32..40].copy_from_slice(&phoff.to_le_bytes());
        d[52..54].copy_from_slice(&64u16.to_le_bytes());
        d[54..56].copy_from_slice(&PHDR_SIZE.to_le_bytes());
        d[56..58].copy_from_slice(&(phs.len() as u16).to_le_bytes());
        for (i, p) in phs.iter().enumerate() {
            let o = phoff as usize + i * 56;
            d[o..o + 4].copy_from_slice(&p.ty.to_le_bytes());
            d[o + 4..o + 8].copy_from_slice(&p.flags.to_le_bytes());
            d[o + 8..o + 16].copy_from_slice(&p.offset.to_le_bytes());
            d[o + 16..o + 24].copy_from_slice(&p.vaddr.to_le_bytes());
            d[o + 24..o + 32].copy_from_slice(&p.vaddr.to_le_bytes());
            d[o + 32..o + 40].copy_from_slice(&p.filesz.to_le_bytes());
            d[o + 40..o + 48].copy_from_slice(&p.memsz.to_le_bytes());
            d[o + 48..o + 56].copy_from_slice(&p.align.to_le_bytes());
        }
        d
    }

    /// A Go-shaped image: PHDR, LOAD R-X (incl. headers), LOAD R, LOAD RW+bss, GNU_STACK.
    fn go_like(with_pt_phdr: bool) -> (Vec<Ph>, usize) {
        let mut v = Vec::new();
        if with_pt_phdr {
            v.push(other(PT_PHDR, PF_R, 64, BASE + 64));
        }
        v.push(load(PF_R | PF_X, 0, BASE, 0x2000, 0x2000));
        v.push(load(PF_R, 0x2000, BASE + 0x2000, 0x1000, 0x1000));
        v.push(load(PF_R | PF_W, 0x3000, BASE + 0x3000, 0x800, 0x5000));
        v.push(other(PT_GNU_STACK, PF_R | PF_W, 0, 0));
        (v, 0x3800)
    }

    fn good(with_pt_phdr: bool) -> Vec<u8> {
        let (phs, len) = go_like(with_pt_phdr);
        build(ET_EXEC, EM_X86_64, BASE + 0x100, 64, &phs, len)
    }

    #[test]
    fn parses_synthetic() {
        for with in [true, false] {
            let d = good(with);
            let img = parse(&d, Machine::X86_64).unwrap();
            assert_eq!(img.entry, BASE + 0x100);
            assert_eq!(img.phdr_vaddr, BASE + 64, "with PT_PHDR: {with}");
            assert_eq!(img.phent, 56);
            assert_eq!(img.phnum, if with { 5 } else { 4 });
            let segs: Vec<Segment> = img.segments().collect();
            assert_eq!(segs.len(), 3);
            assert_eq!(segs[0].flags.to_string(), "R-X");
            assert_eq!(segs[1].flags.to_string(), "R--");
            assert_eq!(segs[2].flags.to_string(), "RW-");
            assert_eq!(img.data(&segs[1]).len(), 0x1000);
            assert_eq!(img.data(&segs[0])[..4], *b"\x7fELF");
            assert_eq!(segs[2].page_range(), (BASE + 0x3000, BASE + 0x8000));
            assert_eq!(img.highest_mapped_vaddr(), BASE + 0x8000);
            assert!(!img.stack_executable());
        }
    }

    #[test]
    fn phdr_from_load_with_offset() {
        // Table at file offset 0x1040 inside the second load (offset 0x1000 → vaddr 0x7000).
        let phs = [
            load(PF_R | PF_X, 0, BASE, 0x1000, 0x1000),
            load(PF_R, 0x1000, BASE + 0x7000, 0x1000, 0x1000),
        ];
        let d = build(ET_EXEC, EM_AARCH64, BASE + 8, 0x1040, &phs, 0x2000);
        let img = parse(&d, Machine::Aarch64).unwrap();
        assert_eq!(img.phdr_vaddr, BASE + 0x7040);
        // PT_PHDR wins when present.
        let phs2 = [phs[0], phs[1], other(PT_PHDR, PF_R, 0x1040, 0x1234_5000)];
        let d = build(ET_EXEC, EM_AARCH64, BASE + 8, 0x1040, &phs2, 0x2000);
        assert_eq!(parse(&d, Machine::Aarch64).unwrap().phdr_vaddr, 0x1234_5000);
        // Table not inside any load and no PT_PHDR: 0, as Linux reports.
        let phs3 = [load(PF_R | PF_X, 0, BASE, 0x100, 0x100)];
        let d = build(ET_EXEC, EM_AARCH64, BASE + 8, 0x1040, &phs3, 0x2000);
        let img = parse(&d, Machine::Aarch64).unwrap();
        assert_eq!(img.phdr_vaddr, 0);
        // No PT_GNU_STACK: reported executable (Linux default).
        assert!(img.stack_executable());
    }

    fn err(d: &[u8]) -> Error {
        parse(d, Machine::X86_64).unwrap_err()
    }

    fn with_phs(phs: &[Ph], len: usize) -> Vec<u8> {
        build(ET_EXEC, EM_X86_64, BASE + 0x100, 64, phs, len)
    }

    #[test]
    fn header_errors() {
        assert_eq!(err(b""), Error::NotElf);
        assert_eq!(err(b"\x7fEL"), Error::NotElf);
        assert_eq!(err(b"MZ\x90\x00 not an elf at all"), Error::NotElf);
        assert_eq!(err(b"\x7fELF\x02\x01"), Error::Truncated);

        let g = good(true);
        let mut d = g.clone();
        d[4] = 1;
        assert_eq!(err(&d), Error::Not64Bit);
        let mut d = g.clone();
        d[5] = 2;
        assert_eq!(err(&d), Error::NotLittleEndian);
        let mut d = g.clone();
        d[6] = 0;
        assert!(matches!(err(&d), Error::BadHeader(_)));
        assert_eq!(err(&g[..40]), Error::Truncated);

        let (phs, len) = go_like(true);
        let d = build(ET_DYN, EM_X86_64, BASE, 64, &phs, len);
        assert_eq!(err(&d), Error::Pie);
        assert!(Error::Pie.to_string().contains("-buildmode=pie"));
        let d = build(1, EM_X86_64, BASE, 64, &phs, len);
        assert_eq!(err(&d), Error::NotExecutable(1));

        let d = build(ET_EXEC, EM_AARCH64, BASE + 0x100, 64, &phs, len);
        let e = err(&d);
        assert_eq!(
            e,
            Error::WrongMachine {
                expected: Machine::X86_64,
                found: EM_AARCH64
            }
        );
        assert!(e.to_string().contains("expected x86_64"), "{e}");
        assert_eq!(
            parse(&g, Machine::Aarch64).unwrap_err(),
            Error::WrongMachine {
                expected: Machine::Aarch64,
                found: EM_X86_64
            }
        );

        let mut d = g.clone();
        d[54] = 32;
        assert!(matches!(err(&d), Error::BadHeader(_)));
        let mut d = g.clone();
        d[56..58].copy_from_slice(&0u16.to_le_bytes());
        assert!(matches!(err(&d), Error::BadHeader(_)));
        let mut d = g.clone();
        d[56..58].copy_from_slice(&0xffffu16.to_le_bytes());
        assert!(matches!(err(&d), Error::BadHeader(_)));
        // Program header table past EOF / phoff overflow.
        let mut d = g.clone();
        d[32..40].copy_from_slice(&(g.len() as u64 - 10).to_le_bytes());
        assert_eq!(err(&d), Error::Truncated);
        let mut d = g.clone();
        d[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(err(&d), Error::Truncated);
    }

    #[test]
    fn dynamic_rejected() {
        let (mut phs, len) = go_like(true);
        phs.push(other(PT_INTERP, PF_R, 0x200, BASE + 0x200));
        assert_eq!(err(&with_phs(&phs, len)), Error::Dynamic);
        let (mut phs, len) = go_like(false);
        phs.push(other(PT_DYNAMIC, PF_R | PF_W, 0x3000, BASE + 0x3000));
        assert_eq!(err(&with_phs(&phs, len)), Error::Dynamic);
        assert!(Error::Dynamic.to_string().contains("CGO_ENABLED=0"));
    }

    #[test]
    fn segment_errors() {
        let rx = PF_R | PF_X;
        let bad = |phs: &[Ph]| err(&with_phs(phs, 0x3000));
        // filesz > memsz
        assert!(matches!(
            bad(&[load(rx, 0, BASE, 0x200, 0x100)]),
            Error::BadSegment { index: 0, .. }
        ));
        // empty
        assert!(matches!(
            bad(&[load(rx, 0, BASE, 0, 0)]),
            Error::BadSegment { index: 0, .. }
        ));
        // file bytes past EOF, and offset+filesz overflow
        assert_eq!(bad(&[load(rx, 0, BASE, 0x4000, 0x4000)]), Error::Truncated);
        assert_eq!(
            bad(&[load(rx, u64::MAX - 0xfff, BASE + 0x1000, 0x2000, 0x2000)]),
            Error::Truncated
        );
        // vaddr not congruent with offset mod 4 KiB (but would be fine mod align 0).
        assert!(matches!(
            bad(&[load(rx, 0x10, BASE, 0x100, 0x100)]),
            Error::BadSegment { index: 0, .. }
        ));
        // vaddr+memsz overflows / above user VA end
        assert_eq!(
            bad(&[load(rx, 0, u64::MAX - 0xfff, 0x100, 0x2000)]),
            Error::OutOfRange { index: 0 }
        );
        assert_eq!(
            bad(&[load(rx, 0, USER_VA_END - 0x1000, 0x100, 0x2000)]),
            Error::OutOfRange { index: 0 }
        );
        assert_eq!(
            bad(&[load(rx, 0, USER_VA_END, 0x100, 0x100)]),
            Error::OutOfRange { index: 0 }
        );
        // Ending exactly at USER_VA_END is fine.
        let d = build(
            ET_EXEC,
            EM_X86_64,
            USER_VA_END - 0x1000,
            64,
            &[load(rx, 0, USER_VA_END - 0x1000, 0x100, 0x1000)],
            0x3000,
        );
        assert_eq!(
            parse(&d, Machine::X86_64).unwrap().highest_mapped_vaddr(),
            USER_VA_END
        );
        // Unsorted
        assert_eq!(
            bad(&[
                load(rx, 0, BASE + 0x1000, 0x100, 0x100),
                load(PF_R, 0x1000, BASE, 0x100, 0x100)
            ]),
            Error::Unsorted { index: 1 }
        );
        // Overlap after page rounding (second starts in the page where the first ends).
        assert_eq!(
            bad(&[
                load(rx, 0, BASE, 0x1100, 0x1100),
                load(PF_R, 0x1200, BASE + 0x1200, 0x100, 0x100)
            ]),
            Error::Overlap { index: 1 }
        );
        // Overlap via bss reaching into the next segment.
        assert_eq!(
            bad(&[
                load(rx, 0, BASE, 0x100, 0x3000),
                load(PF_R, 0x1000, BASE + 0x2000, 0x100, 0x100)
            ]),
            Error::Overlap { index: 1 }
        );
        // No loads
        assert_eq!(
            bad(&[other(PT_GNU_STACK, PF_R | PF_W, 0, 0)]),
            Error::NoLoadSegments
        );
        // Entry outside any executable segment
        let d = build(
            ET_EXEC,
            EM_X86_64,
            BASE + 0x2100,
            64,
            &go_like(true).0,
            0x3800,
        );
        assert_eq!(err(&d), Error::BadEntry(BASE + 0x2100));
        let d = build(ET_EXEC, EM_X86_64, 0, 64, &go_like(true).0, 0x3800);
        assert_eq!(err(&d), Error::BadEntry(0));
        // Duplicate PT_PHDR
        let (mut phs, len) = go_like(true);
        phs.push(other(PT_PHDR, PF_R, 64, BASE + 64));
        assert!(matches!(
            err(&with_phs(&phs, len)),
            Error::BadSegment { .. }
        ));
    }

    #[test]
    fn display_all_errors() {
        let all = [
            Error::NotElf,
            Error::Not64Bit,
            Error::NotLittleEndian,
            Error::WrongMachine {
                expected: Machine::Aarch64,
                found: 3,
            },
            Error::Pie,
            Error::NotExecutable(1),
            Error::Dynamic,
            Error::Truncated,
            Error::BadHeader("x"),
            Error::BadSegment {
                index: 2,
                reason: "y",
            },
            Error::Unsorted { index: 1 },
            Error::Overlap { index: 1 },
            Error::OutOfRange { index: 1 },
            Error::NoLoadSegments,
            Error::BadEntry(0x10),
        ];
        for e in all {
            assert!(!format!("{e}").is_empty());
        }
    }

    #[test]
    fn never_panics_on_mutations() {
        // Flip/overwrite bytes of a good image and truncate it at every length.
        let g = good(true);
        for n in 0..g.len().min(512) {
            let _ = parse(&g[..n], Machine::X86_64);
        }
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut d = g.clone();
            let pos = (state % 400) as usize;
            let val = (state >> 32) as u8;
            d[pos] = val;
            if state & 1 == 0 {
                let pos2 = ((state >> 16) % 400) as usize;
                d[pos2] = 0xff;
            }
            let _ = parse(&d, Machine::X86_64);
        }
    }

    // ---- Real Go binaries -------------------------------------------------

    fn testapps_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testapps")
    }

    fn have_go() -> bool {
        Command::new("go")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn go_build(goarch: &str, extra: &[&str], out: &Path) -> bool {
        let mut cmd = Command::new("go");
        cmd.current_dir(testapps_dir())
            .env("GOOS", "linux")
            .env("GOARCH", goarch)
            .env("CGO_ENABLED", "0")
            .args(["build", "-trimpath", "-ldflags=-extldflags=-static"])
            .args(extra)
            .arg("-o")
            .arg(out)
            .arg("./hello");
        let o = cmd.output().expect("run go");
        if !o.status.success() {
            std::eprintln!(
                "go build ({goarch} {extra:?}) failed: {}",
                std::string::String::from_utf8_lossy(&o.stderr)
            );
        }
        o.status.success()
    }

    #[test]
    fn real_go_binaries() {
        if !have_go() {
            std::eprintln!("skipping real_go_binaries: `go` not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("patina-loader-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (m, goarch, base) in [
            (Machine::Aarch64, "arm64", 0x10000u64),
            (Machine::X86_64, "amd64", 0x400000),
        ] {
            let out = dir.join(format!("hello-{goarch}"));
            assert!(go_build(goarch, &[], &out), "go build {goarch}");
            let data = std::fs::read(&out).unwrap();
            let img = parse(&data, m).unwrap_or_else(|e| panic!("{goarch}: {e}"));
            let segs: Vec<Segment> = img.segments().collect();
            assert_eq!(segs.len(), 3, "{goarch}: {segs:?}");
            assert_eq!(segs[0].vaddr, base, "{goarch}");
            assert_eq!(segs[0].flags.to_string(), "R-X");
            assert_eq!(segs[1].flags.to_string(), "R--");
            assert_eq!(segs[2].flags.to_string(), "RW-");
            assert!(segs[2].memsz > segs[2].filesz, "{goarch}: no bss?");
            assert_eq!(img.phdr_vaddr, base + 64, "{goarch}");
            assert_eq!(img.phent, 56);
            assert_eq!(img.phnum, 6, "{goarch}");
            assert!(!img.stack_executable());
            assert!(img.entry >= segs[0].vaddr && img.entry < segs[0].vaddr + segs[0].memsz);
            assert_eq!(img.highest_mapped_vaddr(), segs[2].page_range().1);
            // Wrong machine is rejected.
            let other = if m == Machine::X86_64 {
                Machine::Aarch64
            } else {
                Machine::X86_64
            };
            assert!(matches!(
                parse(&data, other),
                Err(Error::WrongMachine { .. })
            ));
            std::eprintln!(
                "{goarch}: entry {:#x} phdr {:#x} segs {:?}",
                img.entry,
                img.phdr_vaddr,
                segs
            );
        }
        let pie = dir.join("hello-arm64-pie");
        assert!(go_build("arm64", &["-buildmode=pie"], &pie), "go build pie");
        let data = std::fs::read(&pie).unwrap();
        assert_eq!(parse(&data, Machine::Aarch64).unwrap_err(), Error::Pie);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
