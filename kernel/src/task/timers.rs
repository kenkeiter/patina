//! POSIX timers (`timer_create` & co.), interval timers (`setitimer`) and
//! the CPU-time clocks (design §6, §7).
//!
//! # Model
//! One table per process (there is one process): POSIX timers by id plus
//! the three itimers. Every timer is an [`Arm`]: an absolute expiry in its
//! clock's time base (0 = disarmed) and a reload interval. Clocks:
//! * wall clocks (`CLOCK_REALTIME`/`MONOTONIC`/`BOOTTIME`, `ITIMER_REAL`):
//!   `arch::timer::now_ns` (+ `patina.epoch` for realtime);
//! * thread CPU clocks (`CLOCK_THREAD_CPUTIME_ID`, encoded per-thread
//!   ids): the target's `sched::thread_cpu_time_ns` (running slice included);
//! * the process CPU clock (`CLOCK_PROCESS_CPUTIME_ID`, encoded process
//!   ids, `ITIMER_VIRTUAL`, `ITIMER_PROF`): [`process_cpu_time_ns`].
//!
//! # Expiry
//! [`tick`] runs from `sched::tick` on every CPU (10 ms at 100 Hz, IRQs
//! masked). It is one atomic load while nothing is armed ([`ARMED`]).
//! Otherwise it takes the table lock (`try_lock`: if another CPU is
//! already expiring, this tick skips), compares each armed timer with its
//! clock, re-arms periodic ones by whole intervals (the skipped intervals
//! are the overrun, reported in the one signal sent, as Linux), collects
//! `(target, siginfo)` pairs, drops the lock and only then sends — the
//! signal code takes the thread table, sigaction and pending locks.
//! Expiries are therefore honoured at the first tick at or after them
//! (never early, up to one tick late). `timer_settime`/`setitimer` run
//! the same pass after arming, so an already-expired value fires at once.
//!
//! # Overruns
//! A fire whose signal could not be queued (the previous one from this
//! timer is still pending: standard signals do not stack) adds `1 +
//! missed` to the timer's accumulated overrun, reported with the next
//! signal that is queued; `timer_getoverrun` returns the overrun carried
//! by the last queued signal (Linux: the last *delivered* one).
//!
//! # Lock order
//! `TABLE` → (`task::THREADS` via [`process_cpu_time_ns`]); never a signal
//! send under `TABLE`.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use patina_abi::errno::{EAGAIN, EINVAL, Errno};
use patina_abi::signal::{SIGALRM, SIGPROF, SIGVTALRM, SigInfo};
use patina_abi::time::{
    CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID,
    CLOCK_REALTIME, CLOCK_REALTIME_ALARM, CLOCK_TAI, CLOCK_THREAD_CPUTIME_ID, ITIMER_PROF,
    ITIMER_REAL, ITIMER_VIRTUAL, decode_cpuclock,
};

use super::{Thread, ThreadKind, ThreadState, signal};
use crate::arch::timer;
use crate::sync::SpinLock;

/// Upper bound on POSIX timers per process (Linux bounds them by
/// `RLIMIT_SIGPENDING`; Go creates one per M while profiling).
pub const MAX_TIMERS: usize = 4096;

/// A clock a timer runs on / `clock_gettime` reads.
#[derive(Clone)]
pub enum Clock {
    Realtime,
    Monotonic,
    /// A thread's CPU time; dead once the thread is gone.
    ThreadCpu(Weak<Thread>),
    ProcessCpu,
}

impl Clock {
    pub fn is_cpu(&self) -> bool {
        matches!(self, Clock::ThreadCpu(_) | Clock::ProcessCpu)
    }

    /// Current value in ns; `None` if the clock's thread has exited.
    pub fn now(&self) -> Option<u64> {
        match self {
            Clock::Realtime => Some(timer::now_ns() + crate::wallclock::epoch_ns()),
            Clock::Monotonic => Some(timer::now_ns()),
            Clock::ThreadCpu(w) => w
                .upgrade()
                .filter(|t| t.state() != ThreadState::Dead)
                .map(|t| super::sched::thread_cpu_time_ns(&t)),
            Clock::ProcessCpu => Some(process_cpu_time_ns()),
        }
    }
}

