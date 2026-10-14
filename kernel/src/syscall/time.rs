//! Clocks and sleeping. `CLOCK_MONOTONIC` and friends are
//! `arch::timer::now_ns` (time since boot); `CLOCK_REALTIME` adds the
//! `patina.epoch` given on the command line. CPU-time clocks, POSIX
//! timers and itimers are `task::timers`.

use patina_abi::errno::{EFAULT, EINVAL, ENOTSUP, Errno};
use patina_abi::misc::{RUSAGE_CHILDREN, RUSAGE_SELF, RUSAGE_THREAD, Rusage, Tms, USER_HZ};
use patina_abi::signal::{SIGALRM, SIGEV_NONE, SIGEV_SIGNAL, SIGEV_THREAD_ID, SigEvent};
use patina_abi::time::{
    CLOCK_REALTIME, CLOCK_REALTIME_ALARM, CLOCK_REALTIME_COARSE, CLOCK_TAI, ITIMER_REAL,
    Itimerspec, Itimerval, TIMER_ABSTIME, Timespec, Timeval,
};

use super::{SysResult, with_space};
use crate::arch::timer;
use crate::task::signal::{ERESTART_RESTARTBLOCK, ERESTARTNOHAND, RestartBlock};
use crate::task::timers::{self, Clock, NewTimer, Notify};
use crate::task::{self, WakeReason, wait};

fn is_realtime(clk: i32) -> bool {
    matches!(
        clk,
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | CLOCK_TAI
    )
}

/// Current value of `clk` in ns, or EINVAL. CPU-time clocks (static and
/// encoded per-thread/process ids) read `task::timers`.
fn clock_now(clk: i32) -> Result<u64, Errno> {
    timers::resolve_clock(clk, false)?
        .now()
        .ok_or(Errno(EINVAL))
}

pub fn clock_gettime(clk: i32, tp: usize) -> SysResult {
    let ns = clock_now(clk)?;
    with_space(|s| s.write_val(tp, &Timespec::from_nanos(ns)))?;
    Ok(0)
}

pub fn clock_getres(clk: i32, tp: usize) -> SysResult {
    clock_now(clk)?;
    if tp != 0 {
        with_space(|s| s.write_val(tp, &Timespec::from_nanos(1)))?;
    }
    Ok(0)
}

pub fn gettimeofday(tv: usize) -> SysResult {
    if tv != 0 {
        let ns = timer::now_ns() + crate::wallclock::epoch_ns();
        with_space(|s| s.write_val(tv, &Timeval::from_nanos(ns)))?;
    }
    Ok(0)
}

fn read_timespec(p: usize) -> Result<u64, Errno> {
    let ts: Timespec = with_space(|s| s.read_val(p))?;
    ts.to_nanos().ok_or(Errno(EINVAL))
}

/// Sleep until `deadline` (monotonic ns), interruptible. On a signal:
/// write the remaining time to `rem` (if non-zero) and return
/// `ERESTART_RESTARTBLOCK` with a restart block, so the call becomes
/// `EINTR` when a handler runs (whatever `SA_RESTART` says, as Linux) and
/// continues to the same deadline through `restart_syscall` otherwise.
pub fn sleep_restartable(deadline: u64, rem: usize) -> SysResult {
    if wait::sleep_until_interruptible(deadline) != WakeReason::Interrupted {
        return Ok(0);
    }
    if rem != 0 {
        let left = deadline.saturating_sub(timer::now_ns());
        with_space(|s| s.write_val(rem, &Timespec::from_nanos(left)))?;
    }
    *task::current().restart_block.lock() = Some(RestartBlock::Nanosleep { deadline, rem });
    Err(Errno(ERESTART_RESTARTBLOCK))
}

pub fn nanosleep(req: usize, rem: usize) -> SysResult {
    let ns = read_timespec(req)?;
    sleep_restartable(timer::now_ns().saturating_add(ns), rem)
}

