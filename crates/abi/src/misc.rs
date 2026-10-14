//! Miscellaneous structs and constants: rlimits, `uname`, `rusage`, `times`,
//! tty ioctls, `prctl`, ids.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::time::Timeval;

/// `struct rlimit` / `struct rlimit64` (identical on 64-bit).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Rlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

pub const RLIM_INFINITY: u64 = u64::MAX;
pub const RLIMIT_CPU: u32 = 0;
pub const RLIMIT_FSIZE: u32 = 1;
pub const RLIMIT_DATA: u32 = 2;
pub const RLIMIT_STACK: u32 = 3;
pub const RLIMIT_CORE: u32 = 4;
pub const RLIMIT_RSS: u32 = 5;
pub const RLIMIT_NPROC: u32 = 6;
pub const RLIMIT_NOFILE: u32 = 7;
pub const RLIMIT_MEMLOCK: u32 = 8;
pub const RLIMIT_AS: u32 = 9;
pub const RLIMIT_LOCKS: u32 = 10;
pub const RLIMIT_SIGPENDING: u32 = 11;
pub const RLIMIT_MSGQUEUE: u32 = 12;
pub const RLIMIT_NICE: u32 = 13;
pub const RLIMIT_RTPRIO: u32 = 14;
pub const RLIMIT_RTTIME: u32 = 15;
pub const RLIM_NLIMITS: u32 = 16;

/// Length of each `utsname` field including the NUL.
pub const UTSNAME_LEN: usize = 65;

/// `struct new_utsname` (390 bytes) for `uname`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Utsname {
    pub sysname: [u8; UTSNAME_LEN],
    pub nodename: [u8; UTSNAME_LEN],
    pub release: [u8; UTSNAME_LEN],
    pub version: [u8; UTSNAME_LEN],
    pub machine: [u8; UTSNAME_LEN],
    pub domainname: [u8; UTSNAME_LEN],
}

fn uts_field(s: &str) -> [u8; UTSNAME_LEN] {
    let mut f = [0u8; UTSNAME_LEN];
    let n = s.len().min(UTSNAME_LEN - 1);
    f[..n].copy_from_slice(&s.as_bytes()[..n]);
    f
}

impl Utsname {
    /// Build a `utsname`; each field is truncated to 64 bytes and
    /// NUL-terminated.
    pub fn new(
        sysname: &str,
        nodename: &str,
        release: &str,
        version: &str,
        machine: &str,
        domainname: &str,
    ) -> Self {
        Utsname {
            sysname: uts_field(sysname),
            nodename: uts_field(nodename),
            release: uts_field(release),
            version: uts_field(version),
            machine: uts_field(machine),
            domainname: uts_field(domainname),
        }
    }
}

pub const RUSAGE_SELF: i32 = 0;
pub const RUSAGE_CHILDREN: i32 = -1;
pub const RUSAGE_THREAD: i32 = 1;

/// `struct rusage` (144 bytes).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Rusage {
    pub ru_utime: Timeval,
    pub ru_stime: Timeval,
    pub ru_maxrss: i64,
    pub ru_ixrss: i64,
    pub ru_idrss: i64,
    pub ru_isrss: i64,
    pub ru_minflt: i64,
    pub ru_majflt: i64,
    pub ru_nswap: i64,
    pub ru_inblock: i64,
    pub ru_oublock: i64,
    pub ru_msgsnd: i64,
    pub ru_msgrcv: i64,
    pub ru_nsignals: i64,
    pub ru_nvcsw: i64,
    pub ru_nivcsw: i64,
}

/// `struct tms` for `times` (clock ticks, `USER_HZ` = 100).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Tms {
    pub tms_utime: i64,
    pub tms_stime: i64,
    pub tms_cutime: i64,
    pub tms_cstime: i64,
}

/// `USER_HZ` (`AT_CLKTCK`, units of `times`).
pub const USER_HZ: u64 = 100;

