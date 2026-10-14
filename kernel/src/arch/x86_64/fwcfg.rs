//! QEMU fw_cfg through the legacy I/O ports (docs/specs/fw_cfg.rst):
//! selector 0x510 (16-bit write), data 0x511 (byte reads, sequential from
//! offset 0 of the selected item). Only used for CPU enumeration when no
//! RSDP / MP table is found in guest memory (QEMU `microvm` with PVH boot:
//! the ACPI tables exist only as fw_cfg files for the firmware's table
//! loader).

use super::port::{inb, outw};

const PORT_SEL: u16 = 0x510;
const PORT_DATA: u16 = 0x511;
const SEL_SIGNATURE: u16 = 0x00;
pub const SEL_NB_CPUS: u16 = 0x05;
const SEL_FILE_DIR: u16 = 0x19;

fn select(sel: u16) {
    // SAFETY: fw_cfg selector port; on machines without fw_cfg the write is
    // ignored (nothing else decodes 0x510).
    unsafe { outw(PORT_SEL, sel) };
}

fn read_bytes(buf: &mut [u8]) {
    for b in buf {
        // SAFETY: fw_cfg data port (reads 0xff/0 without a device).
        *b = unsafe { inb(PORT_DATA) };
    }
}

fn skip(mut n: usize) {
    let mut tmp = [0u8; 64];
    while n > 0 {
        let k = n.min(tmp.len());
        read_bytes(&mut tmp[..k]);
        n -= k;
    }
}

/// Whether a fw_cfg device answers ("QEMU" signature).
pub fn present() -> bool {
    select(SEL_SIGNATURE);
    let mut sig = [0u8; 4];
    read_bytes(&mut sig);
    &sig == b"QEMU"
}

/// A little-endian u16 item (e.g. [`SEL_NB_CPUS`]).
pub fn read_u16(sel: u16) -> u16 {
    select(sel);
    let mut b = [0u8; 2];
    read_bytes(&mut b);
    u16::from_le_bytes(b)
}

/// Look up `name` in the file directory: (selector, size).
pub fn find_file(name: &str) -> Option<(u16, usize)> {
    select(SEL_FILE_DIR);
    let mut cnt = [0u8; 4];
    read_bytes(&mut cnt);
    let n = u32::from_be_bytes(cnt).min(4096);
    for _ in 0..n {
        // struct FWCfgFile { be32 size; be16 select; u16 reserved; char name[56]; }
        let mut e = [0u8; 64];
        read_bytes(&mut e);
        let size = u32::from_be_bytes([e[0], e[1], e[2], e[3]]) as usize;
        let sel = u16::from_be_bytes([e[4], e[5]]);
        let raw = &e[8..];
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        if &raw[..len] == name.as_bytes() {
            return Some((sel, size));
        }
    }
    None
}

/// Read up to `buf.len()` bytes of item `sel` starting at `offset`;
/// returns the bytes read (`size` bounds the item).
pub fn read_item(sel: u16, size: usize, offset: usize, buf: &mut [u8]) -> usize {
    if offset >= size {
        return 0;
    }
    select(sel);
    skip(offset);
    let n = buf.len().min(size - offset);
    read_bytes(&mut buf[..n]);
    n
}
