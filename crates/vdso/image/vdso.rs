//! The patina vDSO (decision D75): `clock_gettime`, `gettimeofday`,
//! `time`, `clock_getres` and `getrandom` without a syscall.
//!
//! Built by `crates/vdso/build.rs` as a standalone object (`rustc
//! --emit=obj`, no core library code linked in) and linked by rust-lld
//! into a position-independent shared object with `image/vdso.lds` and the
//! per-arch version script. It must not have relocations, undefined
//! symbols, writable data or more than a few hundred bytes of stack (Go
//! calls `getrandom` on the goroutine stack).
//!
//! The kernel maps the image read-exec and the vvar page read-only right
//! after it; `__vvar_page` (defined by the linker script) is reached
//! PC-relatively.
//!
//! Exported names are the ones Go looks up: amd64 `__vdso_*` in version
//! `LINUX_2.6`, arm64 `__kernel_*` in `LINUX_2.6.39`.

#![no_std]
#![no_builtins]
#![allow(clippy::missing_safety_doc)]

use core::arch::asm;
use core::sync::atomic::{Ordering, compiler_fence};

#[path = "../src/vvar.rs"]
#[allow(dead_code)]
mod vvar;

#[path = "chacha.rs"]
mod chacha;

use vvar::{CLOCK_MODE_COUNTER, Vvar};

#[repr(C)]
pub struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
pub struct Timeval {
    sec: i64,
    usec: i64,
}

#[repr(C)]
pub struct Timezone {
    minuteswest: i32,
    dsttime: i32,
}

const CLOCK_REALTIME: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_REALTIME_COARSE: i32 = 5;
const CLOCK_MONOTONIC_COARSE: i32 = 6;
const CLOCK_BOOTTIME: i32 = 7;
const CLOCK_TAI: i32 = 11;

const GRND_NONBLOCK: u32 = 1;
const GRND_INSECURE: u32 = 4;

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const GETTIMEOFDAY: usize = 96;
    pub const CLOCK_GETTIME: usize = 228;
    pub const CLOCK_GETRES: usize = 229;
    pub const GETRANDOM: usize = 318;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const GETTIMEOFDAY: usize = 169;
    pub const CLOCK_GETTIME: usize = 113;
    pub const CLOCK_GETRES: usize = 114;
    pub const GETRANDOM: usize = 278;
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn syscall3(n: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    // SAFETY: a Linux syscall; the kernel preserves everything but
    // rax/rcx/r11.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as isize => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        )
    };
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn syscall3(n: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    // SAFETY: a Linux syscall; the kernel preserves everything but x0.
    unsafe {
        asm!(
            "svc #0",
            in("x8") n,
            inlateout("x0") a0 as isize => ret,
            in("x1") a1,
            in("x2") a2,
            options(nostack)
        )
    };
    ret
}

/// The vvar page (placed after the image by the linker script).
#[inline(always)]
fn vvar() -> &'static Vvar {
    let p: *const Vvar;
    // SAFETY: address computation only.
    #[cfg(target_arch = "x86_64")]
    unsafe {
        asm!("lea {}, [rip + __vvar_page]", out(reg) p, options(pure, nomem, nostack, preserves_flags))
    };
    #[cfg(target_arch = "aarch64")]
    unsafe {
        asm!(
            "adrp {0}, __vvar_page",
            "add {0}, {0}, :lo12:__vvar_page",
            out(reg) p,
            options(pure, nomem, nostack, preserves_flags)
        )
    };
    // SAFETY: the kernel maps the vvar page there for the program's life.
    unsafe { &*p }
}

/// The raw counter, ordered after earlier loads (as Linux's
/// `rdtsc_ordered` / `__arch_get_hw_counter`).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn counter() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC is allowed at CPL 3 (CR4.TSD clear).
    unsafe {
        asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags))
    };
    ((hi as u64) << 32) | lo as u64
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn counter() -> u64 {
    let v: u64;
    // SAFETY: CNTKCTL_EL1.EL0VCTEN lets EL0 read CNTVCT_EL0.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// (is realtime) for the clocks served here; `None` = take the syscall.
#[inline(always)]
fn fast_clock(clk: i32) -> Option<bool> {
    match clk {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_TAI => Some(true),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            Some(false)
        }
        _ => None,
    }
}

/// ns for a fast clock, or `None` when the kernel published no counter.
#[inline(always)]
fn now_ns(realtime: bool) -> Option<u64> {
    let c = vvar().clock();
    if c.mode != CLOCK_MODE_COUNTER || c.scale == 0 {
        return None;
    }
    let mono = vvar::counter_to_ns(counter().wrapping_sub(c.counter_base), c.scale);
    Some(if realtime {
        mono.wrapping_add(c.realtime_offset)
    } else {
        mono
    })
}

