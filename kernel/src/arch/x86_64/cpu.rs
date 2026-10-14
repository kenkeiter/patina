//! CPU identification, idling, control registers, MSRs and the optional
//! CPU features the kernel turns on at boot ([`init_features`]).

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::percpu;

/// Maximum number of CPUs (per-CPU static slots; extra CPUs the firmware
/// reports are not started).
pub const MAX_CPUS: usize = 32;

/// Logical CPU index, from the per-CPU block (GS; see `percpu.rs`).
#[inline]
pub fn id() -> usize {
    percpu::cpu_id()
}

/// Mask interrupts and halt forever.
pub fn halt() -> ! {
    loop {
        // SAFETY: cli/hlt are always permitted in ring 0.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// Halt until the next interrupt, once.
#[inline]
pub fn wait_for_interrupt() {
    // SAFETY: hlt has no memory effects.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

/// Idle until an interrupt and let it run: called with IRQs masked,
/// returns with IRQs masked after the interrupt was handled.
///
/// No lost wake-up: `sti` keeps interrupts blocked for one more
/// instruction, so `sti; hlt` enters the halt state before any pending
/// interrupt is taken and the interrupt then ends the halt.
#[inline]
pub fn idle_wait() {
    // SAFETY: sti/hlt/cli are always permitted in ring 0; IRQs end masked
    // as on entry. Not `nomem`: interrupt handlers run in between.
    unsafe { asm!("sti", "hlt", "cli", options(nostack)) };
}

#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr4() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// Changes paging-related CPU state.
#[inline]
pub unsafe fn write_cr4(v: u64) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags)) };
}

/// # Safety
/// `v` must be the physical address of a valid PML4 that maps the running
/// code, stack and everything the kernel touches next.
#[inline]
pub unsafe fn write_cr3(v: u64) {
    // SAFETY: caller's responsibility.
    unsafe { asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags)) };
}

// ---------------------------------------------------------------------------
// MSRs, CPUID, TSC.

pub const MSR_APIC_BASE: u32 = 0x1b;
pub const MSR_EFER: u32 = 0xc000_0080;
pub const MSR_STAR: u32 = 0xc000_0081;
pub const MSR_LSTAR: u32 = 0xc000_0082;
pub const MSR_SFMASK: u32 = 0xc000_0084;
pub const MSR_FS_BASE: u32 = 0xc000_0100;
pub const MSR_GS_BASE: u32 = 0xc000_0101;
pub const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

/// # Safety
/// `msr` must exist on this CPU (else #GP).
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: caller guarantees the MSR exists.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags))
    };
    ((hi as u64) << 32) | lo as u64
}

/// # Safety
/// `msr` must exist and `v` must be valid for it; MSR writes can change
/// arbitrary CPU behaviour.
#[inline]
pub unsafe fn wrmsr(msr: u32, v: u64) {
    // SAFETY: caller's responsibility.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32,
             options(nostack, preserves_flags))
    };
}

/// CPUID leaf `leaf`, subleaf `sub`: (eax, ebx, ecx, edx).
#[inline]
pub fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) {
    // SAFETY: CPUID exists on every x86_64 CPU and has no side effects.
    #[allow(unused_unsafe)]
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, sub) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

#[inline]
pub fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC is allowed in ring 0 and has no side effects.
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags))
    };
    ((hi as u64) << 32) | lo as u64
}

// ---------------------------------------------------------------------------
// Optional features.

const CR4_FSGSBASE: u64 = 1 << 16;
const CR4_OSXSAVE: u64 = 1 << 18;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

/// XCR0 the kernel enables when XSAVE is used: x87 | SSE | AVX. AVX-512 and
/// the other components stay off, so user code (Go checks XGETBV) never uses
/// state that `ctx::FpState` does not hold.
pub const XCR0_USED: u64 = 0b111;

static FSGSBASE: AtomicBool = AtomicBool::new(false);
static XSAVE: AtomicBool = AtomicBool::new(false);
static XSAVE_SIZE: AtomicU32 = AtomicU32::new(0);
static SMEP: AtomicBool = AtomicBool::new(false);
static SMAP: AtomicBool = AtomicBool::new(false);

