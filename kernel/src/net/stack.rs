//! The TCP/IP stack (design §9, D67): one smoltcp `Interface` over the
//! [`Mux`] devices (NIC + loopback), one smoltcp `SocketSet` per socket,
//! each behind its own lock, the flow table, the bind table and the
//! per-set poll timers.
//!
//! **Socket sets.** smoltcp's `poll_egress` and `poll_delay` walk every
//! socket of the set they are given, and its ingress scans the set for a
//! socket that accepts the packet. So that no lock hold costs time in
//! proportion to the number of sockets, every socket gets its own set
//! (a [`Slot`]): a TCP connection or UDP socket holds one smoltcp socket,
//! a TCP listener its pool of `Listen` sockets plus its accept queue, the
//! DHCP client its own, and a closed TCP connection still finishing its
//! shutdown (a zombie) keeps the set it had. `accept` moves the smoltcp
//! socket from the listener's set into the new connection's (`remove` +
//! `add`). A slot also holds its owner's state ([`Owner`]: for an inet
//! socket all of `inet::Sock`: kind, options, error, readiness hooks).
//!
//! **Locks.** Each slot is behind its own lock ([`SetLock`]; the inet
//! socket's `File` holds the `Arc`). The interface lock (the
//! `SpinLock<Stack>`) covers what all sockets share: the smoltcp
//! `Interface`, the `Mux` (transmit routing, the loopback queue, the NIC
//! handle), the empty set, the flow table, the port map, the bind table,
//! the timers, the slot registry with each set's backlog, configuration
//! and DHCP state. Lock order: **set lock → interface lock → leaves**
//! (the virtio-net driver's lock, the heap). Nobody waits for a set lock
//! while holding the interface lock or another set lock (a `try_lock` of a
//! set under the interface lock is allowed: it never waits). Hooks
//! (`Notify::fire`) run with no set or interface lock held: readiness is
//! collected under the set lock and fired after it is released
//! (fs/file.rs lock order). The interface lock is held for one set's
//! smoltcp calls at a time (its ingress, egress until it has nothing more
//! to send, `track`/refill, `poll_delay`); the syscall's own work
//! (`send_slice`/`recv_slice`, readiness) runs under the set lock alone.
//! `check_order` panics on a blocking acquisition that breaks the order.
//!
//! **Ingress demux.** Frames are taken one at a time under the interface
//! lock (loopback queue first, then the NIC) into a buffer owned by the
//! taker, and their transport tuple read (`patina_inet::demux::parse`). The
//! tuple is looked up (a) exactly in [`Stack::flows`] — connections (from
//! `connect` on, including zombies) and a listener's not-yet-accepted
//! connections (pointing at the listener's set until `accept` re-points
//! them); (b) else by local port in [`Stack::bound`] — listeners and bound
//! UDP sockets, an exact local address before a wildcard, only sockets that
//! cover the packet's family; UDP 67 → 68 goes to the DHCP client; (c)
//! else the frame goes to an empty set (fed right there, under the
//! interface lock alone): smoltcp still answers ARP, ICMP echo and NDISC at
//! the interface level, a TCP segment for no socket gets a RST and a UDP
//! datagram an ICMP port unreachable. A set gets its frame through a
//! one-frame device (`mux::OneFrame`).
//!
//! **Backlogs (frame order).** A frame bound for set Y is fed while the
//! interface lock is still held from its demux: the taker `try_lock`s Y
//! and, if that succeeds, feeds it, polls Y's egress, settles it, and only
//! then drops the interface lock (`SpinLockGuard::swap_irq_state` lets it
//! release the interface lock before Y's). If Y is busy (a syscall on Y,
//! another CPU feeding it), the frame is copied onto Y's backlog (in its
//! registry entry, under the interface lock) and `SetLock::pending` set.
//! Whoever holds Y feeds the backlog before anything else the next time it
//! takes the interface lock, and every release of a set lock checks
//! `pending` afterwards (`service`: a `try_lock`, so it never waits). The
//! taker that queued a frame calls `service` too; with `SeqCst` fences on
//! both sides either the taker gets the lock or the releasing holder sees
//! the flag, so no frame is stranded. Frames of one set are therefore fed
//! in the order they were taken off the device, however many CPUs take
//! frames (re-validating the demux after re-locking, as an alternative,
//! reorders segments of one flow between CPUs). A frame is bound to the set
//! its demux named when it was taken: operations that move smoltcp sockets
//! out of a set (`accept`, closing a listener) feed the set's backlog
//! first; a set that is freed drops its backlog.
//!
//! Flow-table invariant: an entry exists for the tuple of every smoltcp TCP
//! socket that has one (left `Listen`, or `connect`ed), and points at the
//! set that holds that socket. Entries are added at `connect`, when a
//! listener's pool socket leaves `Listen` (`refill`, run after every frame
//! fed to the listener) and re-pointed at `accept` / when a socket becomes
//! a zombie in a set of its own; they are removed when the socket loses its
//! tuple (back to `Listen`, `Closed`) or leaves the set. Removal only
//! deletes an entry that still points at the remover's set, so a newer
//! socket with the same tuple keeps its entry. An exact match therefore
//! always reaches the socket smoltcp itself would pick (a SYN for a tuple
//! still in `TimeWait` reaches that socket). The flow table, port map,
//! timers and registry are only touched under the interface lock, by a
//! holder of the set concerned (or for a set not yet registered).
//!
//! **Polling a set** ([`Stack::run_set`], set and interface lock held):
//! feed its backlog (and the frame at hand), `poll_egress` until it has
//! nothing more to send, settle it (`inet::track`: smoltcp state
//! transitions → socket-API events; a listener's pool refill; a zombie's
//! reaping; DHCP events), then `poll_delay` plus socket-level deadlines
//! (connect timeout, deferred keep-alive, zombie linger) → the set's timer
//! in `timers`. Readiness (`inet::Sock::refresh`) is recomputed after the
//! interface lock is dropped, under the set lock.
//!
//! Who polls:
//!
//! * every **socket syscall** ([`sys`]): lock its set (feed a pending
//!   backlog first), do the operation (taking the interface lock inside
//!   when it needs shared state), poll the set if the operation changed
//!   something, release, fire; then **drain** received frames (loopback
//!   first, every one of them, then at most `SYS_BUDGET` NIC frames). So a
//!   write is in the peer's receive buffer (over loopback) before the
//!   syscall returns — or on its backlog while another CPU holds the peer,
//!   to be fed before that holder lets go — and the peer's ACK is
//!   processed too;
//! * the **net task** (a kernel thread): polls the sets whose timer is due
//!   (and the dirty ones: zombies made by closing a listener), drains with
//!   the large budget (`TASK_BUDGET`), then sleeps on an `Event` until the
//!   earliest timer (at most 1 s). The event is the NIC's (its IRQ handler
//!   signals it and never takes a stack lock) or a stack-owned one without
//!   a NIC. Other holders signal it when they file an earlier timer, leave
//!   NIC frames behind (a syscall's budget ran out) or need a rescan.
//!
//! ARP and ICMPv6 frames may teach the interface a neighbor that some set
//! is waiting for, and a DHCP lease change may invalidate source addresses:
//! those (rare) events make the drainer (or the net task) poll every set
//! once (`rescan`).

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::net::{Ipv4Addr, Ipv6Addr};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering, fence};

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Cidr, Ipv6Cidr};

use super::device::MAX_FRAME;
use super::inet::{self, Sock};
use super::mux::Mux;
use super::prof::{self, Site};
use crate::arch::cpu::MAX_CPUS;
use crate::drivers::Event;
use crate::fs::file::Notify;
use crate::sync::{OnceCell, SpinLock, SpinLockGuard};
use patina_inet::PortTable;
use patina_inet::demux::{self, Flow, Parsed, PortMap};

/// Longest the net task sleeps between polls.
const MAX_WAIT_NS: u64 = 1_000_000_000;
/// Drain rounds (take frames until none is left, then poll every set if a
/// rescan was asked for) per drain.
const MAX_ROUNDS: usize = 64;
/// NIC frames the net task takes per drain.
const TASK_BUDGET: usize = 1024;
/// NIC frames a syscall takes per drain after its own poll: it helps, but
/// does not do bulk work for everyone (the net task is kicked for the
/// rest).
const SYS_BUDGET: usize = 64;
/// Loopback frames per drain (a drain normally empties the loopback queue).
const LO_BUDGET: usize = 4096;
/// Frames one set's backlog holds at most (beyond: dropped).
const MAX_BACKLOG: usize = 1024;
/// "No timer".
pub const NEVER: u64 = u64::MAX;

