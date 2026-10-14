//! x86_64-specific Linux ABI.
//!
//! Syscall convention: number in `rax`; arguments in `rdi, rsi, rdx, r10, r8,
//! r9`; result (or `-errno`) in `rax`; `rcx` and `r11` are clobbered by
//! `SYSCALL`/`SYSRET`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::sched::CloneArgs;
use crate::signal::{SigInfo, SigSet, StackT};

pub mod nr;

/// Name of syscall `n` for strace / ENOSYS logging.
pub fn syscall_name(n: usize) -> Option<&'static str> {
    crate::syscall_name(nr::TABLE, n)
}

crate::open_flags!(
    direct = 0o40000,
    largefile = 0o100000,
    directory = 0o200000,
    nofollow = 0o400000
);

/// `O_DIRECTORY` etc. as plain constants.
pub const O_DIRECT: u32 = OpenFlags::DIRECT.bits();
pub const O_LARGEFILE: u32 = OpenFlags::LARGEFILE.bits();
pub const O_DIRECTORY: u32 = OpenFlags::DIRECTORY.bits();
pub const O_NOFOLLOW: u32 = OpenFlags::NOFOLLOW.bits();
pub const O_TMPFILE: u32 = OpenFlags::TMPFILE.bits();

/// x86_64-only `mmap` flags.
pub const MAP_32BIT: u32 = 0x40;
pub const MAP_ABOVE4G: u32 = 0x80;

/// `asm/signal.h` (x86).
pub const MINSIGSTKSZ: usize = 2048;
pub const SIGSTKSZ: usize = 8192;

/// `arch_prctl` codes.
pub const ARCH_SET_GS: i32 = 0x1001;
pub const ARCH_SET_FS: i32 = 0x1002;
pub const ARCH_GET_FS: i32 = 0x1003;
pub const ARCH_GET_GS: i32 = 0x1004;
pub const ARCH_GET_CPUID: i32 = 0x1011;
pub const ARCH_SET_CPUID: i32 = 0x1012;

/// `AT_PLATFORM` string.
pub const PLATFORM: &str = "x86_64";
/// `uname -m`.
pub const UTS_MACHINE: &str = "x86_64";

/// Undo x86_64's `clone(flags, stack, parent_tid, child_tid, tls)` order.
pub const fn clone_args(a: [u64; 5]) -> CloneArgs {
    CloneArgs {
        flags: a[0],
        stack: a[1],
        parent_tid: a[2],
        child_tid: a[3],
        tls: a[4],
    }
}

/// `struct stat` from `arch/x86/include/uapi/asm/stat.h` (144 bytes).
/// `st_nlink` is 64-bit and precedes `st_mode` on x86_64.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub _pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: u64,
    pub st_mtime: i64,
    pub st_mtime_nsec: u64,
    pub st_ctime: i64,
    pub st_ctime_nsec: u64,
    pub _unused: [i64; 3],
}

/// `struct epoll_event`: `__attribute__((packed))` on x86_64 (12 bytes,
/// `data` at offset 4).
#[repr(C, packed)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

impl EpollEvent {
    pub const ZERO: EpollEvent = EpollEvent { events: 0, data: 0 };
}
crate::epoll_event_impl!(EpollEvent);

/// `ucontext.uc_flags` bits.
pub const UC_FP_XSTATE: u64 = 0x1;
pub const UC_SIGCONTEXT_SS: u64 = 0x2;
pub const UC_STRICT_RESTORE_SS: u64 = 0x4;

/// `struct sigcontext` (64-bit, `arch/x86/include/uapi/asm/sigcontext.h`),
/// i.e. `uc_mcontext`. 256 bytes. Go reads the general registers by name.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SigContext {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rsp: u64,
    pub rip: u64,
    pub eflags: u64,
    pub cs: u16,
    pub gs: u16,
    pub fs: u16,
    /// `ss` (was `__pad0` before Linux 4.6; valid when `UC_SIGCONTEXT_SS`).
    pub ss: u16,
    pub err: u64,
    pub trapno: u64,
    pub oldmask: u64,
    pub cr2: u64,
    /// User address of the `FpState`/XSAVE area, 64-byte aligned, or 0.
    pub fpstate: u64,
    pub reserved1: [u64; 8],
}

