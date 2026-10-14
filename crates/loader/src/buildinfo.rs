//! Go version from a Go executable's `.go.buildinfo` section (for the
//! control channel's `ready` event, design §10.2).
//!
//! Layout (Go's `debug/buildinfo`): the section starts with the 14-byte
//! magic `\xff Go buildinf:`, then byte 14 = pointer size, byte 15 =
//! flags (bit 0: big-endian, bit 1: inline strings), padded to 32 bytes.
//! * Inline form (flag 0x2, Go ≥ 1.18 always): at offset 32 the version
//!   as a uvarint length + bytes, then the module info the same way.
//! * Pointer form (older): two pointers at 16 and 16+ptrsize, each to a
//!   Go string header `{data ptr, len}` in the image (virtual addresses,
//!   resolved through the `PT_LOAD` segments).
//!
//! The section is located by name through the section headers; without
//! them (stripped section table), the file is scanned at 16-byte
//! alignment for the magic, as Go's own reader does. Malformed input
//! yields `None`, never a panic.

const MAGIC: &[u8; 14] = b"\xff Go buildinf:";
const HDR: usize = 32;
const FLAG_BIG_ENDIAN: u8 = 0x1;
const FLAG_INLINE: u8 = 0x2;
/// Longest version string accepted ("go1.27.1 X:nocoverageredesign"...).
const MAX_VERSION: usize = 128;

fn u16le(d: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(off..off + 2)?.try_into().ok()?))
}

fn u32le(d: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(off..off + 4)?.try_into().ok()?))
}

fn u64le(d: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(off..off + 8)?.try_into().ok()?))
}

/// File range of the section named `want` (ELF64 little-endian only).
fn section(data: &[u8], want: &[u8]) -> Option<(usize, usize)> {
    if data.get(..4)? != b"\x7fELF" || *data.get(4)? != 2 || *data.get(5)? != 1 {
        return None;
    }
    let shoff = usize::try_from(u64le(data, 0x28)?).ok()?;
    let shentsize = u16le(data, 0x3a)? as usize;
    let shnum = u16le(data, 0x3c)? as usize;
    let shstrndx = u16le(data, 0x3e)? as usize;
    if shoff == 0 || shentsize < 64 || shstrndx >= shnum {
        return None;
    }
    let sh = |i: usize| -> Option<(u32, usize, usize)> {
        let b = shoff.checked_add(i.checked_mul(shentsize)?)?;
        let name = u32le(data, b)?;
        let off = usize::try_from(u64le(data, b + 0x18)?).ok()?;
        let size = usize::try_from(u64le(data, b + 0x20)?).ok()?;
        Some((name, off, size))
    };
    let (_, stroff, strsize) = sh(shstrndx)?;
    let strtab = data.get(stroff..stroff.checked_add(strsize)?)?;
    for i in 0..shnum {
        let (name, off, size) = sh(i)?;
        let n = strtab.get(name as usize..)?;
        let end = n.iter().position(|&c| c == 0)?;
        if &n[..end] == want {
            data.get(off..off.checked_add(size)?)?;
            return Some((off, size));
        }
    }
    None
}

/// Offset of the buildinfo header: the named section, else a scan.
fn find_header(data: &[u8]) -> Option<usize> {
    if let Some((off, size)) = section(data, b".go.buildinfo") {
        if size >= HDR && data.get(off..off + MAGIC.len()) == Some(&MAGIC[..]) {
            return Some(off);
        }
    }
    let mut off = 0;
    while off + HDR <= data.len() {
        if &data[off..off + MAGIC.len()] == MAGIC {
            return Some(off);
        }
        off += 16;
    }
    None
}