/// MAC used when there is no NIC: locally administered, unicast.
const LO_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

/// How long boot waits for a DHCP lease before user mode starts anyway.
const DHCP_WAIT_NS: u64 = 10_000_000_000;
/// DHCP options requested: subnet mask, router, DNS, host name, domain.
static DHCP_PARAMS: [u8; 5] = [1, 3, 6, 12, 15];

/// The effective IPv4 configuration: the static `ip=` values, replaced by
/// a DHCP lease when there is one. `/etc/resolv.conf` and `/etc/hosts` are
/// generated from it (`fs::rootfs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetConfig {
    /// Interface address and prefix length (not loopback / link-local).
    pub addr: Option<(Ipv4Addr, u8)>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    /// Host name from the DHCP server (option 12), if any.
    pub hostname: Option<String>,
    /// Domain name from the DHCP server (option 15), if any.
    pub domain: Option<String>,
    /// True when `addr` comes from a DHCP lease.
    pub leased: bool,
}

/// The current effective configuration (default without a stack).
pub fn effective_config() -> NetConfig {
    match get() {
        Some(_) => lock_at(Site::SysNoPoll).cfg.clone(),
        None => NetConfig::default(),
    }
}

/// Index of a [`Slot`] (a socket set) in the stack's registry.
pub type SetId = usize;

/// Who a socket set belongs to.
// The socket's state lives inline: a slot is allocated once per socket,
// and a box would add an indirection to every operation.
#[allow(clippy::large_enum_variant)]
pub enum Owner {
    /// Released: no longer registered (a holder of a stale reference finds
    /// this and does nothing).
    Free,
    /// An inet socket (its whole state).
    Sock(Sock),
    /// A closed TCP connection finishing its shutdown: its handle, the
    /// give-up deadline (ns) and its flow-table entry.
    Zombie {
        h: SocketHandle,
        until: u64,
        flow: Option<Flow>,
    },
    /// The DHCPv4 client socket.
    Dhcp(SocketHandle),
}

/// A socket set and its owner.
pub struct Slot {
    /// Registry index (stale once `owner` is [`Owner::Free`]).
    pub id: SetId,
    pub set: SocketSet<'static>,
    pub owner: Owner,
}

/// A slot behind its lock (see the module docs).
pub struct SetLock {
    slot: SpinLock<Slot>,
    /// The set's backlog is not empty (written under the interface lock;
    /// read by whoever releases the set lock).
    pending: AtomicBool,
}

pub type SetRef = Arc<SetLock>;

impl SetLock {
    fn new(slot: Slot) -> SetRef {
        Arc::new(SetLock {
            slot: SpinLock::new(slot),
            pending: AtomicBool::new(false),
        })
    }
}

/// A registry entry (under the interface lock).
struct Entry {
    set: Option<SetRef>,
    /// Poll time this set is filed under in `timers` ([`NEVER`]: none).
    due: u64,
    /// Frames taken for this set while its lock was busy, in order.
    backlog: VecDeque<Box<[u8]>>,
}

/// What the interface lock covers (see the module docs).
pub struct Stack {
    pub iface: Interface,
    pub mux: Mux,
    /// Socket sets by [`SetId`], with their timers and backlogs.
    entries: Vec<Entry>,
    free_ids: Vec<SetId>,
    live_sets: usize,
    /// Open inet sockets (for `MAX_SOCKETS`).
    pub nsocks: usize,
    /// Exact transport tuples → the set holding that TCP socket (see the
    /// module docs for the invariant).
    pub flows: BTreeMap<Flow, SetId>,
    /// Listeners and bound UDP sockets by local port → their set.
    pub bound: PortMap<SetId>,
    /// Sets the net task should poll (zombies made from a closed
    /// listener's queue).
    dirty: Vec<SetId>,
    /// Frames for no socket are fed to this (always empty) set.
    empty: SocketSet<'static>,
    /// (poll time ns, set): every set's earliest poll time.
    timers: BTreeSet<(u64, SetId)>,
    /// Poll every registered set (a neighbor may have been learned, or the
    /// addresses changed).
    rescan: bool,
    /// The last drain stopped taking NIC frames at its budget.
    nic_backlog: bool,
    /// Wake the net task when this hold ends (an earlier timer, work left).
    kick: bool,
    pub ports: PortTable,
    pub next_id: u64,
    /// Static IPv4 default gateway (`ip=`), for route checks.
    pub gw4: Option<Ipv4Addr>,
    /// When the net task will next poll on its own (ns).
    pub task_deadline: u64,
    /// The DHCPv4 client's set (`ip=dhcp`), kept for renewals.
    pub dhcp: Option<SetId>,
    /// When the DHCP client started (ns), for the lease log line.
    pub dhcp_t0: u64,
    /// Effective configuration (see [`NetConfig`]).
    pub cfg: NetConfig,
    /// DNS servers from `ip=` (used when a lease names none).
    pub static_dns: Vec<Ipv4Addr>,
    /// `patina.stats=1` was set when this lock hold began: count and time
    /// the phases (`net::prof`).
    pub prof: bool,
}

/// `ip=` asked for DHCP (whether or not there is a NIC to run it on).
static DHCP_REQUESTED: AtomicBool = AtomicBool::new(false);

static STACK: OnceCell<SpinLock<Stack>> = OnceCell::new();
static LOCAL_EVENT: Event = Event::new();
static EVENT: OnceCell<&'static Event> = OnceCell::new();

pub fn now() -> Instant {
    Instant::from_micros((crate::arch::timer::now_ns() / 1000) as i64)
}

/// Remove the flow-table entry for `f` if it still points at set `i` (a
/// newer socket with the same tuple keeps its entry).
pub fn flow_remove(flows: &mut BTreeMap<Flow, SetId>, f: Flow, i: SetId) {
    if flows.get(&f) == Some(&i) {
        flows.remove(&f);
    }
}

/// The stack (its interface lock), once [`init`] ran.
pub fn get() -> Option<&'static SpinLock<Stack>> {
    STACK.get()
}

fn event() -> &'static Event {
    EVENT.get().copied().unwrap_or(&LOCAL_EVENT)
}

// -- Lock order check -----------------------------------------------------

/// CPU (+1) holding the interface lock; 0: none.
static IFACE_CPU: AtomicUsize = AtomicUsize::new(0);
/// Set locks held per CPU.
static SETS_HELD: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];

/// Panic if this CPU is about to wait for a lock out of order: the
/// interface lock while holding it, a set lock while holding the
/// interface lock or another set lock. Called when the wanted lock is busy
/// (a deadlock needs a wait), so the fast path costs nothing. (A thread
/// holding a spinlock cannot migrate: IRQs are masked.)
fn check_order(set: bool) {
    let f = crate::arch::irq::save_disable();
    let cpu = crate::arch::cpu::id();
    let iface = IFACE_CPU.load(Ordering::Relaxed) == cpu + 1;
    let sets = SETS_HELD
        .get(cpu)
        .map_or(0, |c| c.load(Ordering::Relaxed));
    crate::arch::irq::restore(f);
    if iface {
        panic!(
            "net: {} lock wanted while holding the interface lock",
            if set { "a socket-set" } else { "the interface" }
        );
    }
    if set && sets > 0 {
        panic!("net: a socket-set lock wanted while holding another one");
    }
}

