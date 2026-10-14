//! Clocks and sleeping. `CLOCK_MONOTONIC` and friends are
//! `arch::timer::now_ns` (time since boot); `CLOCK_REALTIME` adds the
//! `patina.epoch` given on the command line.

use patina_abi::errno::{EINVAL, Errno};
use patina_abi::time::{
    CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE,
    CLOCK_MONOTONIC_RAW, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_REALTIME_ALARM,
    CLOCK_REALTIME_COARSE, CLOCK_TAI, TIMER_ABSTIME, Timespec, Timeval,
};

use super::{SysResult, with_space};
use crate::arch::{cpu, timer};
use crate::task;

fn is_realtime(clk: i32) -> bool {
    matches!(
        clk,
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | CLOCK_TAI
    )
}

/// Current value of `clk` in ns, or EINVAL.
fn clock_now(clk: i32) -> Result<u64, Errno> {
    let now = timer::now_ns();
    match clk {
        c if is_realtime(c) => Ok(now + task::process().epoch_ns),
        CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_BOOTTIME_ALARM => Ok(now),
        // CPU-time clocks: one thread that is always running, so wall time
        // since boot is an upper bound. W2b/Phase 2: per-thread accounting.
        CLOCK_PROCESS_CPUTIME_ID | patina_abi::time::CLOCK_THREAD_CPUTIME_ID => Ok(now),
        c if c < 0 => Ok(now),
        _ => Err(Errno(EINVAL)),
    }
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
        let ns = timer::now_ns() + task::process().epoch_ns;
        with_space(|s| s.write_val(tv, &Timeval::from_nanos(ns)))?;
    }
    Ok(0)
}

/// Block until `now_ns() >= deadline` (monotonic).
///
/// W2b: replace with a real sleep (timer queue + switch to another
/// thread); this just idles the CPU between ticks.
pub fn sleep_until(deadline: u64) {
    while timer::now_ns() < deadline {
        if crate::arch::irq::enabled() {
            cpu::wait_for_interrupt();
        } else {
            core::hint::spin_loop();
        }
    }
}

fn read_timespec(p: usize) -> Result<u64, Errno> {
    let ts: Timespec = with_space(|s| s.read_val(p))?;
    ts.to_nanos().ok_or(Errno(EINVAL))
}

pub fn nanosleep(req: usize, _rem: usize) -> SysResult {
    let ns = read_timespec(req)?;
    sleep_until(timer::now_ns().saturating_add(ns));
    Ok(0)
}

pub fn clock_nanosleep(clk: i32, flags: i32, req: usize, _rem: usize) -> SysResult {
    let ns = read_timespec(req)?;
    clock_now(clk)?;
    let deadline = if flags & TIMER_ABSTIME != 0 {
        if is_realtime(clk) {
            ns.saturating_sub(task::process().epoch_ns)
        } else {
            ns
        }
    } else {
        timer::now_ns().saturating_add(ns)
    };
    sleep_until(deadline);
    Ok(0)
}
