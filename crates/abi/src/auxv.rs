//! Auxiliary vector (`AT_*`) keys (`include/uapi/linux/auxvec.h`, plus
//! `AT_SYSINFO_EHDR` and `AT_MINSIGSTKSZ` from the arch headers; both arches
//! use the same values).

pub const AT_NULL: u64 = 0;
pub const AT_IGNORE: u64 = 1;
pub const AT_EXECFD: u64 = 2;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_PAGESZ: u64 = 6;
pub const AT_BASE: u64 = 7;
pub const AT_FLAGS: u64 = 8;
pub const AT_ENTRY: u64 = 9;
pub const AT_NOTELF: u64 = 10;
pub const AT_UID: u64 = 11;
pub const AT_EUID: u64 = 12;
pub const AT_GID: u64 = 13;
pub const AT_EGID: u64 = 14;
pub const AT_PLATFORM: u64 = 15;
pub const AT_HWCAP: u64 = 16;
pub const AT_CLKTCK: u64 = 17;
pub const AT_SECURE: u64 = 23;
pub const AT_BASE_PLATFORM: u64 = 24;
pub const AT_RANDOM: u64 = 25;
pub const AT_HWCAP2: u64 = 26;
pub const AT_RSEQ_FEATURE_SIZE: u64 = 27;
pub const AT_RSEQ_ALIGN: u64 = 28;
pub const AT_HWCAP3: u64 = 29;
pub const AT_HWCAP4: u64 = 30;
pub const AT_EXECFN: u64 = 31;
/// x86 only (32-bit vsyscall entry); not used on x86_64.
pub const AT_SYSINFO: u64 = 32;
pub const AT_SYSINFO_EHDR: u64 = 33;
pub const AT_MINSIGSTKSZ: u64 = 51;

/// Bytes of randomness `AT_RANDOM` points to.
pub const AT_RANDOM_LEN: usize = 16;
