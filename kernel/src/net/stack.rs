//! The TCP/IP stack (design §9, D67): one smoltcp `Interface` over the
//! [`Mux`] devices (NIC + loopback), one smoltcp `SocketSet` per socket,
//! the per-socket state of every inet socket (`inet::Sock`), the flow
//! table, the bind table and the per-set poll timers, all behind one
//! IRQ-safe `SpinLock<Stack>`.
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
//! `add`). Owners: [`Owner`].
//!
//! **Ingress demux.** Frames are taken one at a time (loopback queue
//! first, then the NIC: at most `MAX_INGRESS` NIC frames per pass) and their
//! transport tuple read (`patina_inet::demux::parse`). The tuple is looked
//! up (a) exactly in [`Stack::flows`] — connections (from `connect` on,
//! including zombies) and a listener's not-yet-accepted connections
//! (pointing at the listener's set until `accept` re-points them); (b)
//! else by local port in [`Stack::bound`] — listeners and bound UDP sockets,
//! an exact local address before a wildcard, only sockets that cover the
//! packet's family; UDP 67 → 68 goes to the DHCP client; (c) else the
//! frame goes to an empty set: smoltcp still answers ARP, ICMP echo and
//! NDISC at the interface level, a TCP segment for no socket gets a RST and
//! a UDP datagram an ICMP port unreachable, as before. The set gets the
//! frame through a one-frame device (`mux::OneFrame`).
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
//! still in `TimeWait` reaches that socket).
//!
//! **A poll pass** ([`Stack::pass`]) runs `poll_egress` on the sets that
//! may have something to send (the syscall's own set, sets changed by an
//! operation, sets whose timer is due), then takes received frames, feeds
//! each to its set and queues that set for egress, and repeats (at most
//! `MAX_ROUNDS` rounds) until nothing is queued and the loopback queue is
//! empty. So a write is in the peer's receive buffer (over loopback)
//! before the syscall returns, and the peer's ACK is processed too. Then
//! the NIC's deferred doorbells are flushed (`Mux::flush`) and every set
//! the pass touched is settled: `inet` turns smoltcp state transitions of
//! its sockets into socket-API events (`track`), refills a listener's pool,
//! recomputes readiness (`Sock::refresh`) and the set's next poll time
//! (`poll_delay`, plus socket-level deadlines: connect timeout, deferred
//! keep-alive, zombie linger). The earliest poll time per set lives in
//! `timers`. ARP and ICMPv6 frames may teach the interface a neighbor that
//! some set is waiting for, and a DHCP lease change may invalidate source
//! addresses: those (rare) events queue every set once.
//!
//! Who polls:
//!
//! * the **net task** (a kernel thread): a pass over the due sets and the
//!   received frames, fire readiness changes, then sleep on an `Event`
//!   until the earliest timer (at most 1 s). The event is the NIC's (its
//!   IRQ handler signals it and never takes the stack lock) or a
//!   stack-owned one without a NIC;
//! * every **socket syscall** that changes state (send, recv that read
//!   data, connect, listen, accept, close, ...): a pass for its own set
//!   right after the change, under the same lock hold. If that leaves an
//!   earlier timer than the one the task sleeps on, it signals the event.
//!
//! Readiness notifications are collected under the lock and fired after
//! it is released (fs/file.rs lock order).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, Ipv4Cidr, Ipv6Cidr};

use super::inet::Sock;
use super::mux::Mux;
use super::prof::{self, Site};
use crate::drivers::Event;
use crate::fs::file::Notify;
use crate::sync::{OnceCell, SpinLock, SpinLockGuard};
use core::sync::atomic::{AtomicBool, Ordering};
use patina_inet::PortTable;
use patina_inet::demux::{self, Flow, Parsed, PortMap};

/// Longest the net task sleeps between polls.
const MAX_WAIT_NS: u64 = 1_000_000_000;
/// Egress/ingress rounds per [`Stack::pass`].
const MAX_ROUNDS: usize = 64;
/// Frames ingested per round (bounds the time spent under the lock).
const MAX_INGRESS: usize = 1024;
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
    get().map_or_else(NetConfig::default, |l| l.lock().cfg.clone())
}

