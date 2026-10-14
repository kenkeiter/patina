//! A minimal, bounds-checked ELF64 little-endian reader for the vDSO
//! image: program headers, the dynamic section, symbol lookup through the
//! SysV hash table with version checks (mirroring Go's
//! `vdsoParseSymbols` / `vdsoFindVersion`), and the section checks the
//! host tests use. No allocation.

pub const ET_DYN: u16 = 3;
pub const EM_X86_64: u16 = 62;
pub const EM_AARCH64: u16 = 183;
pub const PT_LOAD: u32 = 1;
pub const PT_DYNAMIC: u32 = 2;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_SYMENT: u64 = 11;
const DT_REL: u64 = 17;
const DT_TEXTREL: u64 = 22;
const DT_JMPREL: u64 = 23;
const DT_RELR: u64 = 36;
const DT_VERSYM: u64 = 0x6fff_fff0;
const DT_VERDEF: u64 = 0x6fff_fffc;
const DT_VERDEFNUM: u64 = 0x6fff_fffd;

const SHT_SYMTAB: u32 = 2;
const SHT_RELA: u32 = 4;
const SHT_REL: u32 = 9;
const SHT_NOBITS: u32 = 8;
const SHT_RELR: u32 = 19;
const SHF_WRITE: u64 = 1;

const STT_NOTYPE: u8 = 0;
const STT_FUNC: u8 = 2;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const VER_FLG_BASE: u16 = 1;

/// The SysV ELF hash (`DT_HASH`, `vd_hash`).
pub fn sysv_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &c in name {
        h = (h << 4).wrapping_add(c as u32);
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
        }
        h &= !g;
    }
    h
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}
fn cstr_at(b: &[u8], off: usize) -> Option<&[u8]> {
    let s = b.get(off..)?;
    let n = s.iter().position(|&c| c == 0)?;
    Some(&s[..n])
}

/// One program header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Phdr {
    pub p_type: u32,
    pub flags: u32,
    pub offset: usize,
    pub vaddr: usize,
    pub filesz: usize,
    pub memsz: usize,
}

/// Dynamic-section entries the reader uses (vaddrs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dynamic {
    pub hash: Option<usize>,
    pub symtab: Option<usize>,
    pub strtab: Option<usize>,
    pub syment: Option<u64>,
    pub versym: Option<usize>,
    pub verdef: Option<usize>,
    pub verdefnum: Option<u64>,
    /// A relocation-related tag that is present (`DT_RELA`, ...).
    pub reloc_tag: Option<u64>,
}

/// A parsed image.
#[derive(Clone, Copy, Debug)]
pub struct Image<'a> {
    pub bytes: &'a [u8],
    pub e_type: u16,
    pub machine: u16,
    phoff: usize,
    phnum: usize,
    shoff: usize,
    shnum: usize,
}

/// Where the kernel maps things, relative to the image base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Bytes of the image to map read-exec (the PT_LOAD, page rounded;
    /// the file bytes past the PT_LOAD are not needed at run time).
    pub text_len: usize,
    /// Bytes of the file to copy to the start of the text (the PT_LOAD's
    /// file size; the rest of the text pages is zero).
    pub file_len: usize,
    /// Offset of the vvar page (`__vvar_page` in the link script).
    pub vvar_offset: usize,
}

/// The kernel's view: map `[0, text_len)` of the image, then the vvar
/// page at `vvar_offset`. `None` if the image is malformed.
pub fn layout(bytes: &[u8]) -> Option<Layout> {
    let img = Image::parse(bytes)?;
    let load = img.load()?;
    if load.vaddr != 0 || load.offset != 0 || load.filesz > bytes.len() {
        return None;
    }
    let text_len = load.memsz.checked_add(4095)? & !4095;
    Some(Layout {
        text_len,
        file_len: load.filesz,
        vvar_offset: text_len,
    })
}

