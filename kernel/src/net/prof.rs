//! `patina.stats=1` counters for the TCP/IP stack: who takes the stack
//! lock and for how long, where the time under it goes (ingress, listener
//! refills, egress, `track`, `refresh_all`, `poll_delay`), how the net task
//! wakes up, and the scheduler IPIs / wake-ups that reach it. Printed at
//! exit by [`super::stack::print_stats`].
//!
//! Cost: relaxed atomic adds and `timer::now_ns` reads, all only when
//! `crate::STATS` is set, which is read once per stack-lock hold (into
//! `Stack::prof`). With stats off nothing is counted or timed.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arch::cpu::MAX_CPUS;

pub type Counter = AtomicU64;

#[inline]
pub fn add(c: &Counter, v: u64) {
    c.fetch_add(v, Ordering::Relaxed);
}

#[inline]
pub fn max(c: &Counter, v: u64) {
    c.fetch_max(v, Ordering::Relaxed);
}

#[inline]
pub fn get(c: &Counter) -> u64 {
    c.load(Ordering::Relaxed)
}

#[inline]
pub fn on() -> bool {
    crate::STATS.load(Ordering::Relaxed)
}

#[inline]
pub fn now() -> u64 {
    crate::arch::timer::now_ns()
}

/// Who took the stack lock.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// The net task's poll round.
    Task = 0,
    /// `stack::with(true, ..)`: a syscall that polls.
    SysPoll = 1,
    /// `stack::with(false, ..)`.
    SysNoPoll = 2,
    /// `InetSocket::poll` (epoll / poll / select readiness).
    FilePoll = 3,
    /// Boot-time DHCP wait.
    Dhcp = 4,
}

pub const NSITES: usize = 5;
pub const SITE_NAMES: [&str; NSITES] = ["net-task", "sys-poll", "sys-nopoll", "file-poll", "dhcp"];

/// Hold-time histogram bucket upper bounds (ns); the last bucket is ≥100 µs.
pub const HIST_NS: [u64; 7] = [1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000];
/// Spin-wait histogram bucket upper bounds (ns): <10 µs, <100 µs, <1 ms,
/// <10 ms, ≥10 ms.
pub const SPIN_HIST_NS: [u64; 4] = [10_000, 100_000, 1_000_000, 10_000_000];

pub struct SiteStats {
    pub acq: Counter,
    pub contended: Counter,
    pub spin_ns: Counter,
    pub hold_ns: Counter,
    pub max_hold_ns: Counter,
    /// Holds that were timed (stats on); the histogram sums to this.
    pub timed: Counter,
    pub hist: [Counter; 8],
    pub max_spin_ns: Counter,
    /// Spin waits of contended acquisitions by [`SPIN_HIST_NS`] bucket.
    pub spin_hist: [Counter; 5],
}

impl SiteStats {
    const fn new() -> SiteStats {
        SiteStats {
            acq: Counter::new(0),
            contended: Counter::new(0),
            spin_ns: Counter::new(0),
            hold_ns: Counter::new(0),
            max_hold_ns: Counter::new(0),
            timed: Counter::new(0),
            hist: [const { Counter::new(0) }; 8],
            max_spin_ns: Counter::new(0),
            spin_hist: [const { Counter::new(0) }; 5],
        }
    }

    pub fn record_spin(&self, ns: u64) {
        add(&self.contended, 1);
        add(&self.spin_ns, ns);
        max(&self.max_spin_ns, ns);
        let b = SPIN_HIST_NS.iter().position(|&lim| ns < lim).unwrap_or(4);
        add(&self.spin_hist[b], 1);
    }

    pub fn record_hold(&self, ns: u64) {
        add(&self.hold_ns, ns);
        max(&self.max_hold_ns, ns);
        add(&self.timed, 1);
        let b = HIST_NS.iter().position(|&lim| ns < lim).unwrap_or(7);
        add(&self.hist[b], 1);
    }
}

pub static SITES: [SiteStats; NSITES] = [const { SiteStats::new() }; NSITES];
/// Stack-lock acquisitions per CPU.
pub static LOCK_CPU: [Counter; MAX_CPUS] = [const { Counter::new(0) }; MAX_CPUS];

/// A phase inside a lock hold: calls and ns (ns only with stats on).
pub struct Phase {
    pub calls: Counter,
    pub ns: Counter,
}

impl Phase {
    const fn new() -> Phase {
        Phase {
            calls: Counter::new(0),
            ns: Counter::new(0),
        }
    }

    /// Count one call that started at `t0` (0: stats off, not counted).
    #[inline]
    pub fn done(&self, t0: u64) {
        if t0 != 0 {
            add(&self.calls, 1);
            add(&self.ns, now().saturating_sub(t0));
        }
    }
}

/// Start time for a phase, or 0 when not timing.
#[inline]
pub fn start(timing: bool) -> u64 {
    if timing { now() } else { 0 }
}