/// Index of a [`Slot`] (a socket set) in [`Stack::slots`].
pub type SetId = usize;

/// Who a socket set belongs to.
pub enum Owner {
    /// Unused slot (on the free list).
    Free,
    /// The inet socket with this id (`inet::Sock::set` is this slot).
    Sock(u64),
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

/// A socket set and its bookkeeping.
pub struct Slot {
    pub set: SocketSet<'static>,
    pub owner: Owner,
    /// A TCP listener's set (its pool is refilled after every frame fed
    /// to it).
    pub listen: bool,
    /// Poll time this set is filed under in `timers` ([`NEVER`]: none).
    due: u64,
    /// Round / pass in which this set was last queued for egress /
    /// settling (dedups the queues).
    q_mark: u64,
    a_mark: u64,
}

pub struct Stack {
    pub iface: Interface,
    pub mux: Mux,
    /// Socket sets, by [`SetId`].
    pub slots: Vec<Slot>,
    free_slots: Vec<SetId>,
    live_sets: usize,
    /// Exact transport tuples → the set holding that TCP socket (see the
    /// module docs for the invariant).
    pub flows: BTreeMap<Flow, SetId>,
    /// Listeners and bound UDP sockets by local port → their set.
    pub bound: PortMap<SetId>,
    /// Sets changed by an operation on another socket (a connection made
    /// by `accept`, zombies made by `close`): queued by the next pass.
    pub dirty: Vec<SetId>,
    /// Frames for no socket are fed to this (always empty) set.
    empty: SocketSet<'static>,
    /// (poll time ns, set): every set's earliest poll time.
    timers: BTreeSet<(u64, SetId)>,
    /// Queue every live set in the next round (a neighbor may have been
    /// learned, or the addresses changed).
    rescan: bool,
    /// The last pass stopped taking NIC frames at its budget.
    nic_backlog: bool,
    round: u64,
    pass_no: u64,
    egress_q: Vec<SetId>,
    after_q: Vec<SetId>,
    pub ports: PortTable,
    /// Per-socket state, by socket id (also the bind-table owner id).
    pub socks: BTreeMap<u64, Sock>,
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

/// The stack, once [`init`] ran.
pub fn get() -> Option<&'static SpinLock<Stack>> {
    STACK.get()
}

fn event() -> &'static Event {
    EVENT.get().copied().unwrap_or(&LOCAL_EVENT)
}

/// Run `f` on the stack for socket `id`; then, if `poll`, run a poll pass
/// for that socket's set ([`Stack::pass`]), else just recompute its
/// readiness and poll time. Readiness changes are fired after the lock is
/// released, and the net task is woken if it should poll earlier than
/// planned.
pub fn with<R>(poll: bool, id: u64, f: impl FnOnce(&mut Stack) -> R) -> R {
    with_cond(id, |s| (f(s), poll))
}

/// [`with`], where `f` decides whether to poll (a `recv` that read
/// nothing changed nothing).
pub fn with_cond<R>(id: u64, f: impl FnOnce(&mut Stack) -> (R, bool)) -> R {
    let lock = get().expect("net: stack not initialised");
    let (r, notify, kick, timing) = {
        let mut s = lock_at(lock, Site::SysNoPoll);
        let timing = s.prof;
        let t = prof::start(timing);
        let (r, poll) = f(&mut s);
        prof::WITH_OP.done(t);
        let origin = s.socks.get(&id).map(|x| x.set);
        let n = if poll {
            s.site = Site::SysPoll;
            s.pass(origin, false)
        } else {
            s.settle(origin)
        };
        let kick = s.earliest() < s.task_deadline;
        (r, n, kick, timing)
    };
    fire(notify, timing, false);
    if kick {
        if timing {
            prof::add(&prof::KICKS, 1);
        }
        event().signal();
    }
    r
}

/// The stack lock, held for `site`. With `patina.stats=1` the
/// acquisition (contention, spin time) and the hold time are counted per
/// site (`net::prof`), under the site the holder ends up as.
pub struct Held<'a> {
    g: SpinLockGuard<'a, Stack>,
    pub site: Site,
    t0: u64,
    /// Spin wait of a contended acquisition (ns), if it was contended.
    spin: Option<u64>,
}

