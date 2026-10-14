//! Signals: numbers, `sigaction`, `stack_t`, `siginfo_t`, `sigevent`.
//!
//! Signal numbers, `SA_*` flags, `siginfo_t`, `stack_t`, `sigevent` and the
//! kernel `struct sigaction` are identical on x86_64 and aarch64 (x86's
//! `asm/signal.h` uses the same numbering as `asm-generic/signal.h`; verified
//! against x/sys `zerrors_linux_{amd64,arm64}.go`). `MINSIGSTKSZ`/`SIGSTKSZ`
//! and the frame layouts differ and live in the arch modules.

use bitflags::bitflags;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGABRT: i32 = 6;
pub const SIGIOT: i32 = 6;
pub const SIGBUS: i32 = 7;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGUSR1: i32 = 10;
pub const SIGSEGV: i32 = 11;
pub const SIGUSR2: i32 = 12;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGTERM: i32 = 15;
pub const SIGSTKFLT: i32 = 16;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
pub const SIGURG: i32 = 23;
pub const SIGXCPU: i32 = 24;
pub const SIGXFSZ: i32 = 25;
pub const SIGVTALRM: i32 = 26;
pub const SIGPROF: i32 = 27;
pub const SIGWINCH: i32 = 28;
pub const SIGIO: i32 = 29;
pub const SIGPOLL: i32 = SIGIO;
pub const SIGPWR: i32 = 30;
pub const SIGSYS: i32 = 31;
/// First real-time signal as the kernel sees it (glibc reserves 32..34).
pub const SIGRTMIN: i32 = 32;
pub const SIGRTMAX: i32 = 64;
/// Number of signals (`_NSIG`); valid signal numbers are `1..=NSIG`.
pub const NSIG: i32 = 64;

/// Signal set as passed to `rt_sigprocmask`/`rt_sigaction` (`sigsetsize` 8):
/// bit `sig - 1` represents signal `sig`.
pub type SigSet = u64;

/// Bit for `sig` in a [`SigSet`]; 0 for out-of-range numbers.
#[inline]
pub const fn sigbit(sig: i32) -> SigSet {
    if sig >= 1 && sig <= NSIG {
        1u64 << (sig - 1)
    } else {
        0
    }
}

/// Signals that can never be blocked or caught.
pub const UNBLOCKABLE: SigSet = sigbit(SIGKILL) | sigbit(SIGSTOP);

/// Size userspace must pass as `sigsetsize` to `rt_sig*` syscalls.
pub const SIGSET_SIZE: usize = 8;

/// `how` for `rt_sigprocmask`.
pub const SIG_BLOCK: i32 = 0;
pub const SIG_UNBLOCK: i32 = 1;
pub const SIG_SETMASK: i32 = 2;

/// Special handler values.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
pub const SIG_ERR: u64 = u64::MAX;

bitflags! {
    /// `sa_flags` (kernel `struct sigaction`).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct SaFlags: u64 {
        const NOCLDSTOP = 0x0000_0001;
        const NOCLDWAIT = 0x0000_0002;
        const SIGINFO = 0x0000_0004;
        /// Kernel-internal on most arches but accepted by `rt_sigaction`.
        const UNSUPPORTED = 0x0000_0400;
        const EXPOSE_TAGBITS = 0x0000_0800;
        const RESTORER = 0x0400_0000;
        const ONSTACK = 0x0800_0000;
        const RESTART = 0x1000_0000;
        const NODEFER = 0x4000_0000;
        const RESETHAND = 0x8000_0000;
    }
}

/// Flags Go passes to every `rt_sigaction` it installs.
pub const GO_SA_FLAGS: SaFlags = SaFlags::SIGINFO
    .union(SaFlags::ONSTACK)
    .union(SaFlags::RESTORER)
    .union(SaFlags::RESTART);

/// `ss_flags` for `sigaltstack`.
pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;
pub const SS_AUTODISARM: i32 = 1 << 31;

/// `si_code` values.
pub const SI_USER: i32 = 0;
pub const SI_KERNEL: i32 = 0x80;
pub const SI_QUEUE: i32 = -1;
pub const SI_TIMER: i32 = -2;
pub const SI_MESGQ: i32 = -3;
pub const SI_ASYNCIO: i32 = -4;
pub const SI_SIGIO: i32 = -5;
pub const SI_TKILL: i32 = -6;

pub const SEGV_MAPERR: i32 = 1;
pub const SEGV_ACCERR: i32 = 2;
pub const SEGV_BNDERR: i32 = 3;
pub const SEGV_PKUERR: i32 = 4;