#[inline(always)]
unsafe fn do_clock_gettime(clk: i32, ts: *mut Timespec) -> i32 {
    if let Some(rt) = fast_clock(clk)
        && let Some(ns) = now_ns(rt)
    {
        // SAFETY: the caller's pointer, as for the syscall (a bad one
        // faults here instead of returning EFAULT, as on Linux).
        unsafe {
            (*ts).sec = (ns / 1_000_000_000) as i64;
            (*ts).nsec = (ns % 1_000_000_000) as i64;
        }
        return 0;
    }
    // SAFETY: plain syscall.
    unsafe { syscall3(nr::CLOCK_GETTIME, clk as usize, ts as usize, 0) as i32 }
}

#[inline(always)]
unsafe fn do_gettimeofday(tv: *mut Timeval, tz: *mut Timezone) -> i32 {
    match now_ns(true) {
        Some(ns) => {
            // SAFETY: the caller's pointers (null is allowed).
            unsafe {
                if !tv.is_null() {
                    (*tv).sec = (ns / 1_000_000_000) as i64;
                    (*tv).usec = ((ns % 1_000_000_000) / 1000) as i64;
                }
                if !tz.is_null() {
                    (*tz).minuteswest = 0;
                    (*tz).dsttime = 0;
                }
            }
            0
        }
        // SAFETY: plain syscall.
        None => unsafe { syscall3(nr::GETTIMEOFDAY, tv as usize, tz as usize, 0) as i32 },
    }
}

#[inline(always)]
unsafe fn do_clock_getres(clk: i32, res: *mut Timespec) -> i32 {
    if fast_clock(clk).is_some() && vvar().clock().mode == CLOCK_MODE_COUNTER {
        if !res.is_null() {
            // SAFETY: the caller's pointer. 1 ns, as the syscall reports.
            unsafe {
                (*res).sec = 0;
                (*res).nsec = 1;
            }
        }
        return 0;
    }
    // SAFETY: plain syscall.
    unsafe { syscall3(nr::CLOCK_GETRES, clk as usize, res as usize, 0) as i32 }
}

// ---------------------------------------------------------------------
// getrandom (Linux 6.11 vgetrandom contract).

/// Bytes of buffered keystream in a state.
const BATCH: usize = 96;

/// The per-thread opaque state (`opaque_state`), in memory the program
/// mmaps with the parameters we report. `batch` and `key` are contiguous:
/// one refill writes two ChaCha20 blocks over both (fast key erasure: the
/// key that produced them is overwritten).
#[repr(C)]
struct State {
    batch: [u8; BATCH],
    key: [u32; 8],
    generation: u64,
    pos: u32,
    in_use: u32,
}

const STATE_SIZE: usize = core::mem::size_of::<State>();
const _: () = assert!(STATE_SIZE == 144);

/// `struct vgetrandom_opaque_params`.
#[repr(C)]
struct OpaqueParams {
    size_of_opaque_state: u32,
    mmap_prot: u32,
    mmap_flags: u32,
    reserved: [u32; 13],
}

const PROT_READ: u32 = 1;
const PROT_WRITE: u32 = 2;
const MAP_PRIVATE: u32 = 0x02;
const MAP_ANONYMOUS: u32 = 0x20;
/// Linux `MAX_RW_COUNT` (on 4 KiB pages).
const MAX_RW_COUNT: usize = 0x7fff_f000;

#[inline(always)]
unsafe fn zero(p: *mut u8, n: usize) {
    let mut i = 0;
    while i < n {
        // SAFETY: within the caller's range; volatile so it is not elided.
        unsafe { p.add(i).write_volatile(0) };
        i += 1;
    }
}

/// Copy `n` bytes of the batch from `pos` to `out` and wipe them.
#[inline(always)]
unsafe fn take(st: *mut State, pos: usize, out: *mut u8, n: usize) {
    // SAFETY: pos + n <= BATCH (callers), out valid for n bytes.
    unsafe {
        let src = (st as *mut u8).add(pos);
        let mut i = 0;
        while i < n {
            out.add(i).write(src.add(i).read());
            i += 1;
        }
        zero(src, n);
    }
}