impl core::ops::Deref for Held<'_> {
    type Target = Stack;
    fn deref(&self) -> &Stack {
        &self.g
    }
}

impl core::ops::DerefMut for Held<'_> {
    fn deref_mut(&mut self) -> &mut Stack {
        &mut self.g
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.t0 != 0 {
            let st = &prof::SITES[self.site as usize];
            prof::add(&st.acq, 1);
            if let Some(ns) = self.spin {
                st.record_spin(ns);
            }
            st.record_hold(prof::now().saturating_sub(self.t0));
        }
    }
}

/// Take the stack lock for `site` (every taker goes through here).
pub fn lock_at(lock: &SpinLock<Stack>, site: Site) -> Held<'_> {
    if !prof::on() {
        let mut g = lock.lock();
        g.prof = false;
        return Held {
            g,
            site,
            t0: 0,
            spin: None,
        };
    }
    let (mut g, spin) = match lock.try_lock() {
        Some(g) => (g, None),
        None => {
            let t = prof::now();
            let g = lock.lock();
            (g, Some(prof::now().saturating_sub(t)))
        }
    };
    g.prof = true;
    if let Some(c) = prof::LOCK_CPU.get(crate::arch::cpu::id()) {
        prof::add(c, 1);
    }
    let t0 = prof::now();
    let _ = prof::T_FIRST.compare_exchange(0, t0, Ordering::Relaxed, Ordering::Relaxed);
    Held { g, site, t0, spin }
}

