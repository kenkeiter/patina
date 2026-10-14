//! aarch64-specific Linux ABI.
//!
//! Syscall convention: `svc #0` with number in `x8`; arguments in `x0..x5`;
//! result (or `-errno`) in `x0`. The table is the `asm-generic` one: no
//! `open`, `stat`, `pipe`, `epoll_wait`, `dup2`, `fork`, `arch_prctl`, ...;
//! only the `*at`/`2`/`pwait` variants.

use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

use crate::sched::CloneArgs;
use crate::signal::{SigInfo, SigSet, StackT};

pub mod nr;

/// Name of syscall `n` for strace / ENOSYS logging.
pub fn syscall_name(n: usize) -> Option<&'static str> {
    crate::syscall_name(nr::TABLE, n)
}

crate::open_flags!(
    direct = 0o200000,
    largefile = 0o400000,
    directory = 0o40000,
    nofollow = 0o100000
);

pub const O_DIRECT: u32 = OpenFlags::DIRECT.bits();
pub const O_LARGEFILE: u32 = OpenFlags::LARGEFILE.bits();
pub const O_DIRECTORY: u32 = OpenFlags::DIRECTORY.bits();
pub const O_NOFOLLOW: u32 = OpenFlags::NOFOLLOW.bits();
pub const O_TMPFILE: u32 = OpenFlags::TMPFILE.bits();

/// aarch64-only `mmap`/`mprotect` prot bits.
pub const PROT_BTI: u32 = 0x10;
pub const PROT_MTE: u32 = 0x20;

/// `arch/arm64/include/uapi/asm/signal.h`.
pub const MINSIGSTKSZ: usize = 5120;
pub const SIGSTKSZ: usize = 16384;

/// `AT_PLATFORM` string.
pub const PLATFORM: &str = "aarch64";
/// `uname -m`.
pub const UTS_MACHINE: &str = "aarch64";

/// Undo arm64's `clone(flags, stack, parent_tid, tls, child_tid)` order
/// (`CONFIG_CLONE_BACKWARDS`).
pub const fn clone_args(a: [u64; 5]) -> CloneArgs {
    CloneArgs {
        flags: a[0],
        stack: a[1],
        parent_tid: a[2],
        tls: a[3],
        child_tid: a[4],
    }
}

/// `AT_HWCAP` bits (`arch/arm64/include/uapi/asm/hwcap.h`).
pub mod hwcap {
    pub const FP: u64 = 1 << 0;
    pub const ASIMD: u64 = 1 << 1;
    pub const EVTSTRM: u64 = 1 << 2;
    pub const AES: u64 = 1 << 3;
    pub const PMULL: u64 = 1 << 4;
    pub const SHA1: u64 = 1 << 5;
    pub const SHA2: u64 = 1 << 6;
    pub const CRC32: u64 = 1 << 7;
    pub const ATOMICS: u64 = 1 << 8;
    pub const FPHP: u64 = 1 << 9;
    pub const ASIMDHP: u64 = 1 << 10;
    pub const CPUID: u64 = 1 << 11;
    pub const ASIMDRDM: u64 = 1 << 12;
    pub const JSCVT: u64 = 1 << 13;
    pub const FCMA: u64 = 1 << 14;
    pub const LRCPC: u64 = 1 << 15;
    pub const DCPOP: u64 = 1 << 16;
    pub const SHA3: u64 = 1 << 17;
    pub const SM3: u64 = 1 << 18;
    pub const SM4: u64 = 1 << 19;
    pub const ASIMDDP: u64 = 1 << 20;
    pub const SHA512: u64 = 1 << 21;
    pub const SVE: u64 = 1 << 22;
    pub const ASIMDFHM: u64 = 1 << 23;
    pub const DIT: u64 = 1 << 24;
    pub const USCAT: u64 = 1 << 25;
    pub const ILRCPC: u64 = 1 << 26;
    pub const FLAGM: u64 = 1 << 27;
    pub const SSBS: u64 = 1 << 28;
    pub const SB: u64 = 1 << 29;
    pub const PACA: u64 = 1 << 30;
    pub const PACG: u64 = 1 << 31;