fn uvarint(d: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in d.iter().enumerate().take(10) {
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// File offset of virtual address `va` (`len` bytes) through `PT_LOAD`.
fn va_to_off(data: &[u8], va: u64, len: u64) -> Option<usize> {
    let phoff = usize::try_from(u64le(data, 0x20)?).ok()?;
    let phentsize = u16le(data, 0x36)? as usize;
    let phnum = u16le(data, 0x38)? as usize;
    for i in 0..phnum {
        let b = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if u32le(data, b)? != 1 {
            continue;
        }
        let off = u64le(data, b + 0x08)?;
        let vaddr = u64le(data, b + 0x10)?;
        let filesz = u64le(data, b + 0x20)?;
        if va >= vaddr && va.checked_add(len)? <= vaddr.checked_add(filesz)? {
            return usize::try_from(off + (va - vaddr)).ok();
        }
    }
    None
}

fn printable(s: &[u8]) -> Option<&str> {
    if s.is_empty() || s.len() > MAX_VERSION || !s.iter().all(|&c| (0x20..0x7f).contains(&c)) {
        return None;
    }
    core::str::from_utf8(s).ok()
}

/// The Go toolchain version (e.g. `go1.27.1`) recorded in the executable
/// `data`, or `None` if it carries no (readable) build info.
pub fn go_version(data: &[u8]) -> Option<&str> {
    let h = find_header(data)?;
    let ptrsize = *data.get(h + 14)? as usize;
    let flags = *data.get(h + 15)?;
    if flags & FLAG_INLINE != 0 {
        let rest = data.get(h + HDR..)?;
        let (n, k) = uvarint(rest)?;
        let n = usize::try_from(n).ok()?;
        return printable(rest.get(k..k.checked_add(n)?)?);
    }
    // Pointer form: little-endian, 4- or 8-byte pointers only.
    if flags & FLAG_BIG_ENDIAN != 0 || !(ptrsize == 4 || ptrsize == 8) {
        return None;
    }
    let ptr = |off: usize| -> Option<u64> {
        if ptrsize == 8 {
            u64le(data, off)
        } else {
            u32le(data, off).map(u64::from)
        }
    };
    let hdr_va = ptr(h + 16)?;
    let hdr_off = va_to_off(data, hdr_va, 2 * ptrsize as u64)?;
    let str_va = ptr(hdr_off)?;
    let str_len = ptr(hdr_off + ptrsize)?;
    if str_len as usize > MAX_VERSION {
        return None;
    }
    let off = va_to_off(data, str_va, str_len)?;
    printable(data.get(off..off + str_len as usize)?)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::vec::Vec;

    fn inline_blob(version: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        b.push(8);
        b.push(FLAG_INLINE);
        b.resize(HDR, 0);
        b.push(version.len() as u8);
        b.extend_from_slice(version.as_bytes());
        b.push(0);
        b
    }

    #[test]
    fn inline_form_by_scan() {
        let mut data = std::vec![0u8; 48];
        data.extend(inline_blob("go1.27.0"));
        assert_eq!(go_version(&data), Some("go1.27.0"));
    }

    #[test]
    fn misaligned_magic_is_ignored() {
        let mut data = std::vec![0u8; 40];
        data.extend(inline_blob("go1.27.0"));
        assert_eq!(go_version(&data), None);
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(go_version(b""), None);
        assert_eq!(go_version(&[0xffu8; 4096]), None);
        let mut b = inline_blob("go1.2");
        b[HDR] = 0xff; // varint never ends within the buffer / too long
        assert_eq!(go_version(&b), None);
        let mut b = inline_blob("go\x01x");
        b[15] = FLAG_INLINE;
        assert_eq!(go_version(&b), None);
        // Pointer form pointing nowhere.
        let mut b = inline_blob("go1");
        b[15] = 0;
        assert_eq!(go_version(&b), None);
        // Truncated everywhere.
        let full = inline_blob("go1.27.0");
        for n in 0..full.len() {
            let _ = go_version(&full[..n]);
        }
    }

    #[test]
    fn pointer_form() {
        // Minimal ELF64: one PT_LOAD mapping the whole file at va 0x1000.
        let mut d = std::vec![0u8; 0x200];
        d[..4].copy_from_slice(b"\x7fELF");
        d[4] = 2;
        d[5] = 1;
        d[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // phoff
        d[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        d[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        let ph = 64;
        d[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
        d[ph + 0x10..ph + 0x18].copy_from_slice(&0x1000u64.to_le_bytes());
        d[ph + 0x20..ph + 0x28].copy_from_slice(&0x200u64.to_le_bytes());
        // Header at 0x100; string header at 0x140; string at 0x150.
        let h = 0x100;
        d[h..h + 14].copy_from_slice(MAGIC);
        d[h + 14] = 8;
        d[h + 15] = 0;
        d[h + 16..h + 24].copy_from_slice(&0x1140u64.to_le_bytes());
        d[0x140..0x148].copy_from_slice(&0x1150u64.to_le_bytes());
        d[0x148..0x150].copy_from_slice(&8u64.to_le_bytes());
        d[0x150..0x158].copy_from_slice(b"go1.16.3");
        assert_eq!(go_version(&d), Some("go1.16.3"));
    }

    fn have_go() -> bool {
        Command::new("go")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn real_go_binaries() {
        if !have_go() {
            std::eprintln!("skipping real_go_binaries: `go` not on PATH");
            return;
        }
        let want = Command::new("go")
            .args(["env", "GOVERSION"])
            .output()
            .unwrap();
        let want = std::string::String::from_utf8(want.stdout).unwrap();
        let want = want.trim();
        let dir: PathBuf =
            std::env::temp_dir().join(format!("patina-buildinfo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let apps = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testapps");
        for goarch in ["arm64", "amd64"] {
            let out = dir.join(format!("hello-{goarch}"));
            let st = Command::new("go")
                .current_dir(&apps)
                .env("GOOS", "linux")
                .env("GOARCH", goarch)
                .env("CGO_ENABLED", "0")
                .args(["build", "-trimpath", "-ldflags=-extldflags=-static", "-o"])
                .arg(&out)
                .arg("./hello")
                .status()
                .unwrap();
            assert!(st.success(), "go build {goarch}");
            let data = std::fs::read(&out).unwrap();
            assert!(section(&data, b".go.buildinfo").is_some(), "{goarch}");
            assert_eq!(go_version(&data), Some(want), "{goarch}");
            // Without the section table the scan still finds it.
            let mut stripped = data.clone();
            stripped[0x28..0x30].copy_from_slice(&0u64.to_le_bytes());
            assert_eq!(go_version(&stripped), Some(want), "{goarch} (scan)");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