/// Whole `Stack::poll_all` calls.
pub static POLL_ALL: Phase = Phase::new();
/// `poll_maintenance`.
pub static MAINT: Phase = Phase::new();
/// `poll_ingress_single` calls that processed a packet.
pub static INGRESS: Phase = Phase::new();
/// `poll_ingress_single` calls that found nothing (one per round).
pub static INGRESS_NONE: Phase = Phase::new();
/// Packets processed by ingress (calls that did not return None).
pub static INGRESS_PKTS: Counter = Counter::new(0);
pub static REFILL: Phase = Phase::new();
/// `poll_egress` calls that emitted something.
pub static EGRESS: Phase = Phase::new();
/// The final `poll_egress` call of each drain loop (nothing to send: a scan
/// of every socket).
pub static EGRESS_NONE: Phase = Phase::new();
/// poll_egress calls that returned something other than None.
pub static EGRESS_SOME: Counter = Counter::new(0);
pub static DHCP_EV: Phase = Phase::new();
pub static TRACK: Phase = Phase::new();
/// Sum of `socks.len()` over `track` calls.
pub static TRACK_SOCKS: Counter = Counter::new(0);
pub static REFRESH: Phase = Phase::new();
/// Socket readiness evaluations (sum of `socks.len()` per `refresh_all`).
pub static REFRESH_EVALS: Counter = Counter::new(0);
/// Hooks collected by `refresh_all` (one per hook per notification).
pub static REFRESH_HOOKS: Counter = Counter::new(0);
pub static DEADLINE: Phase = Phase::new();
/// The closure passed to `stack::with` (the syscall's own work).
pub static WITH_OP: Phase = Phase::new();
pub static MAX_SOCKS: Counter = Counter::new(0);
pub static MAX_SET: Counter = Counter::new(0);
/// Sampled `SocketSet` sizes (every [`SET_SAMPLE`]th poll_all): sum and
/// number of samples.
pub static SET_SUM: Counter = Counter::new(0);
pub static SET_SAMPLES: Counter = Counter::new(0);
pub static SOCKS_SUM: Counter = Counter::new(0);
pub const SET_SAMPLE: u64 = 64;

/// virtio-net queue notifications (a doorbell write: a VM exit), receive
/// (after reposting buffers) and transmit (per frame).
pub static NIC_RX_KICK: Phase = Phase::new();
pub static NIC_TX_KICK: Phase = Phase::new();

/// Net task.
pub static TASK_ROUNDS: Counter = Counter::new(0);
pub static TASK_WAKE_SIGNAL: Counter = Counter::new(0);
pub static TASK_WAKE_DEADLINE: Counter = Counter::new(0);
pub static TASK_SPINS: Counter = Counter::new(0);
pub static TASK_SPIN_LIMIT: Counter = Counter::new(0);
/// Net-task poll rounds per CPU.
pub static TASK_CPU: [Counter; MAX_CPUS] = [const { Counter::new(0) }; MAX_CPUS];
/// Signal → net task running again (ns), for signalled wake-ups whose
/// signal time is known (stats on).
pub static TASK_WAKE_LAT_NS: Counter = Counter::new(0);
pub static TASK_WAKE_LAT_N: Counter = Counter::new(0);
pub static TASK_WAKE_LAT_MAX: Counter = Counter::new(0);
/// ns the net task spent asleep in `Event::wait`.
pub static TASK_SLEEP_NS: Counter = Counter::new(0);
/// Kicks (`event().signal()`) from `stack::with`.
pub static KICKS: Counter = Counter::new(0);

/// `Notify::fire` with at least one hook: (calls, hooks, ns) from the net
/// task and from syscalls.
pub static FIRE_TASK: Phase = Phase::new();
pub static FIRE_TASK_HOOKS: Counter = Counter::new(0);
pub static FIRE_SYS: Phase = Phase::new();
pub static FIRE_SYS_HOOKS: Counter = Counter::new(0);

/// Scheduler: Reschedule IPIs sent by `enqueue_placed`, per sending CPU;
/// enqueues of the net task and how many of those sent an IPI.
pub static IPI_CPU: [Counter; MAX_CPUS] = [const { Counter::new(0) }; MAX_CPUS];
pub static NET_TASK_ENQ: Counter = Counter::new(0);
pub static NET_TASK_IPI: Counter = Counter::new(0);
/// The net task's tid (u32::MAX: not started).
pub static NET_TASK_TID: AtomicU32 = AtomicU32::new(u32::MAX);

/// When stats started counting time (first timed hold) and when the stack
/// was initialised (ns).
pub static T_FIRST: Counter = Counter::new(0);
pub static T_INIT: Counter = Counter::new(0);

/// `sched::enqueue_placed` placed `tid` (sending an IPI from `cpu` if
/// `ipi`).
#[inline]
pub fn on_enqueue(tid: u32, cpu: usize, ipi: bool) {
    if !on() {
        return;
    }
    if ipi && let Some(c) = IPI_CPU.get(cpu) {
        add(c, 1);
    }
    if tid == NET_TASK_TID.load(Ordering::Relaxed) {
        add(&NET_TASK_ENQ, 1);
        if ipi {
            add(&NET_TASK_IPI, 1);
        }
    }
}