pub const BUS_ADRALN: i32 = 1;
pub const BUS_ADRERR: i32 = 2;
pub const BUS_OBJERR: i32 = 3;

pub const ILL_ILLOPC: i32 = 1;
pub const ILL_ILLOPN: i32 = 2;
pub const ILL_ILLADR: i32 = 3;
pub const ILL_ILLTRP: i32 = 4;
pub const ILL_PRVOPC: i32 = 5;
pub const ILL_PRVREG: i32 = 6;
pub const ILL_COPROC: i32 = 7;
pub const ILL_BADSTK: i32 = 8;

pub const FPE_INTDIV: i32 = 1;
pub const FPE_INTOVF: i32 = 2;
pub const FPE_FLTDIV: i32 = 3;
pub const FPE_FLTOVF: i32 = 4;
pub const FPE_FLTUND: i32 = 5;
pub const FPE_FLTRES: i32 = 6;
pub const FPE_FLTINV: i32 = 7;
pub const FPE_FLTSUB: i32 = 8;

pub const TRAP_BRKPT: i32 = 1;
pub const TRAP_TRACE: i32 = 2;

pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_DUMPED: i32 = 3;

/// `sigev_notify` values.
pub const SIGEV_SIGNAL: i32 = 0;
pub const SIGEV_NONE: i32 = 1;
pub const SIGEV_THREAD: i32 = 2;
pub const SIGEV_THREAD_ID: i32 = 4;

/// What `SIG_DFL` does for a signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultAction {
    /// Terminate the process (exit status `128 + sig`).
    Terminate,
    /// Terminate with a core dump (patina: same as `Terminate`).
    Core,
    Ignore,
    Stop,
    Continue,
}

/// Default disposition of `sig` (signal(7)). Real-time signals terminate.
pub const fn default_action(sig: i32) -> DefaultAction {
    match sig {
        SIGQUIT | SIGILL | SIGTRAP | SIGABRT | SIGBUS | SIGFPE | SIGSEGV | SIGXCPU | SIGXFSZ
        | SIGSYS => DefaultAction::Core,
        SIGCHLD | SIGURG | SIGWINCH => DefaultAction::Ignore,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => DefaultAction::Stop,
        SIGCONT => DefaultAction::Continue,
        _ => DefaultAction::Terminate,
    }
}

const NAMES: [&str; 31] = [
    "SIGHUP",
    "SIGINT",
    "SIGQUIT",
    "SIGILL",
    "SIGTRAP",
    "SIGABRT",
    "SIGBUS",
    "SIGFPE",
    "SIGKILL",
    "SIGUSR1",
    "SIGSEGV",
    "SIGUSR2",
    "SIGPIPE",
    "SIGALRM",
    "SIGTERM",
    "SIGSTKFLT",
    "SIGCHLD",
    "SIGCONT",
    "SIGSTOP",
    "SIGTSTP",
    "SIGTTIN",
    "SIGTTOU",
    "SIGURG",
    "SIGXCPU",
    "SIGXFSZ",
    "SIGVTALRM",
    "SIGPROF",
    "SIGWINCH",
    "SIGIO",
    "SIGPWR",
    "SIGSYS",
];

/// `"SIGTERM"` for 15, etc. Only the classic signals 1..=31 have names.
pub fn name(sig: i32) -> Option<&'static str> {
    if (1..=31).contains(&sig) {
        Some(NAMES[(sig - 1) as usize])
    } else {
        None
    }
}

/// Parse a signal name (`"SIGTERM"` or `"TERM"`, case-insensitive) or a
/// decimal number in `1..=NSIG`.
pub fn from_name(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return if (1..=NSIG).contains(&n) {
            Some(n)
        } else {
            None
        };
    }
    let b = s.as_bytes();
    let stem = if b.len() > 3 && b[..3].eq_ignore_ascii_case(b"SIG") {
        &b[3..]
    } else {
        b
    };
    for (i, n) in NAMES.iter().enumerate() {
        if n.as_bytes()[3..].eq_ignore_ascii_case(stem) {
            return Some(i as i32 + 1);
        }
    }
    match () {
        _ if stem.eq_ignore_ascii_case(b"IOT") => Some(SIGIOT),
        _ if stem.eq_ignore_ascii_case(b"POLL") => Some(SIGPOLL),
        _ => None,
    }
}

/// Kernel `struct sigaction` as read/written by `rt_sigaction` (NOT glibc's
/// layout: the mask is last and is a single 64-bit word). Same on x86_64 and
/// aarch64 (arm64 defines `SA_RESTORER`, so `sa_restorer` is present).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SigAction {
    /// Handler address, or `SIG_DFL`/`SIG_IGN`.
    pub sa_handler: u64,
    pub sa_flags: u64,
    pub sa_restorer: u64,
    pub sa_mask: SigSet,
}