    /// Derive the `ID_AA64ISAR0_EL1`-based subset of `AT_HWCAP` (what Go
    /// checks: AES, PMULL, SHA1, SHA2, CRC32, ATOMICS, SHA3, SHA512, ...).
    /// FP/ASIMD are always set (the kernel requires them).
    pub const fn from_id_aa64isar0(isar0: u64) -> u64 {
        const fn field(isar0: u64, shift: u32) -> u64 {
            (isar0 >> shift) & 0xf
        }
        let mut h = FP | ASIMD;
        if field(isar0, 4) >= 1 {
            h |= AES;
        }
        if field(isar0, 4) >= 2 {
            h |= PMULL;
        }
        if field(isar0, 8) >= 1 {
            h |= SHA1;
        }
        if field(isar0, 12) >= 1 {
            h |= SHA2;
        }
        if field(isar0, 12) >= 2 {
            h |= SHA512;
        }
        if field(isar0, 16) >= 1 {
            h |= CRC32;
        }
        if field(isar0, 20) >= 2 {
            h |= ATOMICS;
        }
        if field(isar0, 28) >= 1 {
            h |= ASIMDRDM;
        }
        if field(isar0, 32) >= 1 {
            h |= SHA3;
        }
        if field(isar0, 36) >= 1 {
            h |= SM3;
        }
        if field(isar0, 40) >= 1 {
            h |= SM4;
        }
        if field(isar0, 44) >= 1 {
            h |= ASIMDDP;
        }
        if field(isar0, 48) >= 1 {
            h |= ASIMDFHM;
        }
        if field(isar0, 52) >= 1 {
            h |= FLAGM;
        }
        h
    }
}

/// `struct stat` from `include/uapi/asm-generic/stat.h` (128 bytes).
/// Differs from x86_64: `st_mode` before a 32-bit `st_nlink`, 32-bit
/// `st_blksize`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub _pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    pub _pad2: i32,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: u64,
    pub st_mtime: i64,
    pub st_mtime_nsec: u64,
    pub st_ctime: i64,
    pub st_ctime_nsec: u64,
    pub _unused4: u32,
    pub _unused5: u32,
}

/// `struct epoll_event`: natural alignment on aarch64 (16 bytes, `data`
/// at offset 8).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct EpollEvent {
    pub events: u32,
    pub _pad: u32,
    pub data: u64,
}

impl EpollEvent {
    pub const ZERO: EpollEvent = EpollEvent {
        events: 0,
        _pad: 0,
        data: 0,
    };
}
crate::epoll_event_impl!(EpollEvent);

/// Size of `sigcontext.__reserved`.
pub const SIGCONTEXT_RESERVED_SIZE: usize = 4096;

/// `struct sigcontext` (`arch/arm64/include/uapi/asm/sigcontext.h`), i.e.
/// `uc_mcontext`: 4384 bytes, 16-byte aligned. Records
/// ([`FpsimdContext`], [`EsrContext`], ...) are packed into `__reserved`
/// and terminated by a zero header.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct SigContext {
    pub fault_address: u64,
    /// x0..x30.
    pub regs: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
    /// Padding so `__reserved` is 16-byte aligned (offset 288).
    pub _pad: u64,
    pub reserved: [u8; SIGCONTEXT_RESERVED_SIZE],
}

impl Default for SigContext {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

/// `struct _aarch64_ctx`: header of every record in `__reserved`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Aarch64Ctx {
    pub magic: u32,
    pub size: u32,
}

pub const FPSIMD_MAGIC: u32 = 0x4650_8001;
pub const ESR_MAGIC: u32 = 0x4553_5201;
pub const EXTRA_MAGIC: u32 = 0x4558_5401;
pub const SVE_MAGIC: u32 = 0x5356_4501;
pub const TPIDR2_MAGIC: u32 = 0x5450_4902;
pub const ZA_MAGIC: u32 = 0x5436_6345;
pub const ZT_MAGIC: u32 = 0x5a54_4e01;
pub const FPMR_MAGIC: u32 = 0x4650_4d52;