fn sets_held(delta: i8) {
    if let Some(c) = SETS_HELD.get(crate::arch::cpu::id()) {
        if delta > 0 {
            c.fetch_add(1, Ordering::Relaxed);
        } else {
            c.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

// -- The interface lock ---------------------------------------------------

/// The interface lock, held for `site`. With `patina.stats=1` the
/// acquisition (contention, spin time) and the hold time are counted per
/// site (`net::prof`), under the site the holder ends up as. Dropping it
/// wakes the net task if the hold asked for that (`Stack::kick`).
pub struct Held {
    g: ManuallyDrop<SpinLockGuard<'static, Stack>>,
    pub site: Site,
    t0: u64,
    /// Spin wait of a contended acquisition (ns), if it was contended.
    spin: Option<u64>,
}

impl core::ops::Deref for Held {
    type Target = Stack;
    fn deref(&self) -> &Stack {
        &self.g
    }
}

impl core::ops::DerefMut for Held {
    fn deref_mut(&mut self) -> &mut Stack {
        &mut self.g
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let kick = core::mem::take(&mut self.g.kick) && self.site != Site::Task;
        if self.t0 != 0 {
            let st = &prof::SITES[self.site as usize];
            prof::add(&st.acq, 1);
            if let Some(ns) = self.spin {
                st.record_spin(ns);
            }
            st.record_hold(prof::now().saturating_sub(self.t0));
        }
        IFACE_CPU.store(0, Ordering::Relaxed);
        // SAFETY: dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut self.g) };
        if kick {
            if prof::on() {
                prof::add(&prof::KICKS, 1);
            }
            event().signal();
        }
    }
}

/// Take the interface lock for `site` (every taker but netlink's read-only
/// snapshot goes through here).
pub fn lock_at(site: Site) -> Held {
    let lock = get().expect("net: stack not initialised");
    let (mut g, spin, t0) = if !prof::on() {
        let g = lock.try_lock().unwrap_or_else(|| {
            check_order(false);
            lock.lock()
        });
        (g, None, 0)
    } else {
        let (g, spin) = match lock.try_lock() {
            Some(g) => (g, None),
            None => {
                check_order(false);
                let t = prof::now();
                let g = lock.lock();
                (g, Some(prof::now().saturating_sub(t)))
            }
        };
        if let Some(c) = prof::LOCK_CPU.get(crate::arch::cpu::id()) {
            prof::add(c, 1);
        }
        let t0 = prof::now();
        let _ = prof::T_FIRST.compare_exchange(0, t0, Ordering::Relaxed, Ordering::Relaxed);
        (g, spin, t0)
    };
    g.prof = t0 != 0;
    IFACE_CPU.store(crate::arch::cpu::id() + 1, Ordering::Relaxed);
    Held {
        g: ManuallyDrop::new(g),
        site,
        t0,
        spin,
    }
}

/// The interface lock, taken on first use by a set holder (a syscall's
/// operation needs it only for shared state: binds, flows, routes).
pub struct Iface {
    h: Option<Held>,
}

impl Iface {
    fn new() -> Iface {
        Iface { h: None }
    }

    /// The stack, locking the interface if not yet held.
    pub fn get(&mut self) -> &mut Stack {
        self.at(Site::SysNoPoll)
    }

    /// The same, counted under `site` from here on.
    fn at(&mut self, site: Site) -> &mut Held {
        let h = self.h.get_or_insert_with(|| lock_at(site));
        h.site = site;
        h
    }
}

// -- Set locks --------------------------------------------------------------

/// A set lock (see the module docs). Releasing it feeds a backlog that
/// built up meanwhile ([`service`]).
pub struct SetGuard<'a> {
    g: ManuallyDrop<SpinLockGuard<'a, Slot>>,
    l: &'a SetLock,
    t0: u64,
    spin: Option<u64>,
    /// Run [`service`] after release (not for `service`'s own guards).
    serve: bool,
}

impl core::ops::Deref for SetGuard<'_> {
    type Target = Slot;
    fn deref(&self) -> &Slot {
        &self.g
    }
}

impl core::ops::DerefMut for SetGuard<'_> {
    fn deref_mut(&mut self) -> &mut Slot {
        &mut self.g
    }
}

impl Drop for SetGuard<'_> {
    fn drop(&mut self) {
        if self.t0 != 0 {
            let st = &prof::SITES[Site::SetLock as usize];
            prof::add(&st.acq, 1);
            if let Some(ns) = self.spin {
                st.record_spin(ns);
            }
            st.record_hold(prof::now().saturating_sub(self.t0));
        }
        sets_held(-1);
        // SAFETY: dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut self.g) };
        if self.serve {
            fence(Ordering::SeqCst);
            if self.l.pending.load(Ordering::Relaxed) {
                service(self.l, Site::Drain);
            }
        }
    }
}

fn set_guard<'a>(
    l: &'a SetLock,
    g: SpinLockGuard<'a, Slot>,
    spin: Option<u64>,
    timed: bool,
) -> SetGuard<'a> {
    sets_held(1);
    SetGuard {
        g: ManuallyDrop::new(g),
        l,
        t0: if timed { prof::now() } else { 0 },
        spin,
        serve: true,
    }
}

/// Lock set `l` (waiting).
pub fn lock_set(l: &SetLock) -> SetGuard<'_> {
    // The order check runs only when the lock is busy: a deadlock needs a
    // wait. The lock itself is tried first.
    let timed = prof::on();
    match l.slot.try_lock() {
        Some(g) => set_guard(l, g, None, timed),
        None if !timed => {
            check_order(true);
            set_guard(l, l.slot.lock(), None, false)
        }
        None => {
            check_order(true);
            let t = prof::now();
            let g = l.slot.lock();
            set_guard(l, g, Some(prof::now().saturating_sub(t)), true)
        }
    }
}

/// Lock set `l` if it is free (allowed under the interface lock).
fn try_lock_set(l: &SetLock) -> Option<SetGuard<'_>> {
    let g = l.slot.try_lock()?;
    Some(set_guard(l, g, None, prof::on()))
}

/// Feed set `l`'s backlog if it has one and its lock is free; repeat while
/// frames keep arriving. Never waits for the set lock.
fn service(l: &SetLock, site: Site) {
    loop {
        if !l.pending.load(Ordering::Relaxed) {
            return;
        }
        let Some(mut g) = try_lock_set(l) else {
            // The holder sees `pending` when it lets go.
            return;
        };
        g.serve = false;
        {
            let mut s = lock_at(site);
            s.run_set(&mut g, None, false, false);
            s.mux.flush();
        }
        let n = refresh(&mut g);
        drop(g);
        fire(n, site == Site::Task);
        fence(Ordering::SeqCst);
    }
}

/// Recompute set `y`'s readiness (under its lock): the notification to
/// fire once it is released.
fn refresh(y: &mut Slot) -> Notify {
    let Owner::Sock(sock) = &mut y.owner else {
        return Notify::NONE;
    };
    let t = prof::start(prof::on());
    let n = sock.refresh(&y.set);
    if t != 0 {
        prof::REFRESH.done(t);
        prof::add(&prof::REFRESH_EVALS, 1);
        prof::add(&prof::REFRESH_HOOKS, n.hooks() as u64);
    }
    n
}

/// Fire `n`; with stats on and at least one hook, count it as from the
/// net task or from a syscall.
fn fire(n: Notify, from_task: bool) {
    let hooks = n.hooks() as u64;
    if hooks == 0 {
        return;
    }
    if !prof::on() {
        n.fire();
        return;
    }
    let t = prof::now();
    n.fire();
    if from_task {
        prof::FIRE_TASK.done(t);
        prof::add(&prof::FIRE_TASK_HOOKS, hooks);
    } else {
        prof::FIRE_SYS.done(t);
        prof::add(&prof::FIRE_SYS_HOOKS, hooks);
    }
}

// -- Entry points -------------------------------------------------------------

/// A socket syscall on set `l` (see the module docs): `f` does the
/// operation under the set lock (taking the interface lock through the
/// [`Iface`] if it needs to) and says whether the set must be polled
/// (something to send, state changed). If it polls, the same interface
/// lock hold goes on to drain received frames (the set lock is released
/// first); readiness changes are fired once no lock is held.
pub fn sys<R>(l: &SetLock, f: impl FnOnce(&mut Slot, &mut Iface) -> (R, bool)) -> R {
    let mut g = lock_set(l);
    let mut ifc = Iface::new();
    if l.pending.load(Ordering::Relaxed) {
        // Frames that arrived while the set was busy come first.
        ifc.get().run_set(&mut g, None, false, false);
    }
    let t = prof::start(prof::on());
    let (r, poll) = f(&mut g, &mut ifc);
    prof::WITH_OP.done(t);
    if poll {
        let mut h = match ifc.h.take() {
            Some(h) => h,
            None => lock_at(Site::SysPoll),
        };
        h.site = Site::SysPoll;
        h.run_set(&mut g, None, false, true);
        let n = refresh(&mut g);
        // Let go of the set but keep the interface lock for the drain. No
        // frame can have been queued on the set's backlog since `run_set`
        // (that needs the interface lock), so nothing is left to serve.
        g.serve = false;
        g.g.swap_irq_state(&mut h.g);
        drop(g);
        drain(Site::Drain, SYS_BUDGET, Some(h), n);
        return r;
    }
    if let Some(h) = ifc.h.as_mut() {
        // A backlog fed above may have queued replies.
        h.mux.flush();
    }
    drop(ifc);
    let n = refresh(&mut g);
    drop(g);
    fire(n, false);
    r
}

/// `File::poll` of an inet socket: `f` runs under the set lock, after a
/// pending backlog was fed.
pub fn poll_file<R>(l: &SetLock, f: impl FnOnce(&mut Slot) -> R) -> R {
    let mut g = lock_set(l);
    let mut n = Notify::NONE;
    if l.pending.load(Ordering::Relaxed) {
        {
            let mut s = lock_at(Site::FilePoll);
            s.run_set(&mut g, None, false, false);
            s.mux.flush();
        }
        n = refresh(&mut g);
    }
    let r = f(&mut g);
    drop(g);
    fire(n, false);
    r
}

