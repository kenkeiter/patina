//! The patina vDSO (decision D75).
//!
//! * [`IMAGE`]: the vDSO shared object for the target arch (both arches'
//!   images are built by `build.rs` from `image/`; [`IMAGE_X86_64`] and
//!   [`IMAGE_AARCH64`] are available everywhere for the host tests).
//! * [`vvar`]: the layout of the kernel-written page the vDSO reads.
//! * [`elf`]: a small ELF64 reader: what the kernel needs to map the image
//!   ([`elf::layout`]) and what the host tests check (symbols resolve the
//!   way Go's `runtime/vdso_linux.go` resolves them, no relocations, no
//!   writable data).

#![cfg_attr(not(test), no_std)]

pub mod elf;
pub mod vvar;

/// The x86_64 vDSO image.
pub static IMAGE_X86_64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso-x86_64.so"));
/// The aarch64 vDSO image.
pub static IMAGE_AARCH64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso-aarch64.so"));

/// The vDSO image for the arch being compiled for.
#[cfg(target_arch = "x86_64")]
pub static IMAGE: &[u8] = IMAGE_X86_64;
/// The vDSO image for the arch being compiled for.
#[cfg(not(target_arch = "x86_64"))]
pub static IMAGE: &[u8] = IMAGE_AARCH64;