impl SigAction {
    pub fn flags(&self) -> SaFlags {
        SaFlags::from_bits_retain(self.sa_flags)
    }
}

/// `stack_t` for `sigaltstack` and `uc_stack`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct StackT {
    pub ss_sp: u64,
    pub ss_flags: i32,
    pub _pad: i32,
    pub ss_size: u64,
}

/// `siginfo_t`: 128 bytes. `si_signo`, `si_errno`, `si_code`, then a union
/// at offset 16 (pointer-aligned on 64-bit). The union is kept as raw words;
/// use the constructors/accessors for the arms Go reads:
///
/// | arm | fields (offset) |
/// |---|---|
/// | kill / tkill / rt | `si_pid` (16) i32, `si_uid` (20) u32, rt: `si_value` (24) |
/// | POSIX timer | `si_tid` (16) i32, `si_overrun` (20) i32, `si_value` (24) |
/// | SIGCHLD | `si_pid` (16), `si_uid` (20), `si_status` (24) i32, `si_utime` (32), `si_stime` (40) |
/// | SIGSEGV/BUS/ILL/FPE/TRAP | `si_addr` (16) |
/// | SIGPOLL | `si_band` (16) i64, `si_fd` (24) i32 |
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SigInfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    pub _pad: i32,
    /// `_sifields` union, 112 bytes.
    pub fields: [u64; 14],
}

impl SigInfo {
    /// Byte offsets (from the start of `siginfo_t`) of union members.
    pub const OFF_PID: usize = 16;
    pub const OFF_UID: usize = 20;
    pub const OFF_TID: usize = 16;
    pub const OFF_OVERRUN: usize = 20;
    pub const OFF_VALUE: usize = 24;
    pub const OFF_STATUS: usize = 24;
    pub const OFF_ADDR: usize = 16;
    pub const OFF_BAND: usize = 16;
    pub const OFF_FD: usize = 24;

    fn put_u32(&mut self, off: usize, v: u32) {
        let o = off - 16;
        self.fields.as_mut_bytes()[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(&mut self, off: usize, v: u64) {
        let o = off - 16;
        self.fields.as_mut_bytes()[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn get_u32(&self, off: usize) -> u32 {
        let o = off - 16;
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.fields.as_bytes()[o..o + 4]);
        u32::from_le_bytes(b)
    }
    fn get_u64(&self, off: usize) -> u64 {
        let o = off - 16;
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.fields.as_bytes()[o..o + 8]);
        u64::from_le_bytes(b)
    }

    fn base(signo: i32, code: i32) -> Self {
        SigInfo {
            si_signo: signo,
            si_code: code,
            ..Default::default()
        }
    }

    /// Fault-generated signal (SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGTRAP) with
    /// `si_addr`.
    pub fn fault(signo: i32, code: i32, addr: u64) -> Self {
        let mut s = Self::base(signo, code);
        s.put_u64(Self::OFF_ADDR, addr);
        s
    }

    /// `kill`/`tgkill`/host-injected signal: `si_pid`, `si_uid`.
    /// `code` is `SI_USER` for kill, `SI_TKILL` for tkill/tgkill.
    pub fn kill(signo: i32, code: i32, pid: i32, uid: u32) -> Self {
        let mut s = Self::base(signo, code);
        s.put_u32(Self::OFF_PID, pid as u32);
        s.put_u32(Self::OFF_UID, uid);
        s
    }

    /// POSIX timer expiry (`SI_TIMER`): `si_tid` (kernel timer id),
    /// `si_overrun`, `si_value` (from `sigev_value`).
    pub fn timer(signo: i32, timer_id: i32, overrun: i32, value: u64) -> Self {
        let mut s = Self::base(signo, SI_TIMER);
        s.put_u32(Self::OFF_TID, timer_id as u32);
        s.put_u32(Self::OFF_OVERRUN, overrun as u32);
        s.put_u64(Self::OFF_VALUE, value);
        s
    }

    /// Kernel-generated signal with no extra info (`SI_KERNEL`), e.g.
    /// `setitimer` SIGPROF/SIGALRM.
    pub fn kernel(signo: i32) -> Self {
        Self::base(signo, SI_KERNEL)
    }

    pub fn addr(&self) -> u64 {
        self.get_u64(Self::OFF_ADDR)
    }
    pub fn pid(&self) -> i32 {
        self.get_u32(Self::OFF_PID) as i32
    }
    pub fn uid(&self) -> u32 {
        self.get_u32(Self::OFF_UID)
    }
    pub fn timer_id(&self) -> i32 {
        self.get_u32(Self::OFF_TID) as i32
    }
    pub fn overrun(&self) -> i32 {
        self.get_u32(Self::OFF_OVERRUN) as i32
    }
    pub fn value(&self) -> u64 {
        self.get_u64(Self::OFF_VALUE)
    }
}

/// `struct sigevent` (64 bytes) for `timer_create`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SigEvent {
    /// `sigev_value` (union sigval: int or pointer).
    pub sigev_value: u64,
    pub sigev_signo: i32,
    pub sigev_notify: i32,
    /// Union at offset 16: `_tid` (i32) for `SIGEV_THREAD_ID`, or
    /// `_sigev_thread { function, attribute }`, padded to 64 bytes.
    pub un: [u64; 6],
}

impl SigEvent {
    /// `sigev_notify_thread_id` (`_sigev_un._tid`), meaningful with
    /// `SIGEV_THREAD_ID`.
    pub fn thread_id(&self) -> i32 {
        self.un[0] as u32 as i32
    }