/// `clock_nanosleep`: relative sleeps behave like `nanosleep`; with
/// `TIMER_ABSTIME` an interrupted sleep returns `ERESTARTNOHAND` (EINTR
/// with a handler, transparently re-executed otherwise) and never writes
/// `rem`.
pub fn clock_nanosleep(clk: i32, flags: i32, req: usize, rem: usize) -> SysResult {
    let ns = read_timespec(req)?;
    match timers::resolve_clock(clk, false)? {
        // Linux: thread CPU clocks cannot be slept on; process CPU clock
        // sleeps are allowed there but Go never uses them.
        Clock::ThreadCpu(_) => return Err(Errno(EINVAL)),
        Clock::ProcessCpu => return Err(Errno(ENOTSUP)),
        _ => {}
    }
    if flags & TIMER_ABSTIME == 0 {
        return sleep_restartable(timer::now_ns().saturating_add(ns), rem);
    }
    let deadline = if is_realtime(clk) {
        ns.saturating_sub(crate::wallclock::epoch_ns())
    } else {
        ns
    };
    match wait::sleep_until_interruptible(deadline) {
        WakeReason::Interrupted => Err(Errno(ERESTARTNOHAND)),
        _ => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// POSIX timers and itimers (`task::timers`).

fn read_itimerspec(p: usize) -> Result<(u64, u64), Errno> {
    let its: Itimerspec = with_space(|s| s.read_val(p))?;
    let interval = its.it_interval.to_nanos().ok_or(Errno(EINVAL))?;
    let value = its.it_value.to_nanos().ok_or(Errno(EINVAL))?;
    Ok((interval, value))
}

fn write_itimerspec(p: usize, (interval, value): (u64, u64)) -> Result<(), Errno> {
    let its = Itimerspec {
        it_interval: Timespec::from_nanos(interval),
        it_value: Timespec::from_nanos(value),
    };
    with_space(|s| s.write_val(p, &its))
}

/// `timer_create(clockid, sevp, timerid)`. Notifications: `SIGEV_SIGNAL`
/// (process), `SIGEV_THREAD_ID` (a thread of the process, else EINVAL),
/// `SIGEV_NONE`; NULL `sevp` = `SIGEV_SIGNAL`/SIGALRM with the timer id
/// as value. `SIGEV_THREAD` is a libc construct → EINVAL.
pub fn timer_create(clk: i32, sevp: usize, idp: usize) -> SysResult {
    let clock = timers::resolve_clock(clk, true)?;
    let new = if sevp == 0 {
        NewTimer {
            clock,
            notify: Notify::Process,
            signo: SIGALRM,
            sigval: None,
        }
    } else {
        let ev: SigEvent = with_space(|s| s.read_val(sevp))?;
        let notify = match ev.sigev_notify {
            SIGEV_NONE => Notify::None,
            SIGEV_SIGNAL => Notify::Process,
            n if n == SIGEV_THREAD_ID | SIGEV_SIGNAL => {
                let t = timers::user_thread(ev.thread_id()).ok_or(Errno(EINVAL))?;
                Notify::Thread(alloc::sync::Arc::downgrade(&t))
            }
            _ => return Err(Errno(EINVAL)),
        };
        if !matches!(notify, Notify::None) && !(1..=64).contains(&ev.sigev_signo) {
            return Err(Errno(EINVAL));
        }
        NewTimer {
            clock,
            notify,
            signo: ev.sigev_signo,
            sigval: Some(ev.sigev_value),
        }
    };
    let id = timers::create(new)?;
    if with_space(|s| s.write_val(idp, &id)).is_err() {
        let _ = timers::delete(id);
        return Err(Errno(EFAULT));
    }
    Ok(0)
}

pub fn timer_settime(id: i32, flags: i32, new: usize, old: usize) -> SysResult {
    let (interval, value) = read_itimerspec(new)?;
    let prev = timers::settime(id, flags & TIMER_ABSTIME != 0, interval, value)?;
    if old != 0 {
        write_itimerspec(old, prev)?;
    }
    Ok(0)
}

pub fn timer_gettime(id: i32, cur: usize) -> SysResult {
    write_itimerspec(cur, timers::gettime(id)?)?;
    Ok(0)
}

pub fn timer_getoverrun(id: i32) -> SysResult {
    Ok(timers::getoverrun(id)? as isize)
}

pub fn timer_delete(id: i32) -> SysResult {
    timers::delete(id)?;
    Ok(0)
}

fn write_itimerval(p: usize, (interval, value): (u64, u64)) -> Result<(), Errno> {
    let itv = Itimerval {
        it_interval: Timeval::from_nanos(interval),
        it_value: Timeval::from_nanos(value),
    };
    with_space(|s| s.write_val(p, &itv))
}

pub fn getitimer(which: i32, cur: usize) -> SysResult {
    let v = timers::getitimer(which)?;
    write_itimerval(cur, v)?;
    Ok(0)
}

/// `setitimer(which, new, old)`; a NULL `new` disarms (as Linux).
pub fn setitimer(which: i32, new: usize, old: usize) -> SysResult {
    let (interval, value) = if new == 0 {
        (0, 0)
    } else {
        let itv: Itimerval = with_space(|s| s.read_val(new))?;
        (
            itv.it_interval.to_nanos().ok_or(Errno(EINVAL))?,
            itv.it_value.to_nanos().ok_or(Errno(EINVAL))?,
        )
    };
    let prev = timers::setitimer(which, interval, value)?;
    if old != 0 {
        write_itimerval(old, prev)?;
    }
    Ok(0)
}

/// `alarm(secs)` (x86_64): `ITIMER_REAL` one-shot; returns the seconds
/// left on the previous alarm (Linux rounding: up from half a second, at
/// least 1 if armed).
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub fn alarm(secs: u32) -> SysResult {
    let (_, left) = timers::setitimer(ITIMER_REAL, 0, secs as u64 * 1_000_000_000)?;
    if left == 0 {
        return Ok(0);
    }
    let (sec, usec) = (left / 1_000_000_000, left % 1_000_000_000 / 1000);
    let round = (sec == 0 && usec != 0) || usec >= 500_000;
    Ok((sec + round as u64) as isize)
}

/// `getrusage`: `ru_utime` = CPU time (no user/system split), the rest 0.
pub fn getrusage(who: i32, ru: usize) -> SysResult {
    let cpu = match who {
        RUSAGE_SELF => timers::process_cpu_time_ns(),
        RUSAGE_THREAD => task::sched::current_cpu_time_ns(),
        RUSAGE_CHILDREN => 0,
        _ => return Err(Errno(EINVAL)),
    };
    let r = Rusage {
        ru_utime: Timeval::from_nanos(cpu),
        ..Rusage::default()
    };
    with_space(|s| s.write_val(ru, &r))?;
    Ok(0)
}

/// `times(buf)`: process CPU time in `USER_HZ` ticks (all user); returns
/// ticks since boot.
pub fn times(buf: usize) -> SysResult {
    let ns_per_tick = 1_000_000_000 / USER_HZ;
    if buf != 0 {
        let t = Tms {
            tms_utime: (timers::process_cpu_time_ns() / ns_per_tick) as i64,
            ..Tms::default()
        };
        with_space(|s| s.write_val(buf, &t))?;
    }
    Ok((timer::now_ns() / ns_per_tick) as isize)
}
