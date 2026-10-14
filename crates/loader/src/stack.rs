//! Linux-exact initial user stack image (design §4 "Stacks", §7 auxv).
//!
//! Mirrors `fs/exec.c` (`__bprm_mm_init`, `copy_strings`) and
//! `fs/binfmt_elf.c` (`create_elf_tables`) with no stack randomization.
//! From `top` downwards:
//!
//! ```text
//! top - 8      8 zero bytes (Linux starts at vm_end - sizeof(void *))
//!              execfn\0                      <- AT_EXECFN
//!              envp[n-1]\0 ... envp[0]\0     (envp[0] lowest)
//!              argv[n-1]\0 ... argv[0]\0     (argv[0] lowest)
//!              align down to 16              (arch_align_stack)
//!              platform\0                    <- AT_PLATFORM (omitted if empty)
//!              16 random bytes               <- AT_RANDOM
//!              padding                       (STACK_ROUND: makes sp 16-aligned)
//!              auxv pairs ..., (AT_NULL, 0)
//!              NULL, envp pointers
//!              NULL, argv pointers
//! sp ->        argc
//! ```
//!
//! So strings are ascending in memory: `argv[0] .. argv[n-1] envp[0] ..
//! envp[m-1] execfn`, exactly like Linux.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use patina_abi::auxv::{AT_EXECFN, AT_NULL, AT_PLATFORM, AT_RANDOM, AT_RANDOM_LEN};

/// The finished stack: write `bytes` at user address `sp`; then
/// `sp + bytes.len() == top` and `sp % 16 == 0`. Start the program with the
/// stack pointer at `sp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackImage {
    pub sp: u64,
    pub bytes: Vec<u8>,
}

/// Stack build / decode failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// `top` is not 16-byte aligned.
    MisalignedTop(u64),
    /// An argv/envp/platform/execfn string contains a NUL byte.
    InteriorNul,
    /// The caller passed an auxv key that [`build`] adds itself
    /// (`AT_RANDOM`, `AT_PLATFORM`, `AT_EXECFN`) or `AT_NULL`.
    ReservedAuxKey(u64),
    /// The image does not fit below `top` (address underflow).
    TooLarge,
    /// [`parse`] found a malformed image.
    Decode(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::MisalignedTop(t) => write!(f, "stack top {t:#x} is not 16-byte aligned"),
            Error::InteriorNul => f.write_str("argument/environment string contains a NUL byte"),
            Error::ReservedAuxKey(k) => {
                write!(f, "auxv key {k} is reserved: the stack builder supplies it")
            }
            Error::TooLarge => f.write_str("initial stack image does not fit below the stack top"),
            Error::Decode(r) => write!(f, "malformed stack image: {r}"),
        }
    }
}

impl core::error::Error for Error {}

const PTR: u64 = 8;

