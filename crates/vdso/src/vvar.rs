//! The vvar page: data the kernel publishes for the vDSO (decision D75).
//!
//! This file is shared verbatim by the kernel (through `patina_vdso::vvar`)
//! and the vDSO image (`image/vdso.rs` includes it with `#[path]`), so it
//! uses nothing but `core`.
//!
//! One 4 KiB page, mapped read-only into user space and written by the
//! kernel through its direct map. Only [`Vvar`] at offset 0 is used.
//!
//! Seqlock: a writer (the kernel, writers serialized by the caller) makes
//! `seq` odd, writes the fields, then makes it even again; a reader
//! retries while `seq` is odd or changed across its reads. The clock
//! parameters are written once before the program starts; afterwards only
//! `realtime_offset` (the control channel's `time` op) and
//! `rng_generation` (kernel RNG reseeds) change.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// Size of the vvar area (one page).
pub const VVAR_SIZE: usize = 4096;

/// `clock_mode`: no usable counter; every call takes the syscall.
pub const CLOCK_MODE_NONE: u32 = 0;
/// `clock_mode`: read the arch counter (TSC / CNTVCT_EL0) and scale it.
pub const CLOCK_MODE_COUNTER: u32 = 1;

/// The kernel/vDSO shared data at the start of the vvar page.
#[repr(C)]
pub struct Vvar {
    /// Seqlock sequence: odd while the kernel is updating.
    pub seq: AtomicU32,
    /// [`CLOCK_MODE_NONE`] or [`CLOCK_MODE_COUNTER`].
    pub clock_mode: AtomicU32,
    /// Counter value at monotonic time 0 (the kernel's boot count).
    pub counter_base: AtomicU64,
    /// Counter scale: x86_64 ns per TSC tick as 32.32 fixed point;
    /// aarch64 the counter frequency in Hz (see [`counter_to_ns`]).
    pub scale: AtomicU64,
    /// `CLOCK_REALTIME` - `CLOCK_MONOTONIC`, in ns.
    pub realtime_offset: AtomicU64,
    /// Bumped whenever the kernel RNG is reseeded: vgetrandom states with
    /// another generation fetch a fresh key through the syscall.
    pub rng_generation: AtomicU64,
    /// Non-zero once the kernel RNG is seeded (vgetrandom falls back to
    /// the syscall before that).
    pub rng_ready: AtomicU32,
    pub _pad: u32,
}

/// One consistent read of the clock fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSnapshot {
    pub mode: u32,
    pub counter_base: u64,
    pub scale: u64,
    pub realtime_offset: u64,
}

impl Default for Vvar {
    fn default() -> Self {
        Self::new()
    }
}

impl Vvar {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            clock_mode: AtomicU32::new(CLOCK_MODE_NONE),
            counter_base: AtomicU64::new(0),
            scale: AtomicU64::new(0),
            realtime_offset: AtomicU64::new(0),
            rng_generation: AtomicU64::new(0),
            rng_ready: AtomicU32::new(0),
            _pad: 0,
        }
    }

    /// Seqlock read of the clock fields.
    #[inline(always)]
    pub fn clock(&self) -> ClockSnapshot {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let snap = ClockSnapshot {
                mode: self.clock_mode.load(Ordering::Relaxed),
                counter_base: self.counter_base.load(Ordering::Relaxed),
                scale: self.scale.load(Ordering::Relaxed),
                realtime_offset: self.realtime_offset.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return snap;
            }
        }
    }

    /// Seqlock write: `f` updates fields with relaxed stores. Writers must
    /// be serialized by the caller.
    pub fn write(&self, f: impl FnOnce(&Self)) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        f(self);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }
}

/// ns for `delta` counter ticks with `scale` as stored in [`Vvar::scale`].
/// The kernel's own `timer::now_ns` uses this too, so the vDSO and the
/// syscalls agree to the nanosecond.
#[inline(always)]
pub fn counter_to_ns(delta: u64, scale: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        mult32_to_ns(delta, scale)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        freq_to_ns(delta, scale)
    }
}

/// `delta * mult >> 32` (x86_64: `mult` = ns per tick, 32.32).
#[inline(always)]
pub fn mult32_to_ns(delta: u64, mult: u64) -> u64 {
    ((delta as u128 * mult as u128) >> 32) as u64
}

/// `delta * 1e9 / freq` without overflow for frequencies below 18 GHz
/// (aarch64: `freq` = CNTFRQ_EL0). `freq` must be non-zero.
#[inline(always)]
pub fn freq_to_ns(delta: u64, freq: u64) -> u64 {
    (delta / freq) * 1_000_000_000 + (delta % freq) * 1_000_000_000 / freq
}