/// Fire `n`; with stats on and at least one hook, count it as from the
/// net task or from a syscall.
fn fire(n: Notify, timing: bool, from_task: bool) {
    let hooks = n.hooks() as u64;
    if !timing || hooks == 0 {
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

impl Stack {
    // -- Socket sets ------------------------------------------------------

    /// A new, empty socket set for `owner`.
    pub fn alloc_set(&mut self, owner: Owner) -> SetId {
        self.live_sets += 1;
        if let Some(i) = self.free_slots.pop() {
            let s = &mut self.slots[i];
            s.owner = owner;
            s.listen = false;
            return i;
        }
        self.slots.push(Slot {
            set: SocketSet::new(Vec::new()),
            owner,
            listen: false,
            due: NEVER,
            q_mark: 0,
            a_mark: 0,
        });
        self.slots.len() - 1
    }

    /// Release set `i`; its sockets must have been removed already.
    pub fn free_set(&mut self, i: SetId) {
        if matches!(self.slots[i].owner, Owner::Free) {
            return;
        }
        self.unschedule(i);
        let s = &mut self.slots[i];
        s.owner = Owner::Free;
        s.listen = false;
        if s.set.iter().next().is_some() {
            // Not expected: drop whatever is left rather than hand it to
            // the next owner.
            s.set = SocketSet::new(Vec::new());
        }
        self.free_slots.push(i);
        self.live_sets -= 1;
    }

    /// The socket set `i`.
    pub fn set(&mut self, i: SetId) -> &mut SocketSet<'static> {
        &mut self.slots[i].set
    }

    /// Point the flow table's entry for `f` at set `i`.
    pub fn flow_insert(&mut self, f: Flow, i: SetId) {
        self.flows.insert(f, i);
    }

    /// Remove the entry for `f` if it still points at set `i`.
    pub fn flow_remove(&mut self, f: Flow, i: SetId) {
        flow_remove(&mut self.flows, f, i);
    }

    fn unschedule(&mut self, i: SetId) {
        let d = self.slots[i].due;
        if d != NEVER {
            self.timers.remove(&(d, i));
            self.slots[i].due = NEVER;
        }
    }

    fn schedule(&mut self, i: SetId, at: u64) {
        if self.slots[i].due == at {
            return;
        }
        self.unschedule(i);
        if at != NEVER {
            self.timers.insert((at, i));
            self.slots[i].due = at;
        }
    }

    /// When the stack wants to be polled next (ns; [`NEVER`]: no timer).
    pub fn earliest(&self) -> u64 {
        if self.mux.lo_pending() || self.rescan || self.nic_backlog || !self.dirty.is_empty() {
            return crate::arch::timer::now_ns();
        }
        self.timers.first().map_or(NEVER, |&(t, _)| t)
    }

    /// Queue set `i` for egress in the current round.
    fn enqueue(&mut self, i: SetId) {
        let s = &mut self.slots[i];
        if s.q_mark != self.round {
            s.q_mark = self.round;
            self.egress_q.push(i);
        }
    }

    /// Queue set `i` for settling at the end of the pass.
    fn touch(&mut self, i: SetId) {
        let s = &mut self.slots[i];
        if s.a_mark != self.pass_no {
            s.a_mark = self.pass_no;
            self.after_q.push(i);
        }
    }

    // -- Poll passes ------------------------------------------------------

    /// One poll pass (see the module docs): egress for `origin`, the dirty
    /// sets and (if `due`) the sets whose timer expired; then received
    /// frames, each fed to its set, whose egress follows in the next round;
    /// then flush the NIC and settle every set touched. Returns the
    /// readiness notifications to fire.
    pub fn pass(&mut self, origin: Option<SetId>, due: bool) -> Notify {
        let timing = self.prof;
        let t_all = prof::start(timing);
        let now = now();
        let now_ns = crate::arch::timer::now_ns();
        let t = prof::start(timing);
        self.iface.poll_maintenance(now);
        prof::MAINT.done(t);
        self.pass_no += 1;
        self.round += 1;
        if let Some(o) = origin {
            self.enqueue(o);
        }
        while let Some(i) = self.dirty.pop() {
            if !matches!(self.slots[i].owner, Owner::Free) {
                self.enqueue(i);
            }
        }
        if due {
            while let Some(&(at, i)) = self.timers.first() {
                if at > now_ns {
                    break;
                }
                self.timers.pop_first();
                self.slots[i].due = NEVER;
                self.enqueue(i);
            }
        }
        if self.rescan {
            self.rescan_all();
        }
        for round in 0..MAX_ROUNDS {
            let mut q = core::mem::take(&mut self.egress_q);
            self.round += 1;
            for &i in &q {
                self.egress(i, now);
                self.touch(i);
            }
            if timing {
                prof::add(&prof::SETS_EGRESSED, q.len() as u64);
            }
            q.clear();
            self.egress_q = q;
            // NIC frames only in the first round (at most MAX_INGRESS per
            // pass): later rounds only carry the loopback replies, so a
            // steady stream from the NIC cannot keep one pass going.
            let nic = round == 0;
            let mut n = 0;
            while n < MAX_INGRESS {
                let t = prof::start(timing);
                let Some(len) = self.mux.next_frame(nic) else {
                    prof::INGRESS_NONE.done(t);
                    break;
                };
                n += 1;
                self.ingest(len, now, t);
            }
            if nic {
                // Budget used up: frames may be left for the next pass.
                self.nic_backlog = n == MAX_INGRESS;
            }
            if self.rescan {
                self.rescan_all();
            }
            if self.egress_q.is_empty() && !self.mux.lo_pending() {
                break;
            }
        }
        // Deferred NIC doorbells (receive buffers reposted, frames queued).
        self.mux.flush();
        let mut notify = Notify::NONE;
        let mut q = core::mem::take(&mut self.after_q);
        for &i in &q {
            notify = notify.and(self.after(i, now, now_ns, true));
        }
        q.clear();
        self.after_q = q;
        if timing {
            let n = self.socks.len() as u64;
            prof::max(&prof::MAX_SOCKS, n);
            prof::max(&prof::MAX_SET, self.live_sets as u64);
            if prof::get(&prof::POLL_ALL.calls).is_multiple_of(prof::SET_SAMPLE) {
                prof::add(&prof::SET_SUM, self.live_sets as u64);
                prof::add(&prof::SOCKS_SUM, n);
                prof::add(&prof::SET_SAMPLES, 1);
            }
        }
        prof::POLL_ALL.done(t_all);
        notify
    }

    /// Queue every live set (rare: see [`Stack::rescan`]).
    fn rescan_all(&mut self) {
        self.rescan = false;
        if self.prof {
            prof::add(&prof::RESCANS, 1);
        }
        for i in 0..self.slots.len() {
            if !matches!(self.slots[i].owner, Owner::Free) {
                self.enqueue(i);
            }
        }
    }

    /// `poll_egress` on set `i` until it has nothing more to send.
    fn egress(&mut self, i: SetId, now: Instant) {
        let timing = self.prof;
        loop {
            let t = prof::start(timing);
            let r = self
                .iface
                .poll_egress(now, &mut self.mux, &mut self.slots[i].set);
            if r == PollResult::None {
                prof::EGRESS_NONE.done(t);
                break;
            }
            prof::EGRESS.done(t);
            if timing {
                prof::add(&prof::EGRESS_SOME, 1);
            }
        }
    }

    /// Feed the received frame (`len` bytes in the mux's buffer) to its
    /// set; `t` is when taking it started (stats).
    fn ingest(&mut self, len: usize, now: Instant, t: u64) {
        let timing = self.prof;
        let target = self.demux(len);
        let Stack {
            iface,
            mux,
            slots,
            empty,
            ..
        } = self;
        let set = match target {
            Some(i) => &mut slots[i].set,
            None => empty,
        };
        let _ = iface.poll_ingress_single(now, &mut mux.one_frame(len), set);
        prof::INGRESS.done(t);
        if timing {
            prof::add(&prof::INGRESS_PKTS, 1);
        }
        let Some(i) = target else {
            return;
        };
        self.enqueue(i);
        self.touch(i);
        if self.slots[i].listen
            && let Owner::Sock(id) = self.slots[i].owner
        {
            // A burst of SYNs must each find a listening socket.
            let t = prof::start(timing);
            self.refill(id);
            prof::REFILL.done(t);
        }
    }

    /// The set the received frame (`len` bytes) belongs to; `None`: no
    /// socket's (see the module docs).
    fn demux(&mut self, len: usize) -> Option<SetId> {
        let timing = self.prof;
        let f = match demux::parse(self.mux.frame(len)) {
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

    /// After a pass touched set `i` (or, `track` false, after a syscall
    /// that did not poll): socket bookkeeping, readiness, the set's next
    /// poll time.
    fn after(&mut self, i: SetId, now: Instant, now_ns: u64, track: bool) -> Notify {
        let (n, own) = match self.slots[i].owner {
            Owner::Free => return Notify::NONE,
            Owner::Sock(id) => self.settle_sock(id, i, now_ns, track),
            Owner::Zombie { .. } => match self.reap(i, now_ns) {
                Some(d) => (Notify::NONE, d),
                None => return Notify::NONE,
            },
            Owner::Dhcp(_) => {
                let t = prof::start(self.prof);
                self.dhcp_events();
                prof::DHCP_EV.done(t);
                (Notify::NONE, None)
            }
        };
        let t = prof::start(self.prof);
        let at = match self.iface.poll_delay(now, &self.slots[i].set) {
            Some(d) => now_ns.saturating_add(d.total_micros().saturating_mul(1000)),
            None => NEVER,
        };
        prof::DEADLINE.done(t);
        self.schedule(i, own.map_or(at, |o| o.min(at)));
        n
    }

    /// A syscall that did not poll: settle its own set and any dirty ones
    /// (readiness and poll time only).
    fn settle(&mut self, origin: Option<SetId>) -> Notify {
        let now = now();
        let now_ns = crate::arch::timer::now_ns();
        let mut n = Notify::NONE;
        if let Some(o) = origin {
            n = self.after(o, now, now_ns, false);
        }
        n
    }

    /// Apply a DHCP client event (lease acquired, renewed with changes, or
    /// lost). Runs whenever the client's set was polled.
    fn dhcp_events(&mut self) {
        let Some(i) = self.dhcp else {
            return;
        };
        let Owner::Dhcp(h) = self.slots[i].owner else {
            return;
        };
        let lease = match self.slots[i].set.get_mut::<dhcpv4::Socket>(h).poll() {
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
                    // This runs while a pass settles its sets, after the
                    // pass's own flush: ring the doorbell for the request
                    // now rather than at the next poll (up to 1 s away).
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
        slots: Vec::new(),
        free_slots: Vec::new(),
        live_sets: 0,
        flows: BTreeMap::new(),
        bound: PortMap::new(),
        dirty: Vec::new(),
        empty: SocketSet::new(Vec::new()),
        timers: BTreeSet::new(),
        rescan: false,
        nic_backlog: false,
        round: 0,
        pass_no: 0,
        egress_q: Vec::new(),
        after_q: Vec::new(),
        ports: PortTable::new(seed.rotate_left(17)),
        socks: BTreeMap::new(),
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
        let i = s.alloc_set(Owner::Free);
        let h = s.set(i).add(sock);
        s.slots[i].owner = Owner::Dhcp(h);
        s.dirty.push(i);
        s.dhcp = Some(i);
        s.dhcp_t0 = crate::arch::timer::now_ns();
    }
    DHCP_REQUESTED.store(dhcp, Ordering::Relaxed);
    prof::T_INIT.store(crate::arch::timer::now_ns(), Ordering::Relaxed);
    if STACK.set(SpinLock::new(s)).is_err() {
        return;
    }
    crate::drivers::spawn(task);
}

/// `ip=dhcp` (or `ip=` without a client address): wait, at boot and before
/// user mode, until the DHCP client has a lease or [`DHCP_WAIT_NS`] passed.
/// Called from `kernel_main` after `ctl::init`, so a timeout is also
/// reported as a control-channel log event. Interrupts are not needed: the
/// NIC's `recv` polls its used ring. The client socket keeps its set;
/// whoever polls it afterwards (the net task at its timer, or a pass that
/// received a DHCP reply) applies renewals and losses.
pub fn wait_dhcp() {
    if !DHCP_REQUESTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(lock) = get() else {
        return;
    };
    if lock_at(lock, Site::Dhcp).dhcp.is_none() {
        crate::kprintln!("[patina] net: ip=dhcp: no network device, continuing without an address");
        crate::ctl::log_warn(format_args!(
            "net: ip=dhcp: no network device, continuing without an address"
        ));
        return;
    }
    let t0 = crate::arch::timer::now_ns();
    loop {
        let (leased, next) = {
            let mut s = lock_at(lock, Site::Dhcp);
            let _ = s.pass(None, true);
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

/// The net task: a pass over the due sets and received frames, notify,
/// sleep until the earliest set timer (at most 1 s) or an event.
fn task() {
    let lock = get().expect("net: no stack");
    let ev = event();
    prof::NET_TASK_TID.store(crate::task::sched::current().tid, Ordering::Relaxed);
    let mut spins = 0u32;
    loop {
        let (notify, mut deadline, timing) = {
            let mut s = lock_at(lock, Site::Task);
            let timing = s.prof;
            if timing {
                prof::add(&prof::TASK_ROUNDS, 1);
                if let Some(c) = prof::TASK_CPU.get(crate::arch::cpu::id()) {
                    prof::add(c, 1);
                }
            }
            let n = s.pass(None, true);
            let now = crate::arch::timer::now_ns();
            let d = s.earliest().min(now + MAX_WAIT_NS);
            s.task_deadline = d;
            (n, d, timing)
        };
        fire(notify, timing, true);
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
/// counters, a few derived numbers and the stack's current size.
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
    let total_hold: u64 = prof::SITES.iter().map(|s| get(&s.hold_ns)).sum();
    let total_acq: u64 = prof::SITES.iter().map(|s| get(&s.acq)).sum();
    let total_cont: u64 = prof::SITES.iter().map(|s| get(&s.contended)).sum();
    let total_spin: u64 = prof::SITES.iter().map(|s| get(&s.spin_ns)).sum();
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
    ph("fire_task", &prof::FIRE_TASK);
    ph("fire_sys", &prof::FIRE_SYS);
    let (socks_now, set_now, flows_now, mux) = match lock.try_lock() {
        Some(s) => (
            s.socks.len() as u64,
            s.live_sets as u64,
            s.flows.len() as u64,
            Some(s.mux.tx.stats),
        ),
        None => (0, 0, 0, None),
    };
    p(format_args!(
        "[patina] stats net-stack demux: exact {}, port {}, no socket {}, not transport {}, rescans {}, sets egressed {}, flows now {}\n",
        get(&prof::DEMUX_EXACT),
        get(&prof::DEMUX_PORT),
        get(&prof::DEMUX_MISS),
        get(&prof::DEMUX_OTHER),
        get(&prof::RESCANS),
        get(&prof::SETS_EGRESSED),
        flows_now,
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