/// `struct winsize` (`TIOCGWINSZ`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// tty / generic ioctls (same on x86_64 and aarch64, `asm-generic/ioctls.h`).
pub const TCGETS: u32 = 0x5401;
pub const TCSETS: u32 = 0x5402;
pub const TCSETSW: u32 = 0x5403;
pub const TCSETSF: u32 = 0x5404;
pub const TIOCSCTTY: u32 = 0x540E;
pub const TIOCGPGRP: u32 = 0x540F;
pub const TIOCSPGRP: u32 = 0x5410;
pub const TIOCGWINSZ: u32 = 0x5413;
pub const TIOCSWINSZ: u32 = 0x5414;
pub const FIONREAD: u32 = 0x541B;
pub const TIOCINQ: u32 = FIONREAD;
pub const FIONBIO: u32 = 0x5421;
pub const TIOCGPTN: u32 = 0x8004_5430;
pub const FIONCLEX: u32 = 0x5450;
pub const FIOCLEX: u32 = 0x5451;
pub const FIOASYNC: u32 = 0x5452;

/// `prctl` options (subset).
pub const PR_SET_PDEATHSIG: i32 = 1;
pub const PR_GET_PDEATHSIG: i32 = 2;
pub const PR_GET_DUMPABLE: i32 = 3;
pub const PR_SET_DUMPABLE: i32 = 4;
pub const PR_SET_NAME: i32 = 15;
pub const PR_GET_NAME: i32 = 16;
pub const PR_GET_TIMERSLACK: i32 = 30;
pub const PR_SET_TIMERSLACK: i32 = 29;
pub const PR_SET_NO_NEW_PRIVS: i32 = 38;
pub const PR_GET_NO_NEW_PRIVS: i32 = 39;
pub const PR_SET_THP_DISABLE: i32 = 41;
pub const PR_GET_THP_DISABLE: i32 = 42;
pub const PR_SET_VMA: i32 = 0x53564d41;
pub const PR_SET_VMA_ANON_NAME: u64 = 0;
/// Length of a task comm name including NUL (`PR_SET_NAME`).
pub const TASK_COMM_LEN: usize = 16;

/// `struct sysinfo` (64-bit: 112 bytes; `_f` is empty).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Sysinfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub pad: u16,
    pub _pad1: u32,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
    pub _pad2: u32,
}

/// `capget`/`capset` header (`struct __user_cap_header_struct`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct CapUserHeader {
    pub version: u32,
    pub pid: i32,
}

/// One `struct __user_cap_data_struct` (v2/v3 pass two of them).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct CapUserData {
    pub effective: u32,
    pub permitted: u32,
    pub inheritable: u32,
}

pub const LINUX_CAPABILITY_VERSION_1: u32 = 0x1998_0330;
pub const LINUX_CAPABILITY_VERSION_2: u32 = 0x2007_1026;
pub const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

/// `membarrier` commands (patina may return `ENOSYS` for all of them).
pub const MEMBARRIER_CMD_QUERY: i32 = 0;
pub const MEMBARRIER_CMD_PRIVATE_EXPEDITED: i32 = 1 << 3;
pub const MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED: i32 = 1 << 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utsname() {
        let u = Utsname::new("Linux", "patina", "6.1.0-patina", "#1", "x86_64", "(none)");
        assert_eq!(&u.sysname[..6], b"Linux\0");
        assert_eq!(&u.as_bytes()[65..72], b"patina\0");
        let long = core::str::from_utf8(&[b'x'; 100]).unwrap();
        let u = Utsname::new(long, "", "", "", "", "");
        assert_eq!(u.sysname[63], b'x');
        assert_eq!(u.sysname[64], 0);
    }

    #[test]
    fn sysinfo_caps_layout() {
        assert_eq!(core::mem::size_of::<Sysinfo>(), 112);
        assert_eq!(core::mem::offset_of!(Sysinfo, procs), 80);
        assert_eq!(core::mem::offset_of!(Sysinfo, totalhigh), 88);
        assert_eq!(core::mem::offset_of!(Sysinfo, mem_unit), 104);
        assert_eq!(core::mem::size_of::<CapUserHeader>(), 8);
        assert_eq!(core::mem::size_of::<CapUserData>(), 12);
    }
}