/// `struct _fpx_sw_bytes`: lives in `FpState::sw_reserved` and describes the
/// XSAVE area that follows the legacy 512 bytes.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct FpxSwBytes {
    /// [`FP_XSTATE_MAGIC1`] when an XSAVE area is present.
    pub magic1: u32,
    /// Total size of the fpstate area including the trailing
    /// [`FP_XSTATE_MAGIC2`] word.
    pub extended_size: u32,
    /// XFEATURE bitmap present in the area.
    pub xfeatures: u64,
    /// Size of the XSAVE area (without the trailing magic).
    pub xstate_size: u32,
    pub padding: [u32; 7],
}

pub const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
pub const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
pub const FP_XSTATE_MAGIC2_SIZE: usize = 4;
/// Size of the legacy FXSAVE region at the start of the XSAVE area.
pub const XSTATE_LEGACY_SIZE: usize = 512;
/// Size of the XSAVE header that follows the legacy region.
pub const XSAVE_HEADER_SIZE: usize = 64;
/// Required alignment of the fpstate area on the signal stack.
pub const FPSTATE_ALIGN: u64 = 64;

/// `struct _fpstate_64` (512 bytes): the FXSAVE legacy region.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct FpState {
    pub cwd: u16,
    pub swd: u16,
    /// Abridged tag word.
    pub twd: u16,
    pub fop: u16,
    pub rip: u64,
    pub rdp: u64,
    pub mxcsr: u32,
    pub mxcsr_mask: u32,
    /// 8 x87 registers, 16 bytes each.
    pub st_space: [u32; 32],
    /// 16 XMM registers, 16 bytes each.
    pub xmm_space: [u32; 64],
    pub reserved2: [u32; 12],
    /// `sw_reserved` ([`FpxSwBytes`]) when `UC_FP_XSTATE`.
    pub sw_reserved: FpxSwBytes,
}

impl Default for FpState {
    fn default() -> Self {
        <Self as zerocopy::FromZeros>::new_zeroed()
    }
}

/// Kernel `struct ucontext` (`arch/x86/include/uapi/asm/ucontext.h`):
/// 304 bytes, `uc_sigmask` is the 8-byte kernel sigset (glibc's
/// `ucontext_t` is larger but agrees on every offset up to here).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct UContext {
    pub uc_flags: u64,
    pub uc_link: u64,
    pub uc_stack: StackT,
    pub uc_mcontext: SigContext,
    pub uc_sigmask: SigSet,
}

/// `struct rt_sigframe` (`arch/x86/include/asm/sigframe.h`, 64-bit): what
/// `rsp` points at when the handler starts. The fpstate area lives above it
/// (at `uc.uc_mcontext.fpstate`).
///
/// Handler entry: `rdi = signo`, `rsi = &info`, `rdx = &uc`, `rax = 0`,
/// `rsp = &pretcode`, `rip = sa_handler`. `pretcode = sa_restorer` (Go's
/// `runtime.sigreturn__sigaction`, which issues `rt_sigreturn`); at
/// `rt_sigreturn` time `rsp` points at `uc` (the restorer's `ret` popped
/// `pretcode`), i.e. the frame base is `rsp - 8`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RtSigframe {
    pub pretcode: u64,
    pub uc: UContext,
    pub info: SigInfo,
}

/// Bytes below the user `rsp` the ABI reserves (System V red zone); the
/// kernel skips them when building a frame on the current stack.
pub const REDZONE: u64 = 128;

/// Where a signal frame goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigframePlacement {
    /// Address of the [`RtSigframe`] (new `rsp`); `≡ 8 (mod 16)` as after a
    /// `call`.
    pub frame: u64,
    /// Address of the fpstate/XSAVE area (64-byte aligned).
    pub fpstate: u64,
}

/// Stack alignment of the frame before the `- 8` "return address" slot
/// (`FRAME_ALIGNMENT`, arch/x86/kernel/signal.c).
pub const FRAME_ALIGNMENT: u64 = 16;

/// Bytes reserved on the signal stack for the fpstate area
/// (`xstate_sigframe_size`, arch/x86/kernel/fpu/signal.c): the user XSAVE
/// area plus the trailing [`FP_XSTATE_MAGIC2`] word when the CPU uses
/// XSAVE; just the 512-byte FXSAVE image (no magic) otherwise.
pub const fn xstate_sigframe_size(xstate_size: u64, xsave: bool) -> u64 {
    if xsave {
        xstate_size + FP_XSTATE_MAGIC2_SIZE as u64
    } else {
        xstate_size
    }
}

