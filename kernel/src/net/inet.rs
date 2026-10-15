//! AF_INET / AF_INET6 sockets: TCP and UDP over smoltcp (D68).
//!
//! An [`InetSocket`] is the `File` an fd points at; it holds its socket
//! set (a `stack::Slot` behind its own lock, `stack::SetRef`). The slot
//! holds the socket's state ([`Sock`], as `stack::Owner::Sock`) and the
//! smoltcp sockets it owns in its own smoltcp `SocketSet`, so polling one
//! socket never walks the others (D67) and operations on different sockets
//! run in parallel. Every operation runs under the set lock
//! (`stack::sys`); the interface lock is taken inside, only when the
//! operation touches shared state (the bind table, the port map, the flow
//! table, routes, `iface.context()`) or polls the set:
//!
//! * TCP, idle: an empty set until `connect` or `listen`.
//! * TCP listener: smoltcp has one socket per connection, so a listener's
//!   set holds a small pool of sockets `listen()`ing on its endpoint
//!   (`LISTEN_POOL`) plus a queue of sockets that left `Listen` (a SYN
//!   arrived): the accept queue, bounded by the backlog. The pool is
//!   refilled after every frame fed to the listener ([`refill`]), so
//!   a burst of SYNs each find a listening socket; a socket that leaves
//!   `Listen` gets a flow-table entry pointing at the listener's set.
//!   `accept` takes the first queued socket that is past `SynReceived` and
//!   moves it into the new connection's own set (re-pointing its entry),
//!   after feeding the listener's backlog (frames for its queued sockets
//!   taken while its lock was busy).
//!   The listener is in the stack's port map, which picks among listeners
//!   on one port by address and family.
//! * TCP connection: one smoltcp socket, with a flow-table entry from
//!   `connect` (or `accept`) on. After every poll of its set, [`track`] turns
//!   smoltcp state transitions into socket-API events: `SynSent → Closed`
//!   is ECONNREFUSED, a synchronized state → `Closed` is a reset
//!   (ECONNRESET once, then EOF), 75 s in `SynSent` is ETIMEDOUT.
//! * UDP: one smoltcp socket from `socket()` on, bound on `bind` or on the
//!   first `sendto`/`connect` (ephemeral port), when it joins the port map.
//!
//! Dual stack: an AF_INET6 socket with `IPV6_V6ONLY` = 0 (the default)
//! bound to `::` gets a smoltcp endpoint without an address, which accepts
//! both families; IPv4 peers are shown as `::ffff:a.b.c.d` and `::ffff:`
//! destinations are sent as IPv4. Wildcard endpoints match both families
//! in smoltcp, so the port map hands a packet only to a socket that covers
//! its family (`patina_inet::Cover`: AF_INET sockets take IPv4 only,
//! V6ONLY sockets IPv6 only), and the family filter is still applied on
//! accept and on UDP receive.
//!
//! Closing: UDP sockets go at once; a TCP connection is `close()`d
//! (graceful FIN after the queued data) and its set kept as a zombie
//! (`stack::Owner::Zombie`) until smoltcp reaches `Closed`/`TimeWait` (at
//! most `LINGER_NS`); unread data or `SO_LINGER` {1, 0} abort it with a
//! RST, as on Linux. A closed listener's queued connections are aborted,
//! each moved into a zombie set of its own.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use core::sync::atomic::{AtomicBool, Ordering};

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::fs::S_IFSOCK;
use patina_abi::misc::FIONREAD;
use patina_abi::net::*;
use patina_inet::demux::{Flow, IPPROTO_TCP as P_TCP, IPPROTO_UDP as P_UDP};
use patina_inet::{BindError, Cover, Proto, Reuse, SockAddr};
use smoltcp::iface::{SocketHandle, SocketSet};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Duration;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};

use super::socket::{self as so, RecvInfo, SocketOps};
use super::stack::{self, Iface, Owner, SetId, SetRef, Slot, Stack};
use crate::fs::file::{File, FileFlags, Notify, PollHook, PollList, alloc_ino, anon_stat};
use crate::mm::heap;

pub const TCP_RX_BUF: usize = 64 * 1024;
pub const TCP_TX_BUF: usize = 64 * 1024;
const UDP_PACKETS: usize = 16;
const UDP_PAYLOAD: usize = UDP_PACKETS * 2048;
/// Open inet sockets (beyond: `ENFILE`).
pub const MAX_SOCKETS: usize = 4096;
/// Accept-queue bound (`listen` backlog is clamped to this).
const MAX_BACKLOG: usize = 128;
/// Sockets kept in `Listen` per listener.
const LISTEN_POOL: usize = 4;
const CONNECT_TIMEOUT_NS: u64 = 75_000_000_000;
const LINGER_NS: u64 = 60_000_000_000;
/// `OUT` is reported once this much send space is free (Linux reports
/// writability at a third of the buffer; partial writes still go through
/// below it).
const WRITE_SPACE: usize = TCP_TX_BUF / 4;
const SIOCOUTQ: u32 = 0x5411;

// ---------------------------------------------------------------------------
// Per-socket state (in its set's slot).

pub struct Listener {
    ep: IpListenEndpoint,
    backlog: usize,
    listening: Vec<SocketHandle>,
    /// The accept queue, with each socket's flow-table entry.
    pending: VecDeque<(SocketHandle, Option<Flow>)>,
}

pub struct Conn {
    h: SocketHandle,
    /// The flow-table entry for this connection (while it has a tuple).
    flow: Option<Flow>,
    connecting: bool,
    started: u64,
    last: tcp::State,
    /// Reached a synchronized state at some point.
    est: bool,
    /// Reset (or aborted by a timeout) after being established.
    reset: bool,
    peer: Option<IpEndpoint>,
    local: Option<IpEndpoint>,
    /// `SO_KEEPALIVE` was turned on after the handshake and is not yet
    /// handed to smoltcp: the start (ns) of the idle period the first probe
    /// waits out (see [`apply_live_tcp_opts`]).
    ka_since: Option<u64>,
}

pub enum Tcp {
    Idle,
    Listen(Listener),
    Conn(Conn),
}

pub struct Udp {
    h: SocketHandle,
    peer: Option<IpEndpoint>,
    /// Source address picked at `connect` (what `getsockname` shows).
    local: Option<IpAddr>,
}

pub enum Kind {
    Tcp(Tcp),
    Udp(Udp),
}

#[derive(Clone)]
pub struct Opts {
    reuse: Reuse,
    v6only: bool,
    nodelay: bool,
    keepalive: bool,
    keepidle: i32,
    keepintvl: i32,
    keepcnt: i32,
    user_timeout: i32,
    linger: Linger,
    rcvtimeo: Option<u64>,
    sndtimeo: Option<u64>,
    /// Options accepted and stored without effect: (level, name) → value.
    misc: BTreeMap<(i32, i32), i32>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            reuse: Reuse::default(),
            v6only: false,
            nodelay: false,
            keepalive: false,
            keepidle: 7200,
            keepintvl: 75,
            keepcnt: 9,
            user_timeout: 0,
            linger: Linger::default(),
            rcvtimeo: None,
            sndtimeo: None,
            misc: BTreeMap::new(),
        }
    }
}

pub struct Sock {
    /// Owner id in the bind table (`Stack::ports`).
    id: u64,
    family: u16,
    kind: Kind,
    /// Local binding (address `None`: wildcard).
    bound: Option<(Option<IpAddr>, u16)>,
    opts: Opts,
    /// Pending `SO_ERROR`.
    error: i32,
    shut_rd: bool,
    shut_wr: bool,
    hooks: PollList,
    last: EpollFlags,
    last_rxq: usize,
}

fn tcp_sock<'a>(sockets: &'a SocketSet<'static>, h: SocketHandle) -> &'a tcp::Socket<'static> {
    sockets.get::<tcp::Socket>(h)
}

/// The exact tuple smoltcp matches `t` on, once it has one.
fn flow_of(t: &tcp::Socket) -> Option<Flow> {
    let l = t.local_endpoint()?;
    let r = t.remote_endpoint()?;
    Some(Flow {
        rport: r.port,
        lport: l.port,
        proto: P_TCP,
        remote: ip_of(r.addr),
        local: ip_of(l.addr),
    })
}