/// Lock the interface for a socket being created (no set yet).
pub fn lock_new() -> Held {
    lock_at(Site::SysNoPoll)
}

/// Poll set `l` (egress, settle, timer) and fire its readiness.
fn poll_set(l: &SetLock, site: Site) {
    let mut g = lock_set(l);
    {
        let mut s = lock_at(site);
        s.run_set(&mut g, None, false, true);
    }
    let n = refresh(&mut g);
    drop(g);
    fire(n, site == Site::Task);
}

/// Frames fed per interface-lock hold in a drain: between holds the
/// collected readiness changes are fired and others get the lock.
const BATCH: usize = 32;

/// The set a drain hold is feeding: frames for it are fed as they come,
/// its egress and settling run once, when the hold moves on to another set
/// or ends ([`Drain::finish`]). So a run of segments for one connection
/// (a bulk transfer over loopback) is answered by one ACK, as one poll of
/// the old shared set did.
struct Cur {
    /// Declared before `r`: the guard borrows the lock `r` keeps alive.
    g: SetGuard<'static>,
    r: SetRef,
}

impl Cur {
    /// Lock `r` if it is free (under the interface lock: never waits).
    fn try_new(r: SetRef) -> Option<Cur> {
        // SAFETY: the guard borrows the `SetLock` inside `r`'s allocation,
        // which `r` (dropped after the guard) keeps alive.
        let l: &'static SetLock = unsafe { &*Arc::as_ptr(&r) };
        let mut g = try_lock_set(l)?;
        // Released under the interface lock, which every backlog push
        // needs: nothing can be pending.
        g.serve = false;
        Some(Cur { g, r })
    }
}

/// A drain in progress.
struct Drain {
    /// The set being fed (see [`Cur`]).
    cur: Option<Cur>,
    budget: usize,
    nic_n: usize,
    lo_n: usize,
    /// Readiness changes of the sets fed under the current hold.
    notify: Notify,
    /// Sets that got a frame on their backlog under the current hold.
    serve: Vec<SetRef>,
    /// Sets to poll (a rescan).
    work: Vec<SetRef>,
}

/// Take received frames — the loopback queue first (up to `LO_BUDGET`),
/// then at most `budget` from the NIC — and feed each to its set (see the
/// module docs), `BATCH` frames per interface-lock hold (the first hold
/// may be the caller's, `first`; `notify` is the caller's readiness change,
/// fired with the first batch's); then poll every set if a rescan was
/// asked for, and take the frames that produced, for at most `MAX_ROUNDS`
/// rounds.
fn drain(site: Site, budget: usize, first: Option<Held>, notify: Notify) {
    let mut buf = [0u8; MAX_FRAME];
    let mut d = Drain {
        cur: None,
        budget,
        nic_n: 0,
        lo_n: 0,
        notify,
        serve: Vec::new(),
        work: Vec::new(),
    };
    let from_task = site == Site::Task;
    let mut held = first;
    for _ in 0..MAX_ROUNDS {
        loop {
            let mut s = match held.take() {
                Some(h) => h,
                None => lock_at(site),
            };
            let done = drain_hold(&mut s, &mut d, &mut buf);
            drop(s);
            fire(core::mem::replace(&mut d.notify, Notify::NONE), from_task);
            if !d.serve.is_empty() {
                fence(Ordering::SeqCst);
                for r in d.serve.drain(..) {
                    service(&r, site);
                }
            }
            if done {
                break;
            }
        }
        if d.work.is_empty() {
            return;
        }
        for r in core::mem::take(&mut d.work) {
            poll_set(&r, site);
        }
    }
}

/// One hold of a drain: feed up to `BATCH` frames. A frame for a set whose
/// lock is free is fed right here (the set is locked with `try_lock` and
/// released before the interface lock: nothing can reach its backlog
/// meanwhile); otherwise it goes on the set's backlog and the set is
/// served after the hold. Returns true when no frame is left (within the
/// budgets).
fn drain_hold(s: &mut Held, d: &mut Drain, buf: &mut [u8; MAX_FRAME]) -> bool {
    let timing = s.prof;
    for _ in 0..BATCH {
        let t = prof::start(timing);
        let Some((len, nic)) = s
            .mux
            .next_frame(d.lo_n < LO_BUDGET, d.nic_n < d.budget, buf)
        else {
            if d.cur.is_some() {
                // Its replies may come back over loopback: look again.
                d.finish(s);
                continue;
            }
            prof::INGRESS_NONE.done(t);
            s.mux.flush();
            s.nic_backlog = d.nic_n >= d.budget;
            if s.nic_backlog || s.mux.lo_pending() {
                s.kick = true;
            }
            if s.rescan {
                s.rescan = false;
                if timing {
                    prof::add(&prof::RESCANS, 1);
                }
                d.work.extend(s.entries.iter().filter_map(|e| e.set.clone()));
            }
            return true;
        };
        if nic {
            d.nic_n += 1;
        } else {
            d.lo_n += 1;
        }
        let f = &buf[..len];
        let Some(id) = s.demux(f) else {
            let now = now();
            let Stack {
                iface, mux, empty, ..
            } = &mut **s;
            let _ = iface.poll_ingress_single(now, &mut mux.one_frame(f), empty);
            prof::INGRESS.done(t);
            if timing {
                prof::add(&prof::INGRESS_PKTS, 1);
            }
            continue;
        };
        let e = &mut s.entries[id];
        let Some(r) = e.set.clone() else {
            continue; // not expected: demux only names registered sets
        };
        if e.backlog.is_empty() {
            if d.cur.as_ref().is_some_and(|c| Arc::ptr_eq(&c.r, &r)) {
                let now = now();
                if let Some(c) = d.cur.as_mut() {
                    s.feed(&mut c.g, f, now);
                }
                continue;
            }
            if let Some(mut c) = Cur::try_new(r.clone()) {
                d.finish(s);
                let now = now();
                s.feed(&mut c.g, f, now);
                d.cur = Some(c);
                continue;
            }
        }
        if e.backlog.len() < MAX_BACKLOG {
            e.backlog.push_back(Box::from(f));
            r.pending.store(true, Ordering::Relaxed);
            if timing {
                prof::add(&prof::BACKLOGGED, 1);
            }
            if !d.serve.iter().any(|x| Arc::ptr_eq(x, &r)) {
                d.serve.push(r);
            }
        } else if timing {
            prof::add(&prof::BACKLOG_DROPS, 1);
        }
    }
    d.finish(s);
    // Ring the doorbells for this batch's replies.
    s.mux.flush();
    false
}

impl Drain {
    /// Poll the set being fed (its frames are in: egress, settling, timer),
    /// collect its readiness change and release it.
    fn finish(&mut self, s: &mut Stack) {
        let Some(Cur { mut g, r }) = self.cur.take() else {
            return;
        };
        s.run_set(&mut g, None, true, false);
        let n = refresh(&mut g);
        self.notify = core::mem::replace(&mut self.notify, Notify::NONE).and(n);
        drop(g);
        drop(r);
    }
}

impl Stack {
    // -- Registry -----------------------------------------------------------

    /// A registry index for a new set (filled by [`Stack::register`]; give
    /// it back with [`Stack::unreserve`] if the set is not made).
    pub fn reserve_set(&mut self) -> SetId {
        if let Some(i) = self.free_ids.pop() {
            return i;
        }
        self.entries.push(Entry {
            set: None,
            due: NEVER,
            backlog: VecDeque::new(),
        });
        self.entries.len() - 1
    }

    pub fn unreserve(&mut self, i: SetId) {
        self.free_ids.push(i);
    }

    /// Register `slot` (its `id` from [`Stack::reserve_set`]) and file its
    /// first poll time. It is reachable by the demux once the flow table or
    /// port map points at it.
    pub fn register(&mut self, slot: Slot) -> SetRef {
        let id = slot.id;
        let now_ns = crate::arch::timer::now_ns();
        let at = match self
            .iface
            .poll_delay(Instant::from_micros((now_ns / 1000) as i64), &slot.set)
        {
            Some(d) => now_ns.saturating_add(d.total_micros().saturating_mul(1000)),
            None => NEVER,
        };
        let r = SetLock::new(slot);
        self.entries[id].set = Some(r.clone());
        self.live_sets += 1;
        self.schedule(id, at);
        r
    }