/// Resolve a clock id for `clock_gettime`/`timer_create`. `timer` limits
/// the static ids to those Linux accepts for timers (no `_RAW`/`_COARSE`).
pub fn resolve_clock(clk: i32, for_timer: bool) -> Result<Clock, Errno> {
    use patina_abi::time::{CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME_COARSE};
    match clk {
        CLOCK_REALTIME | CLOCK_REALTIME_ALARM | CLOCK_TAI => Ok(Clock::Realtime),
        CLOCK_REALTIME_COARSE if !for_timer => Ok(Clock::Realtime),
        CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_BOOTTIME_ALARM => Ok(Clock::Monotonic),
        CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE if !for_timer => Ok(Clock::Monotonic),
        CLOCK_PROCESS_CPUTIME_ID => Ok(Clock::ProcessCpu),
        CLOCK_THREAD_CPUTIME_ID => Ok(Clock::ThreadCpu(Arc::downgrade(
            &super::sched::current_arc(),
        ))),
        c if c < 0 => {
            let cc = decode_cpuclock(c).ok_or(Errno(EINVAL))?;
            if cc.per_thread {
                let t = if cc.id == 0 {
                    super::sched::current_arc()
                } else {
                    user_thread(cc.id).ok_or(Errno(EINVAL))?
                };
                Ok(Clock::ThreadCpu(Arc::downgrade(&t)))
            } else if cc.id == 0 || cc.id as u32 == super::process().pid {
                Ok(Clock::ProcessCpu)
            } else {
                Err(Errno(EINVAL))
            }
        }
        _ => Err(Errno(EINVAL)),
    }
}

/// A live user thread of the process by tid.
pub fn user_thread(tid: i32) -> Option<Arc<Thread>> {
    if tid <= 0 {
        return None;
    }
    super::thread_by_tid(tid as u32)
        .filter(|t| matches!(t.kind, ThreadKind::User) && t.state() != ThreadState::Dead)
}

// ---------------------------------------------------------------------------
// CPU time.

/// CPU time of threads that have exited (ns).
static DEAD_CPU_NS: AtomicU64 = AtomicU64::new(0);
/// Last value returned by [`process_cpu_time_ns`] (keeps it monotonic
/// across the exit window where a thread is counted twice or not at all).
static LAST_PROC_CPU_NS: AtomicU64 = AtomicU64::new(0);

/// A thread exits: fold its CPU time into the process total (called from
/// `syscall::thread::exit`, after the thread left the thread table).
pub fn account_dead(ns: u64) {
    DEAD_CPU_NS.fetch_add(ns, Ordering::Relaxed);
}

/// The process's CPU time: live user threads plus exited ones.
pub fn process_cpu_time_ns() -> u64 {
    let live: u64 = super::threads()
        .iter()
        .filter(|t| matches!(t.kind, ThreadKind::User))
        .map(|t| super::sched::thread_cpu_time_ns(t))
        .sum();
    let v = live + DEAD_CPU_NS.load(Ordering::Relaxed);
    let prev = LAST_PROC_CPU_NS.fetch_max(v, Ordering::Relaxed);
    v.max(prev)
}

// ---------------------------------------------------------------------------
// The table.

/// An armed (or disarmed) timer: absolute expiry in its clock (0 =
/// disarmed) and reload interval (0 = one-shot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Arm {
    pub expiry: u64,
    pub interval: u64,
}

impl Arm {
    pub const fn armed(&self) -> bool {
        self.expiry != 0
    }

    /// If expired at `now`: re-arm (periodic) or disarm (one-shot) and
    /// return the number of whole intervals skipped (the overrun).
    pub fn expire(&mut self, now: u64) -> Option<u64> {
        if self.expiry == 0 || now < self.expiry {
            return None;
        }
        if self.interval == 0 {
            self.expiry = 0;
            return Some(0);
        }
        let missed = (now - self.expiry) / self.interval;
        self.expiry = self
            .expiry
            .saturating_add((missed + 1).saturating_mul(self.interval));
        Some(missed)
    }

    /// Time left until expiry (`it_value` for `*gettime`); an expiry that
    /// passed but was not yet processed reads as 1 ns (as Linux).
    pub fn remaining(&self, now: u64) -> u64 {
        if self.expiry == 0 {
            0
        } else {
            self.expiry.saturating_sub(now).max(1)
        }
    }
}

/// Who a POSIX timer signals.
#[derive(Clone)]
pub enum Notify {
    /// `SIGEV_NONE`: nothing (the timer can still be read).
    None,
    /// `SIGEV_SIGNAL`: the process.
    Process,
    /// `SIGEV_THREAD_ID`: one thread.
    Thread(Weak<Thread>),
}

struct PosixTimer {
    clock: Clock,
    notify: Notify,
    signo: i32,
    sigval: u64,
    arm: Arm,
    /// Overrun carried by the last queued signal (`timer_getoverrun`).
    overrun_last: i32,
    /// Fires whose signal could not be queued since then.
    overrun_acc: u64,
}