/// Heap a TCP socket's buffers take, charged to the program from
/// [`new_tcp`] until the socket leaves the set ([`remove_tcp`], D87).
const TCP_BUFS: usize = heap::alloc_size(TCP_RX_BUF) + heap::alloc_size(TCP_TX_BUF);
/// The same for a UDP socket ([`new_udp`]): two payload rings and two
/// metadata arrays.
const UDP_BUFS: usize = 2
    * (heap::alloc_size(UDP_PAYLOAD)
        + heap::alloc_size(UDP_PACKETS * size_of::<udp::PacketMetadata>()));

/// `n` copies of `v`, allocated fallibly.
fn try_vec<T: Clone>(n: usize, v: T) -> Option<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(n).ok()?;
    out.resize(n, v);
    Some(out)
}

/// A TCP socket with its 2 × 64 KiB buffers, charged to the program
/// (ENOBUFS when the charge or the allocation fails: never a panic). The
/// charge is returned by [`remove_tcp`] / [`discard_tcp`].
fn new_tcp(opts: &Opts) -> Result<tcp::Socket<'static>, Errno> {
    if !heap::charge(TCP_BUFS) {
        return Err(Errno(ENOBUFS));
    }
    let (Some(rx), Some(tx)) = (try_vec(TCP_RX_BUF, 0u8), try_vec(TCP_TX_BUF, 0u8)) else {
        heap::uncharge(TCP_BUFS);
        return Err(Errno(ENOBUFS));
    };
    let mut t = tcp::Socket::new(tcp::SocketBuffer::new(rx), tcp::SocketBuffer::new(tx));
    // Quick ACKs: delayed ACKs only cost latency over loopback.
    t.set_ack_delay(None);
    apply_tcp_opts(opts, &mut t);
    Ok(t)
}

/// Drop a [`new_tcp`] socket that never joined the set.
fn discard_tcp(t: tcp::Socket<'static>) {
    drop(t);
    heap::uncharge(TCP_BUFS);
}

/// Remove a TCP socket from the set (its buffers are freed).
fn remove_tcp(sockets: &mut SocketSet<'static>, h: SocketHandle) {
    sockets.remove(h);
    heap::uncharge(TCP_BUFS);
}

/// The keep-alive interval `o` asks for (`TCP_KEEPIDLE`), if enabled.
fn keep_alive(o: &Opts) -> Option<Duration> {
    o.keepalive
        .then(|| Duration::from_secs(o.keepidle.max(1) as u64))
}

/// Options for a socket that has not started a handshake yet (the smoltcp
/// keep-alive timer is reset by `listen`/`connect` and started by the
/// handshake, so keep-alive can be set directly).
fn apply_tcp_opts(o: &Opts, t: &mut tcp::Socket) {
    t.set_keep_alive(keep_alive(o));
    apply_tcp_common(o, t);
}

/// Options set on a connection (`setsockopt` after `connect`/`accept`).
///
/// Keep-alive cannot simply be handed to smoltcp here: enabling it on an
/// idle synchronized socket arms smoltcp's keep-alive timer at time 0
/// (`Timer::set_keep_alive`), so the next poll sends a probe (one garbage
/// byte at SND.NXT-1) right away. Go turns on `SO_KEEPALIVE` for every
/// accepted and dialed connection, which put that 1-byte segment after
/// every handshake (D85). Linux sends the first probe only after
/// `TCP_KEEPIDLE` s without traffic, so it is deferred: `ka_since` starts
/// the idle period, our send/recv paths restart it, and [`Stack::track`]
/// hands the interval to smoltcp once it has elapsed (smoltcp then probes
/// and rewinds its own timer on every segment).
fn apply_live_tcp_opts(o: &Opts, c: &mut Conn, t: &mut tcp::Socket, now_ns: u64) {
    match keep_alive(o) {
        None => {
            c.ka_since = None;
            t.set_keep_alive(None);
        }
        Some(d)
            if t.keep_alive().is_some()
                || matches!(
                    t.state(),
                    tcp::State::Closed | tcp::State::SynSent | tcp::State::SynReceived
                ) =>
        {
            // Already running (the timer is armed: only the interval
            // changes), or the handshake will start the timer.
            c.ka_since = None;
            t.set_keep_alive(Some(d));
        }
        Some(_) => {
            c.ka_since.get_or_insert(now_ns);
        }
    }
    apply_tcp_common(o, t);
}

fn apply_tcp_common(o: &Opts, t: &mut tcp::Socket) {
    t.set_nagle_enabled(!o.nodelay);
    t.set_timeout(if o.user_timeout > 0 {
        Some(Duration::from_millis(o.user_timeout as u64))
    } else {
        None
    });
}

/// A UDP socket with its buffers, charged like [`new_tcp`] (returned when
/// it is removed in `close_sock`).
fn new_udp() -> Result<udp::Socket<'static>, Errno> {
    if !heap::charge(UDP_BUFS) {
        return Err(Errno(ENOBUFS));
    }
    let bufs = (
        try_vec(UDP_PACKETS, udp::PacketMetadata::EMPTY),
        try_vec(UDP_PAYLOAD, 0u8),
        try_vec(UDP_PACKETS, udp::PacketMetadata::EMPTY),
        try_vec(UDP_PAYLOAD, 0u8),
    );
    let (Some(rm), Some(rp), Some(tm), Some(tp)) = bufs else {
        heap::uncharge(UDP_BUFS);
        return Err(Errno(ENOBUFS));
    };
    Ok(udp::Socket::new(
        udp::PacketBuffer::new(rm, rp),
        udp::PacketBuffer::new(tm, tp),
    ))
}

fn ip_of(a: IpAddress) -> IpAddr {
    a.into()
}

/// Does a connection from `remote` belong on a socket of `family`?
fn family_ok(family: u16, v6only: bool, remote: IpAddr) -> bool {
    match remote {
        IpAddr::V4(_) => family == AF_INET || !v6only,
        IpAddr::V6(_) => family == AF_INET6,
    }
}

impl Sock {
    fn new(id: u64, family: u16, kind: Kind) -> Sock {
        Sock {
            id,
            family,
            kind,
            bound: None,
            opts: Opts::default(),
            error: 0,
            shut_rd: false,
            shut_wr: false,
            hooks: PollList::new(),
            last: EpollFlags::empty(),
            last_rxq: 0,
        }
    }

    fn take_error(&mut self) -> Option<Errno> {
        let e = core::mem::take(&mut self.error);
        (e != 0).then_some(Errno(e))
    }

    /// Current readiness and receive-queue length.
    fn ready(&self, sockets: &SocketSet<'static>) -> (EpollFlags, usize) {
        let mut ev = EpollFlags::empty();
        let mut rxq = 0;
        match &self.kind {
            Kind::Tcp(Tcp::Idle) => {
                ev |= EpollFlags::OUT | EpollFlags::WRNORM | EpollFlags::HUP;
            }
            Kind::Tcp(Tcp::Listen(l)) => {
                if l.pending
                    .iter()
                    .any(|&(h, _)| accept_ready(tcp_sock(sockets, h).state()))
                {
                    ev |= EpollFlags::IN | EpollFlags::RDNORM;
                }
            }
            Kind::Tcp(Tcp::Conn(c)) => {
                use tcp::State::*;
                let t = tcp_sock(sockets, c.h);
                let st = t.state();
                rxq = t.recv_queue();
                let peer_fin = matches!(st, CloseWait | LastAck | Closing | TimeWait)
                    || (st == Closed && c.est);
                let rcv_shut = self.shut_rd || peer_fin;
                let snd_shut = self.shut_wr
                    || matches!(st, FinWait1 | FinWait2 | Closing | LastAck | TimeWait)
                    || (st == Closed && c.est);
                if (rcv_shut && snd_shut) || (st == Closed && !c.connecting) {
                    ev |= EpollFlags::HUP;
                }
                if rcv_shut {
                    ev |= EpollFlags::IN | EpollFlags::RDNORM | EpollFlags::RDHUP;
                }
                if !matches!(st, SynSent | SynReceived) {
                    if rxq > 0 {
                        ev |= EpollFlags::IN | EpollFlags::RDNORM;
                    }
                    if snd_shut
                        || t.send_capacity() - t.send_queue() >= WRITE_SPACE
                        || (st == Closed && !c.est)
                    {
                        ev |= EpollFlags::OUT | EpollFlags::WRNORM;
                    }
                }
            }
            Kind::Udp(u) => {
                let s = sockets.get::<udp::Socket>(u.h);
                if s.can_recv() {
                    ev |= EpollFlags::IN | EpollFlags::RDNORM;
                    rxq = s.recv_queue();
                }
                if s.can_send() {
                    ev |= EpollFlags::OUT | EpollFlags::WRNORM;
                }
                if self.shut_rd {
                    ev |= EpollFlags::IN | EpollFlags::RDNORM | EpollFlags::RDHUP;
                }
                if self.shut_rd && self.shut_wr {
                    ev |= EpollFlags::HUP;
                }
            }
        }
        if self.error != 0 {
            ev |= EpollFlags::ERR;
        }
        (ev, rxq)
    }