impl<'a> Image<'a> {
    pub fn parse(bytes: &'a [u8]) -> Option<Image<'a>> {
        if bytes.get(..6)? != b"\x7fELF\x02\x01" {
            return None;
        }
        if u16_at(bytes, 54)? != 56 {
            return None;
        }
        Some(Image {
            bytes,
            e_type: u16_at(bytes, 16)?,
            machine: u16_at(bytes, 18)?,
            phoff: u64_at(bytes, 32)? as usize,
            phnum: u16_at(bytes, 56)? as usize,
            shoff: u64_at(bytes, 40)? as usize,
            shnum: u16_at(bytes, 60)? as usize,
        })
    }

    pub fn phdrs(&self) -> impl Iterator<Item = Phdr> + '_ {
        (0..self.phnum).filter_map(move |i| {
            let o = self.phoff.checked_add(i * 56)?;
            let b = self.bytes;
            Some(Phdr {
                p_type: u32_at(b, o)?,
                flags: u32_at(b, o + 4)?,
                offset: u64_at(b, o + 8)? as usize,
                vaddr: u64_at(b, o + 16)? as usize,
                filesz: u64_at(b, o + 32)? as usize,
                memsz: u64_at(b, o + 40)? as usize,
            })
        })
    }

    /// The first `PT_LOAD` (Go uses the first one for the load offset).
    pub fn load(&self) -> Option<Phdr> {
        self.phdrs().find(|p| p.p_type == PT_LOAD)
    }

    /// File offset of a vaddr inside the first PT_LOAD.
    fn off(&self, vaddr: usize) -> Option<usize> {
        let l = self.load()?;
        (vaddr >= l.vaddr && vaddr < l.vaddr + l.filesz).then(|| vaddr - l.vaddr + l.offset)
    }

    pub fn dynamic(&self) -> Option<Dynamic> {
        let p = self.phdrs().find(|p| p.p_type == PT_DYNAMIC)?;
        let mut d = Dynamic::default();
        let mut o = p.offset;
        while o + 16 <= p.offset + p.filesz {
            let tag = u64_at(self.bytes, o)?;
            let val = u64_at(self.bytes, o + 8)?;
            match tag {
                DT_NULL => return Some(d),
                DT_HASH => d.hash = Some(val as usize),
                DT_SYMTAB => d.symtab = Some(val as usize),
                DT_STRTAB => d.strtab = Some(val as usize),
                DT_SYMENT => d.syment = Some(val),
                DT_VERSYM => d.versym = Some(val as usize),
                DT_VERDEF => d.verdef = Some(val as usize),
                DT_VERDEFNUM => d.verdefnum = Some(val),
                DT_RELA | DT_REL | DT_JMPREL | DT_TEXTREL | DT_RELR => d.reloc_tag = Some(tag),
                _ => {}
            }
            o += 16;
        }
        None
    }

    /// Go's `vdsoFindVersion`: the version index of `name`/`hash`.
    fn version_index(&self, d: &Dynamic, name: &[u8], hash: u32) -> Option<u16> {
        let strtab = self.off(d.strtab?)?;
        let mut def = self.off(d.verdef?)?;
        loop {
            let flags = u16_at(self.bytes, def + 2)?;
            let ndx = u16_at(self.bytes, def + 4)?;
            let vd_hash = u32_at(self.bytes, def + 8)?;
            let aux = u32_at(self.bytes, def + 12)? as usize;
            let next = u32_at(self.bytes, def + 16)? as usize;
            if flags & VER_FLG_BASE == 0 && vd_hash == hash {
                let vda_name = u32_at(self.bytes, def + aux)? as usize;
                if cstr_at(self.bytes, strtab + vda_name)? == name {
                    return Some(ndx & 0x7fff);
                }
            }
            if next == 0 {
                return None;
            }
            def += next;
        }
    }