/// Build the initial stack image ending at `top` (exclusive).
///
/// `auxv` holds the entries that need no stack address (`AT_PAGESZ`,
/// `AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_ENTRY`, `AT_HWCAP`, ...), in the
/// order they should appear. `build` appends `AT_RANDOM` (pointing at a
/// copy of `random`), `AT_PLATFORM` (pointing at `platform`; skipped when
/// `platform` is empty, as Linux does without `ELF_PLATFORM`) and
/// `AT_EXECFN` (pointing at `execfn`), then `AT_NULL`.
pub fn build(
    top: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    auxv: &[(u64, u64)],
    random: &[u8; 16],
    platform: &[u8],
    execfn: &[u8],
) -> Result<StackImage, Error> {
    if !top.is_multiple_of(16) {
        return Err(Error::MisalignedTop(top));
    }
    let strs = argv.iter().chain(envp.iter()).chain([&platform, &execfn]);
    if strs.into_iter().any(|s| s.contains(&0)) {
        return Err(Error::InteriorNul);
    }
    if let Some(&(k, _)) = auxv
        .iter()
        .find(|(k, _)| matches!(*k, AT_NULL | AT_RANDOM | AT_PLATFORM | AT_EXECFN))
    {
        return Err(Error::ReservedAuxKey(k));
    }

    let sub = |p: u64, n: u64| p.checked_sub(n).ok_or(Error::TooLarge);
    let slen = |s: &[u8]| s.len() as u64 + 1;

    // copy_strings: filename first, then envp, then argv; each list from
    // its last element down.
    let mut p = sub(top, PTR)?;
    p = sub(p, slen(execfn))?;
    let execfn_addr = p;
    let mut env_addrs = vec![0u64; envp.len()];
    for (i, s) in envp.iter().enumerate().rev() {
        p = sub(p, slen(s))?;
        env_addrs[i] = p;
    }
    let mut arg_addrs = vec![0u64; argv.len()];
    for (i, s) in argv.iter().enumerate().rev() {
        p = sub(p, slen(s))?;
        arg_addrs[i] = p;
    }

    // create_elf_tables
    p &= !15;
    let platform_addr = if platform.is_empty() {
        None
    } else {
        p = sub(p, slen(platform))?;
        Some(p)
    };
    p = sub(p, AT_RANDOM_LEN as u64)?;
    let random_addr = p;

    let mut aux: Vec<(u64, u64)> = Vec::with_capacity(auxv.len() + 4);
    aux.extend_from_slice(auxv);
    aux.push((AT_RANDOM, random_addr));
    if let Some(a) = platform_addr {
        aux.push((AT_PLATFORM, a));
    }
    aux.push((AT_EXECFN, execfn_addr));
    aux.push((AT_NULL, 0));

    let words = (argv.len() as u64 + 1) + (envp.len() as u64 + 1) + 1 + 2 * aux.len() as u64;
    let sp = sub(p, words.checked_mul(PTR).ok_or(Error::TooLarge)?)? & !15;

    let len = usize::try_from(top - sp).map_err(|_| Error::TooLarge)?;
    let mut bytes = vec![0u8; len];
    let mut put = |addr: u64, b: &[u8]| {
        let o = (addr - sp) as usize;
        bytes[o..o + b.len()].copy_from_slice(b);
    };
    put(execfn_addr, execfn);
    for (a, s) in env_addrs.iter().zip(envp) {
        put(*a, s);
    }
    for (a, s) in arg_addrs.iter().zip(argv) {
        put(*a, s);
    }
    if let Some(a) = platform_addr {
        put(a, platform);
    }
    put(random_addr, random);

    let mut w = sp;
    let mut word = |v: u64| {
        put(w, &v.to_le_bytes());
        w += PTR;
    };
    word(argv.len() as u64);
    for a in &arg_addrs {
        word(*a);
    }
    word(0);
    for a in &env_addrs {
        word(*a);
    }
    word(0);
    for (k, v) in &aux {
        word(*k);
        word(*v);
    }
    debug_assert!(w <= random_addr);

    Ok(StackImage { sp, bytes })
}

/// A stack image decoded by [`parse`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    pub argc: u64,
    pub argv: Vec<Vec<u8>>,
    pub envp: Vec<Vec<u8>>,
    /// auxv pairs in order, without the terminating `AT_NULL`.
    pub auxv: Vec<(u64, u64)>,
    /// User address one past the `AT_NULL` pair (end of the vectors).
    pub vectors_end: u64,
}