    /// Update the remembered readiness; notify hooks of new events (and of
    /// new data even when `IN` was already set, for edge-triggered users).
    pub fn refresh(&mut self, sockets: &SocketSet<'static>) -> Notify {
        let (ev, rxq) = self.ready(sockets);
        let new = ev - self.last;
        let grew = rxq > self.last_rxq;
        self.last = ev;
        self.last_rxq = rxq;
        if (new.is_empty() && !grew) || self.hooks.is_empty() {
            return Notify::NONE;
        }
        let mut what = new;
        if grew {
            what |= EpollFlags::IN | EpollFlags::RDNORM;
        }
        self.hooks.collect(what)
    }
}

impl Sock {
    /// Data moved through send/recv: restart a deferred keep-alive's idle
    /// period (Linux counts from the last segment received; this counts
    /// from the last successful read or write).
    fn touch(&mut self) {
        if let Kind::Tcp(Tcp::Conn(Conn {
            ka_since: Some(since),
            ..
        })) = &mut self.kind
        {
            *since = crate::arch::timer::now_ns();
        }
    }
}

fn accept_ready(st: tcp::State) -> bool {
    !matches!(
        st,
        tcp::State::Listen | tcp::State::SynReceived | tcp::State::SynSent | tcp::State::Closed
    )
}

// ---------------------------------------------------------------------------
// Bookkeeping under the socket's set lock (and the interface lock where it
// touches shared state: the flow table, the port map, the bind table).

impl Sock {
    /// Socket-level deadlines (ns) the set's `poll_delay` does not know:
    /// the connect timeout and the deferred keep-alive hand-off (`track`).
    pub fn own_deadline(&self) -> Option<u64> {
        let Kind::Tcp(Tcp::Conn(c)) = &self.kind else {
            return None;
        };
        let mut d = c
            .connecting
            .then(|| c.started.saturating_add(CONNECT_TIMEOUT_NS + 1));
        if let (Some(since), Some(k)) = (c.ka_since, keep_alive(&self.opts)) {
            let at = since.saturating_add(k.total_micros().saturating_mul(1000));
            d = Some(d.map_or(at, |x| x.min(at)));
        }
        d
    }
}

/// Is `sock` a TCP listener (whose pool is refilled after every frame)?
pub fn is_listener(sock: &Sock) -> bool {
    matches!(sock.kind, Kind::Tcp(Tcp::Listen(_)))
}

/// Listener `sock` (set `si`, `sockets`): move pool sockets that got a SYN
/// to the accept queue (with a flow-table entry pointing at the listener's
/// set), drop or recycle queued ones that were reset before completing,
/// and keep the pool full. Runs after every frame fed to the listener and
/// whenever its set is polled (set and interface lock held).
pub fn refill(
    sock: &mut Sock,
    sockets: &mut SocketSet<'static>,
    si: SetId,
    flows: &mut BTreeMap<Flow, SetId>,
) {
    let opts = &sock.opts;
    let Kind::Tcp(Tcp::Listen(l)) = &mut sock.kind else {
        return;
    };
    // Sockets that got a SYN join the accept queue.
    let mut i = 0;
    while i < l.listening.len() {
        let h = l.listening[i];
        let t = sockets.get::<tcp::Socket>(h);
        if t.state() != tcp::State::Listen {
            l.listening.swap_remove(i);
            let f = flow_of(t);
            if let Some(f) = f {
                flows.insert(f, si);
            }
            l.pending.push_back((h, f));
        } else {
            i += 1;
        }
    }
    // Queued sockets that were reset before completing: back to the
    // pool (smoltcp returns them to Listen) or dropped.
    let mut i = 0;
    while i < l.pending.len() {
        let (h, f) = l.pending[i];
        match sockets.get::<tcp::Socket>(h).state() {
            tcp::State::Listen if l.listening.len() < LISTEN_POOL => {
                l.pending.remove(i);
                unflow(flows, f, si);
                l.listening.push(h);
            }
            tcp::State::Listen | tcp::State::Closed => {
                l.pending.remove(i);
                unflow(flows, f, si);
                remove_tcp(sockets, h);
            }
            _ => i += 1,
        }
    }
    while l.listening.len() < LISTEN_POOL.min(l.backlog) && l.pending.len() < l.backlog {
        // Out of memory: the pool stays short (SYNs find no listening
        // socket) until the next refill.
        let Ok(mut t) = new_tcp(opts) else {
            break;
        };
        if t.listen(l.ep).is_err() {
            discard_tcp(t);
            break;
        }
        l.listening.push(sockets.add(t));
    }
}

/// After socket `sock`'s set (`i`, `sockets`) was polled: connection state
/// transitions, the connect timeout, the deferred keep-alive, UDP peer
/// filtering (set and interface lock held).
pub fn track(
    sock: &mut Sock,
    sockets: &mut SocketSet<'static>,
    i: SetId,
    flows: &mut BTreeMap<Flow, SetId>,
    now_ns: u64,
) {
    match &mut sock.kind {
        Kind::Tcp(Tcp::Conn(c)) => {
            let t = sockets.get_mut::<tcp::Socket>(c.h);
            let st = t.state();
            if c.connecting {
                if st == tcp::State::Closed {
                    c.connecting = false;
                    if sock.error == 0 {
                        sock.error = ECONNREFUSED;
                    }
                } else if st != tcp::State::SynSent {
                    c.connecting = false;
                    c.est = true;
                    c.local = t.local_endpoint();
                } else if now_ns.saturating_sub(c.started) > CONNECT_TIMEOUT_NS {
                    t.abort();
                    c.connecting = false;
                    sock.error = ETIMEDOUT;
                }
            } else if st == tcp::State::Closed
                && c.est
                && !c.reset
                && matches!(
                    c.last,
                    tcp::State::SynReceived
                        | tcp::State::Established
                        | tcp::State::FinWait1
                        | tcp::State::FinWait2
                        | tcp::State::CloseWait
                )
            {
                c.reset = true;
                sock.error = ECONNRESET;
            }
            if let Some(since) = c.ka_since {
                // Deferred keep-alive (apply_live_tcp_opts): idle
                // long enough, smoltcp probes from here on.
                match keep_alive(&sock.opts) {
                    Some(d) if now_ns.saturating_sub(since) / 1000 >= d.total_micros() => {
                        c.ka_since = None;
                        t.set_keep_alive(Some(d));
                    }
                    Some(_) => {}
                    None => c.ka_since = None,
                }
            }
            if st == tcp::State::Closed {
                // No tuple any more: smoltcp accepts nothing for it.
                unflow(flows, c.flow.take(), i);
            }
            c.last = st;
        }
        Kind::Udp(u) => {
            if let Some(peer) = u.peer {
                let s = sockets.get_mut::<udp::Socket>(u.h);
                while let Ok((_, m)) = s.peek() {
                    if m.endpoint == peer {
                        break;
                    }
                    let _ = s.recv();
                }
            }
        }
        _ => {}
    }
}

/// Settle zombie set `y` (set and interface lock held): `None` once it is
/// gone (the set is unregistered), else its own deadline (the linger
/// give-up time).
pub fn reap(s: &mut Stack, y: &mut Slot, now_ns: u64) -> Option<Option<u64>> {
    let Owner::Zombie { h, until, flow } = y.owner else {
        return None;
    };
    let t = y.set.get_mut::<tcp::Socket>(h);
    let st = t.state();
    let gone = (st == tcp::State::Closed && t.remote_endpoint().is_none())
        || st == tcp::State::TimeWait
        || st == tcp::State::Listen;
    if gone {
        remove_tcp(&mut y.set, h);
        unflow(&mut s.flows, flow, y.id);
        s.unregister(y);
        return None;
    }
    if st != tcp::State::Closed && (t.recv_queue() > 0 || now_ns > until) {
        // Data arrived for a closed socket, or it lingered too long:
        // reset it (removed once the RST is out).
        t.abort();
    }
    Some((now_ns <= until).then_some(until + 1))
}