    /// Release set `y` (registered; held by the caller): no timer, no
    /// backlog, not registered; its owner becomes [`Owner::Free`]. Its
    /// sockets must have been removed and its flow-table / port-map entries
    /// dropped. Called once per set (its owner may already be `Free`).
    pub fn unregister(&mut self, y: &mut Slot) {
        let id = y.id;
        if self.entries[id].set.is_none() {
            return;
        }
        self.unschedule(id);
        let e = &mut self.entries[id];
        e.backlog.clear();
        if let Some(r) = e.set.take() {
            r.pending.store(false, Ordering::Relaxed);
        }
        self.free_ids.push(id);
        self.live_sets -= 1;
        y.owner = Owner::Free;
        if y.set.iter().next().is_some() {
            // Not expected: drop whatever is left.
            y.set = SocketSet::new(Vec::new());
        }
    }

    /// Have the net task poll set `i` soon (it may have something to send).
    pub fn mark_dirty(&mut self, i: SetId) {
        self.dirty.push(i);
        self.kick = true;
    }

    fn unschedule(&mut self, i: SetId) {
        let d = self.entries[i].due;
        if d != NEVER {
            self.timers.remove(&(d, i));
            self.entries[i].due = NEVER;
        }
    }

    fn schedule(&mut self, i: SetId, at: u64) {
        if self.entries[i].due == at {
            return;
        }
        self.unschedule(i);
        if at != NEVER {
            self.timers.insert((at, i));
            self.entries[i].due = at;
            if at < self.task_deadline {
                self.kick = true;
            }
        }
    }

    /// When the stack wants to be polled next (ns; [`NEVER`]: no timer).
    pub fn earliest(&self) -> u64 {
        if self.mux.lo_pending() || self.rescan || self.nic_backlog || !self.dirty.is_empty() {
            return crate::arch::timer::now_ns();
        }
        self.timers.first().map_or(NEVER, |&(t, _)| t)
    }

    /// The net task's share: the dirty sets, the sets whose timer is due
    /// (timer cleared), every set when a rescan was asked for.
    fn collect_due(&mut self, work: &mut Vec<SetRef>) {
        let t = prof::start(self.prof);
        self.iface.poll_maintenance(now());
        prof::MAINT.done(t);
        let now_ns = crate::arch::timer::now_ns();
        while let Some(i) = self.dirty.pop() {
            if let Some(r) = &self.entries[i].set {
                work.push(r.clone());
            }
        }
        while let Some(&(at, i)) = self.timers.first() {
            if at > now_ns {
                break;
            }
            self.timers.pop_first();
            self.entries[i].due = NEVER;
            if let Some(r) = &self.entries[i].set {
                work.push(r.clone());
            }
        }
        if self.rescan {
            self.rescan = false;
            if self.prof {
                prof::add(&prof::RESCANS, 1);
            }
            work.extend(self.entries.iter().filter_map(|e| e.set.clone()));
        }
    }

    // -- Polling one set ------------------------------------------------------

    /// Poll set `y` (its lock and the interface lock held; see the module
    /// docs): feed its backlog and `frame`, run its egress (if `egress` or
    /// a frame was fed) until it has nothing more to send, settle it, file
    /// its next poll time. Readiness is left to the caller ([`refresh`],
    /// after the interface lock is dropped).
    pub fn run_set(&mut self, y: &mut Slot, frame: Option<&[u8]>, pre_fed: bool, egress: bool) {
        if matches!(y.owner, Owner::Free) {
            return;
        }
        let timing = self.prof;
        let t_all = prof::start(timing);
        let now_ns = crate::arch::timer::now_ns();
        let now = Instant::from_micros((now_ns / 1000) as i64);
        let mut fed = self.feed_backlog_at(y, now) || pre_fed;
        if let Some(f) = frame {
            self.feed(y, f, now);
            fed = true;
        }
        let sent = (egress || fed) && self.egress(y, now);
        let own = match y.owner {
            Owner::Free => None,
            Owner::Sock(_) => self.track(y, now_ns, fed, sent),
            Owner::Zombie { .. } => match inet::reap(self, y, now_ns) {
                Some(d) => d,
                None => {
                    prof::POLL_ALL.done(t_all);
                    return;
                }
            },
            Owner::Dhcp(_) => {
                let t = prof::start(timing);
                self.dhcp_events(y);
                prof::DHCP_EV.done(t);
                None
            }
        };
        let t = prof::start(timing);
        let at = match self.iface.poll_delay(now, &y.set) {
            Some(d) => now_ns.saturating_add(d.total_micros().saturating_mul(1000)),
            None => NEVER,
        };
        prof::DEADLINE.done(t);
        self.schedule(y.id, own.map_or(at, |o| o.min(at)));
        if timing {
            prof::add(&prof::SETS_EGRESSED, (egress || fed) as u64);
            if prof::get(&prof::POLL_ALL.calls).is_multiple_of(prof::SET_SAMPLE) {
                prof::add(&prof::SET_SUM, self.live_sets as u64);
                prof::add(&prof::SOCKS_SUM, self.nsocks as u64);
                prof::add(&prof::SET_SAMPLES, 1);
            }
            prof::max(&prof::MAX_SOCKS, self.nsocks as u64);
            prof::max(&prof::MAX_SET, self.live_sets as u64);
        }
        prof::POLL_ALL.done(t_all);
    }

    /// Feed set `y`'s backlog (its lock held), e.g. before moving a socket
    /// out of it. Returns whether there was one (its replies are sent by
    /// the next egress of `y`).
    pub fn feed_backlog(&mut self, y: &mut Slot) -> bool {
        self.feed_backlog_at(y, now())
    }

    fn feed_backlog_at(&mut self, y: &mut Slot, now: Instant) -> bool {
        if matches!(y.owner, Owner::Free) {
            return false;
        }
        let e = &mut self.entries[y.id];
        if e.backlog.is_empty() {
            return false;
        }
        if let Some(r) = &e.set {
            r.pending.store(false, Ordering::Relaxed);
        }
        let mut q = core::mem::take(&mut e.backlog);
        for f in q.drain(..) {
            self.feed(y, &f, now);
        }
        // Keep the deque's capacity (nothing was queued meanwhile: the
        // interface lock is held).
        self.entries[y.id].backlog = q;
        true
    }

    /// Feed one received frame to set `y`.
    fn feed(&mut self, y: &mut Slot, f: &[u8], now: Instant) {
        let timing = self.prof;
        let t = prof::start(timing);
        let _ = self
            .iface
            .poll_ingress_single(now, &mut self.mux.one_frame(f), &mut y.set);
        prof::INGRESS.done(t);
        if timing {
            prof::add(&prof::INGRESS_PKTS, 1);
        }
        if let Owner::Sock(sock) = &mut y.owner
            && inet::is_listener(sock)
        {
            // A burst of SYNs must each find a listening socket.
            let t = prof::start(timing);
            inet::refill(sock, &mut y.set, y.id, &mut self.flows);
            prof::REFILL.done(t);
        }
    }

    /// `poll_egress` on set `y` until it has nothing more to send. Returns
    /// whether it sent anything.
    fn egress(&mut self, y: &mut Slot, now: Instant) -> bool {
        let timing = self.prof;
        let mut sent = false;
        loop {
            let t = prof::start(timing);
            let r = self.iface.poll_egress(now, &mut self.mux, &mut y.set);
            if r == PollResult::None {
                prof::EGRESS_NONE.done(t);
                return sent;
            }
            sent = true;
            prof::EGRESS.done(t);
            if timing {
                prof::add(&prof::EGRESS_SOME, 1);
            }
        }
    }

    /// Settle inet socket set `y` after a poll (`inet::track`, a
    /// listener's refill; `fed`/`sent`: the poll fed it frames / sent
    /// something); returns the socket's own next deadline.
    fn track(&mut self, y: &mut Slot, now_ns: u64, fed: bool, sent: bool) -> Option<u64> {
        let timing = self.prof;
        let Slot { id, set, owner } = y;
        let Owner::Sock(sock) = owner else {
            return None;
        };
        let t = prof::start(timing);
        inet::track(sock, set, *id, &mut self.flows, now_ns, (fed, sent));
        if t != 0 {
            prof::TRACK.done(t);
            prof::add(&prof::TRACK_SOCKS, 1);
        }
        if inet::is_listener(sock) {
            let t = prof::start(timing);
            inet::refill(sock, set, *id, &mut self.flows);
            prof::REFILL.done(t);
        }
        sock.own_deadline()
    }

