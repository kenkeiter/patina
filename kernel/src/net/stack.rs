//! The TCP/IP stack (design §9, D67): smoltcp over the [`mux`] devices
//! (NIC + loopback), one smoltcp `SocketSet` per socket behind its own
//! lock, one smoltcp `Interface` per CPU (a [`Shard`]), and one registry
//! ([`Stack`]: demux tables, timers, backlogs, configuration) behind the
//! stack lock.
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
//! socket all of `inet::Sock`: kind, options, error, readiness hooks) and
//! its poll timer.
//!
//! **Interface shards.** smoltcp's `Interface` holds what its sockets
//! share: addresses, routes, the neighbor cache, the IPv4 ident counter, a
//! random source. All TCP and UDP state lives in the sockets, so the same
//! socket can be polled through any interface that has the same addresses
//! and routes. Each CPU has its own `Interface` (a [`Shard`], created on
//! first use) and polls sets through it, so smoltcp's ingress and egress
//! work runs on every CPU at once. What must agree is kept so: addresses
//! and routes live in the registry and each shard re-applies them when
//! their generation changed (`Shard::sync`); neighbor frames (ARP,
//! ICMPv6) are fed to every shard, only the taker's transmitting replies
//! (`mux::OneFrame` muted for the others), so all learn the neighbor. The
//! shards share the transmit side's loopback queue and the NIC
//! (`mux::Out`).
//!
//! **Locks.** Lock order: **set lock → own shard → stack lock → leaves**
//! (the loopback queue's lock, the virtio-net driver's lock, the heap). A
//! CPU only ever locks its own shard (taken with interrupts masked, so it
//! is uncontended), except to show a neighbor frame to the other shards,
//! which it does holding no other lock. Nobody waits for a set lock while
//! holding a shard, the stack lock or another set lock (a `try_lock` of a
//! set under the stack lock is allowed: it never waits). `check_order`
//! panics on a blocking acquisition that breaks the order. The stack lock
//! covers the registry: the slot registry with each set's backlog, the
//! flow table, the port map, the bind table, the timers, configuration and
//! DHCP state; it is held for short bookkeeping only (taking a frame off a
//! device and demultiplexing it, filing a timer, flow-table changes). The
//! flow table, port map, timers and registry are only touched under it,
//! by a holder of the set concerned (or for a set not yet registered).
//! Hooks (`Notify::fire`) run with no lock held: readiness is collected
//! under the set lock and fired after it is released (fs/file.rs lock
//! order).
//!
//! **Ingress demux.** Frames are taken one at a time under the stack lock
//! (loopback queue first, then the NIC) into a buffer owned by the taker,
//! and their transport tuple read (`patina_inet::demux::parse`). The tuple
//! is looked up (a) exactly in [`Stack::flows`] — connections (from
//! `connect` on, including zombies) and a listener's not-yet-accepted
//! connections (pointing at the listener's set until `accept` re-points
//! them); (b) else by local port in [`Stack::bound`] — listeners and bound
//! UDP sockets, an exact local address before a wildcard, only sockets that
//! cover the packet's family; UDP 67 → 68 goes to the DHCP client; (c)
//! else the frame goes to the taker's shard's empty set: smoltcp still
//! answers ARP, ICMP echo and NDISC at the interface level, a TCP segment
//! for no socket gets a RST and a UDP datagram an ICMP port unreachable. A
//! set gets its frame through a one-frame device (`mux::OneFrame`).
//!
//! **Backlogs (frame order).** While the stack lock is still held from a
//! frame's demux, the taker `try_lock`s the target set Y; if that succeeds
//! it keeps Y, drops the stack lock and feeds the frame through its shard.
//! If Y is busy (a syscall on Y, another CPU feeding it), the frame is
//! copied onto Y's backlog (in its registry entry) and `SetLock::pending`
//! set. Whoever polls Y feeds its backlog first, and every release of a set
//! lock checks `pending` afterwards (`service`: a `try_lock`, so it never
//! waits). The taker that queued a frame calls `service` too; with `SeqCst`
//! fences on both sides either the taker gets the lock or the releasing
//! holder sees the flag, so no frame is stranded. Frames of one set are
//! therefore fed in the order they were taken off the device, however many
//! CPUs take frames (re-validating the demux after re-locking, the
//! alternative, reorders segments of one flow between CPUs). A frame is
//! bound to the set its demux named when it was taken: operations that move
//! smoltcp sockets out of a set (`accept`, closing a listener) feed the
//! set's backlog first; a set that is freed drops its backlog.
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
//! still in `TimeWait` reaches that socket). Changes found while polling a
//! set (`refill`, `track`) are collected in the slot ([`FlowOp`]) and
//! applied at the end of the poll under the stack lock; until then frames
//! for such a tuple still reach the same set (by port, or not at all), so
//! nothing is lost.
//!
//! **Polling a set** ([`run_set`]: set lock and own shard held): feed its
//! backlog (and the frame at hand), `poll_egress` until it has nothing more
//! to send, settle it (`inet::track`: smoltcp state transitions →
//! socket-API events; a listener's pool refill; a zombie's reaping; DHCP
//! events), `poll_delay` plus socket-level deadlines (connect timeout,
//! deferred keep-alive, zombie linger). The set's timer only ever moves
//! earlier outside the net task: a later deadline leaves the timer as it
//! is (it fires early, the net task polls the set and files the real one),
//! so most polls need no stack lock at all. Readiness
//! (`inet::Sock::refresh`) is recomputed after the shard is released,
//! under the set lock.
//!
//! Who polls:
//!
//! * every **socket syscall** ([`sys`]): lock its set (feed a pending
//!   backlog first), do the operation (taking its shard and the stack lock
//!   inside when it needs shared state), poll the set if the operation
//!   changed something, release, fire; then **drain** received frames
//!   (loopback first, every one of them, then at most `SYS_BUDGET` NIC
//!   frames). So a write is in the peer's receive buffer (over loopback)
//!   before the syscall returns — or on its backlog while another CPU
//!   holds the peer, to be fed before that holder lets go — and the peer's
//!   ACK is processed too;
//! * the **net task** (a kernel thread): polls the sets whose timer is due
//!   (and the dirty ones: zombies made by closing a listener), drains with
//!   the large budget (`TASK_BUDGET`), then sleeps on an `Event` until the
//!   earliest timer (at most 1 s). The event is the NIC's (its IRQ handler
//!   signals it and never takes a stack lock) or a stack-owned one without
//!   a NIC. Others signal it when they file an earlier timer, leave NIC
//!   frames behind (a syscall's budget ran out) or need a rescan.
//!
//! ARP and ICMPv6 frames may teach the interfaces a neighbor that some set
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
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Cidr, Ipv6Cidr};