impl Decoded {
    /// Value of the first auxv entry with `key`.
    pub fn aux(&self, key: u64) -> Option<u64> {
        self.auxv.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

fn word_at(sp: u64, bytes: &[u8], addr: u64) -> Result<u64, Error> {
    let o = addr
        .checked_sub(sp)
        .ok_or(Error::Decode("pointer below sp"))? as usize;
    let b = o
        .checked_add(8)
        .and_then(|e| bytes.get(o..e))
        .ok_or(Error::Decode("word past top"))?;
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    Ok(u64::from_le_bytes(a))
}

/// The NUL-terminated string at user address `addr` in an image starting
/// at `sp` (without the NUL).
pub fn read_cstr(sp: u64, bytes: &[u8], addr: u64) -> Result<&[u8], Error> {
    let o = addr
        .checked_sub(sp)
        .ok_or(Error::Decode("string pointer below sp"))?;
    let tail = usize::try_from(o)
        .ok()
        .and_then(|o| bytes.get(o..))
        .ok_or(Error::Decode("string pointer past top"))?;
    let n = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or(Error::Decode("unterminated string"))?;
    Ok(&tail[..n])
}

/// Walk argc/argv/envp/auxv back out of an image (`bytes` at `sp`, ending
/// at `top`). Used by tests and handy for kernel self-checks.
pub fn parse(sp: u64, bytes: &[u8], top: u64) -> Result<Decoded, Error> {
    if sp.checked_add(bytes.len() as u64) != Some(top) {
        return Err(Error::Decode("sp + len != top"));
    }
    let mut d = Decoded::default();
    let mut a = sp;
    let next = |a: &mut u64| -> Result<u64, Error> {
        let v = word_at(sp, bytes, *a)?;
        *a += PTR;
        Ok(v)
    };
    d.argc = next(&mut a)?;
    loop {
        let p = next(&mut a)?;
        if p == 0 {
            break;
        }
        d.argv.push(read_cstr(sp, bytes, p)?.to_vec());
    }
    if d.argv.len() as u64 != d.argc {
        return Err(Error::Decode("argc does not match argv"));
    }
    loop {
        let p = next(&mut a)?;
        if p == 0 {
            break;
        }
        d.envp.push(read_cstr(sp, bytes, p)?.to_vec());
    }
    loop {
        let k = next(&mut a)?;
        let v = next(&mut a)?;
        if k == AT_NULL {
            break;
        }
        d.auxv.push((k, v));
    }
    d.vectors_end = a;
    Ok(d)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::vec::Vec;
    use patina_abi::auxv::*;

    use super::*;

    const TOP: u64 = 0x7fff_ffff_f000;
    const RANDOM: [u8; 16] = *b"0123456789abcdef";

    fn base_auxv() -> Vec<(u64, u64)> {
        vec![
            (AT_PAGESZ, 4096),
            (AT_PHDR, 0x400040),
            (AT_PHENT, 56),
            (AT_PHNUM, 6),
            (AT_ENTRY, 0x401000),
            (AT_HWCAP, 0x1234),
            (AT_SECURE, 0),
        ]
    }

    /// Build, decode and check every property; returns the image.
    fn check(argv: &[&[u8]], envp: &[&[u8]], platform: &[u8], execfn: &[u8]) -> StackImage {
        let aux = base_auxv();
        let img = build(TOP, argv, envp, &aux, &RANDOM, platform, execfn).unwrap();
        assert_eq!(img.sp % 16, 0, "sp alignment");
        assert_eq!(img.sp + img.bytes.len() as u64, TOP);
        let d = parse(img.sp, &img.bytes, TOP).unwrap();
        assert_eq!(d.argc, argv.len() as u64);
        let argv_v: Vec<Vec<u8>> = argv.iter().map(|s| s.to_vec()).collect();
        let envp_v: Vec<Vec<u8>> = envp.iter().map(|s| s.to_vec()).collect();
        assert_eq!(d.argv, argv_v);
        assert_eq!(d.envp, envp_v);

        // auxv: caller entries in order, then RANDOM, PLATFORM, EXECFN.
        let n = aux.len();
        assert_eq!(&d.auxv[..n], &aux[..]);
        let mut keys: Vec<u64> = d.auxv[n..].iter().map(|(k, _)| *k).collect();
        let mut want = vec![AT_RANDOM];
        if !platform.is_empty() {
            want.push(AT_PLATFORM);
        }
        want.push(AT_EXECFN);
        assert_eq!(keys, want);
        keys.clear();

        let rnd = d.aux(AT_RANDOM).unwrap();
        let ro = (rnd - img.sp) as usize;
        assert_eq!(img.bytes[ro..ro + 16], RANDOM);
        let ef = d.aux(AT_EXECFN).unwrap();
        assert_eq!(read_cstr(img.sp, &img.bytes, ef).unwrap(), execfn);
        // Linux leaves the top pointer-sized slot zero; execfn ends just below it.
        assert_eq!(ef + execfn.len() as u64 + 1, TOP - 8);
        assert_eq!(img.bytes[img.bytes.len() - 8..], [0u8; 8]);

        // Regions [start, end) that must be pairwise disjoint and in range.
        let mut regions: Vec<(u64, u64, &str)> = vec![
            (img.sp, d.vectors_end, "vectors"),
            (rnd, rnd + 16, "random"),
            (ef, ef + execfn.len() as u64 + 1, "execfn"),
        ];
        if let Some(pl) = d.aux(AT_PLATFORM) {
            assert_eq!(read_cstr(img.sp, &img.bytes, pl).unwrap(), platform);
            regions.push((pl, pl + platform.len() as u64 + 1, "platform"));
            // platform sits directly above random, below 16-aligned strings.
            assert_eq!(pl, rnd + 16);
        } else {
            assert!(platform.is_empty());
        }
        // Gap between the vectors and random is < 16 bytes (STACK_ROUND only).
        assert!(rnd - d.vectors_end < 16, "padding {}", rnd - d.vectors_end);
        // String pointers, read back from the vectors.
        let ptrs = |base: u64, n: usize| -> Vec<u64> {
            (0..n as u64)
                .map(|i| word_at(img.sp, &img.bytes, base + 8 * i).unwrap())
                .collect()
        };
        let argp = ptrs(img.sp + 8, argv.len());
        let envp_p = ptrs(img.sp + 8 + 8 * (argv.len() as u64 + 1), envp.len());
        for (p, s) in argp.iter().zip(argv) {
            regions.push((*p, p + s.len() as u64 + 1, "arg"));
        }
        for (p, s) in envp_p.iter().zip(envp) {
            regions.push((*p, p + s.len() as u64 + 1, "env"));
        }
        // Linux order: argv strings lowest, then envp, then execfn, contiguous.
        let mut cursor = argp.first().or(envp_p.first()).copied().unwrap_or(ef);
        for (p, s) in argp.iter().zip(argv).chain(envp_p.iter().zip(envp)) {
            assert_eq!(*p, cursor);
            cursor += s.len() as u64 + 1;
        }
        assert_eq!(cursor, ef);

        regions.sort();
        for r in &regions {
            assert!(r.0 >= img.sp && r.1 <= TOP && r.0 <= r.1, "{r:?}");
        }
        for w in regions.windows(2) {
            assert!(w[0].1 <= w[1].0, "overlap {:?} {:?}", w[0], w[1]);
        }
        img
    }

    #[test]
    fn typical() {
        let img = check(
            &[b"/app", b"--port", b"8080"],
            &[b"HOME=/", b"PATH=/bin", b"TERM=linux"],
            b"x86_64",
            b"/app",
        );
        // Hand-computed layout for this exact input.
        // execfn "/app\0" at TOP-8-5; envp total 7+10+11=28; argv 5+7+5=17.
        let ef = TOP - 8 - 5;
        let argv0 = ef - 28 - 17;
        let aligned = argv0 & !15;
        let platform = aligned - 7;
        let random = platform - 16;
        // words: argc 1 + argv 4 + envp 4 + auxv 2*(7+3+1)=22 → 31 words.
        let sp = (random - 31 * 8) & !15;
        assert_eq!(img.sp, sp);
        let d = parse(img.sp, &img.bytes, TOP).unwrap();
        assert_eq!(d.aux(AT_EXECFN), Some(ef));
        assert_eq!(d.aux(AT_PLATFORM), Some(platform));
        assert_eq!(d.aux(AT_RANDOM), Some(random));
        assert_eq!(word_at(img.sp, &img.bytes, img.sp + 8).unwrap(), argv0);
    }

    #[test]
    fn empty_envp() {
        let img = check(&[b"/app"], &[], b"aarch64", b"/app");
        let d = parse(img.sp, &img.bytes, TOP).unwrap();
        assert!(d.envp.is_empty());
        // argc, argv0, NULL, NULL(envp), then auxv.
        assert_eq!(word_at(img.sp, &img.bytes, img.sp + 16).unwrap(), 0);
        assert_eq!(word_at(img.sp, &img.bytes, img.sp + 24).unwrap(), 0);
        assert_eq!(word_at(img.sp, &img.bytes, img.sp + 32).unwrap(), AT_PAGESZ);
    }

    #[test]
    fn no_args_no_env_no_platform() {
        check(&[], &[], b"", b"/app");
        check(&[b""], &[b""], b"", b"");
    }

    #[test]
    fn many_args() {
        let owned: Vec<Vec<u8>> = (0..1000)
            .map(|i| format!("arg-{i}-{}", "x".repeat(i % 37)).into_bytes())
            .collect();
        let argv: Vec<&[u8]> = owned.iter().map(|v| v.as_slice()).collect();
        let envs: Vec<Vec<u8>> = (0..300)
            .map(|i| format!("K{i}=v{i}").into_bytes())
            .collect();
        let envp: Vec<&[u8]> = envs.iter().map(|v| v.as_slice()).collect();
        check(&argv, &envp, b"aarch64", b"/app");
    }

    #[test]
    fn alignment_for_every_padding() {
        // Vary string lengths so every residue of the pre-vector padding occurs.
        for n in 0..40 {
            let a = "a".repeat(n);
            for argc in 1..4 {
                let argv: Vec<&[u8]> = (0..argc).map(|_| a.as_bytes()).collect();
                check(&argv, &[b"E=1"], b"x86_64", b"/app");
                check(&argv, &[], &a.as_bytes()[..n % 9], b"/app");
            }
        }
    }

    #[test]
    fn errors() {
        let a = base_auxv();
        assert_eq!(
            build(TOP + 8, &[], &[], &a, &RANDOM, b"", b"/app"),
            Err(Error::MisalignedTop(TOP + 8))
        );
        assert_eq!(
            build(TOP, &[b"a\0b"], &[], &a, &RANDOM, b"", b"/app"),
            Err(Error::InteriorNul)
        );
        assert_eq!(
            build(TOP, &[], &[], &a, &RANDOM, b"x\0", b"/app"),
            Err(Error::InteriorNul)
        );
        for k in [AT_NULL, AT_RANDOM, AT_PLATFORM, AT_EXECFN] {
            assert_eq!(
                build(
                    TOP,
                    &[],
                    &[],
                    &[(AT_PAGESZ, 4096), (k, 1)],
                    &RANDOM,
                    b"",
                    b"/app"
                ),
                Err(Error::ReservedAuxKey(k))
            );
        }
        assert_eq!(
            build(64, &[b"/app"], &[], &a, &RANDOM, b"x86_64", b"/app"),
            Err(Error::TooLarge)
        );
        // Decoder rejects garbage.
        let img = build(TOP, &[b"/app"], &[], &a, &RANDOM, b"", b"/app").unwrap();
        assert!(parse(img.sp + 16, &img.bytes, TOP).is_err());
        assert!(parse(img.sp, &img.bytes[..img.bytes.len() - 8], TOP).is_err());
        let mut b = img.bytes.clone();
        b[0] = 7; // argc mismatch
        assert!(parse(img.sp, &b, TOP).is_err());
        let mut b = img.bytes.clone();
        b[8..16].copy_from_slice(&(img.sp - 8).to_le_bytes()); // argv[0] below sp
        assert!(parse(img.sp, &b, TOP).is_err());
        assert!(!format!("{}", Error::TooLarge).is_empty());
    }
}