/// Lay out a signal frame below `sp_top` (Linux `get_sigframe`):
/// `sp_top` is `ss_sp + ss_size` when switching to the alternate stack,
/// otherwise the interrupted `rsp - REDZONE`. `math_size` is
/// [`xstate_sigframe_size`].
pub const fn sigframe_placement(sp_top: u64, math_size: u64) -> SigframePlacement {
    let fpstate = (sp_top - math_size) & !(FPSTATE_ALIGN - 1);
    let frame =
        ((fpstate - core::mem::size_of::<RtSigframe>() as u64) & !(FRAME_ALIGNMENT - 1)) - 8;
    SigframePlacement { frame, fpstate }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_flags() {
        assert_eq!(O_DIRECTORY, 0x10000);
        assert_eq!(O_NOFOLLOW, 0x20000);
        assert_eq!(O_DIRECT, 0x4000);
        assert_eq!(O_LARGEFILE, 0x8000);
        assert_eq!(O_TMPFILE, 0x410000);
        let f = OpenFlags::from_bits_retain(
            crate::fs::O_RDWR | crate::fs::O_CREAT | crate::fs::O_CLOEXEC,
        );
        assert!(f.readable() && f.writable());
        assert!(OpenFlags::from_bits_retain(0).readable());
        assert!(!OpenFlags::from_bits_retain(1).readable());
    }

    #[test]
    fn syscall_numbers() {
        assert_eq!(nr::READ, 0);
        assert_eq!(nr::WRITE, 1);
        assert_eq!(nr::MMAP, 9);
        assert_eq!(nr::RT_SIGRETURN, 15);
        assert_eq!(nr::CLONE, 56);
        assert_eq!(nr::EXIT, 60);
        assert_eq!(nr::ARCH_PRCTL, 158);
        assert_eq!(nr::GETTID, 186);
        assert_eq!(nr::FUTEX, 202);
        assert_eq!(nr::CLOCK_GETTIME, 228);
        assert_eq!(nr::EXIT_GROUP, 231);
        assert_eq!(nr::TGKILL, 234);
        assert_eq!(nr::OPENAT, 257);
        assert_eq!(nr::NEWFSTATAT, 262);
        assert_eq!(nr::EPOLL_PWAIT, 281);
        assert_eq!(nr::ACCEPT4, 288);
        assert_eq!(nr::EVENTFD2, 290);
        assert_eq!(nr::EPOLL_CREATE1, 291);
        assert_eq!(nr::PIPE2, 293);
        assert_eq!(nr::PRLIMIT64, 302);
        assert_eq!(nr::GETRANDOM, 318);
        assert_eq!(nr::STATX, 332);
        assert_eq!(nr::FACCESSAT2, 439);
        assert_eq!(syscall_name(0), Some("read"));
        assert_eq!(syscall_name(231), Some("exit_group"));
        assert_eq!(syscall_name(100_000), None);
        assert!(nr::TABLE.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn clone_order() {
        let a = clone_args([1, 2, 3, 4, 5]);
        assert_eq!(
            (a.flags, a.stack, a.parent_tid, a.child_tid, a.tls),
            (1, 2, 3, 4, 5)
        );
    }

    #[test]
    fn epoll_event_packed() {
        let e = EpollEvent::new(0x8000_2005, 0x1122_3344_5566_7788);
        let b = e.as_bytes();
        assert_eq!(b.len(), 12);
        assert_eq!(&b[0..4], &0x8000_2005u32.to_le_bytes());
        assert_eq!(&b[4..12], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(e.data(), 0x1122_3344_5566_7788);
    }

    #[test]
    fn placement() {
        let p = sigframe_placement(0x7fff_0000 - REDZONE, xstate_sigframe_size(512, true));
        assert_eq!(p.fpstate % 64, 0);
        assert!(p.fpstate + 516 <= 0x7fff_0000 - REDZONE);
        assert_eq!(p.frame % 16, 8);
        assert!(p.frame + core::mem::size_of::<RtSigframe>() as u64 <= p.fpstate);
        // 0x7ffeff80 - 516 = 0x7ffefd7c -> round down 64 -> 0x7ffefd40
        assert_eq!(p.fpstate, 0x7ffe_fd40);
        // 0x7ffefd40 - 440 = 0x7ffefb88 -> round down 16 -> 0x7ffefb80 - 8
        assert_eq!(p.frame, 0x7ffe_fb78);
    }
}