    /// The set the received frame `f` belongs to; `None`: no socket's (see
    /// the module docs).
    fn demux(&mut self, f: &[u8]) -> Option<SetId> {
        let timing = self.prof;
        let f = match demux::parse(f) {
            Parsed::Transport(f) => f,
            Parsed::Neighbor => {
                // The interface may learn a neighbor some set waits for.
                self.rescan = true;
                if timing {
                    prof::add(&prof::DEMUX_OTHER, 1);
                }
                return None;
            }
            Parsed::Other => {
                if timing {
                    prof::add(&prof::DEMUX_OTHER, 1);
                }
                return None;
            }
        };
        if let Some(&i) = self.flows.get(&f) {
            if timing {
                prof::add(&prof::DEMUX_EXACT, 1);
            }
            return Some(i);
        }
        if f.proto == demux::IPPROTO_UDP
            && f.lport == 68
            && f.rport == 67
            && let Some(d) = self.dhcp
        {
            if timing {
                prof::add(&prof::DEMUX_PORT, 1);
            }
            return Some(d);
        }
        // A UDP datagram to an address that is not ours can only be a
        // broadcast or multicast (smoltcp drops the rest): any socket on
        // the port takes it, as smoltcp's `accepts` does.
        let any = f.proto == demux::IPPROTO_UDP && !self.iface.has_ip_addr(f.local);
        match self.bound.pick(f.proto, f.lport, f.local, any) {
            Some(i) => {
                if timing {
                    prof::add(&prof::DEMUX_PORT, 1);
                }
                Some(i)
            }
            None => {
                if timing {
                    prof::add(&prof::DEMUX_MISS, 1);
                }
                None
            }
        }
    }

    /// Apply a DHCP client event (lease acquired, renewed with changes, or
    /// lost). Runs whenever the client's set `y` was polled.
    fn dhcp_events(&mut self, y: &mut Slot) {
        let Owner::Dhcp(h) = y.owner else {
            return;
        };
        let lease = match y.set.get_mut::<dhcpv4::Socket>(h).poll() {
            None => return,
            Some(dhcpv4::Event::Deconfigured) => None,
            Some(dhcpv4::Event::Configured(c)) => {
                let (hostname, domain) = c.packet.as_ref().map_or((None, None), dhcp_names);
                Some(NetConfig {
                    addr: Some((c.address.address(), c.address.prefix_len())),
                    gateway: c.router,
                    dns: c.dns_servers.iter().copied().collect(),
                    hostname,
                    domain,
                    leased: true,
                })
            }
        };
        match lease {
            Some(mut new) => {
                if new.dns.is_empty() {
                    new.dns = self.static_dns.clone();
                }
                if new == self.cfg {
                    return; // a renewal with nothing new
                }
                let first = !self.cfg.leased;
                self.set_v4(new.addr, new.gateway);
                let ms = crate::arch::timer::now_ns().saturating_sub(self.dhcp_t0) / 1_000_000;
                let mut dns = String::new();
                for d in &new.dns {
                    if !dns.is_empty() {
                        dns.push(' ');
                    }
                    dns.push_str(&format!("{}", d));
                }
                let (a, p) = new.addr.unwrap_or((Ipv4Addr::UNSPECIFIED, 0));
                let line = format!(
                    "net: dhcp {} {}/{} via {}, dns {}{}{} ({} ms)",
                    if first { "lease" } else { "lease changed:" },
                    a,
                    p,
                    new.gateway
                        .map_or(String::from("none"), |g| format!("{}", g)),
                    if dns.is_empty() { "none" } else { &dns },
                    new.hostname
                        .as_ref()
                        .map_or(String::new(), |h| format!(", host {}", h)),
                    new.domain
                        .as_ref()
                        .map_or(String::new(), |d| format!(", domain {}", d)),
                    ms
                );
                crate::kprintln!("[patina] {}", line);
                // Also a control-channel log event: how `patina run` learns
                // the guest's address (VZ NAT has no port forwarding).
                crate::ctl::log_info(format_args!("{}", line));
                if let (true, Some((ip, _)), Some(gw)) = (first, new.addr, new.gateway) {
                    self.mux.tx.arp_request(ip, gw);
                    // Ring the doorbell for the request now rather than at
                    // the next flush.
                    self.mux.flush();
                }
                self.cfg = new;
            }
            None => {
                // Also the client's first event, before any lease.
                if self.cfg.leased {
                    crate::kprintln!("[patina] net: dhcp lease lost, no IPv4 address");
                    self.set_v4(None, None);
                    self.cfg = NetConfig {
                        dns: self.static_dns.clone(),
                        ..NetConfig::default()
                    };
                }
            }
        }
    }

    /// Replace the interface's (non-loopback) IPv4 address and default
    /// route.
    fn set_v4(&mut self, addr: Option<(Ipv4Addr, u8)>, gw: Option<Ipv4Addr>) {
        self.iface.update_ip_addrs(|a| {
            a.retain(|c| !matches!(c, IpCidr::Ipv4(c) if !c.address().is_loopback()));
            if let Some((ip, p)) = addr {
                let _ = a.push(IpCidr::Ipv4(Ipv4Cidr::new(ip, p)));
            }
        });
        self.sync_own_addrs();
        self.iface.routes_mut().remove_default_ipv4_route();
        self.gw4 = None;
        if let Some(g) = gw
            && self.iface.routes_mut().add_default_ipv4_route(g).is_ok()
        {
            self.gw4 = Some(g);
        }
        // Sockets whose source address went away are reset by smoltcp
        // when they are next polled: poll every set.
        self.rescan = true;
        self.kick = true;
    }

    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Is `ip` one of the interface's addresses? Only 127.0.0.1 of 127/8:
    /// smoltcp accepts packets for its exact addresses, so binding or
    /// connecting to 127.0.0.2 fails at once (`EADDRNOTAVAIL` /
    /// `ENETUNREACH`) instead of hanging.
    pub fn is_local(&self, ip: core::net::IpAddr) -> bool {
        self.iface.has_ip_addr(ip)
    }

    /// Can we send to `ip` at all (a local address, a directly connected
    /// subnet or a default route, on an interface that exists)?
    pub fn routable(&self, ip: core::net::IpAddr) -> bool {
        if self.is_local(ip) {
            return true;
        }
        if !self.mux.has_nic() {
            return false;
        }
        match ip {
            core::net::IpAddr::V4(a) => {
                self.gw4.is_some()
                    || self.iface.ip_addrs().iter().any(|c| match c {
                        IpCidr::Ipv4(c) => !c.address().is_loopback() && c.contains_addr(&a),
                        _ => false,
                    })
            }
            core::net::IpAddr::V6(a) => {
                (a.segments()[0] & 0xffc0) == 0xfe80
                    || self.iface.routes().get_default_ipv6_route().is_some()
            }
        }
    }

    fn sync_own_addrs(&mut self) {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for c in self.iface.ip_addrs() {
            match c {
                IpCidr::Ipv4(c) => v4.push(c.address()),
                IpCidr::Ipv6(c) => v6.push(c.address()),
            }
        }
        self.mux.set_own(v4, v6);
    }

    /// Add an address to the interface (no-op when the table is full).
    pub fn add_addr(&mut self, cidr: IpCidr) {
        self.iface.update_ip_addrs(|a| {
            if !a.contains(&cidr) {
                let _ = a.push(cidr);
            }
        });
        self.sync_own_addrs();
    }
}

/// Host name (option 12) and domain name (option 15) of a DHCP packet.
fn dhcp_names(p: &smoltcp::wire::DhcpPacket<&[u8]>) -> (Option<String>, Option<String>) {
    let text = |d: &[u8]| {
        core::str::from_utf8(d)
            .ok()
            .map(|t| t.trim_end_matches('\0'))
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic()))
            .map(String::from)
    };
    let mut host = None;
    let mut dom = None;
    for o in p.options() {
        match o.kind {
            12 => host = text(o.data),
            15 => dom = text(o.data),
            _ => {}
        }
    }
    (host, dom)
}