struct Table {
    timers: BTreeMap<i32, PosixTimer>,
    next_id: i32,
    /// `ITIMER_REAL`, `ITIMER_VIRTUAL`, `ITIMER_PROF`.
    itimers: [Arm; 3],
}

impl Table {
    /// Recount the armed timers into [`ARMED`].
    fn publish(&self) {
        let n = self.timers.values().filter(|t| t.arm.armed()).count()
            + self.itimers.iter().filter(|a| a.armed()).count();
        ARMED.store(n as u64, Ordering::Release);
    }
}

static TABLE: SpinLock<Table> = SpinLock::new(Table {
    timers: BTreeMap::new(),
    next_id: 0,
    itimers: [Arm {
        expiry: 0,
        interval: 0,
    }; 3],
});

/// Number of armed timers; [`tick`] returns at once while 0.
static ARMED: AtomicU64 = AtomicU64::new(0);

/// The `sigevent` of a new timer, validated by the syscall layer.
pub struct NewTimer {
    pub clock: Clock,
    pub notify: Notify,
    pub signo: i32,
    /// `sigev_value`; `None` = the timer id (NULL `sevp`).
    pub sigval: Option<u64>,
}

/// `timer_create`: allocate an id (a per-process counter from 0, skipping
/// ids in use and wrapping at `i32::MAX`, as Linux ≥ 6.x).
pub fn create(n: NewTimer) -> Result<i32, Errno> {
    let mut tab = TABLE.lock();
    if tab.timers.len() >= MAX_TIMERS {
        return Err(Errno(EAGAIN));
    }
    let mut id = tab.next_id;
    while tab.timers.contains_key(&id) {
        id = if id == i32::MAX { 0 } else { id + 1 };
    }
    tab.next_id = if id == i32::MAX { 0 } else { id + 1 };
    tab.timers.insert(
        id,
        PosixTimer {
            clock: n.clock,
            notify: n.notify,
            signo: n.signo,
            sigval: n.sigval.unwrap_or(id as u64),
            arm: Arm::default(),
            overrun_last: 0,
            overrun_acc: 0,
        },
    );
    Ok(id)
}

/// `timer_delete`. A signal already queued by the timer stays queued (as
/// Linux); a concurrent tick that already collected an expiry may still
/// send it.
pub fn delete(id: i32) -> Result<(), Errno> {
    let mut tab = TABLE.lock();
    tab.timers.remove(&id).ok_or(Errno(EINVAL))?;
    tab.publish();
    Ok(())
}

/// `(it_interval, it_value)` of timer `id`.
pub fn gettime(id: i32) -> Result<(u64, u64), Errno> {
    let tab = TABLE.lock();
    let t = tab.timers.get(&id).ok_or(Errno(EINVAL))?;
    let now = t.clock.now().unwrap_or(0);
    Ok((t.arm.interval, t.arm.remaining(now)))
}

/// `timer_getoverrun`.
pub fn getoverrun(id: i32) -> Result<i32, Errno> {
    let tab = TABLE.lock();
    Ok(tab.timers.get(&id).ok_or(Errno(EINVAL))?.overrun_last)
}

/// `timer_settime`: arm with `value` (relative, or absolute in the
/// timer's clock with `abs`; 0 disarms) and `interval`. Returns the old
/// `(interval, value)`.
pub fn settime(id: i32, abs: bool, interval: u64, value: u64) -> Result<(u64, u64), Errno> {
    let old = {
        let mut tab = TABLE.lock();
        let t = tab.timers.get_mut(&id).ok_or(Errno(EINVAL))?;
        let now = t.clock.now();
        let old = (t.arm.interval, t.arm.remaining(now.unwrap_or(0)));
        t.arm = match now {
            Some(_) if value == 0 => Arm::default(),
            // The clock's thread is gone: never fires (Linux disarms
            // CPU timers of exited threads).
            None => Arm::default(),
            Some(now) => Arm {
                expiry: if abs {
                    value
                } else {
                    now.saturating_add(value)
                }
                .max(1),
                interval,
            },
        };
        t.overrun_acc = 0;
        tab.publish();
        old
    };
    run(true);
    Ok(old)
}

fn itimer_clock(which: i32) -> Clock {
    if which == ITIMER_REAL {
        Clock::Monotonic
    } else {
        Clock::ProcessCpu
    }
}

fn itimer_index(which: i32) -> Result<usize, Errno> {
    match which {
        ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF => Ok(which as usize),
        _ => Err(Errno(EINVAL)),
    }
}