    pub fn set_thread_id(&mut self, tid: i32) {
        self.un[0] = (self.un[0] & !0xffff_ffff) | tid as u32 as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(name(SIGTERM), Some("SIGTERM"));
        assert_eq!(name(SIGURG), Some("SIGURG"));
        assert_eq!(name(SIGSYS), Some("SIGSYS"));
        assert_eq!(name(0), None);
        assert_eq!(name(32), None);
        assert_eq!(from_name("SIGTERM"), Some(15));
        assert_eq!(from_name("term"), Some(15));
        assert_eq!(from_name("SigQuit"), Some(3));
        assert_eq!(from_name("SIGIOT"), Some(6));
        assert_eq!(from_name("9"), Some(9));
        assert_eq!(from_name("65"), None);
        assert_eq!(from_name("SIGFOO"), None);
        assert_eq!(from_name(""), None);
        assert_eq!(from_name("SIG"), None);
        for s in 1..=31 {
            assert_eq!(from_name(name(s).unwrap()), Some(s));
        }
    }

    #[test]
    fn sets_and_defaults() {
        assert_eq!(sigbit(1), 1);
        assert_eq!(sigbit(64), 1 << 63);
        assert_eq!(sigbit(0), 0);
        assert_eq!(sigbit(65), 0);
        assert_eq!(default_action(SIGTERM), DefaultAction::Terminate);
        assert_eq!(default_action(SIGURG), DefaultAction::Ignore);
        assert_eq!(default_action(SIGCHLD), DefaultAction::Ignore);
        assert_eq!(default_action(SIGSEGV), DefaultAction::Core);
        assert_eq!(default_action(SIGRTMIN), DefaultAction::Terminate);
        assert_eq!(GO_SA_FLAGS.bits(), 0x1c00_0004);
    }

    #[test]
    fn siginfo_arms() {
        let s = SigInfo::fault(SIGSEGV, SEGV_MAPERR, 0xdead_beef_0000);
        let b = s.as_bytes();
        assert_eq!(&b[0..4], &11i32.to_le_bytes());
        assert_eq!(&b[8..12], &1i32.to_le_bytes());
        assert_eq!(&b[16..24], &0xdead_beef_0000u64.to_le_bytes());
        assert_eq!(s.addr(), 0xdead_beef_0000);

        let k = SigInfo::kill(SIGURG, SI_TKILL, 1, 0);
        let b = k.as_bytes();
        assert_eq!(&b[8..12], &(-6i32).to_le_bytes());
        assert_eq!(&b[16..20], &1i32.to_le_bytes());
        assert_eq!(&b[20..24], &0u32.to_le_bytes());
        assert_eq!((k.pid(), k.uid()), (1, 0));

        let t = SigInfo::timer(SIGPROF, 7, 2, 0x1234);
        let b = t.as_bytes();
        assert_eq!(&b[8..12], &(-2i32).to_le_bytes());
        assert_eq!(&b[16..20], &7i32.to_le_bytes());
        assert_eq!(&b[20..24], &2i32.to_le_bytes());
        assert_eq!(&b[24..32], &0x1234u64.to_le_bytes());
        assert_eq!((t.timer_id(), t.overrun(), t.value()), (7, 2, 0x1234));
    }

    #[test]
    fn sigevent_tid() {
        let mut e = SigEvent::default();
        e.set_thread_id(42);
        assert_eq!(e.thread_id(), 42);
        assert_eq!(&e.as_bytes()[16..20], &42i32.to_le_bytes());
    }
}