/// Move queued socket `h` (flow `f`) of listener set `from` (`fi`) into a
/// zombie set of its own and abort it (the listener's set lock and the
/// interface lock held; the new set is registered with an immediate timer,
/// so the net task sends the RST).
fn zombify(
    s: &mut Stack,
    from: &mut SocketSet<'static>,
    fi: SetId,
    h: SocketHandle,
    f: Option<Flow>,
    until: u64,
) {
    let sk = from.remove(h);
    let z = s.reserve_set();
    let mut set = SocketSet::new(Vec::new());
    let nh = set.add(sk);
    set.get_mut::<tcp::Socket>(nh).abort();
    move_flow(&mut s.flows, f, fi, z);
    let _ = s.register(Slot {
        id: z,
        set,
        owner: Owner::Zombie {
            h: nh,
            until,
            flow: f,
        },
    });
    s.mark_dirty(z);
}

/// Bind `sock` (set `si`) (port 0: ephemeral) and record it; a UDP socket
/// joins the port map.
fn do_bind(
    s: &mut Stack,
    sock: &mut Sock,
    sockets: &mut SocketSet<'static>,
    si: SetId,
    ip: Option<IpAddr>,
    port: u16,
) -> Result<u16, Errno> {
    let proto = match sock.kind {
        Kind::Tcp(_) => Proto::Tcp,
        Kind::Udp(_) => Proto::Udp,
    };
    let cover = Cover::of(sock.family, ip, sock.opts.v6only);
    let reuse = sock.opts.reuse;
    let port = s
        .ports
        .bind(sock.id, proto, port, cover, reuse)
        .map_err(|e| match e {
            BindError::InUse | BindError::NoPorts => Errno(EADDRINUSE),
        })?;
    sock.bound = Some((ip, port));
    if let Kind::Udp(u) = &sock.kind {
        let ep = IpListenEndpoint {
            addr: ip.map(Into::into),
            port,
        };
        let _ = sockets.get_mut::<udp::Socket>(u.h).bind(ep);
        s.bound.add(P_UDP, port, si, cover);
    }
    Ok(port)
}

fn autobind(
    s: &mut Stack,
    sock: &mut Sock,
    sockets: &mut SocketSet<'static>,
    si: SetId,
) -> Result<(), Errno> {
    if sock.bound.is_none() {
        do_bind(s, sock, sockets, si, None, 0)?;
    }
    Ok(())
}

/// Drop the socket of set `y` (its file was released; set and interface
/// lock held): UDP and idle / listening TCP sockets go at once, a TCP
/// connection becomes a zombie in the same set.
fn close_sock(s: &mut Stack, y: &mut Slot) {
    let Owner::Sock(sock) = core::mem::replace(&mut y.owner, Owner::Free) else {
        return;
    };
    s.nsocks -= 1;
    s.ports.unbind(sock.id);
    let si = y.id;
    let deadline = crate::arch::timer::now_ns() + LINGER_NS;
    match sock.kind {
        Kind::Udp(u) => {
            if let Some((_, port)) = sock.bound {
                s.bound.remove(P_UDP, port, si);
            }
            y.set.remove(u.h);
            heap::uncharge(UDP_BUFS);
            s.unregister(y);
        }
        Kind::Tcp(Tcp::Idle) => s.unregister(y),
        Kind::Tcp(Tcp::Listen(l)) => {
            if let Some((_, port)) = sock.bound {
                s.bound.remove(P_TCP, port, si);
            }
            for h in l.listening {
                remove_tcp(&mut y.set, h);
            }
            for (h, f) in l.pending {
                zombify(s, &mut y.set, si, h, f, deadline);
            }
            s.unregister(y);
        }
        Kind::Tcp(Tcp::Conn(c)) => {
            let t = y.set.get_mut::<tcp::Socket>(c.h);
            let hard = sock.opts.linger.l_onoff != 0 && sock.opts.linger.l_linger == 0;
            if hard || t.recv_queue() > 0 {
                t.abort();
            } else {
                t.close();
            }
            // The set stays (with its flow entry) until the shutdown
            // finishes.
            y.owner = Owner::Zombie {
                h: c.h,
                until: deadline,
                flow: c.flow,
            };
        }
    }
}

/// Remove the flow-table entry `f` if it still points at set `i`.
fn unflow(flows: &mut BTreeMap<Flow, SetId>, f: Option<Flow>, i: SetId) {
    if let Some(f) = f {
        stack::flow_remove(flows, f, i);
    }
}

/// Re-point the entry `f` from set `from` to set `to` (unless a newer
/// socket with the same tuple owns it).
fn move_flow(flows: &mut BTreeMap<Flow, SetId>, f: Option<Flow>, from: SetId, to: SetId) {
    if let Some(f) = f
        && let Some(e) = flows.get_mut(&f)
        && *e == from
    {
        *e = to;
    }
}

// ---------------------------------------------------------------------------
// The file.

pub struct InetSocket {
    /// This socket's set (its state is the set's `Owner::Sock`).
    set: SetRef,
    family: u16,
    ty: u32,
    proto: i32,
    ino: u64,
}

/// Whether the port table got its post-boot seed ([`create`]).
static PORTS_SEEDED: AtomicBool = AtomicBool::new(false);

/// `socket(family, ty, protocol)` for AF_INET/AF_INET6 (`ty` without the
/// flag bits).
pub fn create(family: u16, ty: u32, protocol: i32) -> Result<Arc<InetSocket>, Errno> {
    if stack::get().is_none() {
        return Err(Errno(EAFNOSUPPORT));
    }
    let proto = match ty {
        SOCK_STREAM if protocol == 0 || protocol == IPPROTO_TCP => IPPROTO_TCP,
        SOCK_DGRAM if protocol == 0 || protocol == IPPROTO_UDP => IPPROTO_UDP,
        SOCK_STREAM | SOCK_DGRAM => return Err(Errno(EPROTONOSUPPORT)),
        // Raw IP needs CAP_NET_RAW, which no process has: Linux answers an
        // unprivileged (or containerised) caller with EPERM, and Go's
        // tests treat that as "not supported here".
        SOCK_RAW => return Err(Errno(EPERM)),
        SOCK_SEQPACKET | SOCK_RDM => return Err(Errno(ESOCKTNOSUPPORT)),
        _ => return Err(Errno(EINVAL)),
    };
    // The port table is made at boot, before the RNG has its real seed
    // (virtio-rng comes later): pick the ephemeral starting points again
    // at the first socket, so source ports differ between boots.
    let seed = (!PORTS_SEEDED.swap(true, Ordering::Relaxed)).then(|| {
        let mut b = [0u8; 8];
        crate::random::fill(&mut b);
        u64::from_ne_bytes(b)
    });
    let set = {
        let mut s = stack::lock_new();
        if let Some(seed) = seed {
            s.ports.reseed(seed);
        }
        if s.nsocks >= MAX_SOCKETS {
            return Err(Errno(ENFILE));
        }
        let udp = if ty == SOCK_STREAM {
            None
        } else {
            Some(new_udp()?)
        };
        let id = s.alloc_id();
        let si = s.reserve_set();
        let mut set = SocketSet::new(Vec::new());
        let kind = match udp {
            None => Kind::Tcp(Tcp::Idle),
            Some(u) => Kind::Udp(Udp {
                h: set.add(u),
                peer: None,
                local: None,
            }),
        };
        s.nsocks += 1;
        s.register(Slot {
            id: si,
            set,
            owner: Owner::Sock(Sock::new(id, family, kind)),
        })
    };
    Ok(Arc::new(InetSocket {
        set,
        family,
        ty,
        proto,
        ino: alloc_ino(),
    }))
}