fn mac_str(m: &[u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// EUI-64 link-local address for `mac`.
fn link_local(mac: &[u8; 6]) -> Ipv6Addr {
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([mac[0] ^ 0x02, mac[1]]),
        u16::from_be_bytes([mac[2], 0xff]),
        u16::from_be_bytes([0xfe, mac[3]]),
        u16::from_be_bytes([mac[4], mac[5]]),
    )
}

/// Bring up the stack: claim the NIC registered by `drivers::init` (if
/// any), configure loopback and the static `ip=` address, start the net
/// task. Called from `kernel_main` after `drivers::init`, before the
/// loader starts thread 0.
pub fn init(cmdline: &patina_cmdline::Cmdline) {
    let nic = super::device::take();
    let mac = nic.as_ref().map_or(LO_MAC, |n| n.mac());
    let nic_name = nic.as_ref().map(|n| n.name());
    let mut mux = Mux::new(nic, mac);

    let mut seed = [0u8; 8];
    crate::random::fill(&mut seed);
    let seed = u64::from_ne_bytes(seed);
    let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    cfg.random_seed = seed;
    let iface = Interface::new(cfg, &mut mux, now());

    let mut s = Stack {
        iface,
        mux,
        entries: Vec::new(),
        free_ids: Vec::new(),
        live_sets: 0,
        nsocks: 0,
        flows: BTreeMap::new(),
        bound: PortMap::new(),
        dirty: Vec::new(),
        empty: SocketSet::new(Vec::new()),
        timers: BTreeSet::new(),
        rescan: false,
        nic_backlog: false,
        kick: false,
        ports: PortTable::new(seed.rotate_left(17)),
        next_id: 0,
        gw4: None,
        task_deadline: 0,
        dhcp: None,
        dhcp_t0: 0,
        cfg: NetConfig::default(),
        static_dns: Vec::new(),
        prof: false,
    };
    s.add_addr(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 8)));
    s.add_addr(IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Addr::LOCALHOST, 128)));
    if s.mux.has_nic() {
        s.add_addr(IpCidr::Ipv6(Ipv6Cidr::new(link_local(&mac), 64)));
    }

    let mut dhcp = false;
    if let Some(ip) = &cmdline.ip {
        s.static_dns = ip.dns.iter().flatten().copied().collect();
        s.cfg.dns = s.static_dns.clone();
        if ip.is_static() {
            if let Some(client) = ip.client {
                let prefix = ip.prefix_len().unwrap_or(24);
                s.add_addr(IpCidr::Ipv4(Ipv4Cidr::new(client, prefix)));
                s.cfg.addr = Some((client, prefix));
            }
            if let Some(gw) = ip.gateway
                && s.iface.routes_mut().add_default_ipv4_route(gw).is_ok()
            {
                s.gw4 = Some(gw);
                s.cfg.gateway = Some(gw);
            }
        } else if ip.autoconf.wants_dhcp()
            || (ip.client.is_none() && ip.autoconf == patina_cmdline::Autoconf::Unspecified)
        {
            dhcp = true;
        }
    }

    // The boot line: hardware address, addresses, gateway.
    let mut addrs = String::new();
    for c in s.iface.ip_addrs() {
        if !addrs.is_empty() {
            addrs.push(' ');
        }
        addrs.push_str(&format!("{}", c));
    }
    match nic_name {
        Some(name) => crate::kprintln!(
            "[patina] net: {} hwaddr {}, addrs {}, gateway {}{}",
            name,
            mac_str(&mac),
            addrs,
            s.gw4.map_or(String::from("none"), |g| format!("{}", g)),
            if dhcp { ", dhcp requested" } else { "" }
        ),
        None => crate::kprintln!(
            "[patina] net: no device, loopback only (hwaddr {}, addrs {})",
            mac_str(&mac),
            addrs
        ),
    }

    let ev = s.mux.nic_event();
    let has_nic = s.mux.has_nic();
    if let Some(ev) = ev {
        let _ = EVENT.set(ev);
    }
    if dhcp && has_nic {
        let mut sock = dhcpv4::Socket::new();
        let mut rc = dhcpv4::RetryConfig::default();
        // Retransmit quickly while booting (smoltcp waits 10 s / 5 s).
        rc.discover_timeout = Duration::from_secs(1);
        rc.initial_request_timeout = Duration::from_secs(1);
        sock.set_retry_config(rc);
        sock.set_parameter_request_list(&DHCP_PARAMS);
        // The last received packet, for the host/domain name options.
        sock.set_receive_packet_buffer(alloc::vec![0u8; 1500].leak());
        let id = s.reserve_set();
        let mut set = SocketSet::new(Vec::new());
        let h = set.add(sock);
        let _ = s.register(Slot {
            id,
            set,
            owner: Owner::Dhcp(h),
        });
        s.dirty.push(id);
        s.dhcp = Some(id);
        s.dhcp_t0 = crate::arch::timer::now_ns();
    }
    DHCP_REQUESTED.store(dhcp, Ordering::Relaxed);
    prof::T_INIT.store(crate::arch::timer::now_ns(), Ordering::Relaxed);
    if STACK.set(SpinLock::new(s)).is_err() {
        return;
    }
    crate::drivers::spawn(task);
}

/// One round of the net task's work at `site`: poll the dirty and due sets
/// (rescanning if asked), then drain with the large budget. Returns
/// whether stats are on.
fn round(site: Site, work: &mut Vec<SetRef>) -> bool {
    let timing = {
        let mut s = lock_at(site);
        let timing = s.prof;
        if timing && site == Site::Task {
            prof::add(&prof::TASK_ROUNDS, 1);
            if let Some(c) = prof::TASK_CPU.get(crate::arch::cpu::id()) {
                prof::add(c, 1);
            }
        }
        s.collect_due(work);
        timing
    };
    for r in work.drain(..) {
        poll_set(&r, site);
    }
    drain(site, TASK_BUDGET, None, Notify::NONE);
    timing
}

/// `ip=dhcp` (or `ip=` without a client address): wait, at boot and before
/// user mode, until the DHCP client has a lease or [`DHCP_WAIT_NS`] passed.
/// Called from `kernel_main` after `ctl::init`, so a timeout is also
/// reported as a control-channel log event. Interrupts are not needed: the
/// NIC's `recv` polls its used ring. The client socket keeps its set;
/// whoever polls it afterwards (the net task at its timer, or a drain that
/// received a DHCP reply) applies renewals and losses.
pub fn wait_dhcp() {
    if !DHCP_REQUESTED.load(Ordering::Relaxed) {
        return;
    }
    if get().is_none() {
        return;
    }
    if lock_at(Site::Dhcp).dhcp.is_none() {
        crate::kprintln!("[patina] net: ip=dhcp: no network device, continuing without an address");
        crate::ctl::log_warn(format_args!(
            "net: ip=dhcp: no network device, continuing without an address"
        ));
        return;
    }
    let t0 = crate::arch::timer::now_ns();
    let mut work = Vec::new();
    loop {
        round(Site::Dhcp, &mut work);
        let (leased, next) = {
            let s = lock_at(Site::Dhcp);
            (s.cfg.leased, s.earliest())
        };
        if leased {
            return;
        }
        let now = crate::arch::timer::now_ns();
        if now.saturating_sub(t0) >= DHCP_WAIT_NS {
            break;
        }
        // Poll at least every millisecond (replies are found by polling).
        let until = next.min(now + 1_000_000).min(t0 + DHCP_WAIT_NS);
        while crate::arch::timer::now_ns() < until {
            core::hint::spin_loop();
        }
    }
    crate::kprintln!("[patina] net: dhcp: no lease after 10 s, continuing without an address");
    crate::ctl::log_warn(format_args!(
        "net: dhcp: no lease after 10 s, continuing without an address"
    ));
}

/// The net task: poll the due sets, drain received frames, sleep until the
/// earliest set timer (at most 1 s) or an event.
fn task() {
    let ev = event();
    prof::NET_TASK_TID.store(crate::task::sched::current().tid, Ordering::Relaxed);
    let mut spins = 0u32;
    let mut work = Vec::new();
    loop {
        let timing = round(Site::Task, &mut work);
        let mut deadline = {
            let mut s = lock_at(Site::Task);
            let now = crate::arch::timer::now_ns();
            let d = s.earliest().min(now + MAX_WAIT_NS);
            s.task_deadline = d;
            d
        };
        let now = crate::arch::timer::now_ns();
        if deadline <= now {
            // Something wants polling right away; do not spin forever.
            spins += 1;
            if spins < 8 {
                if timing {
                    prof::add(&prof::TASK_SPINS, 1);
                }
                continue;
            }
            if timing {
                prof::add(&prof::TASK_SPIN_LIMIT, 1);
            }
            deadline = now + 1_000_000;
        }
        spins = 0;
        let t = prof::start(prof::on());
        let signalled = ev.wait(Some(deadline));
        if t != 0 {
            let woke = prof::now();
            prof::add(&prof::TASK_SLEEP_NS, woke.saturating_sub(t));
            let at = ev.take_signalled_at();
            if signalled {
                prof::add(&prof::TASK_WAKE_SIGNAL, 1);
                if at != 0 && at <= woke {
                    let lat = woke - at;
                    prof::add(&prof::TASK_WAKE_LAT_NS, lat);
                    prof::add(&prof::TASK_WAKE_LAT_N, 1);
                    prof::max(&prof::TASK_WAKE_LAT_MAX, lat);
                }
            } else {
                prof::add(&prof::TASK_WAKE_DEADLINE, 1);
            }
        }
    }
}

