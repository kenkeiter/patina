//! `patina.stats=1` counters for the TCP/IP stack: who takes the
//! interface lock and the socket-set locks and for how long, where the time under it goes (ingress, listener
//! refills, egress, `track`, readiness refresh, `poll_delay`; all per
//! socket set since D67's per-socket sets), the ingress demux, how the net task
//! wakes up, and the scheduler IPIs / wake-ups that reach it. Printed at
//! exit by [`super::stack::print_stats`].
//!
//! Cost: relaxed atomic adds and `timer::now_ns` reads, all only when
//! `crate::STATS` is set, which is read once per lock hold (into
//! `Stack::prof` for the interface lock). With stats off nothing is
//! counted or timed.

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

/// Who took a stack lock. Sites below [`STACK_SITES`] are takers of the
/// stack lock (the registry, `stack::Stack`); `Shard` aggregates the
/// per-CPU interface shards and `SetLock` every socket-set lock
/// acquisition (`stack::Slot`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Site {
    /// The net task: timer-due sets, its frame drain, rescans.
    Task = 0,
    /// A socket syscall polling its own set (egress, `poll_delay`).
    SysPoll = 1,
    /// A socket syscall that needs the interface without polling (bind,
    /// socket creation, a set's backlog found at lock time).
    SysNoPoll = 2,
    /// `InetSocket::poll` (epoll / poll / select readiness) that found a
    /// backlog to feed.
    FilePoll = 3,
    /// Boot-time DHCP wait.
    Dhcp = 4,
    /// A syscall draining received frames (loopback, then the NIC up to its
    /// budget) after its own poll, and the backlog service of a set whose
    /// lock was busy.
    Drain = 5,
    /// The per-CPU interface shards (every site, aggregated; also a
    /// shard's own stack-lock acquisition to apply new addresses).
    Shard = 6,
    /// Socket-set locks (every site, aggregated).
    SetLock = 7,
}

pub const NSITES: usize = 8;
/// Sites `0..STACK_SITES` are the stack lock's.
pub const STACK_SITES: usize = 6;
pub const SITE_NAMES: [&str; NSITES] = [
    "net-task",
    "sys-poll",
    "sys-nopoll",
    "file-poll",
    "dhcp",
    "sys-drain",
    "iface-shard",
    "set-lock",
];

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
/// Interface-lock acquisitions per CPU.
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

/// Polls of one socket set under the interface lock (`Stack::run_set`:
/// backlog / frame ingress, egress, settling, `poll_delay`; printed as
/// `poll_all`).
pub static POLL_ALL: Phase = Phase::new();
/// `poll_maintenance`.
pub static MAINT: Phase = Phase::new();
/// Received frames: demux + `poll_ingress_single` into the frame's set.
pub static INGRESS: Phase = Phase::new();
/// Looking for a frame and finding none (one per drain).
pub static INGRESS_NONE: Phase = Phase::new();
/// Packets processed by ingress (calls that did not return None).
pub static INGRESS_PKTS: Counter = Counter::new(0);
pub static REFILL: Phase = Phase::new();
/// `poll_egress` calls that emitted something.
pub static EGRESS: Phase = Phase::new();
/// The final `poll_egress` call on a set (nothing more to send).
pub static EGRESS_NONE: Phase = Phase::new();
/// poll_egress calls that returned something other than None.
pub static EGRESS_SOME: Counter = Counter::new(0);
pub static DHCP_EV: Phase = Phase::new();
pub static TRACK: Phase = Phase::new();
/// Sockets tracked (one per `track` call).
pub static TRACK_SOCKS: Counter = Counter::new(0);
/// Frames queued on a socket set's backlog because its lock was busy, and
/// frames dropped because a backlog was full.
pub static BACKLOGGED: Counter = Counter::new(0);
pub static BACKLOG_DROPS: Counter = Counter::new(0);
pub static REFRESH: Phase = Phase::new();
/// Socket readiness evaluations (one per `Sock::refresh`; printed as
/// `refresh_all`).
pub static REFRESH_EVALS: Counter = Counter::new(0);
/// Hooks collected by readiness refreshes (one per hook per notification).
pub static REFRESH_HOOKS: Counter = Counter::new(0);
/// `poll_delay` of one set (printed as `next_deadline`).
pub static DEADLINE: Phase = Phase::new();
/// Received frames by where the demux sent them: an exact flow-table hit
/// (a connection or a listener's pending one), a listener / bound UDP
/// socket / the DHCP client by local port, no socket (the empty set:
/// unknown TCP tuple → RST, UDP → port unreachable), or not TCP/UDP at all
/// (ARP, ICMP, NDISC, fragments).
pub static DEMUX_EXACT: Counter = Counter::new(0);
pub static DEMUX_PORT: Counter = Counter::new(0);
pub static DEMUX_MISS: Counter = Counter::new(0);
pub static DEMUX_OTHER: Counter = Counter::new(0);
/// Passes that queued every set (a neighbor may have been learned, or the
/// addresses changed).
pub static RESCANS: Counter = Counter::new(0);
/// Socket sets egressed (one `poll_egress` loop each).
pub static SETS_EGRESSED: Counter = Counter::new(0);
/// The closure passed to `stack::sys` (the syscall's own work, under its
/// set lock).
pub static WITH_OP: Phase = Phase::new();
pub static MAX_SOCKS: Counter = Counter::new(0);
pub static MAX_SET: Counter = Counter::new(0);
/// Sampled numbers of live socket sets and of sockets (every
/// [`SET_SAMPLE`]th pass): sums and number of samples.
pub static SET_SUM: Counter = Counter::new(0);
pub static SET_SAMPLES: Counter = Counter::new(0);
pub static SOCKS_SUM: Counter = Counter::new(0);
pub const SET_SAMPLE: u64 = 64;

/// virtio-net queue notifications (a doorbell write: a VM exit), receive
/// (after reposting buffers) and transmit (per frame).
pub static NIC_RX_KICK: Phase = Phase::new();
pub static NIC_TX_KICK: Phase = Phase::new();
/// `NetDevice::send` / a `NetDevice::recv` that returned a frame (the
/// driver's lock, the copy, the descriptors; kicks included).
pub static NIC_SEND: Phase = Phase::new();
pub static NIC_RECV: Phase = Phase::new();

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
/// Kicks (`event().signal()`) of the net task by other lock holders.
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