impl InetSocket {
    /// Run `f` on this socket under its set lock (`stack::sys`); then, if
    /// `poll`, poll its set.
    fn op<R>(
        &self,
        poll: bool,
        f: impl FnOnce(&mut Sock, &mut SocketSet<'static>, SetId, &mut Iface) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        self.op_cond(|sock, set, si, ifc| (f(sock, set, si, ifc), poll))
    }

    /// [`InetSocket::op`], where `f` decides whether to poll (a `recv`
    /// that read nothing changed nothing).
    fn op_cond<R>(
        &self,
        f: impl FnOnce(
            &mut Sock,
            &mut SocketSet<'static>,
            SetId,
            &mut Iface,
        ) -> (Result<R, Errno>, bool),
    ) -> Result<R, Errno> {
        stack::sys(&self.set, |y, ifc| {
            let Slot { id, set, owner } = y;
            match owner {
                Owner::Sock(sock) => f(sock, set, *id, ifc),
                _ => (Err(Errno(EBADF)), false),
            }
        })
    }

    /// Decode a destination/bind address for this socket's family:
    /// (IP with `::ffff:` unmapped, port).
    fn inet_addr(
        &self,
        a: &SockAddr,
        v6only: bool,
        mapped_err: i32,
    ) -> Result<(IpAddr, u16), Errno> {
        match (self.family, a) {
            (AF_INET, SockAddr::V4 { addr, port }) => Ok((IpAddr::V4(*addr), *port)),
            (AF_INET6, SockAddr::V6 { addr, port, .. }) => match addr.to_ipv4_mapped() {
                Some(_) if v6only => Err(Errno(mapped_err)),
                Some(v4) => Ok((IpAddr::V4(v4), *port)),
                None => Ok((IpAddr::V6(*addr), *port)),
            },
            _ => Err(Errno(EAFNOSUPPORT)),
        }
    }

    fn view(&self, ip: IpAddr, port: u16) -> SockAddr {
        SockAddr::for_family(self.family, ip, port)
    }

    fn wildcard(&self) -> IpAddr {
        if self.family == AF_INET6 {
            IpAddr::V6(Ipv6Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        }
    }

    fn tcp_send(
        &self,
        sock: &mut Sock,
        sockets: &mut SocketSet<'static>,
        data: &[u8],
    ) -> Result<usize, Errno> {
        let (h, reset) = match &sock.kind {
            Kind::Tcp(Tcp::Conn(c)) => (c.h, c.reset),
            _ => return Err(Errno(ENOTCONN)),
        };
        if let Some(e) = sock.take_error() {
            return Err(e);
        }
        if reset || sock.shut_wr {
            return Err(Errno(EPIPE));
        }
        let t = sockets.get_mut::<tcp::Socket>(h);
        match t.state() {
            tcp::State::SynSent | tcp::State::SynReceived => return Err(Errno(EAGAIN)),
            _ if !t.may_send() => return Err(Errno(EPIPE)),
            _ => {}
        }
        if data.is_empty() {
            return Ok(0);
        }
        match t.send_slice(data) {
            Ok(0) => Err(Errno(EAGAIN)),
            Ok(n) => {
                sock.touch();
                Ok(n)
            }
            Err(_) => Err(Errno(EPIPE)),
        }
    }

    fn tcp_recv(
        &self,
        sock: &mut Sock,
        sockets: &mut SocketSet<'static>,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<RecvInfo, Errno> {
        let (h, reset, connecting, est) = match &sock.kind {
            Kind::Tcp(Tcp::Conn(c)) => (c.h, c.reset, c.connecting, c.est),
            _ => return Err(Errno(ENOTCONN)),
        };
        let t = sockets.get_mut::<tcp::Socket>(h);
        let st = t.state();
        if t.recv_queue() > 0 && !buf.is_empty() && !matches!(st, tcp::State::SynReceived) {
            let n = if peek {
                t.peek_slice(buf).unwrap_or(0)
            } else {
                t.recv_slice(buf).unwrap_or(0)
            };
            sock.touch();
            return Ok(RecvInfo {
                len: n,
                full: n,
                from: None,
                control: Vec::new(),
            });
        }
        if let Some(e) = sock.take_error() {
            return Err(e);
        }
        let eof = RecvInfo {
            len: 0,
            full: 0,
            from: None,
            control: Vec::new(),
        };
        if sock.shut_rd || reset || buf.is_empty() {
            return Ok(eof);
        }
        match st {
            tcp::State::SynSent | tcp::State::SynReceived => Err(Errno(EAGAIN)),
            _ if connecting => Err(Errno(EAGAIN)),
            tcp::State::Established | tcp::State::FinWait1 | tcp::State::FinWait2 => {
                Err(Errno(EAGAIN))
            }
            tcp::State::Closed if !est => Err(Errno(ENOTCONN)),
            _ => Ok(eof),
        }
    }

    fn udp_send(
        &self,
        sock: &mut Sock,
        sockets: &mut SocketSet<'static>,
        si: SetId,
        ifc: &mut Iface,
        data: &[u8],
        to: Option<&SockAddr>,
    ) -> Result<usize, Errno> {
        if let Some(e) = sock.take_error() {
            return Err(e);
        }
        if sock.shut_wr {
            return Err(Errno(EPIPE));
        }
        let Kind::Udp(u) = &sock.kind else {
            return Err(Errno(EINVAL));
        };
        let (v6only, peer) = (sock.opts.v6only, u.peer);
        let ep = match to {
            Some(a) => {
                let (ip, port) = self.inet_addr(a, v6only, ENETUNREACH)?;
                if port == 0 {
                    return Err(Errno(EINVAL));
                }
                IpEndpoint::new(unspecified_to_loopback(ip).into(), port)
            }
            None => peer.ok_or(Errno(EDESTADDRREQ))?,
        };
        let s = ifc.get();
        if !s.routable(ip_of(ep.addr)) {
            return Err(Errno(ENETUNREACH));
        }
        let max = match ep.addr {
            IpAddress::Ipv4(_) => 1500 - 28,
            IpAddress::Ipv6(_) => 1500 - 48,
        };
        if data.len() > max {
            return Err(Errno(EMSGSIZE));
        }
        autobind(s, sock, sockets, si)?;
        let Kind::Udp(u) = &sock.kind else {
            return Err(Errno(EINVAL));
        };
        let us = sockets.get_mut::<udp::Socket>(u.h);
        match us.send_slice(data, ep) {
            Ok(()) => Ok(data.len()),
            Err(udp::SendError::BufferFull) => Err(Errno(EAGAIN)),
            Err(udp::SendError::Unaddressable) => Err(Errno(EINVAL)),
        }
    }

    fn udp_recv(
        &self,
        sock: &mut Sock,
        sockets: &mut SocketSet<'static>,
        buf: &mut [u8],
        flags: MsgFlags,
    ) -> Result<RecvInfo, Errno> {
        let Kind::Udp(u) = &sock.kind else {
            return Err(Errno(EINVAL));
        };
        let us = sockets.get_mut::<udp::Socket>(u.h);
        loop {
            let (data, meta) = match us.peek() {
                Ok((d, m)) => (d, *m),
                Err(_) => {
                    if let Some(e) = sock.take_error() {
                        return Err(e);
                    }
                    if sock.shut_rd {
                        return Ok(RecvInfo {
                            len: 0,
                            full: 0,
                            from: None,
                            control: Vec::new(),
                        });
                    }
                    return Err(Errno(EAGAIN));
                }
            };
            let from = ip_of(meta.endpoint.addr);
            let wanted = u.peer.is_none_or(|p| p == meta.endpoint)
                && family_ok(sock.family, sock.opts.v6only, from);
            if !wanted {
                let _ = us.recv();
                continue;
            }
            let full = data.len();
            let n = full.min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            if !flags.contains(MsgFlags::PEEK) {
                let _ = us.recv();
            }
            return Ok(RecvInfo {
                len: n,
                full,
                from: Some(self.view(from, meta.endpoint.port)),
                control: Vec::new(),
            });
        }
    }

    fn sol_socket_get(&self, sock: &Sock, name: i32, out: &mut [u8]) -> Result<usize, Errno> {
        let o = &sock.opts;
        let int = |v: i32, out: &mut [u8]| Ok(so::int_bytes(v, out));
        match name {
            SO_TYPE => int(self.ty as i32, out),
            SO_DOMAIN => int(self.family as i32, out),
            SO_PROTOCOL => int(self.proto, out),
            SO_REUSEADDR => int(o.reuse.addr as i32, out),
            SO_REUSEPORT => int(o.reuse.port as i32, out),
            SO_KEEPALIVE => int(o.keepalive as i32, out),
            SO_ACCEPTCONN => int(matches!(sock.kind, Kind::Tcp(Tcp::Listen(_))) as i32, out),
            SO_RCVBUF => int(
                match &sock.kind {
                    Kind::Tcp(_) => TCP_RX_BUF,
                    Kind::Udp(_) => UDP_PAYLOAD,
                } as i32,
                out,
            ),
            SO_SNDBUF => int(
                match &sock.kind {
                    Kind::Tcp(_) => TCP_TX_BUF,
                    Kind::Udp(_) => UDP_PAYLOAD,
                } as i32,
                out,
            ),
            SO_LINGER => {
                out[..4].copy_from_slice(&o.linger.l_onoff.to_ne_bytes());
                out[4..8].copy_from_slice(&o.linger.l_linger.to_ne_bytes());
                Ok(8)
            }
            SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => Ok(so::timeval_bytes(o.rcvtimeo, out)),
            SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => Ok(so::timeval_bytes(o.sndtimeo, out)),
            SO_BROADCAST | SO_OOBINLINE | SO_PRIORITY | SO_RCVLOWAT | SO_SNDLOWAT
            | SO_DONTROUTE | SO_NO_CHECK | SO_TIMESTAMP_OLD => {
                let def = if name == SO_RCVLOWAT || name == SO_SNDLOWAT {
                    1
                } else {
                    0
                };
                int(*o.misc.get(&(SOL_SOCKET, name)).unwrap_or(&def), out)
            }
            SO_BINDTODEVICE => Ok(0),
            _ => Err(Errno(ENOPROTOOPT)),
        }
    }
}

fn unspecified_to_loopback(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(a) if a.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(a) if a.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    }
}

impl SocketOps for InetSocket {
    fn domain(&self) -> u16 {
        self.family
    }