/// `struct fpsimd_context` (528 bytes): FPSR, FPCR, V0..V31.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct FpsimdContext {
    pub head: Aarch64Ctx,
    pub fpsr: u32,
    pub fpcr: u32,
    /// `__uint128_t vregs[32]` as (lo, hi) little-endian halves.
    pub vregs: [[u64; 2]; 32],
}

impl Default for FpsimdContext {
    fn default() -> Self {
        let mut c = Self::new_zeroed();
        c.head = Aarch64Ctx {
            magic: FPSIMD_MAGIC,
            size: core::mem::size_of::<Self>() as u32,
        };
        c
    }
}

/// `struct esr_context` (16 bytes): fault syndrome, present for faults.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct EsrContext {
    pub head: Aarch64Ctx,
    pub esr: u64,
}

impl EsrContext {
    pub const fn new(esr: u64) -> Self {
        EsrContext {
            head: Aarch64Ctx {
                magic: ESR_MAGIC,
                size: 16,
            },
            esr,
        }
    }
}

impl SigContext {
    /// Fill `__reserved` the way Linux does for a plain FPSIMD task:
    /// `fpsimd_context`, then `esr_context` if `esr` is given, then a zero
    /// terminator. The rest is zeroed.
    pub fn write_reserved(&mut self, fpsimd: &FpsimdContext, esr: Option<u64>) {
        let r = &mut self.reserved;
        r.fill(0);
        let mut off = 0;
        let f = fpsimd.as_bytes();
        r[off..off + f.len()].copy_from_slice(f);
        // Force a correct header even if the caller left it zeroed.
        let head = Aarch64Ctx {
            magic: FPSIMD_MAGIC,
            size: f.len() as u32,
        };
        r[off..off + 8].copy_from_slice(head.as_bytes());
        off += f.len();
        if let Some(esr) = esr {
            let e = EsrContext::new(esr);
            r[off..off + 16].copy_from_slice(e.as_bytes());
        }
        // Terminator is the zeroed header that follows.
    }

    /// Walk the `__reserved` records (as `rt_sigreturn` does) and return the
    /// `fpsimd_context`. `None` if the record list is malformed, unterminated,
    /// lacks FPSIMD, or contains records patina does not support
    /// (`extra_context`, SVE, ...).
    pub fn read_fpsimd(&self) -> Option<FpsimdContext> {
        let r = &self.reserved;
        let mut off = 0usize;
        let mut found = None;
        // Leave room for the terminator.
        while off + 8 <= r.len() {
            let head = Aarch64Ctx::read_from_bytes(&r[off..off + 8]).ok()?;
            if head.magic == 0 {
                return if head.size == 0 { found } else { None };
            }
            let size = head.size as usize;
            if size < 8 || !size.is_multiple_of(16) || off + size > r.len() {
                return None;
            }
            match head.magic {
                FPSIMD_MAGIC => {
                    if size != core::mem::size_of::<FpsimdContext>() || found.is_some() {
                        return None;
                    }
                    found = FpsimdContext::read_from_bytes(&r[off..off + size]).ok();
                }
                ESR_MAGIC => {}
                _ => return None,
            }
            off += size;
        }
        None
    }
}

/// Kernel `struct ucontext` for arm64 (`arch/arm64/include/uapi/asm/ucontext.h`):
/// 4560 bytes. `uc_sigmask` comes before `uc_mcontext`, followed by
/// padding up to a 1024-bit sigset, then `uc_mcontext` at offset 176.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct UContext {
    pub uc_flags: u64,
    pub uc_link: u64,
    pub uc_stack: StackT,
    pub uc_sigmask: SigSet,
    /// `__unused[1024 / 8 - sizeof(sigset_t)]`.
    pub _unused: [u8; 120],
    /// Implicit C padding to 16-byte-align `uc_mcontext`.
    pub _pad: u64,
    pub uc_mcontext: SigContext,
}