/// `getitimer`: `(interval, value)`.
pub fn getitimer(which: i32) -> Result<(u64, u64), Errno> {
    let i = itimer_index(which)?;
    let now = itimer_clock(which).now().unwrap_or(0);
    let tab = TABLE.lock();
    let a = tab.itimers[i];
    Ok((a.interval, a.remaining(now)))
}

/// `setitimer`: arm `which` (value 0 disarms). Returns the old
/// `(interval, value)`.
pub fn setitimer(which: i32, interval: u64, value: u64) -> Result<(u64, u64), Errno> {
    let i = itimer_index(which)?;
    // Read the clock before taking TABLE (the process CPU clock takes the
    // thread table; fine under TABLE too, but no need).
    let now = itimer_clock(which).now().unwrap_or(0);
    let old = {
        let mut tab = TABLE.lock();
        let a = tab.itimers[i];
        let old = (a.interval, a.remaining(now));
        tab.itimers[i] = if value == 0 {
            Arm::default()
        } else {
            Arm {
                expiry: now.saturating_add(value).max(1),
                interval,
            }
        };
        tab.publish();
        old
    };
    run(true);
    Ok(old)
}

// ---------------------------------------------------------------------------
// Expiry.

enum Target {
    Process,
    Thread(Arc<Thread>),
}

struct Fire {
    target: Target,
    info: SigInfo,
    /// POSIX timer id and missed intervals (overrun bookkeeping if the
    /// signal could not be queued).
    posix: Option<(i32, u64)>,
}

/// Timer tick hook (`sched::tick`, every CPU, IRQs masked).
#[inline]
pub fn tick() {
    if ARMED.load(Ordering::Acquire) == 0 {
        return;
    }
    run(false);
}

/// Expire every due timer, then send the signals with no lock held.
/// `wait`: take the lock even if contended (syscalls); the tick skips
/// when another CPU is already expiring.
fn run(wait: bool) {
    let mut fires: Vec<Fire> = Vec::new();
    {
        let mut tab = if wait {
            TABLE.lock()
        } else {
            match TABLE.try_lock() {
                Some(g) => g,
                None => return,
            }
        };
        if ARMED.load(Ordering::Acquire) == 0 {
            return;
        }
        let mono = timer::now_ns();
        let mut proc_cpu: Option<u64> = None;
        let mut proc_now = || *proc_cpu.get_or_insert_with(process_cpu_time_ns);

        for (i, sig) in [
            (ITIMER_REAL, SIGALRM),
            (ITIMER_VIRTUAL, SIGVTALRM),
            (ITIMER_PROF, SIGPROF),
        ] {
            let a = &mut tab.itimers[i as usize];
            if !a.armed() {
                continue;
            }
            let now = if i == ITIMER_REAL { mono } else { proc_now() };
            if a.expire(now).is_some() {
                fires.push(Fire {
                    target: Target::Process,
                    info: SigInfo::kernel(sig),
                    posix: None,
                });
            }
        }

        for (&id, t) in tab.timers.iter_mut() {
            if !t.arm.armed() {
                continue;
            }
            let now = match &t.clock {
                Clock::Realtime => Some(mono + crate::wallclock::epoch_ns()),
                Clock::Monotonic => Some(mono),
                Clock::ProcessCpu => Some(proc_now()),
                c @ Clock::ThreadCpu(_) => c.now(),
            };
            let Some(now) = now else {
                // The clock's thread exited: disarm for good.
                t.arm = Arm::default();
                continue;
            };
            let Some(missed) = t.arm.expire(now) else {
                continue;
            };
            let target = match &t.notify {
                Notify::None => continue,
                Notify::Process => Target::Process,
                Notify::Thread(w) => match w.upgrade().filter(|t| t.state() != ThreadState::Dead) {
                    Some(th) => Target::Thread(th),
                    // Target gone: the signal goes nowhere (Linux).
                    None => continue,
                },
            };
            let overrun = t.overrun_acc.saturating_add(missed).min(i32::MAX as u64) as i32;
            t.overrun_last = overrun;
            t.overrun_acc = 0;
            fires.push(Fire {
                target,
                info: SigInfo::timer(t.signo, id, overrun, t.sigval),
                posix: Some((id, missed)),
            });
        }
        // One-shot timers and timers of exited threads disarmed.
        tab.publish();
    }

    for f in fires {
        let queued = match &f.target {
            Target::Process => signal::send_to_process(f.info),
            Target::Thread(t) => signal::send_to_thread(t, f.info),
        };
        if !queued && let Some((id, missed)) = f.posix {
            let mut tab = TABLE.lock();
            if let Some(t) = tab.timers.get_mut(&id) {
                t.overrun_acc = t.overrun_acc.saturating_add(1 + missed);
            }
        }
    }
}