    fn sock_type(&self) -> u32 {
        self.ty
    }

    fn protocol(&self) -> i32 {
        self.proto
    }

    fn bind(&self, addr: &SockAddr) -> Result<(), Errno> {
        self.op(false, |sock, sockets, si, ifc| {
            if sock.bound.is_some() {
                return Err(Errno(EINVAL));
            }
            let v6only = sock.opts.v6only;
            let (ip, port) = match self.inet_addr(addr, v6only, EINVAL) {
                Err(Errno(EAFNOSUPPORT)) if self.family == AF_INET6 => return Err(Errno(EINVAL)),
                r => r?,
            };
            let s = ifc.get();
            let ip = if ip.is_unspecified() {
                None
            } else {
                if !s.is_local(ip) {
                    return Err(Errno(EADDRNOTAVAIL));
                }
                Some(ip)
            };
            do_bind(s, sock, sockets, si, ip, port).map(|_| ())
        })
    }

    fn listen(&self, backlog: i32) -> Result<(), Errno> {
        self.op(true, |sock, sockets, si, ifc| {
            let backlog = backlog.clamp(1, MAX_BACKLOG as i32) as usize;
            match &mut sock.kind {
                Kind::Udp(_) => return Err(Errno(EOPNOTSUPP)),
                Kind::Tcp(Tcp::Conn(_)) => return Err(Errno(EINVAL)),
                Kind::Tcp(Tcp::Listen(l)) => {
                    l.backlog = backlog;
                    return Ok(());
                }
                Kind::Tcp(Tcp::Idle) => {}
            }
            let s = ifc.get();
            autobind(s, sock, sockets, si)?;
            let (ip, port) = sock.bound.unwrap();
            sock.kind = Kind::Tcp(Tcp::Listen(Listener {
                ep: IpListenEndpoint {
                    addr: ip.map(Into::into),
                    port,
                },
                backlog,
                listening: Vec::new(),
                pending: VecDeque::new(),
            }));
            let cover = Cover::of(sock.family, ip, sock.opts.v6only);
            s.ports.set_listening(sock.id, true);
            s.bound.add(P_TCP, port, si, cover);
            refill(sock, sockets, si, &mut s.flows);
            Ok(())
        })
    }

    fn accept(&self) -> Result<(Arc<dyn File>, SockAddr), Errno> {
        let r = stack::sys(&self.set, |y, ifc| {
            let r = (|| {
                if !matches!(y.owner, Owner::Sock(_)) {
                    return Err(Errno(EBADF));
                }
                let s = ifc.get();
                // Frames queued for this set belong to its queued sockets:
                // feed them before one moves out.
                s.feed_backlog(y);
                let Slot { id, set, owner } = y;
                let li = *id;
                let Owner::Sock(sock) = owner else {
                    return Err(Errno(EBADF));
                };
                let full = s.nsocks >= MAX_SOCKETS;
                let linger = crate::arch::timer::now_ns() + LINGER_NS;
                let family = sock.family;
                let v6only = sock.opts.v6only;
                let (h, flow, peer, local, st) = loop {
                    let Kind::Tcp(Tcp::Listen(l)) = &mut sock.kind else {
                        return Err(Errno(EINVAL));
                    };
                    let Some(pos) = l
                        .pending
                        .iter()
                        .position(|&(h, _)| accept_ready(tcp_sock(set, h).state()))
                    else {
                        return Err(Errno(EAGAIN));
                    };
                    let (h, f) = l.pending[pos];
                    let t = tcp_sock(set, h);
                    let peer = t.remote_endpoint();
                    if peer.is_some_and(|p| !family_ok(family, v6only, ip_of(p.addr))) {
                        // Not expected (the port map picks a listener of the
                        // packet's family), but smoltcp's wildcard endpoints
                        // match both families: refuse a connection of the
                        // wrong family.
                        l.pending.remove(pos);
                        zombify(s, set, li, h, f, linger);
                        continue;
                    }
                    if full {
                        return Err(Errno(ENFILE));
                    }
                    let found = (
                        h,
                        f.or_else(|| flow_of(t)),
                        peer,
                        t.local_endpoint(),
                        t.state(),
                    );
                    l.pending.remove(pos);
                    break found;
                };
                // The connection gets a set of its own.
                let id = s.alloc_id();
                let ni = s.reserve_set();
                let mut nset = SocketSet::new(Vec::new());
                let sk = set.remove(h);
                let h = nset.add(sk);
                if let Some(f) = flow {
                    // Re-point the entry (refill made it point at the
                    // listener's set).
                    s.flows.insert(f, ni);
                }
                let mut ns = Sock::new(
                    id,
                    family,
                    Kind::Tcp(Tcp::Conn(Conn {
                        h,
                        flow,
                        connecting: false,
                        started: 0,
                        last: st,
                        est: true,
                        reset: false,
                        peer,
                        local,
                        ka_since: None,
                    })),
                );
                ns.opts = sock.opts.clone();
                ns.opts.misc.clear();
                s.nsocks += 1;
                let nr = s.register(Slot {
                    id: ni,
                    set: nset,
                    owner: Owner::Sock(ns),
                });
                refill(sock, set, li, &mut s.flows);
                Ok((nr, peer))
            })();
            (r, true)
        });
        let (set, peer) = r?;
        let peer = match peer {
            Some(p) => self.view(ip_of(p.addr), p.port),
            None => self.view(self.wildcard(), 0),
        };
        let f: Arc<dyn File> = Arc::new(InetSocket {
            set,
            family: self.family,
            ty: self.ty,
            proto: self.proto,
            ino: alloc_ino(),
        });
        Ok((f, peer))
    }

