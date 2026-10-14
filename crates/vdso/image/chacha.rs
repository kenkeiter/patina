//! ChaCha20 block function (RFC 8439 §2.3) for the vDSO's getrandom:
//! 64-bit block counter in words 12-13, zero nonce (as Linux's
//! `__arch_chacha20_blocks_nostack`). Shared with the host tests
//! (`patina_vdso::tests`), which check it against `patina_rand::chacha`.
//!
//! The state lives in 16 locals (no arrays, so no copies that could turn
//! into `memcpy` calls: the vDSO is linked with `--no-undefined` and has
//! no runtime) and the frame stays small.

macro_rules! qr {
    ($a:ident, $b:ident, $c:ident, $d:ident) => {
        $a = $a.wrapping_add($b);
        $d = ($d ^ $a).rotate_left(16);
        $c = $c.wrapping_add($d);
        $b = ($b ^ $c).rotate_left(12);
        $a = $a.wrapping_add($b);
        $d = ($d ^ $a).rotate_left(8);
        $c = $c.wrapping_add($d);
        $b = ($b ^ $c).rotate_left(7);
    };
}

const C0: u32 = 0x6170_7865;
const C1: u32 = 0x3320_646e;
const C2: u32 = 0x7962_2d32;
const C3: u32 = 0x6b20_6574;

/// Write keystream block `counter` of the key at `key` (8 native-endian
/// words) to the 64 bytes at `out` (any alignment). `out` may overlap the
/// key: the key is read completely before anything is written.
///
/// # Safety
/// `key` must be valid for 32 bytes of reads (4-aligned), `out` for 64
/// bytes of writes.
#[inline(never)]
pub unsafe fn block(key: *const u32, counter: u64, out: *mut u8) {
    // SAFETY: the caller's key, 8 words.
    let k: [u32; 8] = unsafe {
        [
            key.read(),
            key.add(1).read(),
            key.add(2).read(),
            key.add(3).read(),
            key.add(4).read(),
            key.add(5).read(),
            key.add(6).read(),
            key.add(7).read(),
        ]
    };
    let (n0, n1) = (counter as u32, (counter >> 32) as u32);
    let (mut x0, mut x1, mut x2, mut x3) = (C0, C1, C2, C3);
    let (mut x4, mut x5, mut x6, mut x7) = (k[0], k[1], k[2], k[3]);
    let (mut x8, mut x9, mut x10, mut x11) = (k[4], k[5], k[6], k[7]);
    let (mut x12, mut x13, mut x14, mut x15) = (n0, n1, 0u32, 0u32);
    let mut i = 0;
    while i < 10 {
        qr!(x0, x4, x8, x12);
        qr!(x1, x5, x9, x13);
        qr!(x2, x6, x10, x14);
        qr!(x3, x7, x11, x15);
        qr!(x0, x5, x10, x15);
        qr!(x1, x6, x11, x12);
        qr!(x2, x7, x8, x13);
        qr!(x3, x4, x9, x14);
        i += 1;
    }
    let words = [
        x0.wrapping_add(C0),
        x1.wrapping_add(C1),
        x2.wrapping_add(C2),
        x3.wrapping_add(C3),
        x4.wrapping_add(k[0]),
        x5.wrapping_add(k[1]),
        x6.wrapping_add(k[2]),
        x7.wrapping_add(k[3]),
        x8.wrapping_add(k[4]),
        x9.wrapping_add(k[5]),
        x10.wrapping_add(k[6]),
        x11.wrapping_add(k[7]),
        x12.wrapping_add(n0),
        x13.wrapping_add(n1),
        x14,
        x15,
    ];
    let aligned = (out as usize) & 3 == 0;
    let mut j = 0;
    while j < 16 {
        let v = words[j].to_le();
        // SAFETY: j < 16, so the 4 bytes are within the caller's 64.
        unsafe {
            let p = out.add(4 * j).cast::<u32>();
            if aligned {
                p.write(v)
            } else {
                p.write_unaligned(v)
            }
        };
        j += 1;
    }
}