use super::device::MAX_FRAME;
use super::inet::{self, Reap, Sock};
use super::mux::{Out, Tx};
use super::prof::{self, Site};
use crate::arch::cpu::MAX_CPUS;
use crate::arch::irq;
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
/// Addresses an interface holds (smoltcp `iface-max-addr-count-8`).
const MAX_ADDRS: usize = 8;
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

/// A flow-table change found while polling a set (applied under the stack
/// lock at the end of the poll; see the module docs).
#[derive(Clone, Copy)]
pub enum FlowOp {
    /// Point `Flow` at this set.
    Insert(Flow),
    /// Remove `Flow` if it points at this set.
    Remove(Flow),
}

/// A socket set and its owner.
pub struct Slot {
    /// Registry index (stale once `owner` is [`Owner::Free`]).
    pub id: SetId,
    pub set: SocketSet<'static>,
    pub owner: Owner,
    /// Poll time this set is filed under in the timers ([`NEVER`]: none).
    due: u64,
    /// Flow-table changes not yet applied.
    pub ops: Vec<FlowOp>,
}

impl Slot {
    pub fn new(id: SetId, set: SocketSet<'static>, owner: Owner) -> Slot {
        Slot {
            id,
            set,
            owner,
            due: NEVER,
            ops: Vec::new(),
        }
    }
}

/// A slot behind its lock (see the module docs).
pub struct SetLock {
    slot: SpinLock<Slot>,
    /// The set's backlog is not empty (written under the stack lock; read
    /// by whoever releases the set lock).
    pending: AtomicBool,
}

pub type SetRef = Arc<SetLock>;

/// A registry entry (under the stack lock).
struct Entry {
    set: Option<SetRef>,
    /// Frames taken for this set while its lock was busy, in order.
    backlog: VecDeque<Box<[u8]>>,
}

/// The registry: what the stack lock covers (see the module docs).
pub struct Stack {
    /// The shared transmit side and the frame source.
    pub out: &'static Out,
    /// Socket sets by [`SetId`], with their backlogs.
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
    /// The interfaces' addresses (every shard applies them).
    pub addrs: Vec<IpCidr>,
    /// IPv4 default gateway (static `ip=` or the lease), for routes.
    pub gw4: Option<Ipv4Addr>,
    /// Generation of `addrs` / `gw4` (see `Shard::sync`).
    cfg_gen: u64,
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
    /// `patina.stats=1` was set when this lock hold began.
    pub prof: bool,
}

/// `ip=` asked for DHCP (whether or not there is a NIC to run it on).
static DHCP_REQUESTED: AtomicBool = AtomicBool::new(false);

static STACK: OnceCell<SpinLock<Stack>> = OnceCell::new();
static OUT: OnceCell<Out> = OnceCell::new();
static LOCAL_EVENT: Event = Event::new();
static EVENT: OnceCell<&'static Event> = OnceCell::new();
/// `Stack::gen`, readable without the stack lock.
static CFG_GEN: AtomicU64 = AtomicU64::new(0);

pub fn now() -> Instant {
    Instant::from_micros((crate::arch::timer::now_ns() / 1000) as i64)
}

fn instant(ns: u64) -> Instant {
    Instant::from_micros((ns / 1000) as i64)
}

/// Remove the flow-table entry for `f` if it still points at set `i` (a
/// newer socket with the same tuple keeps its entry).
pub fn flow_remove(flows: &mut BTreeMap<Flow, SetId>, f: Flow, i: SetId) {
    if flows.get(&f) == Some(&i) {
        flows.remove(&f);
    }
}

/// The stack lock (the registry), once [`init`] ran.
pub fn get() -> Option<&'static SpinLock<Stack>> {
    STACK.get()
}

fn event() -> &'static Event {
    EVENT.get().copied().unwrap_or(&LOCAL_EVENT)
}

// -- Lock order check -----------------------------------------------------

/// CPU (+1) holding the stack lock; 0: none.
static STACK_CPU: AtomicUsize = AtomicUsize::new(0);
/// Set locks held per CPU.
static SETS_HELD: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];
/// The CPU holds its own shard.
static SHARD_HELD: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Set,
    Shard,
    Stack,
}