/// (version name, version hash, [(symbol, hash)]).
pub type GoTable = (&'static str, u32, &'static [(&'static str, u32)]);

/// Go's symbol table per arch, copied from
/// `runtime/vdso_linux_{amd64,arm64}.go` (go1.27).
pub const GO_X86_64: GoTable = (
    "LINUX_2.6",
    0x3ae75f6,
    &[
        ("__vdso_gettimeofday", 0x315ca59),
        ("__vdso_clock_gettime", 0xd35ec75),
        ("__vdso_getrandom", 0x25425d),
    ],
);
/// See [`GO_X86_64`].
pub const GO_AARCH64: GoTable = (
    "LINUX_2.6.39",
    0x75fcb89,
    &[
        ("__kernel_clock_gettime", 0xb0cd725),
        ("__kernel_getrandom", 0x9800c0d),
    ],
);

#[cfg(test)]
#[path = "../image/chacha.rs"]
mod image_chacha;

#[cfg(test)]
mod tests {
    use super::elf::{self, Image};
    use super::*;

    fn images() -> [(&'static [u8], u16, GoTable); 2] {
        [
            (IMAGE_X86_64, elf::EM_X86_64, GO_X86_64),
            (IMAGE_AARCH64, elf::EM_AARCH64, GO_AARCH64),
        ]
    }

    #[test]
    fn sysv_hash_matches_go_tables() {
        for (_, _, (ver, vh, syms)) in images() {
            assert_eq!(elf::sysv_hash(ver.as_bytes()), vh, "{ver}");
            for (s, h) in syms {
                assert_eq!(elf::sysv_hash(s.as_bytes()), *h, "{s}");
            }
        }
    }

    #[test]
    fn images_are_clean_shared_objects() {
        for (bytes, machine, _) in images() {
            let img = Image::parse(bytes).expect("parse");
            assert_eq!(&bytes[..4], b"\x7fELF");
            assert_eq!(img.e_type, elf::ET_DYN);
            assert_eq!(img.machine, machine);
            let load = img.load().expect("one PT_LOAD");
            assert_eq!((load.vaddr, load.offset), (0, 0), "linked at 0");
            assert_eq!(load.flags, elf::PF_R | elf::PF_X, "read-exec only");
            assert_eq!(load.filesz, load.memsz, "no .bss");
            assert_eq!(img.phdrs().filter(|p| p.p_type == elf::PT_LOAD).count(), 1);
            let dynamic = img
                .phdrs()
                .find(|p| p.p_type == elf::PT_DYNAMIC)
                .expect("PT_DYNAMIC");
            assert!(dynamic.offset + dynamic.filesz <= load.filesz);
            img.check_no_relocations().expect("no relocations");
            img.check_no_writable_sections().expect("no writable data");
            let d = img.dynamic().expect("dynamic");
            assert!(d.hash.is_some() && d.symtab.is_some() && d.strtab.is_some());
            assert!(d.versym.is_some() && d.verdef.is_some());
            assert_eq!(d.syment, Some(24));
        }
    }

    #[test]
    fn go_symbols_resolve_with_their_version() {
        for (bytes, _, (ver, vh, syms)) in images() {
            let img = Image::parse(bytes).unwrap();
            let load = img.load().unwrap();
            for (s, _) in syms {
                let v = img
                    .lookup(s.as_bytes(), ver.as_bytes(), vh)
                    .unwrap_or_else(|| panic!("{s}@{ver} not found"));
                assert!(v > 0 && v < load.memsz, "{s} at {v:#x}");
            }
            // A wrong version does not match.
            assert!(
                img.lookup(syms[0].0.as_bytes(), b"LINUX_9", 0x1234)
                    .is_none()
            );
        }
    }

    #[test]
    fn vvar_follows_the_image() {
        for (bytes, _, _) in images() {
            let img = Image::parse(bytes).unwrap();
            let l = elf::layout(bytes).unwrap();
            assert_eq!(l.text_len % 4096, 0);
            assert_eq!(l.vvar_offset, l.text_len);
            let sym = img.local_symbol(b"__vvar_page").expect("__vvar_page");
            assert_eq!(sym as usize, l.vvar_offset, "code's vvar == mapped vvar");
            assert!(l.text_len <= 2 * 4096, "image is {} bytes", l.text_len);
        }
    }

    #[test]
    fn chacha_matches_rfc8439_implementation() {
        let mut key = [0u8; 32];
        for (i, k) in key.iter_mut().enumerate() {
            *k = (i * 7 + 3) as u8;
        }
        let words: [u32; 8] =
            core::array::from_fn(|i| u32::from_le_bytes(key[4 * i..4 * i + 4].try_into().unwrap()));
        for ctr in [0u64, 1, 2, 77, 0xffff_ffff, 0x1_0000_0005] {
            let mut ours = [0u8; 65];
            // Unaligned output.
            unsafe { image_chacha::block(words.as_ptr(), ctr, ours.as_mut_ptr().add(1)) };
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&((ctr >> 32) as u32).to_le_bytes());
            let mut want = [0u8; 64];
            patina_rand::chacha::block(&key, ctr as u32, &nonce, &mut want);
            assert_eq!(&ours[1..], &want[..], "counter {ctr:#x}");
        }
    }

    #[test]
    fn counter_scaling() {
        // 1 GHz x86 TSC: mult = 1.0 in 32.32.
        assert_eq!(vvar::mult32_to_ns(12345, 1 << 32), 12345);
        // 24 MHz arm counter.
        assert_eq!(vvar::freq_to_ns(24_000_000, 24_000_000), 1_000_000_000);
        assert_eq!(vvar::freq_to_ns(24, 24_000_000), 1000);
        // No overflow after a year at 1 GHz.
        let year = 365 * 86_400 * 1_000_000_000u64;
        assert_eq!(vvar::freq_to_ns(year, 1_000_000_000), year);
    }

    #[test]
    fn seqlock_roundtrip() {
        let v = vvar::Vvar::new();
        v.write(|v| {
            v.scale.store(5, core::sync::atomic::Ordering::Relaxed);
            v.realtime_offset
                .store(9, core::sync::atomic::Ordering::Relaxed);
        });
        let c = v.clock();
        assert_eq!((c.scale, c.realtime_offset), (5, 9));
        assert_eq!(v.seq.load(core::sync::atomic::Ordering::Relaxed), 2);
        assert!(core::mem::size_of::<vvar::Vvar>() <= vvar::VVAR_SIZE);
    }
}