    fn connect(&self, addr: &SockAddr) -> Result<(), Errno> {
        self.op(true, |sock, sockets, si, ifc| {
            let v6only = sock.opts.v6only;
            match &mut sock.kind {
                Kind::Udp(u) => {
                    if *addr == SockAddr::Unspec {
                        u.peer = None;
                        u.local = None;
                        return Ok(());
                    }
                    let (ip, port) = self.inet_addr(addr, v6only, ENETUNREACH)?;
                    let ip = unspecified_to_loopback(ip);
                    let s = ifc.get();
                    if !s.routable(ip) {
                        return Err(Errno(ENETUNREACH));
                    }
                    autobind(s, sock, sockets, si)?;
                    let src = if ip.is_loopback() {
                        Some(ip)
                    } else {
                        s.iface.get_source_address(&ip.into()).map(ip_of)
                    };
                    if let Kind::Udp(u) = &mut sock.kind {
                        u.peer = Some(IpEndpoint::new(ip.into(), port));
                        u.local = src;
                    }
                    return Ok(());
                }
                Kind::Tcp(Tcp::Listen(_)) => return Err(Errno(EINVAL)),
                Kind::Tcp(Tcp::Conn(c)) => {
                    if c.connecting {
                        return Err(Errno(EALREADY));
                    }
                    if let Some(e) = sock.take_error() {
                        return Err(e);
                    }
                    return Err(Errno(EISCONN));
                }
                Kind::Tcp(Tcp::Idle) => {}
            }
            let (ip, port) = self.inet_addr(addr, v6only, ENETUNREACH)?;
            let ip = unspecified_to_loopback(ip);
            if port == 0 {
                return Err(Errno(ECONNREFUSED));
            }
            let s = ifc.get();
            if !s.routable(ip) {
                return Err(Errno(ENETUNREACH));
            }
            autobind(s, sock, sockets, si)?;
            let (lip, lport) = sock.bound.unwrap();
            if lip.is_some_and(|l| l.is_ipv4() != ip.is_ipv4()) {
                return Err(Errno(ENETUNREACH));
            }
            let mut t = new_tcp(&sock.opts)?;
            let remote = IpEndpoint::new(ip.into(), port);
            let local = IpListenEndpoint {
                addr: lip.map(Into::into),
                port: lport,
            };
            if let Err(e) = t.connect(s.iface.context(), remote, local) {
                discard_tcp(t);
                return Err(match e {
                    tcp::ConnectError::InvalidState => Errno(EISCONN),
                    tcp::ConnectError::Unaddressable => Errno(EADDRNOTAVAIL),
                });
            }
            let local = t.local_endpoint();
            // The tuple smoltcp will match replies on is fixed from here.
            let flow = flow_of(&t);
            let h = sockets.add(t);
            if let Some(f) = flow {
                s.flows.insert(f, si);
            }
            sock.kind = Kind::Tcp(Tcp::Conn(Conn {
                h,
                flow,
                connecting: true,
                started: crate::arch::timer::now_ns(),
                last: tcp::State::SynSent,
                est: false,
                reset: false,
                peer: Some(remote),
                local,
                ka_since: None,
            }));
            Err(Errno(EINPROGRESS))
        })
    }

    fn connect_result(&self) -> Option<Result<(), Errno>> {
        self.op(false, |sock, _, _, _| {
            Ok(match &sock.kind {
                Kind::Tcp(Tcp::Conn(c)) if c.connecting => None,
                Kind::Tcp(Tcp::Conn(c)) if c.est && sock.error == 0 => Some(Ok(())),
                _ => Some(Err(sock.take_error().unwrap_or(Errno(ECONNREFUSED)))),
            })
        })
        .unwrap_or(None)
    }

    fn send(
        &self,
        data: &[u8],
        to: Option<&SockAddr>,
        flags: MsgFlags,
        _control: &[u8],
    ) -> Result<usize, Errno> {
        if flags.contains(MsgFlags::OOB) {
            return Err(Errno(EOPNOTSUPP));
        }
        self.op(true, |sock, sockets, si, ifc| {
            if self.ty == SOCK_STREAM {
                // Linux ignores the address on a connected stream socket
                // (EISCONN only for some families).
                self.tcp_send(sock, sockets, data)
            } else {
                self.udp_send(sock, sockets, si, ifc, data, to)
            }
        })
    }

    fn recv(&self, buf: &mut [u8], flags: MsgFlags) -> Result<RecvInfo, Errno> {
        if flags.contains(MsgFlags::OOB) {
            return Err(Errno(EINVAL));
        }
        if flags.contains(MsgFlags::ERRQUEUE) {
            return Err(Errno(EAGAIN));
        }
        let peek = flags.contains(MsgFlags::PEEK);
        // Polling after a read sends the window update. A read that took
        // nothing (EAGAIN, EOF, an error, MSG_PEEK) changed nothing.
        self.op_cond(|sock, sockets, _, _| {
            let r = if self.ty == SOCK_STREAM {
                self.tcp_recv(sock, sockets, buf, peek)
            } else {
                self.udp_recv(sock, sockets, buf, flags)
            };
            let poll = !peek && matches!(&r, Ok(info) if info.len > 0);
            (r, poll)
        })
    }

    fn shutdown(&self, how: i32) -> Result<(), Errno> {
        if !(SHUT_RD..=SHUT_RDWR).contains(&how) {
            return Err(Errno(EINVAL));
        }
        let rd = how != SHUT_WR;
        let wr = how != SHUT_RD;
        self.op(true, |sock, sockets, _, _| {
            match &sock.kind {
                Kind::Tcp(Tcp::Conn(c)) => {
                    if wr {
                        sockets.get_mut::<tcp::Socket>(c.h).close();
                    }
                }
                Kind::Udp(u) if u.peer.is_some() => {}
                Kind::Tcp(Tcp::Listen(_)) => return Ok(()),
                _ => return Err(Errno(ENOTCONN)),
            }
            sock.shut_rd |= rd;
            sock.shut_wr |= wr;
            Ok(())
        })
    }

    fn local_addr(&self) -> Result<SockAddr, Errno> {
        self.op(false, |sock, sockets, _, _| {
            if let Kind::Tcp(Tcp::Conn(c)) = &sock.kind {
                let l = sockets.get::<tcp::Socket>(c.h).local_endpoint().or(c.local);
                if let Some(l) = l {
                    return Ok(self.view(ip_of(l.addr), l.port));
                }
            }
            let conn_local = match &sock.kind {
                Kind::Udp(u) => u.local,
                _ => None,
            };
            Ok(match sock.bound {
                Some((ip, port)) => self.view(ip.or(conn_local).unwrap_or(self.wildcard()), port),
                None => self.view(self.wildcard(), 0),
            })
        })
    }

    fn peer_addr(&self) -> Result<SockAddr, Errno> {
        self.op(false, |sock, _, _, _| {
            let peer = match &sock.kind {
                Kind::Tcp(Tcp::Conn(c)) if c.est && !c.reset => c.peer,
                Kind::Udp(u) => u.peer,
                _ => None,
            };
            match peer {
                Some(p) => Ok(self.view(ip_of(p.addr), p.port)),
                None => Err(Errno(ENOTCONN)),
            }
        })
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        self.op_cond(|sock, sockets, _, ifc| {
            let r = setsockopt_on(self, sock, sockets, ifc, level, name, val);
            // Live TCP options may change the set's timers.
            let poll = r.is_ok() && matches!(sock.kind, Kind::Tcp(Tcp::Conn(_)));
            (r, poll)
        })
    }

    fn getsockopt(&self, level: i32, name: i32, out: &mut [u8]) -> Result<usize, Errno> {
        self.op(false, |sock, _, _, _| {
            if (level, name) == (SOL_SOCKET, SO_ERROR) {
                let e = core::mem::take(&mut sock.error);
                return Ok(so::int_bytes(e, out));
            }
            let o = &sock.opts;
            let is_tcp = matches!(sock.kind, Kind::Tcp(_));
            let int = |v: i32, out: &mut [u8]| Ok(so::int_bytes(v, out));
            match (level, name) {
                (SOL_SOCKET, _) => self.sol_socket_get(sock, name, out),
                (SOL_TCP, _) if !is_tcp => Err(Errno(ENOPROTOOPT)),
                (SOL_TCP, TCP_NODELAY) => int(o.nodelay as i32, out),
                (SOL_TCP, TCP_MAXSEG) => {
                    let ipv6 = match &sock.kind {
                        Kind::Tcp(Tcp::Conn(c)) => c
                            .peer
                            .is_some_and(|p| p.addr.version() == smoltcp::wire::IpVersion::Ipv6),
                        _ => self.family == AF_INET6,
                    };
                    int(if ipv6 { 1440 } else { 1460 }, out)
                }
                (SOL_TCP, TCP_KEEPIDLE) => int(o.keepidle, out),
                (SOL_TCP, TCP_KEEPINTVL) => int(o.keepintvl, out),
                (SOL_TCP, TCP_KEEPCNT) => int(o.keepcnt, out),
                (SOL_TCP, TCP_USER_TIMEOUT) => int(o.user_timeout, out),
                (SOL_TCP, TCP_QUICKACK) => int(*o.misc.get(&(level, name)).unwrap_or(&1), out),
                (SOL_TCP, TCP_CORK | TCP_FASTOPEN) => {
                    int(*o.misc.get(&(level, name)).unwrap_or(&0), out)
                }
                (SOL_IP, IP_TTL) => int(*o.misc.get(&(level, name)).unwrap_or(&64), out),
                (SOL_IP, IP_TOS | IP_RECVERR | IP_MTU_DISCOVER) => {
                    int(*o.misc.get(&(level, name)).unwrap_or(&0), out)
                }
                (SOL_IPV6, _) if self.family != AF_INET6 => Err(Errno(ENOPROTOOPT)),
                (SOL_IPV6, IPV6_V6ONLY) => int(o.v6only as i32, out),
                (SOL_IPV6, IPV6_UNICAST_HOPS) => {
                    int(*o.misc.get(&(level, name)).unwrap_or(&64), out)
                }
                (SOL_IPV6, IPV6_TCLASS | IPV6_RECVERR) => {
                    int(*o.misc.get(&(level, name)).unwrap_or(&0), out)
                }
                _ => Err(Errno(ENOPROTOOPT)),
            }
        })
    }