unsafe fn do_getrandom(
    buf: *mut u8,
    len: usize,
    flags: u32,
    opaque: *mut u8,
    opaque_len: usize,
) -> isize {
    // The parameter query: getrandom(NULL, 0, 0, &params, ~0UL).
    if opaque_len == usize::MAX && buf.is_null() && len == 0 && flags == 0 {
        if opaque.is_null() {
            return -14; // EFAULT
        }
        let p = opaque as *mut OpaqueParams;
        // SAFETY: the caller's params struct.
        unsafe {
            (*p).size_of_opaque_state = STATE_SIZE as u32;
            (*p).mmap_prot = PROT_READ | PROT_WRITE;
            (*p).mmap_flags = MAP_PRIVATE | MAP_ANONYMOUS;
            let mut i = 0;
            while i < 13 {
                (*p).reserved[i] = 0;
                i += 1;
            }
        }
        return 0;
    }
    let v = vvar();
    let len = if len > MAX_RW_COUNT { MAX_RW_COUNT } else { len };
    let st = opaque as *mut State;
    // SAFETY (whole block): `st` is the caller's state of STATE_SIZE
    // bytes (checked size, alignment), `buf` is valid for `len` bytes.
    unsafe {
        if opaque.is_null()
            || opaque_len != STATE_SIZE
            || !(opaque as usize).is_multiple_of(8)
            || flags & !(GRND_NONBLOCK | GRND_INSECURE) != 0
            || v.rng_ready.load(Ordering::Acquire) == 0
            || (*st).in_use != 0
        {
            return syscall3(nr::GETRANDOM, buf as usize, len, flags as usize);
        }
        if len == 0 {
            return 0;
        }
        (*st).in_use = 1;
        compiler_fence(Ordering::SeqCst);
        loop {
            let generation = v.rng_generation.load(Ordering::Acquire);
            if (*st).generation != generation {
                // New key from the kernel; the batch is empty.
                let key = (&raw mut (*st).key) as *mut u8;
                let r = syscall3(nr::GETRANDOM, key as usize, 32, 0);
                if r != 32 {
                    compiler_fence(Ordering::SeqCst);
                    (*st).in_use = 0;
                    return syscall3(nr::GETRANDOM, buf as usize, len, flags as usize);
                }
                (*st).generation = generation;
                (*st).pos = BATCH as u32;
            }
            let mut out = buf;
            let mut left = len;
            let mut pos = (*st).pos as usize;
            if pos > BATCH {
                pos = BATCH;
            }
            let n = if left < BATCH - pos { left } else { BATCH - pos };
            take(st, pos, out, n);
            pos += n;
            out = out.add(n);
            left -= n;
            if left > 0 {
                let key = (&raw const (*st).key) as *const u32;
                let mut ctr = 0u64;
                while left >= 64 {
                    chacha::block(key, ctr, out);
                    ctr += 1;
                    out = out.add(64);
                    left -= 64;
                }
                // Next batch and key: blocks ctr and ctr+1 over batch ‖
                // key (the second overwrites the key it is made from).
                let b = st as *mut u8;
                chacha::block(key, ctr, b);
                chacha::block(key, ctr + 1, b.add(64));
                take(st, 0, out, left);
                pos = left;
            }
            (*st).pos = pos as u32;
            if v.rng_generation.load(Ordering::Acquire) == generation {
                break;
            }
            // Reseeded meanwhile: redo the output with a fresh key.
        }
        compiler_fence(Ordering::SeqCst);
        (*st).in_use = 0;
        len as isize
    }
}

// ---------------------------------------------------------------------
// Exported entry points.

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_clock_gettime(clk: i32, ts: *mut Timespec) -> i32 {
    unsafe { do_clock_gettime(clk, ts) }
}

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_gettimeofday(tv: *mut Timeval, tz: *mut Timezone) -> i32 {
    unsafe { do_gettimeofday(tv, tz) }
}

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_time(t: *mut i64) -> i64 {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: a local timespec.
    unsafe { do_clock_gettime(CLOCK_REALTIME, &mut ts) };
    if !t.is_null() {
        // SAFETY: the caller's pointer.
        unsafe { *t = ts.sec };
    }
    ts.sec
}

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_clock_getres(clk: i32, res: *mut Timespec) -> i32 {
    unsafe { do_clock_getres(clk, res) }
}

#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_getrandom(
    buf: *mut u8,
    len: usize,
    flags: u32,
    opaque: *mut u8,
    opaque_len: usize,
) -> isize {
    unsafe { do_getrandom(buf, len, flags, opaque, opaque_len) }
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kernel_clock_gettime(clk: i32, ts: *mut Timespec) -> i32 {
    unsafe { do_clock_gettime(clk, ts) }
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kernel_gettimeofday(tv: *mut Timeval, tz: *mut Timezone) -> i32 {
    unsafe { do_gettimeofday(tv, tz) }
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kernel_clock_getres(clk: i32, res: *mut Timespec) -> i32 {
    unsafe { do_clock_getres(clk, res) }
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __kernel_getrandom(
    buf: *mut u8,
    len: usize,
    flags: u32,
    opaque: *mut u8,
    opaque_len: usize,
) -> isize {
    unsafe { do_getrandom(buf, len, flags, opaque, opaque_len) }
}