/// Panic if this CPU is about to wait for a lock out of order (see the
/// module docs; called when the lock is busy). A thread holding a spinlock cannot migrate (IRQs are
/// masked), so per-CPU flags describe what it holds.
fn check_order(want: Want) {
    let f = irq::save_disable();
    let cpu = crate::arch::cpu::id();
    let stack = STACK_CPU.load(Ordering::Relaxed) == cpu + 1;
    let shard = SHARD_HELD
        .get(cpu)
        .is_some_and(|c| c.load(Ordering::Relaxed));
    let sets = SETS_HELD
        .get(cpu)
        .map_or(0, |c| c.load(Ordering::Relaxed));
    irq::restore(f);
    let bad = match want {
        Want::Stack => stack,
        Want::Shard => stack || shard,
        Want::Set => stack || shard || sets > 0,
    };
    if bad {
        panic!(
            "net: lock order: {} wanted holding{}{}{}",
            match want {
                Want::Set => "a socket-set lock",
                Want::Shard => "the shard",
                Want::Stack => "the stack lock",
            },
            if stack { " the stack lock" } else { "" },
            if shard { " the shard" } else { "" },
            if sets > 0 { " a socket-set lock" } else { "" },
        );
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

/// Count a hold of a lock at `site` (stats on: `t0` != 0).
fn record(site: Site, t0: u64, spin: Option<u64>) {
    if t0 == 0 {
        return;
    }
    let st = &prof::SITES[site as usize];
    prof::add(&st.acq, 1);
    if let Some(ns) = spin {
        st.record_spin(ns);
    }
    st.record_hold(prof::now().saturating_sub(t0));
}

/// Lock `l` (`want` names it for the order check, which runs only when
/// the lock is busy: a deadlock needs a wait), timing a contended
/// acquisition when stats are on: (guard, hold start or 0, spin).
fn timed_lock<T>(l: &SpinLock<T>, want: Want) -> (SpinLockGuard<'_, T>, u64, Option<u64>) {
    let timing = prof::on();
    let (g, spin) = match l.try_lock() {
        Some(g) => (g, None),
        None => {
            check_order(want);
            let t = prof::start(timing);
            let g = l.lock();
            (g, timing.then(|| prof::now().saturating_sub(t)))
        }
    };
    (g, prof::start(timing), spin)
}

// -- The stack lock ---------------------------------------------------------

/// The stack lock, held for `site`. With `patina.stats=1` the acquisition
/// (contention, spin time) and the hold time are counted per site
/// (`net::prof`), under the site the holder ends up as. Dropping it wakes
/// the net task if the hold asked for that (`Stack::kick`).
pub struct Held {
    g: ManuallyDrop<SpinLockGuard<'static, Stack>>,
    pub site: Site,
    t0: u64,
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
        record(self.site, self.t0, self.spin);
        STACK_CPU.store(0, Ordering::Relaxed);
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

/// Take the stack lock for `site` (every taker but netlink's read-only
/// snapshot goes through here).
pub fn lock_at(site: Site) -> Held {
    let lock = get().expect("net: stack not initialised");
    let (mut g, t0, spin) = timed_lock(lock, Want::Stack);
    g.prof = t0 != 0;
    if t0 != 0 {
        if let Some(c) = prof::LOCK_CPU.get(crate::arch::cpu::id()) {
            prof::add(c, 1);
        }
        let _ = prof::T_FIRST.compare_exchange(0, t0, Ordering::Relaxed, Ordering::Relaxed);
    }
    STACK_CPU.store(crate::arch::cpu::id() + 1, Ordering::Relaxed);
    Held {
        g: ManuallyDrop::new(g),
        site,
        t0,
        spin,
    }
}

// -- Shards -------------------------------------------------------------------

/// One CPU's smoltcp interface (see the module docs).
pub struct Shard {
    pub iface: Interface,
    pub tx: Tx,
    /// Frames for no socket are fed to this (always empty) set.
    empty: SocketSet<'static>,
    /// Generation of the addresses applied (`Stack::gen`).
    cfg_gen: u64,
}

static SHARDS: [OnceCell<SpinLock<Shard>>; MAX_CPUS] = [const { OnceCell::new() }; MAX_CPUS];

impl Shard {
    fn new(out: &'static Out) -> Shard {
        let mut seed = [0u8; 8];
        crate::random::fill(&mut seed);
        let mut tx = Tx::new(out);
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(out.mac)));
        cfg.random_seed = u64::from_ne_bytes(seed);
        let iface = Interface::new(cfg, &mut tx, now());
        Shard {
            iface,
            tx,
            empty: SocketSet::new(Vec::new()),
            cfg_gen: 0,
        }
    }

    /// Apply the registry's addresses and routes if they changed since
    /// this shard last looked (takes the stack lock then).
    fn sync(&mut self) {
        if self.cfg_gen == CFG_GEN.load(Ordering::Acquire) {
            return;
        }
        let s = lock_at(Site::Shard);
        self.apply(&s);
    }

    /// Apply `s`'s addresses and routes.
    fn apply(&mut self, s: &Stack) {
        self.iface.update_ip_addrs(|a| {
            a.clear();
            for c in &s.addrs {
                let _ = a.push(*c);
            }
        });
        self.iface.routes_mut().remove_default_ipv4_route();
        if let Some(g) = s.gw4 {
            let _ = self.iface.routes_mut().add_default_ipv4_route(g);
        }
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for c in &s.addrs {
            match c {
                IpCidr::Ipv4(c) => v4.push(c.address()),
                IpCidr::Ipv6(c) => v6.push(c.address()),
            }
        }
        self.tx.set_own(v4, v6);
        self.cfg_gen = s.cfg_gen;
    }
}

/// This CPU's shard, held (see the module docs). Interrupts stay masked
/// from before the CPU is chosen until the guard is dropped.
pub struct ShardGuard {
    g: ManuallyDrop<SpinLockGuard<'static, Shard>>,
    cpu: usize,
    irq: Option<irq::Flags>,
    t0: u64,
    spin: Option<u64>,
}

impl core::ops::Deref for ShardGuard {
    type Target = Shard;
    fn deref(&self) -> &Shard {
        &self.g
    }
}

impl core::ops::DerefMut for ShardGuard {
    fn deref_mut(&mut self) -> &mut Shard {
        &mut self.g
    }
}

impl Drop for ShardGuard {
    fn drop(&mut self) {
        record(Site::Shard, self.t0, self.spin);
        SHARD_HELD[self.cpu].store(false, Ordering::Relaxed);
        // SAFETY: dropped exactly once, here.
        unsafe { ManuallyDrop::drop(&mut self.g) };
        if let Some(f) = self.irq.take() {
            irq::restore(f);
        }
    }
}

fn shard_cell(cpu: usize) -> &'static SpinLock<Shard> {
    if let Some(c) = SHARDS[cpu].get() {
        return c;
    }
    let out = OUT.get().expect("net: stack not initialised");
    let _ = SHARDS[cpu].set(SpinLock::new(Shard::new(out)));
    SHARDS[cpu].get().unwrap()
}

/// Lock this CPU's shard (creating it on first use) with its addresses up
/// to date.
pub fn lock_shard() -> ShardGuard {
    let f = irq::save_disable();
    let cpu = crate::arch::cpu::id();
    let cell = shard_cell(cpu);
    let (g, t0, spin) = timed_lock(cell, Want::Shard);
    SHARD_HELD[cpu].store(true, Ordering::Relaxed);
    let mut sg = ShardGuard {
        g: ManuallyDrop::new(g),
        cpu,
        irq: Some(f),
        t0,
        spin,
    };
    sg.sync();
    sg
}

/// Show a neighbor frame (ARP, ICMPv6) to every shard but this CPU's
/// (which was fed it already), replies muted, so they all learn the
/// neighbor. Holds no other lock.
fn show_neighbor(f: &[u8]) {
    let me = {
        let fl = irq::save_disable();
        let c = crate::arch::cpu::id();
        irq::restore(fl);
        c
    };
    let now = now();
    for (k, cell) in SHARDS.iter().enumerate() {
        if k == me {
            continue;
        }
        let Some(c) = cell.get() else {
            continue;
        };
        let mut g = c.lock();
        let Shard { iface, tx, empty, .. } = &mut *g;
        let _ = iface.poll_ingress_single(now, &mut tx.one_frame(f, true), empty);
    }
}

/// The locks a syscall's operation may need beyond its set lock: its
/// CPU's shard and the stack lock, taken on first use in that order.
pub struct Ctx {
    sh: Option<ShardGuard>,
    st: Option<Held>,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        self.st = None;
        self.sh = None;
    }
}

impl Ctx {
    fn new() -> Ctx {
        Ctx { sh: None, st: None }
    }

    /// The shard and the registry, locked.
    pub fn get(&mut self) -> (&mut Shard, &mut Stack) {
        if self.sh.is_none() {
            self.st = None;
            self.sh = Some(lock_shard());
        }
        if self.st.is_none() {
            self.st = Some(lock_at(Site::SysNoPoll));
        }
        (self.sh.as_mut().unwrap(), self.st.as_mut().unwrap())
    }