    /// Go's `vdsoParseSymbols` (DT_HASH path) for one symbol: its value
    /// (offset from the load base) if `name` exists as a defined global
    /// function in version `version`.
    pub fn lookup(&self, name: &[u8], version: &[u8], version_hash: u32) -> Option<usize> {
        let d = self.dynamic()?;
        let ver = self.version_index(&d, version, version_hash)?;
        let hash = self.off(d.hash?)?;
        let symtab = self.off(d.symtab?)?;
        let strtab = self.off(d.strtab?)?;
        let versym = self.off(d.versym?)?;
        let nbucket = u32_at(self.bytes, hash)? as usize;
        let nchain = u32_at(self.bytes, hash + 4)? as usize;
        if nbucket == 0 {
            return None;
        }
        let bucket = |i: usize| u32_at(self.bytes, hash + 8 + 4 * i);
        let chain = |i: usize| u32_at(self.bytes, hash + 8 + 4 * nbucket + 4 * i);
        let mut idx = bucket(sysv_hash(name) as usize % nbucket)? as usize;
        let mut steps = 0;
        while idx != 0 && steps <= nchain {
            let s = symtab + 24 * idx;
            let st_name = u32_at(self.bytes, s)? as usize;
            let info = *self.bytes.get(s + 4)?;
            let shndx = u16_at(self.bytes, s + 6)?;
            let value = u64_at(self.bytes, s + 8)? as usize;
            let (typ, bind) = (info & 0xf, info >> 4);
            if (typ == STT_FUNC || typ == STT_NOTYPE)
                && (bind == STB_GLOBAL || bind == STB_WEAK)
                && shndx != 0
                && cstr_at(self.bytes, strtab + st_name)? == name
                && u16_at(self.bytes, versym + 2 * idx)? & 0x7fff == ver
            {
                return Some(value);
            }
            idx = chain(idx)? as usize;
            steps += 1;
        }
        None
    }

    /// (sh_type, sh_flags, sh_offset, sh_size, sh_link) of each section.
    fn sections(&self) -> impl Iterator<Item = (u32, u64, usize, usize, u32)> + '_ {
        (0..self.shnum).filter_map(move |i| {
            let o = self.shoff.checked_add(i * 64)?;
            let b = self.bytes;
            Some((
                u32_at(b, o + 4)?,
                u64_at(b, o + 8)?,
                u64_at(b, o + 24)? as usize,
                u64_at(b, o + 32)? as usize,
                u32_at(b, o + 40)?,
            ))
        })
    }

    /// Value of a symbol in the static `.symtab` (local symbols included).
    pub fn local_symbol(&self, name: &[u8]) -> Option<u64> {
        let (_, _, off, size, link) = self.sections().find(|s| s.0 == SHT_SYMTAB)?;
        let (_, _, stroff, _, _) = self.sections().nth(link as usize)?;
        (0..size / 24).find_map(|i| {
            let s = off + 24 * i;
            let n = u32_at(self.bytes, s)? as usize;
            (cstr_at(self.bytes, stroff + n)? == name).then(|| u64_at(self.bytes, s + 8))?
        })
    }

    /// Err(what) if the image has dynamic relocations of any kind.
    pub fn check_no_relocations(&self) -> Result<(), &'static str> {
        let d = self.dynamic().ok_or("no dynamic section")?;
        if d.reloc_tag.is_some() {
            return Err("relocation tag in .dynamic");
        }
        if self
            .sections()
            .any(|s| matches!(s.0, SHT_RELA | SHT_REL | SHT_RELR) && s.3 > 0)
        {
            return Err("relocation section");
        }
        Ok(())
    }

    /// Err if any allocated section is writable and non-empty (data, bss,
    /// GOT), or the PT_LOAD is writable.
    pub fn check_no_writable_sections(&self) -> Result<(), &'static str> {
        if self
            .phdrs()
            .any(|p| p.p_type == PT_LOAD && p.flags & PF_W != 0)
        {
            return Err("writable PT_LOAD");
        }
        if self.sections().any(|s| {
            s.1 & SHF_WRITE != 0 && s.3 > 0 && s.0 != SHT_NOBITS || s.0 == SHT_NOBITS && s.3 > 0
        }) {
            return Err("writable or bss section");
        }
        Ok(())
    }
}