/// Whether CR4.FSGSBASE is on (RDFSBASE/WRFSBASE usable, also from ring 3).
#[inline]
pub fn has_fsgsbase() -> bool {
    FSGSBASE.load(Ordering::Relaxed)
}

/// Whether FP state is saved with XSAVE (x87|SSE|AVX) instead of FXSAVE.
#[inline]
pub fn uses_xsave() -> bool {
    XSAVE.load(Ordering::Relaxed)
}

/// Turn on the optional CPU features on this CPU: FSGSBASE, SMEP, SMAP and
/// XSAVE with XCR0 = x87|SSE|AVX (only if that area fits `ctx::FpState`;
/// otherwise CR4.OSXSAVE stays clear, so CPUID.1:ECX.OSXSAVE reads 0 and
/// user code such as Go's runtime does not use AVX). Every CPU must call it
/// (APs in Phase 2); the decisions are the same on all CPUs.
pub fn init_features() {
    let (max_leaf, ..) = cpuid(0, 0);
    let (_, _, ecx1, _) = cpuid(1, 0);
    let ebx7 = if max_leaf >= 7 { cpuid(7, 0).1 } else { 0 };
    let mut cr4 = read_cr4();
    if ebx7 & 1 != 0 {
        cr4 |= CR4_FSGSBASE;
        FSGSBASE.store(true, Ordering::Relaxed);
    }
    if ebx7 & (1 << 7) != 0 {
        cr4 |= CR4_SMEP;
        SMEP.store(true, Ordering::Relaxed);
    }
    if ebx7 & (1 << 20) != 0 {
        cr4 |= CR4_SMAP;
        SMAP.store(true, Ordering::Relaxed);
    }
    let want_xsave = ecx1 & (1 << 26) != 0 // XSAVE
        && ecx1 & (1 << 28) != 0 // AVX
        && max_leaf >= 0xd
        && (cpuid(0xd, 0).0 as u64 & XCR0_USED) == XCR0_USED;
    if want_xsave {
        cr4 |= CR4_OSXSAVE;
    }
    // SAFETY: only enables features CPUID reports. SMEP/SMAP: the kernel
    // never executes or touches user pages directly (it uses the direct
    // map). FSGSBASE: the kernel does not use FS/GS bases except via swapgs.
    unsafe { write_cr4(cr4) };
    if want_xsave {
        // SAFETY: CR4.OSXSAVE is set and CPUID reports these components.
        unsafe {
            asm!("xsetbv", in("ecx") 0u32, in("eax") XCR0_USED as u32, in("edx") 0u32,
                 options(nomem, nostack, preserves_flags))
        };
        // EBX of leaf 0xD/0: XSAVE area size for the components now in XCR0.
        let size = cpuid(0xd, 0).1;
        if size as usize <= super::ctx::FP_STATE_SIZE {
            XSAVE_SIZE.store(size, Ordering::Relaxed);
            XSAVE.store(true, Ordering::Relaxed);
        } else {
            // SAFETY: back to CR4 without OSXSAVE (FXSAVE only, AVX hidden).
            unsafe { write_cr4(cr4 & !CR4_OSXSAVE) };
        }
    }
}

/// Feature choices made by [`init_features`], for the boot log.
pub fn features_summary() -> (&'static str, &'static str, &'static str, u32) {
    (
        if has_fsgsbase() {
            "fsgsbase (rd/wrfsbase)"
        } else {
            "no fsgsbase (FS_BASE msr)"
        },
        match (SMEP.load(Ordering::Relaxed), SMAP.load(Ordering::Relaxed)) {
            (true, true) => "smep smap",
            (true, false) => "smep",
            (false, true) => "smap",
            (false, false) => "no smep/smap",
        },
        if uses_xsave() {
            "xsave x87|sse|avx"
        } else {
            "fxsave"
        },
        XSAVE_SIZE.load(Ordering::Relaxed),
    )
}