    /// The registry, locked (with the shard: see [`Ctx::get`]).
    pub fn stack(&mut self) -> &mut Stack {
        self.get().1
    }

    /// The shard, taken out of the context (the stack lock is released).
    fn take_shard(&mut self) -> ShardGuard {
        self.st = None;
        match self.sh.take() {
            Some(sh) => sh,
            None => lock_shard(),
        }
    }

    /// The shard alone (the stack lock is released).
    fn shard(&mut self) -> &mut ShardGuard {
        self.st = None;
        if self.sh.is_none() {
            self.sh = Some(lock_shard());
        }
        self.sh.as_mut().unwrap()
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
        record(Site::SetLock, self.t0, self.spin);
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
    t0: u64,
    spin: Option<u64>,
) -> SetGuard<'a> {
    sets_held(1);
    SetGuard {
        g: ManuallyDrop::new(g),
        l,
        t0,
        spin,
        serve: true,
    }
}

/// Lock set `l` (waiting).
pub fn lock_set(l: &SetLock) -> SetGuard<'_> {
    let (g, t0, spin) = timed_lock(&l.slot, Want::Set);
    set_guard(l, g, t0, spin)
}

/// Lock set `l` if it is free (allowed under the stack lock).
fn try_lock_set(l: &SetLock) -> Option<SetGuard<'_>> {
    let g = l.slot.try_lock()?;
    Some(set_guard(l, g, prof::start(prof::on()), None))
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
            let mut sh = lock_shard();
            run_set(&mut sh, &mut g, None, false, false, site);
        }
        if let Some(o) = OUT.get() {
            o.flush();
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

// -- Polling one set ----------------------------------------------------------

/// Poll set `g` through shard `sh` (see the module docs): feed its backlog
/// and `frame` (`pre_fed`: the caller fed it frames already), run its egress (if `egress` or a frame was fed) until it
/// has nothing more to send, settle it, and file its next poll time if
/// that is earlier than its timer. Takes the stack lock (for `site`) only
/// for a backlog, flow-table changes, an earlier timer, or releasing the
/// set. Readiness is left to the caller ([`refresh`]).
pub fn run_set(
    sh: &mut Shard,
    g: &mut SetGuard<'_>,
    frame: Option<&[u8]>,
    pre_fed: bool,
    egress: bool,
    site: Site,
) {
    let l = g.l;
    let y: &mut Slot = g;
    if matches!(y.owner, Owner::Free) {
        return;
    }
    let timing = prof::on();
    let t_all = prof::start(timing);
    let now_ns = crate::arch::timer::now_ns();
    let now = instant(now_ns);
    let mut fed = pre_fed;
    if l.pending.load(Ordering::Relaxed) {
        let q = lock_at(site).take_backlog(y.id);
        for f in &q {
            feed(sh, y, f, now);
        }
        fed |= !q.is_empty();
    }
    if let Some(f) = frame {
        feed(sh, y, f, now);
        fed = true;
    }
    let sent = (egress || fed) && egress_set(sh, y, now);
    if timing && (egress || fed) {
        prof::add(&prof::SETS_EGRESSED, 1);
    }
    let mut gone = false;
    let own = match y.owner {
        Owner::Free => None,
        Owner::Sock(_) => track(y, now_ns, fed, sent),
        Owner::Zombie { .. } => match inet::reap(y, now_ns) {
            Reap::Gone => {
                gone = true;
                None
            }
            Reap::Keep(d) => d,
        },
        Owner::Dhcp(_) => {
            let t = prof::start(timing);
            dhcp_events(sh, y, site);
            prof::DHCP_EV.done(t);
            None
        }
    };
    let at = if gone {
        NEVER
    } else {
        let t = prof::start(timing);
        let at = match sh.iface.poll_delay(now, &y.set) {
            Some(d) => now_ns.saturating_add(d.total_micros().saturating_mul(1000)),
            None => NEVER,
        };
        prof::DEADLINE.done(t);
        own.map_or(at, |o| o.min(at))
    };
    if gone || !y.ops.is_empty() || at < y.due {
        let mut s = lock_at(site);
        s.apply_ops(y);
        if gone {
            s.unregister(y);
        } else if at < y.due {
            s.schedule(y, at);
        }
        if timing {
            prof::max(&prof::MAX_SOCKS, s.nsocks as u64);
            prof::max(&prof::MAX_SET, s.live_sets as u64);
            if prof::get(&prof::POLL_ALL.calls).is_multiple_of(prof::SET_SAMPLE) {
                prof::add(&prof::SET_SUM, s.live_sets as u64);
                prof::add(&prof::SOCKS_SUM, s.nsocks as u64);
                prof::add(&prof::SET_SAMPLES, 1);
            }
        }
    }
    prof::POLL_ALL.done(t_all);
}

/// Feed one received frame to set `y` through shard `sh`.
fn feed(sh: &mut Shard, y: &mut Slot, f: &[u8], now: Instant) {
    let timing = prof::on();
    let t = prof::start(timing);
    let Shard { iface, tx, .. } = sh;
    let _ = iface.poll_ingress_single(now, &mut tx.one_frame(f, false), &mut y.set);
    prof::INGRESS.done(t);
    if timing {
        prof::add(&prof::INGRESS_PKTS, 1);
    }
    let Slot {
        set, owner, ops, ..
    } = y;
    if let Owner::Sock(sock) = owner
        && inet::is_listener(sock)
    {
        // A burst of SYNs must each find a listening socket.
        let t = prof::start(timing);
        inet::refill(sock, set, ops);
        prof::REFILL.done(t);
    }
}

/// `poll_egress` on set `y` until it has nothing more to send. Returns
/// whether it sent anything.
fn egress_set(sh: &mut Shard, y: &mut Slot, now: Instant) -> bool {
    let timing = prof::on();
    let mut sent = false;
    loop {
        let t = prof::start(timing);
        let r = sh.iface.poll_egress(now, &mut sh.tx, &mut y.set);
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

/// Settle inet socket set `y` after a poll (`inet::track`, a listener's
/// refill; `fed`/`sent`: the poll fed it frames / sent something); returns
/// the socket's own next deadline.
fn track(y: &mut Slot, now_ns: u64, fed: bool, sent: bool) -> Option<u64> {
    let timing = prof::on();
    let Slot {
        set, owner, ops, ..
    } = y;
    let Owner::Sock(sock) = owner else {
        return None;
    };
    let t = prof::start(timing);
    inet::track(sock, set, ops, now_ns, (fed, sent));
    if t != 0 {
        prof::TRACK.done(t);
        prof::add(&prof::TRACK_SOCKS, 1);
    }
    if inet::is_listener(sock) {
        let t = prof::start(timing);
        inet::refill(sock, set, ops);
        prof::REFILL.done(t);
    }
    sock.own_deadline()
}

/// Apply a DHCP client event (lease acquired, renewed with changes, or
/// lost). Runs whenever the client's set `y` was polled.
fn dhcp_events(sh: &mut Shard, y: &mut Slot, site: Site) {
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
    let mut s = lock_at(site);
    let arp = match lease {
        Some(mut new) => {
            if new.dns.is_empty() {
                new.dns = s.static_dns.clone();
            }
            if new == s.cfg {
                return; // a renewal with nothing new
            }
            let first = !s.cfg.leased;
            s.set_v4(new.addr, new.gateway);
            let ms = crate::arch::timer::now_ns().saturating_sub(s.dhcp_t0) / 1_000_000;
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
            let arp = match (first, new.addr, new.gateway) {
                (true, Some((ip, _)), Some(gw)) => Some((ip, gw)),
                _ => None,
            };
            s.cfg = new;
            arp
        }
        None => {
            // Also the client's first event, before any lease.
            if s.cfg.leased {
                crate::kprintln!("[patina] net: dhcp lease lost, no IPv4 address");
                s.set_v4(None, None);
                s.cfg = NetConfig {
                    dns: s.static_dns.clone(),
                    ..NetConfig::default()
                };
            }
            None
        }
    };
    // This shard takes the new addresses now (the others when next locked).
    sh.apply(&s);
    drop(s);
    if let Some((ip, gw)) = arp {
        sh.tx.arp_request(ip, gw);
        // Ring the doorbell for the request now rather than at the next
        // flush.
        sh.tx.out().flush();
    }
}

// -- Entry points -------------------------------------------------------------

/// A socket syscall on set `l` (see the module docs): `f` does the
/// operation under the set lock (taking the shard and the stack lock
/// through the [`Ctx`] if it needs to) and says whether the set must be
/// polled (something to send, state changed). Then readiness is refreshed
/// and fired, and if it polled, received frames are drained.
pub fn sys<R>(l: &SetLock, f: impl FnOnce(&mut Slot, &mut Ctx) -> (R, bool)) -> R {
    let mut g = lock_set(l);
    let mut ctx = Ctx::new();
    let backlog = l.pending.load(Ordering::Relaxed);
    if backlog {
        // Frames that arrived while the set was busy come first.
        run_set(ctx.shard(), &mut g, None, false, false, Site::SysNoPoll);
    }
    let t = prof::start(prof::on());
    let (r, poll) = f(&mut g, &mut ctx);
    prof::WITH_OP.done(t);
    if poll {
        let mut sh = ctx.take_shard();
        run_set(&mut sh, &mut g, None, false, true, Site::SysPoll);
        let n = refresh(&mut g);
        // Let go of the set but keep the shard for the drain. No frame can
        // reach the set's backlog unnoticed: `pending` is checked below.
        g.serve = false;
        g.g.swap_irq_flags(&mut sh.irq);
        drop(g);
        drop(ctx);
        drain(Site::Drain, SYS_BUDGET, Some(sh), n);
        fence(Ordering::SeqCst);
        service(l, Site::Drain);
        return r;
    }
    drop(ctx);
    let n = refresh(&mut g);
    drop(g);
    fire(n, false);
    if backlog && let Some(o) = OUT.get() {
        // The backlog fed above may have queued replies.
        o.flush();
    }
    r
}

/// `File::poll` of an inet socket: `f` runs under the set lock, after a
/// pending backlog was fed.
pub fn poll_file<R>(l: &SetLock, f: impl FnOnce(&mut Slot) -> R) -> R {
    let mut g = lock_set(l);
    let mut n = Notify::NONE;
    if l.pending.load(Ordering::Relaxed) {
        {
            let mut sh = lock_shard();
            run_set(&mut sh, &mut g, None, false, false, Site::FilePoll);
        }
        if let Some(o) = OUT.get() {
            o.flush();
        }
        n = refresh(&mut g);
    }
    let r = f(&mut g);
    drop(g);
    fire(n, false);
    r
}

/// Lock the stack for a socket being created (no set yet).
pub fn lock_new() -> Held {
    lock_at(Site::SysNoPoll)
}

/// Poll set `l` (egress, settle, timer) and fire its readiness. `popped`:
/// the timer the net task took off for it (its `due` is cleared if it is
/// still that one).
fn poll_set(l: &SetLock, popped: Option<u64>, site: Site) {
    let mut g = lock_set(l);
    if popped.is_some_and(|at| at == g.due) {
        g.due = NEVER;
    }
    {
        let mut sh = lock_shard();
        run_set(&mut sh, &mut g, None, false, true, site);
    }
    let n = refresh(&mut g);
    drop(g);
    fire(n, site == Site::Task);
}

/// Feed frame `f` (for no socket, or a neighbor frame) to this CPU's
/// shard's empty set; a neighbor frame also to the other shards.
fn feed_empty(f: &[u8], neighbor: bool, t: u64) {
    {
        let mut sh = lock_shard();
        let Shard {
            iface, tx, empty, ..
        } = &mut *sh;
        let _ = iface.poll_ingress_single(now(), &mut tx.one_frame(f, false), empty);
    }
    prof::INGRESS.done(t);
    if t != 0 {
        prof::add(&prof::INGRESS_PKTS, 1);
    }
    if neighbor {
        show_neighbor(f);
    }
}

/// Frames taken per shard hold in a drain: between batches the collected
/// readiness changes are fired and the shard is released.
const BATCH: usize = 32;

/// A drain in progress (see [`drain`]).
struct Drain {
    site: Site,
    budget: usize,
    nic_n: usize,
    lo_n: usize,
    /// Readiness changes collected under the current batch.
    notify: Notify,
    /// Sets that got frames on their backlog during the batch.
    serve: Vec<SetRef>,
    /// Neighbor frames to show the other shards after the batch.
    neighbors: Vec<Box<[u8]>>,
    /// Sets to poll (a rescan).
    work: Vec<SetRef>,
}

/// The set a drain batch is feeding: its frames are fed as they come, its
/// egress and settling run once, when the batch moves on ([`Drain::finish`]).
struct Cur {
    /// Declared before `r`: the guard borrows the lock `r` keeps alive.
    g: SetGuard<'static>,
    r: SetRef,
}

impl Cur {
    /// Lock `r` if it is free.
    fn try_new(r: SetRef) -> Option<Cur> {
        // SAFETY: the guard borrows the `SetLock` inside `r`'s allocation,
        // which `r` (dropped after the guard) keeps alive.
        let l: &'static SetLock = unsafe { &*Arc::as_ptr(&r) };
        let mut g = try_lock_set(l)?;
        g.serve = false;
        Some(Cur { g, r })
    }
}

impl Drain {
    /// Poll `c`'s set after its frames (egress, settling, timer), collect
    /// its readiness change, release it; serve its backlog later if frames
    /// went there meanwhile.
    fn finish(&mut self, sh: &mut Shard, c: Option<Cur>) {
        let Some(Cur { mut g, r }) = c else {
            return;
        };
        run_set(sh, &mut g, None, true, false, self.site);
        let n = refresh(&mut g);
        self.notify = core::mem::replace(&mut self.notify, Notify::NONE).and(n);
        drop(g);
        fence(Ordering::SeqCst);
        if r.pending.load(Ordering::Relaxed) && !self.serve.iter().any(|x| Arc::ptr_eq(x, &r)) {
            self.serve.push(r);
        }
    }
}

/// Take received frames — the loopback queue first (up to `LO_BUDGET`),
/// then at most `budget` from the NIC — and feed each to its set (see the
/// module docs), `BATCH` per hold of this CPU's shard (the first hold may
/// be the caller's, `first`; `notify` is the caller's readiness change,
/// fired after the first batch); then poll every set
/// if a rescan was asked for, and take the frames that produced, for at
/// most `MAX_ROUNDS` rounds.
fn drain(site: Site, budget: usize, first: Option<ShardGuard>, notify: Notify) {
    let mut buf = [0u8; MAX_FRAME];
    let mut first = first;
    let mut d = Drain {
        site,
        budget,
        nic_n: 0,
        lo_n: 0,
        notify,
        serve: Vec::new(),
        neighbors: Vec::new(),
        work: Vec::new(),
    };
    let from_task = site == Site::Task;
    for _ in 0..MAX_ROUNDS {
        loop {
            let done = {
                let mut sh = match first.take() {
                    Some(sh) => sh,
                    None => lock_shard(),
                };
                let done = drain_batch(&mut sh, &mut d, &mut buf);
                drop(sh);
                done
            };
            fire(core::mem::replace(&mut d.notify, Notify::NONE), from_task);
            for f in core::mem::take(&mut d.neighbors) {
                show_neighbor(&f);
            }
            if !d.serve.is_empty() {
                fence(Ordering::SeqCst);
                for r in core::mem::take(&mut d.serve) {
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
            poll_set(&r, None, site);
        }
    }
}

/// One batch of a drain, holding this CPU's shard `sh` (see the module
/// docs): take up to `BATCH` frames; a frame for a set whose lock is free
/// (`try_lock`) is fed at once, and so are the frames after it for the
/// same set, its egress and settling waiting until the batch moves on to
/// another set; a frame for a busy set goes on its backlog. Returns true
/// when no frame is left (within the budgets).
fn drain_batch(sh: &mut Shard, d: &mut Drain, buf: &mut [u8; MAX_FRAME]) -> bool {
    let mut cur: Option<Cur> = None;
    let mut done = false;
    for _ in 0..BATCH {
        let mut s = lock_at(d.site);
        let timing = s.prof;
        let t = prof::start(timing);
        let Some((len, nic)) = s
            .out
            .next_frame(d.lo_n < LO_BUDGET, d.nic_n < d.budget, buf)
        else {
            prof::INGRESS_NONE.done(t);
            s.nic_backlog = d.nic_n >= d.budget;
            if s.nic_backlog || s.out.lo_pending() {
                s.kick = true;
            }
            if s.rescan {
                s.rescan = false;
                if timing {
                    prof::add(&prof::RESCANS, 1);
                }
                d.work.extend(s.entries.iter().filter_map(|e| e.set.clone()));
            }
            done = true;
            break;
        };
        if nic {
            d.nic_n += 1;
        } else {
            d.lo_n += 1;
        }
        let f = &buf[..len];
        let id = match s.demux(f) {
            Demux::Set(id) => id,
            Demux::Empty { neighbor } => {
                drop(s);
                let Shard {
                    iface, tx, empty, ..
                } = &mut *sh;
                let _ = iface.poll_ingress_single(now(), &mut tx.one_frame(f, false), empty);
                prof::INGRESS.done(t);
                if timing {
                    prof::add(&prof::INGRESS_PKTS, 1);
                }
                if neighbor {
                    d.neighbors.push(Box::from(f));
                }
                continue;
            }
        };
        let e = &mut s.entries[id];
        let Some(r) = e.set.clone() else {
            continue; // not expected: demux only names registered sets
        };
        if e.backlog.is_empty() {
            if let Some(c) = cur.as_mut()
                && Arc::ptr_eq(&c.r, &r)
            {
                drop(s);
                feed(sh, &mut c.g, f, now());
                continue;
            }
            if let Some(mut c) = Cur::try_new(r.clone()) {
                // Every lock taken under the shard saved masked
                // interrupts, so they may be released in any order.
                drop(s);
                d.finish(sh, cur.take());
                feed(sh, &mut c.g, f, now());
                cur = Some(c);
                continue;
            }
        }
        if e.backlog.len() < MAX_BACKLOG {
            e.backlog.push_back(Box::from(f));
            r.pending.store(true, Ordering::Relaxed);
            if timing {
                prof::add(&prof::BACKLOGGED, 1);
            }
        } else if timing {
            prof::add(&prof::BACKLOG_DROPS, 1);
        }
        drop(s);
        if !d.serve.iter().any(|x| Arc::ptr_eq(x, &r)) {
            d.serve.push(r);
        }
    }
    d.finish(sh, cur.take());
    let out = sh.tx.out();
    out.flush();
    // The last set's egress may have looped frames back.
    done && !(out.lo_pending() && d.lo_n < LO_BUDGET)
}

/// Where a received frame goes.
enum Demux {
    Set(SetId),
    /// No socket's (`neighbor`: ARP / ICMPv6, for every shard).
    Empty { neighbor: bool },
}

impl Stack {
    // -- Registry -----------------------------------------------------------

    /// A registry index for a new set (filled by [`Stack::register`]).
    pub fn reserve_set(&mut self) -> SetId {
        if let Some(i) = self.free_ids.pop() {
            return i;
        }
        self.entries.push(Entry {
            set: None,
            backlog: VecDeque::new(),
        });
        self.entries.len() - 1
    }

    /// Register `slot` (its `id` from [`Stack::reserve_set`]) with its first
    /// poll time `at`. It is reachable by the demux once the flow table or
    /// port map points at it.
    pub fn register(&mut self, mut slot: Slot, at: u64) -> SetRef {
        let id = slot.id;
        self.schedule(&mut slot, at);
        let r = Arc::new(SetLock {
            slot: SpinLock::new(slot),
            pending: AtomicBool::new(false),
        });
        self.entries[id].set = Some(r.clone());
        self.live_sets += 1;
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
        if y.due != NEVER {
            self.timers.remove(&(y.due, id));
            y.due = NEVER;
        }
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

    /// File set `y`'s timer at `at` (replacing its current one).
    fn schedule(&mut self, y: &mut Slot, at: u64) {
        if y.due == at {
            return;
        }
        if y.due != NEVER {
            self.timers.remove(&(y.due, y.id));
        }
        y.due = at;
        if at != NEVER {
            self.timers.insert((at, y.id));
            if at < self.task_deadline {
                self.kick = true;
            }
        }
    }

    /// Apply set `y`'s pending flow-table changes.
    pub fn apply_ops(&mut self, y: &mut Slot) {
        self.apply_flow_ops(y.id, &mut y.ops);
    }

    /// Apply flow-table changes `ops` of set `i`.
    pub fn apply_flow_ops(&mut self, i: SetId, ops: &mut Vec<FlowOp>) {
        for op in ops.drain(..) {
            match op {
                FlowOp::Insert(f) => {
                    self.flows.insert(f, i);
                }
                FlowOp::Remove(f) => flow_remove(&mut self.flows, f, i),
            }
        }
    }

    /// Take set `i`'s backlog (its holder feeds it).
    fn take_backlog(&mut self, i: SetId) -> VecDeque<Box<[u8]>> {
        let e = &mut self.entries[i];
        if let Some(r) = &e.set {
            r.pending.store(false, Ordering::Relaxed);
        }
        core::mem::take(&mut e.backlog)
    }

    /// Feed set `y`'s backlog through `sh` (set lock, shard and stack lock
    /// held), e.g. before moving a socket out of it; flow changes are
    /// applied. Its replies are sent by the next egress of `y`.
    pub fn feed_backlog(&mut self, sh: &mut Shard, y: &mut Slot) {
        if matches!(y.owner, Owner::Free) {
            return;
        }
        let q = self.take_backlog(y.id);
        let now = now();
        for f in &q {
            feed(sh, y, f, now);
        }
        self.apply_ops(y);
    }

    /// The first poll time of a new set: `poll_delay` through `sh`.
    pub fn first_poll(sh: &mut Shard, set: &SocketSet<'static>) -> u64 {
        let now_ns = crate::arch::timer::now_ns();
        match sh.iface.poll_delay(instant(now_ns), set) {
            Some(d) => now_ns.saturating_add(d.total_micros().saturating_mul(1000)),
            None => NEVER,
        }
    }

    /// When the stack wants to be polled next (ns; [`NEVER`]: no timer).
    pub fn earliest(&self) -> u64 {
        if self.out.lo_pending() || self.rescan || self.nic_backlog || !self.dirty.is_empty() {
            return crate::arch::timer::now_ns();
        }
        self.timers.first().map_or(NEVER, |&(t, _)| t)
    }

    /// The net task's share: the dirty sets, the sets whose timer is due
    /// (with the time they were filed under), every set when a rescan was
    /// asked for.
    fn collect_due(&mut self, work: &mut Vec<(SetRef, Option<u64>)>) {
        let now_ns = crate::arch::timer::now_ns();
        while let Some(i) = self.dirty.pop() {
            if let Some(r) = &self.entries[i].set {
                work.push((r.clone(), None));
            }
        }
        while let Some(&(at, i)) = self.timers.first() {
            if at > now_ns {
                break;
            }
            self.timers.pop_first();
            if let Some(r) = &self.entries[i].set {
                work.push((r.clone(), Some(at)));
            }
        }
        if self.rescan {
            self.rescan = false;
            if self.prof {
                prof::add(&prof::RESCANS, 1);
            }
            work.extend(
                self.entries
                    .iter()
                    .filter_map(|e| e.set.clone().map(|r| (r, None))),
            );
        }
    }

    /// Where the received frame `f` goes (see the module docs).
    fn demux(&mut self, f: &[u8]) -> Demux {
        let timing = self.prof;
        let f = match demux::parse(f) {
            Parsed::Transport(f) => f,
            Parsed::Neighbor => {
                // The interfaces may learn a neighbor some set waits for.
                self.rescan = true;
                if timing {
                    prof::add(&prof::DEMUX_OTHER, 1);
                }
                return Demux::Empty { neighbor: true };
            }
            Parsed::Other => {
                if timing {
                    prof::add(&prof::DEMUX_OTHER, 1);
                }
                return Demux::Empty { neighbor: false };
            }
        };
        if let Some(&i) = self.flows.get(&f) {
            if timing {
                prof::add(&prof::DEMUX_EXACT, 1);
            }
            return Demux::Set(i);
        }
        if f.proto == demux::IPPROTO_UDP
            && f.lport == 68
            && f.rport == 67
            && let Some(d) = self.dhcp
        {
            if timing {
                prof::add(&prof::DEMUX_PORT, 1);
            }
            return Demux::Set(d);
        }
        // A UDP datagram to an address that is not ours can only be a
        // broadcast or multicast (smoltcp drops the rest): any socket on
        // the port takes it, as smoltcp's `accepts` does.
        let any = f.proto == demux::IPPROTO_UDP && !self.is_local(f.local);
        match self.bound.pick(f.proto, f.lport, f.local, any) {
            Some(i) => {
                if timing {
                    prof::add(&prof::DEMUX_PORT, 1);
                }
                Demux::Set(i)
            }
            None => {
                if timing {
                    prof::add(&prof::DEMUX_MISS, 1);
                }
                Demux::Empty { neighbor: false }
            }
        }
    }

    // -- Configuration --------------------------------------------------------

    /// A new generation of addresses / routes for the shards.
    fn bump_gen(&mut self) {
        self.cfg_gen += 1;
        CFG_GEN.store(self.cfg_gen, Ordering::Release);
    }

    /// Replace the (non-loopback) IPv4 address and the default route.
    fn set_v4(&mut self, addr: Option<(Ipv4Addr, u8)>, gw: Option<Ipv4Addr>) {
        self.addrs
            .retain(|c| !matches!(c, IpCidr::Ipv4(c) if !c.address().is_loopback()));
        if let Some((ip, p)) = addr
            && self.addrs.len() < MAX_ADDRS
        {
            self.addrs.push(IpCidr::Ipv4(Ipv4Cidr::new(ip, p)));
        }
        self.gw4 = gw;
        self.bump_gen();
        // Sockets whose source address went away are reset by smoltcp
        // when they are next polled: poll every set.
        self.rescan = true;
        self.kick = true;
    }

    /// Add an address (no-op when the table is full).
    fn add_addr(&mut self, cidr: IpCidr) {
        if !self.addrs.contains(&cidr) && self.addrs.len() < MAX_ADDRS {
            self.addrs.push(cidr);
            self.bump_gen();
        }
    }

    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Is `ip` one of the interface's addresses? Only 127.0.0.1 of 127/8:
    /// smoltcp accepts packets for its exact addresses, so binding or
    /// connecting to 127.0.0.2 fails at once (`EADDRNOTAVAIL` /
    /// `ENETUNREACH`) instead of hanging.
    pub fn is_local(&self, ip: IpAddr) -> bool {
        self.addrs.iter().any(|c| IpAddr::from(c.address()) == ip)
    }

    /// Can we send to `ip` at all (a local address, a directly connected
    /// subnet or a default route, on an interface that exists)?
    pub fn routable(&self, ip: IpAddr) -> bool {
        if self.is_local(ip) {
            return true;
        }
        if !self.out.has_nic() {
            return false;
        }
        match ip {
            IpAddr::V4(a) => {
                self.gw4.is_some()
                    || self.addrs.iter().any(|c| match c {
                        IpCidr::Ipv4(c) => !c.address().is_loopback() && c.contains_addr(&a),
                        _ => false,
                    })
            }
            // No IPv6 default route is ever configured.
            IpAddr::V6(a) => (a.segments()[0] & 0xffc0) == 0xfe80,
        }
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
    if OUT.set(Out::new(nic, mac)).is_err() {
        return;
    }
    let out = OUT.get().unwrap();

    let mut seed = [0u8; 8];
    crate::random::fill(&mut seed);
    let seed = u64::from_ne_bytes(seed);

    let mut s = Stack {
        out,
        entries: Vec::new(),
        free_ids: Vec::new(),
        live_sets: 0,
        nsocks: 0,
        flows: BTreeMap::new(),
        bound: PortMap::new(),
        dirty: Vec::new(),
        timers: BTreeSet::new(),
        rescan: false,
        nic_backlog: false,
        kick: false,
        ports: PortTable::new(seed.rotate_left(17)),
        next_id: 0,
        addrs: Vec::new(),
        gw4: None,
        cfg_gen: 0,
        task_deadline: 0,
        dhcp: None,
        dhcp_t0: 0,
        cfg: NetConfig::default(),
        static_dns: Vec::new(),
        prof: false,
    };
    s.add_addr(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Addr::LOCALHOST, 8)));
    s.add_addr(IpCidr::Ipv6(Ipv6Cidr::new(Ipv6Addr::LOCALHOST, 128)));
    if out.has_nic() {
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
            if let Some(gw) = ip.gateway {
                s.gw4 = Some(gw);
                s.cfg.gateway = Some(gw);
                s.bump_gen();
            }
        } else if ip.autoconf.wants_dhcp()
            || (ip.client.is_none() && ip.autoconf == patina_cmdline::Autoconf::Unspecified)
        {
            dhcp = true;
        }
    }

    // The boot line: hardware address, addresses, gateway.
    let mut addrs = String::new();
    for c in &s.addrs {
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

    if let Some(ev) = out.nic_event() {
        let _ = EVENT.set(ev);
    }
    if dhcp && out.has_nic() {
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
        let _ = s.register(Slot::new(id, set, Owner::Dhcp(h)), NEVER);
        s.dirty.push(id);
        s.dhcp = Some(id);
        s.dhcp_t0 = crate::arch::timer::now_ns();
    }
    DHCP_REQUESTED.store(dhcp, Ordering::Relaxed);
    prof::T_INIT.store(crate::arch::timer::now_ns(), Ordering::Relaxed);
    CFG_GEN.store(s.cfg_gen, Ordering::Release);
    if STACK.set(SpinLock::new(s)).is_err() {
        return;
    }
    crate::drivers::spawn(task);
}

/// One round of the net task's work at `site`: poll the dirty and due sets
/// (rescanning if asked), then drain with the large budget. Returns
/// whether stats are on.
fn round(site: Site, work: &mut Vec<(SetRef, Option<u64>)>) -> bool {
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
    {
        let t = prof::start(timing);
        let mut sh = lock_shard();
        sh.iface.poll_maintenance(now());
        prof::MAINT.done(t);
    }
    for (r, at) in work.drain(..) {
        poll_set(&r, at, site);
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
/// header line and its totals are the stack lock's; the shards and the set
/// locks have their own `lock iface-shard` / `lock set-lock` lines.
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
    let regs = &prof::SITES[..prof::STACK_SITES];
    let total_hold: u64 = regs.iter().map(|s| get(&s.hold_ns)).sum();
    let total_acq: u64 = regs.iter().map(|s| get(&s.acq)).sum();
    let total_cont: u64 = regs.iter().map(|s| get(&s.contended)).sum();
    let total_spin: u64 = regs.iter().map(|s| get(&s.spin_ns)).sum();
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
    let (socks_now, set_now, flows_now) = match lock.try_lock() {
        Some(s) => (s.nsocks as u64, s.live_sets as u64, s.flows.len() as u64),
        None => (0, 0, 0),
    };
    let shards = SHARDS.iter().filter(|c| c.get().is_some()).count();
    p(format_args!(
        "[patina] stats net-stack demux: exact {}, port {}, no socket {}, not transport {}, rescans {}, sets egressed {}, flows now {}, backlogged {}, backlog drops {}, shards {}\n",
        get(&prof::DEMUX_EXACT),
        get(&prof::DEMUX_PORT),
        get(&prof::DEMUX_MISS),
        get(&prof::DEMUX_OTHER),
        get(&prof::RESCANS),
        get(&prof::SETS_EGRESSED),
        flows_now,
        get(&prof::BACKLOGGED),
        get(&prof::BACKLOG_DROPS),
        shards,
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
        MuxStats(OUT.get()),
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

struct MuxStats(Option<&'static Out>);

impl core::fmt::Display for MuxStats {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Some(o) = self.0 else {
            return f.write_str("(none)");
        };
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let s = &o.stats;
        write!(
            f,
            "nic tx {} rx {} tx-drops {} tx-waits {}, lo frames {} drops {} overflow {}",
            g(&s.nic_tx),
            g(&s.nic_rx),
            g(&s.nic_tx_drops),
            g(&s.nic_tx_waits),
            g(&s.lo_frames),
            g(&s.lo_drops),
            g(&s.lo_overflow),
        )
    }
}