impl Default for UContext {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

/// `struct rt_sigframe` (`arch/arm64/kernel/signal.c`): `siginfo` then
/// `ucontext`. 4688 bytes.
///
/// Handler entry: `x0 = signo`, `x1 = &info`, `x2 = &uc`, `sp = &frame`,
/// `x29 = &frame_record.fp`, `x30 = sa_restorer` (Go always sets
/// `SA_RESTORER`), `pc = sa_handler`. At `rt_sigreturn` time `sp` must
/// still equal the frame address (Linux rejects misaligned `sp`).
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct RtSigframe {
    pub info: SigInfo,
    pub uc: UContext,
}

impl Default for RtSigframe {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

/// `struct frame_record`: the AAPCS64 frame record the kernel places just
/// above the sigframe so unwinders can walk through the handler.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct FrameRecord {
    /// Interrupted `x29`.
    pub fp: u64,
    /// Interrupted `x30`.
    pub lr: u64,
}

/// Where a signal frame goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigframePlacement {
    /// Address of the [`RtSigframe`] (new `sp`), 16-byte aligned.
    pub frame: u64,
    /// Address of the [`FrameRecord`] (new `x29` points at it).
    pub frame_record: u64,
}

/// Lay out a signal frame below `sp_top` (Linux `get_sigframe`): `sp_top`
/// is `ss_sp + ss_size` on the alternate stack, otherwise the interrupted
/// `sp` (arm64 has no red zone).
pub const fn sigframe_placement(sp_top: u64) -> SigframePlacement {
    let rec = (sp_top - core::mem::size_of::<FrameRecord>() as u64) & !15;
    let size = (core::mem::size_of::<RtSigframe>() as u64 + 15) & !15;
    let frame = (rec & !15) - size;
    SigframePlacement {
        frame,
        frame_record: rec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_flags() {
        assert_eq!(O_DIRECTORY, 0x4000);
        assert_eq!(O_NOFOLLOW, 0x8000);
        assert_eq!(O_DIRECT, 0x10000);
        assert_eq!(O_LARGEFILE, 0x20000);
        assert_eq!(O_TMPFILE, 0x404000);
    }

    #[test]
    fn syscall_numbers() {
        assert_eq!(nr::GETCWD, 17);
        assert_eq!(nr::EVENTFD2, 19);
        assert_eq!(nr::EPOLL_CREATE1, 20);
        assert_eq!(nr::EPOLL_CTL, 21);
        assert_eq!(nr::EPOLL_PWAIT, 22);
        assert_eq!(nr::DUP3, 24);
        assert_eq!(nr::FCNTL, 25);
        assert_eq!(nr::IOCTL, 29);
        assert_eq!(nr::OPENAT, 56);
        assert_eq!(nr::CLOSE, 57);
        assert_eq!(nr::PIPE2, 59);
        assert_eq!(nr::GETDENTS64, 61);
        assert_eq!(nr::READ, 63);
        assert_eq!(nr::WRITE, 64);
        assert_eq!(nr::NEWFSTATAT, 79);
        assert_eq!(nr::FSTAT, 80);
        assert_eq!(nr::EXIT, 93);
        assert_eq!(nr::EXIT_GROUP, 94);
        assert_eq!(nr::FUTEX, 98);
        assert_eq!(nr::NANOSLEEP, 101);
        assert_eq!(nr::SETITIMER, 103);
        assert_eq!(nr::TIMER_CREATE, 107);
        assert_eq!(nr::CLOCK_GETTIME, 113);
        assert_eq!(nr::SCHED_GETAFFINITY, 123);
        assert_eq!(nr::SCHED_YIELD, 124);
        assert_eq!(nr::KILL, 129);
        assert_eq!(nr::TGKILL, 131);
        assert_eq!(nr::SIGALTSTACK, 132);
        assert_eq!(nr::RT_SIGACTION, 134);
        assert_eq!(nr::RT_SIGPROCMASK, 135);
        assert_eq!(nr::RT_SIGRETURN, 139);
        assert_eq!(nr::UNAME, 160);
        assert_eq!(nr::GETPID, 172);
        assert_eq!(nr::GETTID, 178);
        assert_eq!(nr::SOCKET, 198);
        assert_eq!(nr::MUNMAP, 215);
        assert_eq!(nr::CLONE, 220);
        assert_eq!(nr::MMAP, 222);
        assert_eq!(nr::MPROTECT, 226);
        assert_eq!(nr::MADVISE, 233);
        assert_eq!(nr::ACCEPT4, 242);
        assert_eq!(nr::PRLIMIT64, 261);
        assert_eq!(nr::GETRANDOM, 278);
        assert_eq!(nr::STATX, 291);
        assert_eq!(nr::FACCESSAT2, 439);
        assert_eq!(syscall_name(63), Some("read"));
        assert_eq!(syscall_name(1024), None);
        assert!(nr::TABLE.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn clone_order() {
        let a = clone_args([1, 2, 3, 4, 5]);
        assert_eq!(
            (a.flags, a.stack, a.parent_tid, a.tls, a.child_tid),
            (1, 2, 3, 4, 5)
        );
    }

    #[test]
    fn epoll_event() {
        let e = EpollEvent::new(1, 0xabcd);
        let b = e.as_bytes();
        assert_eq!(b.len(), 16);
        assert_eq!(&b[8..16], &0xabcdu64.to_le_bytes());
    }

    #[test]
    fn reserved_records() {
        let mut sc = SigContext::default();
        let mut fp = FpsimdContext {
            fpsr: 0x1234,
            ..Default::default()
        };
        fp.vregs[31] = [5, 6];
        sc.write_reserved(&fp, Some(0x9200_0047));
        let r = &sc.reserved;
        assert_eq!(&r[0..4], &FPSIMD_MAGIC.to_le_bytes());
        assert_eq!(&r[4..8], &528u32.to_le_bytes());
        assert_eq!(&r[8..12], &0x1234u32.to_le_bytes());
        assert_eq!(&r[528..532], &ESR_MAGIC.to_le_bytes());
        assert_eq!(&r[532..536], &16u32.to_le_bytes());
        assert_eq!(&r[536..544], &0x9200_0047u64.to_le_bytes());
        assert_eq!(&r[544..552], &[0u8; 8]);
        let back = sc.read_fpsimd().unwrap();
        assert_eq!(back.fpsr, 0x1234);
        assert_eq!(back.vregs[31], [5, 6]);

        // Unknown record -> reject.
        let mut bad = sc;
        bad.reserved[528..532].copy_from_slice(&SVE_MAGIC.to_le_bytes());
        assert_eq!(bad.read_fpsimd(), None);
        // Bogus size -> reject.
        let mut bad = sc;
        bad.reserved[4..8].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(bad.read_fpsimd(), None);
        // No records at all -> no FPSIMD.
        assert_eq!(SigContext::default().read_fpsimd(), None);
        // Unterminated: fill with 16-byte ESR records to the end.
        let mut full = SigContext::default();
        for off in (0..SIGCONTEXT_RESERVED_SIZE).step_by(16) {
            full.reserved[off..off + 16].copy_from_slice(EsrContext::new(0).as_bytes());
        }
        assert_eq!(full.read_fpsimd(), None);
    }

    #[test]
    fn placement() {
        let p = sigframe_placement(0x7fff_1238);
        assert_eq!(p.frame_record, 0x7fff_1220);
        assert_eq!(p.frame % 16, 0);
        assert_eq!(p.frame, 0x7fff_1220 - 4688);
        assert_eq!(
            p.frame + core::mem::size_of::<RtSigframe>() as u64,
            p.frame_record
        );
    }

    #[test]
    fn hwcaps() {
        // Neoverse-N1-like ID_AA64ISAR0_EL1: AES=2, SHA1=1, SHA2=1, CRC32=1, ATOMIC=2, RDM=1, DP=1.
        let isar0: u64 =
            (2 << 4) | (1 << 8) | (1 << 12) | (1 << 16) | (2 << 20) | (1 << 28) | (1 << 44);
        let h = hwcap::from_id_aa64isar0(isar0);
        use hwcap::*;
        assert_eq!(
            h,
            FP | ASIMD | AES | PMULL | SHA1 | SHA2 | CRC32 | ATOMICS | ASIMDRDM | ASIMDDP
        );
    }
}