/// Current FS base (the user TLS pointer: the kernel never uses FS).
#[inline]
pub fn read_fs_base() -> u64 {
    if has_fsgsbase() {
        let v: u64;
        // SAFETY: CR4.FSGSBASE is set.
        unsafe { asm!("rdfsbase {}", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    } else {
        // SAFETY: FS_BASE exists on every x86_64 CPU.
        unsafe { rdmsr(MSR_FS_BASE) }
    }
}

/// Set the FS base. A non-canonical value would #GP in the kernel; such a
/// value is replaced by 0 (the core validates `arch_prctl` anyway).
#[inline]
pub fn write_fs_base(v: u64) {
    let v = if (((v as i64) << 16) >> 16) as u64 == v {
        v
    } else {
        0
    };
    if has_fsgsbase() {
        // SAFETY: CR4.FSGSBASE is set; `v` is canonical.
        unsafe { asm!("wrfsbase {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
    } else {
        // SAFETY: FS_BASE exists; `v` is canonical.
        unsafe { wrmsr(MSR_FS_BASE, v) };
    }
}

/// Kernel stack for the next entry from user mode on this CPU: TSS.rsp0
/// (interrupts/exceptions) and the per-CPU SYSCALL stack. `top` must be
/// 16-byte aligned. Call with interrupts disabled, before `ctx::switch` to
/// a thread / `ctx::enter_user`.
pub fn set_kernel_stack(top: usize) {
    debug_assert!(top.is_multiple_of(16));
    percpu::set_kernel_stack(top as u64);
}

/// Linux `AT_HWCAP2` bit: FSGSBASE instructions usable from ring 3.
const HWCAP2_FSGSBASE: u64 = 1 << 1;

/// (AT_HWCAP, AT_HWCAP2): like Linux's x86 `ELF_HWCAP` = CPUID.1:EDX, and
/// `HWCAP2_FSGSBASE` when CR4.FSGSBASE is on.
pub fn hwcap() -> (u64, u64) {
    let edx = cpuid(1, 0).3 as u64;
    (edx, if has_fsgsbase() { HWCAP2_FSGSBASE } else { 0 })
}

/// Initial APIC id of the executing CPU (CPUID 0xB x2APIC id when the leaf
/// exists, else CPUID.1:EBX[31:24]).
pub fn apic_id() -> u32 {
    let (max_leaf, ..) = cpuid(0, 0);
    if max_leaf >= 0xb {
        let (_, ebx, _, edx) = cpuid(0xb, 0);
        if ebx != 0 {
            return edx;
        }
    }
    cpuid(1, 0).1 >> 24
}

/// A 64-bit value from the hardware RNG: RDSEED (CPUID.7.0:EBX bit 18,
/// retried a few times since it may transiently run dry), else RDRAND
/// (CPUID.1:ECX bit 30, likewise); `None` without either or if both keep
/// failing.
pub fn hw_random() -> Option<u64> {
    let (max_leaf, ..) = cpuid(0, 0);
    let rdseed = max_leaf >= 7 && cpuid(7, 0).1 & (1 << 18) != 0;
    let rdrand = cpuid(1, 0).2 & (1 << 30) != 0;
    if rdseed {
        for _ in 0..64 {
            let (v, ok): (u64, u8);
            // SAFETY: CPUID reports RDSEED; it only writes the register and CF.
            unsafe {
                asm!("rdseed {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack))
            };
            if ok != 0 {
                return Some(v);
            }
            core::hint::spin_loop();
        }
    }
    if rdrand {
        for _ in 0..16 {
            let (v, ok): (u64, u8);
            // SAFETY: CPUID reports RDRAND; it only writes the register and CF.
            unsafe {
                asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack))
            };
            if ok != 0 {
                return Some(v);
            }
        }
    }
    None
}

/// Barrier before a device doorbell (virtio notify). x86 stores are not
/// reordered with other stores and port/UC MMIO accesses are serializing
/// enough; `mfence` keeps it explicit and orders against WC memory too.
#[inline]
pub fn io_barrier() {
    // SAFETY: barrier only.
    unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
}