/// `patina.stats=1` at exit (other CPUs stopped): the `net::prof`
/// counters, a few derived numbers and the stack's current size. The
/// header line and its totals are the interface lock's; the set locks have
/// their own `lock set-lock` line.
pub fn print_stats() {
    let Some(lock) = get() else {
        return;
    };
    let get = prof::get;
    let p = |a: core::fmt::Arguments<'_>| crate::console::panic_write_fmt(a);
    let now = prof::now();
    let t_first = get(&prof::T_FIRST);
    let window = if t_first == 0 { 0 } else { now - t_first };
    let uptime = now.saturating_sub(get(&prof::T_INIT));
    let ncpu = crate::arch::smp::cpu_count().clamp(1, crate::arch::cpu::MAX_CPUS);
    let pct = |a: u64, b: u64| Pct(a, b);
    let avg = |a: u64, b: u64| a.checked_div(b).unwrap_or(0);
    let ifs = &prof::SITES[..prof::IFACE_SITES];
    let total_hold: u64 = ifs.iter().map(|s| get(&s.hold_ns)).sum();
    let total_acq: u64 = ifs.iter().map(|s| get(&s.acq)).sum();
    let total_cont: u64 = ifs.iter().map(|s| get(&s.contended)).sum();
    let total_spin: u64 = ifs.iter().map(|s| get(&s.spin_ns)).sum();
    p(format_args!(
        "[patina] stats net-stack: window {} us (uptime since stack init {} us), lock acq {}, contended {} ({}%), spin {} us, held {} us = busy {}% of window ({}% of uptime), acq per cpu {}\n",
        window / 1000,
        uptime / 1000,
        total_acq,
        total_cont,
        pct(total_cont, total_acq),
        total_spin / 1000,
        total_hold / 1000,
        pct(total_hold, window),
        pct(total_hold, uptime),
        Slash(&prof::LOCK_CPU[..ncpu]),
    ));
    for (i, s) in prof::SITES.iter().enumerate() {
        let acq = get(&s.acq);
        if acq == 0 {
            continue;
        }
        let h = |b: usize| get(&s.hist[b]);
        p(format_args!(
            "[patina] stats net-stack lock {}: acq {}, contended {} ({}%), spin {} ns (avg {} ns/contended), held {} ns (avg {} ns, max {} ns), hist us <1 {} <2 {} <5 {} <10 {} <20 {} <50 {} <100 {} >=100 {}, spin max {} ns, spin hist <10us {} <100us {} <1ms {} <10ms {} >=10ms {}\n",
            prof::SITE_NAMES[i],
            acq,
            get(&s.contended),
            pct(get(&s.contended), acq),
            get(&s.spin_ns),
            avg(get(&s.spin_ns), get(&s.contended)),
            get(&s.hold_ns),
            avg(get(&s.hold_ns), get(&s.timed)),
            get(&s.max_hold_ns),
            h(0),
            h(1),
            h(2),
            h(3),
            h(4),
            h(5),
            h(6),
            h(7),
            get(&s.max_spin_ns),
            get(&s.spin_hist[0]),
            get(&s.spin_hist[1]),
            get(&s.spin_hist[2]),
            get(&s.spin_hist[3]),
            get(&s.spin_hist[4]),
        ));
    }
    let ph = |name: &str, ph: &prof::Phase| {
        p(format_args!(
            "[patina] stats net-stack phase {}: calls {}, ns {} (avg {} ns)\n",
            name,
            get(&ph.calls),
            get(&ph.ns),
            avg(get(&ph.ns), get(&ph.calls)),
        ));
    };
    ph("poll_all", &prof::POLL_ALL);
    ph("maintenance", &prof::MAINT);
    ph("ingress (packet)", &prof::INGRESS);
    ph("ingress (none)", &prof::INGRESS_NONE);
    ph("refill_listeners", &prof::REFILL);
    ph("egress (sent)", &prof::EGRESS);
    ph("egress (none)", &prof::EGRESS_NONE);
    ph("dhcp_events", &prof::DHCP_EV);
    ph("track", &prof::TRACK);
    ph("refresh_all", &prof::REFRESH);
    ph("next_deadline", &prof::DEADLINE);
    ph("with_op", &prof::WITH_OP);
    ph("nic_rx_kick", &prof::NIC_RX_KICK);
    ph("nic_tx_kick", &prof::NIC_TX_KICK);
    ph("nic_send", &prof::NIC_SEND);
    ph("nic_recv", &prof::NIC_RECV);
    ph("fire_task", &prof::FIRE_TASK);
    ph("fire_sys", &prof::FIRE_SYS);
    let (socks_now, set_now, flows_now, mux) = match lock.try_lock() {
        Some(s) => (
            s.nsocks as u64,
            s.live_sets as u64,
            s.flows.len() as u64,
            Some(s.mux.tx.stats),
        ),
        None => (0, 0, 0, None),
    };
    p(format_args!(
        "[patina] stats net-stack demux: exact {}, port {}, no socket {}, not transport {}, rescans {}, sets egressed {}, flows now {}, backlogged {}, backlog drops {}\n",
        get(&prof::DEMUX_EXACT),
        get(&prof::DEMUX_PORT),
        get(&prof::DEMUX_MISS),
        get(&prof::DEMUX_OTHER),
        get(&prof::RESCANS),
        get(&prof::SETS_EGRESSED),
        flows_now,
        get(&prof::BACKLOGGED),
        get(&prof::BACKLOG_DROPS),
    ));
    p(format_args!(
        "[patina] stats net-stack counts: ingress pkts {}, egress some {}, track socks {}, refresh evals {}, refresh hooks {}, fire hooks task {} sys {}, socks max {} now {}, socketset max {} now {} (sampled avg socks {} set {}), mux {}\n",
        get(&prof::INGRESS_PKTS),
        get(&prof::EGRESS_SOME),
        get(&prof::TRACK_SOCKS),
        get(&prof::REFRESH_EVALS),
        get(&prof::REFRESH_HOOKS),
        get(&prof::FIRE_TASK_HOOKS),
        get(&prof::FIRE_SYS_HOOKS),
        get(&prof::MAX_SOCKS),
        socks_now,
        get(&prof::MAX_SET),
        set_now,
        avg(get(&prof::SOCKS_SUM), get(&prof::SET_SAMPLES)),
        avg(get(&prof::SET_SUM), get(&prof::SET_SAMPLES)),
        MuxStats(mux),
    ));
    p(format_args!(
        "[patina] stats net-stack task: rounds {} per cpu {}, wakeups signalled {} deadline {}, spins {} (limit hit {}), wake latency avg {} ns max {} ns (n {}), asleep {} us, kicks from with {}, ipis per sending cpu {}, net-task enqueues {} (with ipi {})\n",
        get(&prof::TASK_ROUNDS),
        Slash(&prof::TASK_CPU[..ncpu]),
        get(&prof::TASK_WAKE_SIGNAL),
        get(&prof::TASK_WAKE_DEADLINE),
        get(&prof::TASK_SPINS),
        get(&prof::TASK_SPIN_LIMIT),
        avg(get(&prof::TASK_WAKE_LAT_NS), get(&prof::TASK_WAKE_LAT_N)),
        get(&prof::TASK_WAKE_LAT_MAX),
        get(&prof::TASK_WAKE_LAT_N),
        get(&prof::TASK_SLEEP_NS) / 1000,
        get(&prof::KICKS),
        Slash(&prof::IPI_CPU[..ncpu]),
        get(&prof::NET_TASK_ENQ),
        get(&prof::NET_TASK_IPI),
    ));
}

/// Counters joined with '/'.
struct Slash<'a>(&'a [prof::Counter]);

impl core::fmt::Display for Slash<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, c) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            write!(f, "{}", prof::get(c))?;
        }
        Ok(())
    }
}

/// `a / b` as a percentage with two decimals (integer math).
struct Pct(u64, u64);

impl core::fmt::Display for Pct {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let v = if self.1 == 0 {
            0
        } else {
            (self.0 as u128 * 10_000 / self.1 as u128) as u64
        };
        write!(f, "{}.{:02}", v / 100, v % 100)
    }
}

struct MuxStats(Option<super::mux::Stats>);

impl core::fmt::Display for MuxStats {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(s) => write!(
                f,
                "nic tx {} rx {} tx-drops {}, lo frames {} drops {}",
                s.nic_tx, s.nic_rx, s.nic_tx_drops, s.lo_frames, s.lo_drops
            ),
            None => f.write_str("(stack locked)"),
        }
    }
}
