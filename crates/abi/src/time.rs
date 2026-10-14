//! Time structs and clock/timer constants (same on x86_64 and aarch64).

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const NSEC_PER_SEC: i64 = 1_000_000_000;

/// `struct timespec` (64-bit `time_t`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

impl Timespec {
    pub const ZERO: Timespec = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };

    pub const fn from_nanos(ns: u64) -> Self {
        Timespec {
            tv_sec: (ns / 1_000_000_000) as i64,
            tv_nsec: (ns % 1_000_000_000) as i64,
        }
    }

    /// Normalised check used by syscalls that return `EINVAL` on bad input.
    pub const fn is_valid(&self) -> bool {
        self.tv_sec >= 0 && self.tv_nsec >= 0 && self.tv_nsec < NSEC_PER_SEC
    }

    /// Total nanoseconds, saturating; `None` if invalid.
    pub const fn to_nanos(&self) -> Option<u64> {
        if !self.is_valid() {
            return None;
        }
        Some(
            (self.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(self.tv_nsec as u64),
        )
    }
}

/// `struct timeval`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Timeval {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl Timeval {
    pub const fn from_nanos(ns: u64) -> Self {
        Timeval {
            tv_sec: (ns / 1_000_000_000) as i64,
            tv_usec: ((ns % 1_000_000_000) / 1000) as i64,
        }
    }

    pub const fn is_valid(&self) -> bool {
        self.tv_sec >= 0 && self.tv_usec >= 0 && self.tv_usec < 1_000_000
    }

    pub const fn to_nanos(&self) -> Option<u64> {
        if !self.is_valid() {
            return None;
        }
        Some(
            (self.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(self.tv_usec as u64 * 1000),
        )
    }
}

/// `struct itimerval` for `setitimer`/`getitimer`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Itimerval {
    pub it_interval: Timeval,
    pub it_value: Timeval,
}

/// `struct itimerspec` for `timer_settime`/`timerfd`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Itimerspec {
    pub it_interval: Timespec,
    pub it_value: Timespec,
}

/// `struct timezone` for `gettimeofday`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Timezone {
    pub tz_minuteswest: i32,
    pub tz_dsttime: i32,
}

pub const CLOCK_REALTIME: i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
pub const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
pub const CLOCK_MONOTONIC_RAW: i32 = 4;
pub const CLOCK_REALTIME_COARSE: i32 = 5;
pub const CLOCK_MONOTONIC_COARSE: i32 = 6;
pub const CLOCK_BOOTTIME: i32 = 7;
pub const CLOCK_REALTIME_ALARM: i32 = 8;
pub const CLOCK_BOOTTIME_ALARM: i32 = 9;
pub const CLOCK_TAI: i32 = 11;

/// Dynamic per-thread/process CPU-time clock ids (`clock_getcpuclockid`,
/// `pthread_getcpuclockid`): `(~pid << 3) | type | (thread ? 4 : 0)`.
pub const CPUCLOCK_PROF: i32 = 0;
pub const CPUCLOCK_VIRT: i32 = 1;
pub const CPUCLOCK_SCHED: i32 = 2;
pub const CPUCLOCK_PERTHREAD_MASK: i32 = 4;
pub const CPUCLOCK_CLOCK_MASK: i32 = 3;
/// Number of CPU clock kinds; `kind == CPUCLOCK_MAX` (3) is `CLOCKFD`, not a CPU clock.
pub const CPUCLOCK_MAX: i32 = 3;

/// A decoded negative (dynamic) clock id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuClock {
    /// pid or tid; 0 means "self".
    pub id: i32,
    pub per_thread: bool,
    pub kind: i32,
}

/// Build a dynamic CPU clock id.
pub const fn make_cpuclock(id: i32, per_thread: bool, kind: i32) -> i32 {
    ((!id) << 3)
        | kind
        | if per_thread {
            CPUCLOCK_PERTHREAD_MASK
        } else {
            0
        }
}

/// Decode a dynamic CPU clock id (only for `clockid < 0`).
pub const fn decode_cpuclock(clockid: i32) -> Option<CpuClock> {
    if clockid >= 0 || (clockid & CPUCLOCK_CLOCK_MASK) == 3 {
        return None;
    }
    Some(CpuClock {
        id: !(clockid >> 3),
        per_thread: clockid & CPUCLOCK_PERTHREAD_MASK != 0,
        kind: clockid & CPUCLOCK_CLOCK_MASK,
    })
}

/// `flags` for `clock_nanosleep` / `timer_settime`.
pub const TIMER_ABSTIME: i32 = 1;

/// `which` for `setitimer`.
pub const ITIMER_REAL: i32 = 0;
pub const ITIMER_VIRTUAL: i32 = 1;
pub const ITIMER_PROF: i32 = 2;

/// `tv_nsec` sentinels for `utimensat`.
pub const UTIME_NOW: i64 = (1 << 30) - 1;
pub const UTIME_OMIT: i64 = (1 << 30) - 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let t = Timespec::from_nanos(1_500_000_001);
        assert_eq!(
            t,
            Timespec {
                tv_sec: 1,
                tv_nsec: 500_000_001
            }
        );
        assert_eq!(t.to_nanos(), Some(1_500_000_001));
        assert_eq!(
            Timespec {
                tv_sec: 0,
                tv_nsec: NSEC_PER_SEC
            }
            .to_nanos(),
            None
        );
        assert_eq!(
            Timespec {
                tv_sec: -1,
                tv_nsec: 0
            }
            .to_nanos(),
            None
        );
        assert_eq!(
            Timespec {
                tv_sec: i64::MAX,
                tv_nsec: 0
            }
            .to_nanos(),
            Some(u64::MAX)
        );
        let v = Timeval::from_nanos(2_000_003_000);
        assert_eq!(
            v,
            Timeval {
                tv_sec: 2,
                tv_usec: 3
            }
        );
        assert_eq!(v.to_nanos(), Some(2_000_003_000));
    }

    #[test]
    fn cpuclock() {
        // glibc: pthread_getcpuclockid(tid=1234) => MAKE_THREAD_CPUCLOCK(1234, CPUCLOCK_SCHED)
        let c = make_cpuclock(1234, true, CPUCLOCK_SCHED);
        assert!(c < 0);
        assert_eq!(c, ((!1234i32) << 3) | 2 | 4);
        assert_eq!(
            decode_cpuclock(c),
            Some(CpuClock {
                id: 1234,
                per_thread: true,
                kind: CPUCLOCK_SCHED
            })
        );
        assert_eq!(decode_cpuclock(CLOCK_MONOTONIC), None);
        // clock_getcpuclockid(0) for the calling process.
        assert_eq!(make_cpuclock(0, false, CPUCLOCK_SCHED), -6);
    }
}