    fn timeouts(&self) -> (Option<u64>, Option<u64>) {
        self.op(false, |sock, _, _, _| Ok((sock.opts.rcvtimeo, sock.opts.sndtimeo)))
            .unwrap_or((None, None))
    }
}

/// `setsockopt` on `sock` (its set lock held; the interface lock taken
/// for the bind table's reuse flags).
fn setsockopt_on(
    me: &InetSocket,
    sock: &mut Sock,
    sockets: &mut SocketSet<'static>,
    ifc: &mut Iface,
    level: i32,
    name: i32,
    val: &[u8],
) -> Result<(), Errno> {
    let is_tcp = matches!(sock.kind, Kind::Tcp(_));
    let o = &mut sock.opts;
    match (level, name) {
        (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
            o.rcvtimeo = so::timeval_opt(val)?;
            return Ok(());
        }
        (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
            o.sndtimeo = so::timeval_opt(val)?;
            return Ok(());
        }
        (SOL_SOCKET, SO_LINGER) => {
            if val.len() < 8 {
                return Err(Errno(EINVAL));
            }
            o.linger = Linger {
                l_onoff: so::int_opt(val)?,
                l_linger: so::int_opt(&val[4..])?,
            };
            return Ok(());
        }
        (SOL_SOCKET, SO_BINDTODEVICE) => {
            let n = val.iter().position(|&b| b == 0).unwrap_or(val.len());
            return match &val[..n] {
                b"" | b"lo" | b"eth0" => Ok(()),
                _ => Err(Errno(ENODEV)),
            };
        }
        _ => {}
    }
    let v = so::int_opt(val)?;
    match (level, name) {
        (SOL_SOCKET, SO_REUSEADDR) => {
            o.reuse.addr = v != 0;
            ifc.get().ports.set_reuse(sock.id, o.reuse);
        }
        (SOL_SOCKET, SO_REUSEPORT) => {
            o.reuse.port = v != 0;
            ifc.get().ports.set_reuse(sock.id, o.reuse);
        }
        (SOL_SOCKET, SO_KEEPALIVE) => o.keepalive = v != 0,
        (SOL_SOCKET, SO_RCVBUF | SO_SNDBUF | SO_RCVBUFFORCE | SO_SNDBUFFORCE) => {}
        (
            SOL_SOCKET,
            SO_BROADCAST | SO_OOBINLINE | SO_PRIORITY | SO_RCVLOWAT | SO_SNDLOWAT | SO_DONTROUTE
            | SO_NO_CHECK | SO_TIMESTAMP_OLD,
        ) => {
            o.misc.insert((level, name), v);
        }
        (SOL_TCP, _) if !is_tcp => return Err(Errno(ENOPROTOOPT)),
        (SOL_TCP, TCP_NODELAY) => o.nodelay = v != 0,
        (SOL_TCP, TCP_KEEPIDLE | TCP_KEEPINTVL) if !(1..=32767).contains(&v) => {
            return Err(Errno(EINVAL));
        }
        (SOL_TCP, TCP_KEEPCNT) if !(1..=127).contains(&v) => return Err(Errno(EINVAL)),
        (SOL_TCP, TCP_KEEPIDLE) => o.keepidle = v,
        (SOL_TCP, TCP_KEEPINTVL) => o.keepintvl = v,
        (SOL_TCP, TCP_KEEPCNT) => o.keepcnt = v,
        (SOL_TCP, TCP_USER_TIMEOUT) if v < 0 => return Err(Errno(EINVAL)),
        (SOL_TCP, TCP_USER_TIMEOUT) => o.user_timeout = v,
        (SOL_TCP, TCP_MAXSEG | TCP_QUICKACK | TCP_CORK | TCP_FASTOPEN) => {
            o.misc.insert((level, name), v);
        }
        (SOL_IP, IP_TOS | IP_TTL | IP_RECVERR | IP_MTU_DISCOVER) => {
            o.misc.insert((level, name), v);
        }
        (SOL_IPV6, _) if me.family != AF_INET6 => return Err(Errno(ENOPROTOOPT)),
        (SOL_IPV6, IPV6_V6ONLY) => {
            if sock.bound.is_some() {
                return Err(Errno(EINVAL));
            }
            o.v6only = v != 0;
        }
        (SOL_IPV6, IPV6_UNICAST_HOPS | IPV6_TCLASS | IPV6_RECVERR) => {
            o.misc.insert((level, name), v);
        }
        _ => return Err(Errno(ENOPROTOOPT)),
    }
    // Live TCP options reach the smoltcp socket.
    if let Kind::Tcp(Tcp::Conn(c)) = &mut sock.kind {
        let t = sockets.get_mut::<tcp::Socket>(c.h);
        apply_live_tcp_opts(&sock.opts, c, t, crate::arch::timer::now_ns());
    }
    Ok(())
}

const IP_MTU_DISCOVER: i32 = 10;
const IPV6_RECVERR: i32 = 25;

impl File for InetSocket {
    fn kind(&self) -> &'static str {
        "socket"
    }

    fn read(&self, buf: &mut [u8], _flags: FileFlags) -> Result<usize, Errno> {
        self.recv(buf, MsgFlags::empty()).map(|r| r.len)
    }

    fn write(&self, buf: &[u8], _flags: FileFlags) -> Result<usize, Errno> {
        self.send(buf, None, MsgFlags::empty(), &[])
    }

    fn poll(&self, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        stack::poll_file(&self.set, |y| {
            let Slot { set, owner, .. } = y;
            let Owner::Sock(sock) = owner else {
                return EpollFlags::ERR | EpollFlags::HUP;
            };
            if let Some(h) = hook {
                sock.hooks.add(h);
            }
            sock.ready(set).0
        })
    }

    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        let n: i32 = match cmd {
            FIONREAD | SIOCOUTQ => self.op(false, |sock, sockets, _, _| {
                Ok(match &sock.kind {
                    Kind::Tcp(Tcp::Conn(c)) => {
                        let t = sockets.get::<tcp::Socket>(c.h);
                        if cmd == FIONREAD {
                            t.recv_queue()
                        } else {
                            t.send_queue()
                        }
                    }
                    Kind::Tcp(Tcp::Listen(_)) => return Err(Errno(EINVAL)),
                    Kind::Tcp(Tcp::Idle) => 0,
                    Kind::Udp(u) => {
                        let us = sockets.get_mut::<udp::Socket>(u.h);
                        if cmd == FIONREAD {
                            us.peek().map_or(0, |(d, _)| d.len())
                        } else {
                            us.send_queue()
                        }
                    }
                } as i32)
            })?,
            _ => return Err(Errno(ENOTTY)),
        };
        crate::syscall::with_space(|s| s.write_val(arg as usize, &n))?;
        Ok(0)
    }

    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFSOCK | 0o777, self.ino))
    }

    fn proc_name(&self) -> Vec<u8> {
        format!("socket:[{}]", self.ino).into_bytes()
    }

    fn release(&self) {
        stack::sys(&self.set, |y, ifc| {
            let s = ifc.get();
            // A listener's queued frames go to its queued sockets before
            // they are moved into zombie sets.
            s.feed_backlog(y);
            close_sock(s, y);
            ((), true)
        });
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }
}
