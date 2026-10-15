//! AF_INET / AF_INET6 sockets: TCP and UDP over smoltcp (D68).
//!
//! An [`InetSocket`] is the `File` an fd points at; it holds only an id.
//! Its state ([`Sock`]) lives in the `Stack` (under the one stack lock)
//! next to the smoltcp sockets it owns:
//!
//! * TCP, idle: no smoltcp socket until `connect` or `listen`.
//! * TCP listener: smoltcp has one socket per connection, so a listener
//!   owns a small pool of sockets `listen()`ing on its endpoint
//!   (`LISTEN_POOL`) plus a queue of sockets that left `Listen` (a SYN
//!   arrived): the accept queue, bounded by the backlog. The pool is
//!   refilled after every ingress packet (`Stack::refill_listeners`), so a
//!   burst of SYNs each find a listening socket. `accept` takes the first
//!   queued socket that is past `SynReceived`.
//! * TCP connection: one smoltcp socket. After every poll, `track` turns
//!   smoltcp state transitions into socket-API events: `SynSent → Closed`
//!   is ECONNREFUSED, a synchronized state → `Closed` is a reset
//!   (ECONNRESET once, then EOF), 75 s in `SynSent` is ETIMEDOUT.
//! * UDP: one smoltcp socket from `socket()` on, bound on `bind` or on the
//!   first `sendto`/`connect` (ephemeral port).
//!
//! Dual stack: an AF_INET6 socket with `IPV6_V6ONLY` = 0 (the default)
//! bound to `::` gets a smoltcp endpoint without an address, which accepts
//! both families; IPv4 peers are shown as `::ffff:a.b.c.d` and `::ffff:`
//! destinations are sent as IPv4. Wildcard endpoints match both families
//! in smoltcp, so the family filter (AF_INET sockets, V6ONLY sockets) is
//! applied on accept and on UDP receive.
//!
//! Closing: UDP sockets go at once; a TCP connection is `close()`d
//! (graceful FIN after the queued data) and kept as a zombie until smoltcp
//! reaches `Closed`/`TimeWait` (at most `LINGER_NS`); unread data or
//! `SO_LINGER` {1, 0} abort it with a RST, as on Linux.

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
use patina_inet::{BindError, Cover, Proto, Reuse, SockAddr};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Duration;
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint};

use super::socket::{self as so, RecvInfo, SocketOps};
use super::stack::{self, Stack};
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
// Per-socket state (in the stack).

pub struct Listener {
    ep: IpListenEndpoint,
    backlog: usize,
    listening: Vec<SocketHandle>,
    pending: VecDeque<SocketHandle>,
}

pub struct Conn {
    h: SocketHandle,
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

fn tcp_sock<'a>(
    sockets: &'a smoltcp::iface::SocketSet<'static>,
    h: SocketHandle,
) -> &'a tcp::Socket<'static> {
    sockets.get::<tcp::Socket>(h)
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
fn remove_tcp(sockets: &mut smoltcp::iface::SocketSet<'static>, h: SocketHandle) {
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
    fn new(family: u16, kind: Kind) -> Sock {
        Sock {
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
    fn ready(&self, sockets: &smoltcp::iface::SocketSet<'static>) -> (EpollFlags, usize) {
        let mut ev = EpollFlags::empty();
        let mut rxq = 0;
        match &self.kind {
            Kind::Tcp(Tcp::Idle) => {
                ev |= EpollFlags::OUT | EpollFlags::WRNORM | EpollFlags::HUP;
            }
            Kind::Tcp(Tcp::Listen(l)) => {
                if l.pending
                    .iter()
                    .any(|&h| accept_ready(tcp_sock(sockets, h).state()))
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
    pub fn refresh(&mut self, sockets: &smoltcp::iface::SocketSet<'static>) -> Notify {
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
// Stack-side bookkeeping (called with the stack lock held).

impl Stack {
    /// After each ingress packet: keep every listener's pool full.
    pub fn refill_listeners(&mut self) {
        for i in 0..self.listeners.len() {
            let id = self.listeners[i];
            self.refill(id);
        }
    }

    fn refill(&mut self, id: u64) {
        let Stack { socks, sockets, .. } = self;
        let Some(sock) = socks.get_mut(&id) else {
            return;
        };
        let opts = sock.opts.clone();
        let Kind::Tcp(Tcp::Listen(l)) = &mut sock.kind else {
            return;
        };
        // Sockets that got a SYN join the accept queue.
        let mut i = 0;
        while i < l.listening.len() {
            let h = l.listening[i];
            if sockets.get::<tcp::Socket>(h).state() != tcp::State::Listen {
                l.listening.swap_remove(i);
                l.pending.push_back(h);
            } else {
                i += 1;
            }
        }
        // Queued sockets that were reset before completing: back to the
        // pool (smoltcp returns them to Listen) or dropped.
        let mut i = 0;
        while i < l.pending.len() {
            let h = l.pending[i];
            match sockets.get::<tcp::Socket>(h).state() {
                tcp::State::Listen if l.listening.len() < LISTEN_POOL => {
                    l.pending.remove(i);
                    l.listening.push(h);
                }
                tcp::State::Listen | tcp::State::Closed => {
                    l.pending.remove(i);
                    remove_tcp(sockets, h);
                }
                _ => i += 1,
            }
        }
        while l.listening.len() < LISTEN_POOL.min(l.backlog) && l.pending.len() < l.backlog {
            // Out of memory: the pool stays short (SYNs find no listening
            // socket) until the next refill.
            let Ok(mut t) = new_tcp(&opts) else {
                break;
            };
            if t.listen(l.ep).is_err() {
                discard_tcp(t);
                break;
            }
            l.listening.push(sockets.add(t));
        }
    }

    /// After each poll: connection state transitions, connect timeouts,
    /// UDP peer filtering, zombie reaping.
    pub fn track(&mut self, now_ns: u64) {
        let Stack {
            socks,
            sockets,
            zombies,
            ..
        } = self;
        for sock in socks.values_mut() {
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
        zombies.retain(|&(h, deadline)| {
            let t = sockets.get_mut::<tcp::Socket>(h);
            let st = t.state();
            let gone = (st == tcp::State::Closed && t.remote_endpoint().is_none())
                || st == tcp::State::TimeWait
                || st == tcp::State::Listen;
            if gone {
                remove_tcp(sockets, h);
                return false;
            }
            if st != tcp::State::Closed && (t.recv_queue() > 0 || now_ns > deadline) {
                // Data arrived for a closed socket, or it lingered too
                // long: reset it (removed once the RST is out).
                t.abort();
            }
            true
        });
    }

    fn sock(&mut self, id: u64) -> Result<&mut Sock, Errno> {
        self.socks.get_mut(&id).ok_or(Errno(EBADF))
    }

    /// Bind `id` (port 0: ephemeral) and record it.
    fn do_bind(&mut self, id: u64, ip: Option<IpAddr>, port: u16) -> Result<u16, Errno> {
        let sock = self.socks.get(&id).ok_or(Errno(EBADF))?;
        let proto = match sock.kind {
            Kind::Tcp(_) => Proto::Tcp,
            Kind::Udp(_) => Proto::Udp,
        };
        let cover = Cover::of(sock.family, ip, sock.opts.v6only);
        let reuse = sock.opts.reuse;
        let port = self
            .ports
            .bind(id, proto, port, cover, reuse)
            .map_err(|e| match e {
                BindError::InUse | BindError::NoPorts => Errno(EADDRINUSE),
            })?;
        let Stack { socks, sockets, .. } = self;
        let sock = socks.get_mut(&id).unwrap();
        sock.bound = Some((ip, port));
        if let Kind::Udp(u) = &sock.kind {
            let ep = IpListenEndpoint {
                addr: ip.map(Into::into),
                port,
            };
            let _ = sockets.get_mut::<udp::Socket>(u.h).bind(ep);
        }
        Ok(port)
    }

    fn autobind(&mut self, id: u64) -> Result<(), Errno> {
        if self.sock(id)?.bound.is_none() {
            self.do_bind(id, None, 0)?;
        }
        Ok(())
    }

    /// Drop socket `id` (its file was released).
    fn close_sock(&mut self, id: u64) {
        let Some(sock) = self.socks.remove(&id) else {
            return;
        };
        self.ports.unbind(id);
        self.listeners.retain(|&l| l != id);
        let deadline = crate::arch::timer::now_ns() + LINGER_NS;
        match sock.kind {
            Kind::Udp(u) => {
                self.sockets.remove(u.h);
                heap::uncharge(UDP_BUFS);
            }
            Kind::Tcp(Tcp::Idle) => {}
            Kind::Tcp(Tcp::Listen(l)) => {
                for h in l.listening {
                    remove_tcp(&mut self.sockets, h);
                }
                for h in l.pending {
                    self.sockets.get_mut::<tcp::Socket>(h).abort();
                    self.zombies.push((h, deadline));
                }
            }
            Kind::Tcp(Tcp::Conn(c)) => {
                let t = self.sockets.get_mut::<tcp::Socket>(c.h);
                let hard = sock.opts.linger.l_onoff != 0 && sock.opts.linger.l_linger == 0;
                if hard || t.recv_queue() > 0 {
                    t.abort();
                } else {
                    t.close();
                }
                self.zombies.push((c.h, deadline));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The file.

pub struct InetSocket {
    id: u64,
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
    let id = stack::with(false, |s| {
        if let Some(seed) = seed {
            s.ports.reseed(seed);
        }
        if s.socks.len() >= MAX_SOCKETS {
            return Err(Errno(ENFILE));
        }
        let kind = if ty == SOCK_STREAM {
            Kind::Tcp(Tcp::Idle)
        } else {
            Kind::Udp(Udp {
                h: s.sockets.add(new_udp()?),
                peer: None,
                local: None,
            })
        };
        let id = s.alloc_id();
        s.socks.insert(id, Sock::new(family, kind));
        Ok(id)
    })?;
    Ok(Arc::new(InetSocket {
        id,
        family,
        ty,
        proto,
        ino: alloc_ino(),
    }))
}

impl InetSocket {
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

    fn tcp_send(&self, s: &mut Stack, data: &[u8]) -> Result<usize, Errno> {
        let Stack { socks, sockets, .. } = s;
        let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
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

    fn tcp_recv(&self, s: &mut Stack, buf: &mut [u8], peek: bool) -> Result<RecvInfo, Errno> {
        let Stack { socks, sockets, .. } = s;
        let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
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

    fn udp_send(&self, s: &mut Stack, data: &[u8], to: Option<&SockAddr>) -> Result<usize, Errno> {
        let (v6only, peer) = {
            let sock = s.sock(self.id)?;
            if let Some(e) = sock.take_error() {
                return Err(e);
            }
            if sock.shut_wr {
                return Err(Errno(EPIPE));
            }
            let Kind::Udp(u) = &sock.kind else {
                return Err(Errno(EINVAL));
            };
            (sock.opts.v6only, u.peer)
        };
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
        s.autobind(self.id)?;
        let Stack { socks, sockets, .. } = s;
        let Some(Kind::Udp(u)) = socks.get(&self.id).map(|x| &x.kind) else {
            return Err(Errno(EINVAL));
        };
        let us = sockets.get_mut::<udp::Socket>(u.h);
        match us.send_slice(data, ep) {
            Ok(()) => Ok(data.len()),
            Err(udp::SendError::BufferFull) => Err(Errno(EAGAIN)),
            Err(udp::SendError::Unaddressable) => Err(Errno(EINVAL)),
        }
    }

    fn udp_recv(&self, s: &mut Stack, buf: &mut [u8], flags: MsgFlags) -> Result<RecvInfo, Errno> {
        let Stack { socks, sockets, .. } = s;
        let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
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
        stack::with(false, |s| {
            let sock = s.sock(self.id)?;
            if sock.bound.is_some() {
                return Err(Errno(EINVAL));
            }
            let v6only = sock.opts.v6only;
            let (ip, port) = match self.inet_addr(addr, v6only, EINVAL) {
                Err(Errno(EAFNOSUPPORT)) if self.family == AF_INET6 => return Err(Errno(EINVAL)),
                r => r?,
            };
            let ip = if ip.is_unspecified() {
                None
            } else {
                if !s.is_local(ip) {
                    return Err(Errno(EADDRNOTAVAIL));
                }
                Some(ip)
            };
            s.do_bind(self.id, ip, port).map(|_| ())
        })
    }

    fn listen(&self, backlog: i32) -> Result<(), Errno> {
        stack::with(true, |s| {
            let sock = s.sock(self.id)?;
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
            s.autobind(self.id)?;
            let sock = s.sock(self.id)?;
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
            s.ports.set_listening(self.id, true);
            s.listeners.push(self.id);
            s.refill(self.id);
            Ok(())
        })
    }

    fn accept(&self) -> Result<(Arc<dyn File>, SockAddr), Errno> {
        let (id, peer) = stack::with(true, |s| {
            let full = s.socks.len() >= MAX_SOCKETS;
            let linger = crate::arch::timer::now_ns() + LINGER_NS;
            let Stack {
                socks,
                sockets,
                zombies,
                ..
            } = &mut *s;
            let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
            let opts = sock.opts.clone();
            let family = sock.family;
            let Kind::Tcp(Tcp::Listen(l)) = &mut sock.kind else {
                return Err(Errno(EINVAL));
            };
            let mut found = None;
            let mut i = 0;
            while i < l.pending.len() {
                let h = l.pending[i];
                let t = sockets.get_mut::<tcp::Socket>(h);
                if !accept_ready(t.state()) {
                    i += 1;
                    continue;
                }
                let peer = t.remote_endpoint();
                if peer.is_some_and(|p| !family_ok(family, opts.v6only, ip_of(p.addr))) {
                    // Wildcard endpoints match both families in smoltcp:
                    // refuse a connection of the wrong family.
                    t.abort();
                    l.pending.remove(i);
                    zombies.push((h, linger));
                    continue;
                }
                if full {
                    return Err(Errno(ENFILE));
                }
                l.pending.remove(i);
                found = Some((h, peer, t.local_endpoint(), t.state()));
                break;
            }
            let Some((h, peer, local, st)) = found else {
                return Err(Errno(EAGAIN));
            };
            let mut ns = Sock::new(
                family,
                Kind::Tcp(Tcp::Conn(Conn {
                    h,
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
            ns.opts = opts;
            ns.opts.misc.clear();
            let id = s.alloc_id();
            s.socks.insert(id, ns);
            s.refill(self.id);
            Ok((id, peer))
        })?;
        let peer = match peer {
            Some(p) => self.view(ip_of(p.addr), p.port),
            None => self.view(self.wildcard(), 0),
        };
        let f: Arc<dyn File> = Arc::new(InetSocket {
            id,
            family: self.family,
            ty: self.ty,
            proto: self.proto,
            ino: alloc_ino(),
        });
        Ok((f, peer))
    }

    fn connect(&self, addr: &SockAddr) -> Result<(), Errno> {
        stack::with(true, |s| {
            let sock = s.sock(self.id)?;
            let v6only = sock.opts.v6only;
            match &mut sock.kind {
                Kind::Udp(_) => {
                    if *addr == SockAddr::Unspec {
                        if let Kind::Udp(u) = &mut sock.kind {
                            u.peer = None;
                            u.local = None;
                        }
                        return Ok(());
                    }
                    let (ip, port) = self.inet_addr(addr, v6only, ENETUNREACH)?;
                    let ip = unspecified_to_loopback(ip);
                    if !s.routable(ip) {
                        return Err(Errno(ENETUNREACH));
                    }
                    s.autobind(self.id)?;
                    let src = if ip.is_loopback() {
                        Some(ip)
                    } else {
                        s.iface.get_source_address(&ip.into()).map(ip_of)
                    };
                    if let Kind::Udp(u) = &mut s.sock(self.id)?.kind {
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
            if !s.routable(ip) {
                return Err(Errno(ENETUNREACH));
            }
            s.autobind(self.id)?;
            let sock = s.sock(self.id)?;
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
            let h = s.sockets.add(t);
            let sock = s.sock(self.id)?;
            sock.kind = Kind::Tcp(Tcp::Conn(Conn {
                h,
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
        stack::with(false, |s| {
            let sock = s.sock(self.id).ok()?;
            match &sock.kind {
                Kind::Tcp(Tcp::Conn(c)) if c.connecting => None,
                Kind::Tcp(Tcp::Conn(c)) if c.est && sock.error == 0 => Some(Ok(())),
                _ => Some(Err(sock.take_error().unwrap_or(Errno(ECONNREFUSED)))),
            }
        })
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
        stack::with(true, |s| {
            if self.ty == SOCK_STREAM {
                // Linux ignores the address on a connected stream socket
                // (EISCONN only for some families).
                self.tcp_send(s, data)
            } else {
                self.udp_send(s, data, to)
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
        // Polling after a read sends the window update.
        stack::with(!peek, |s| {
            if self.ty == SOCK_STREAM {
                self.tcp_recv(s, buf, peek)
            } else {
                self.udp_recv(s, buf, flags)
            }
        })
    }

    fn shutdown(&self, how: i32) -> Result<(), Errno> {
        if !(SHUT_RD..=SHUT_RDWR).contains(&how) {
            return Err(Errno(EINVAL));
        }
        let rd = how != SHUT_WR;
        let wr = how != SHUT_RD;
        stack::with(true, |s| {
            let Stack { socks, sockets, .. } = s;
            let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
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
        stack::with(false, |s| {
            let Stack { socks, sockets, .. } = s;
            let sock = socks.get(&self.id).ok_or(Errno(EBADF))?;
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
        stack::with(false, |s| {
            let sock = s.sock(self.id)?;
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
        stack::with(false, |s| {
            let Stack {
                socks,
                sockets,
                ports,
                ..
            } = s;
            let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
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
                    ports.set_reuse(self.id, o.reuse);
                }
                (SOL_SOCKET, SO_REUSEPORT) => {
                    o.reuse.port = v != 0;
                    ports.set_reuse(self.id, o.reuse);
                }
                (SOL_SOCKET, SO_KEEPALIVE) => o.keepalive = v != 0,
                (SOL_SOCKET, SO_RCVBUF | SO_SNDBUF | SO_RCVBUFFORCE | SO_SNDBUFFORCE) => {}
                (
                    SOL_SOCKET,
                    SO_BROADCAST | SO_OOBINLINE | SO_PRIORITY | SO_RCVLOWAT | SO_SNDLOWAT
                    | SO_DONTROUTE | SO_NO_CHECK | SO_TIMESTAMP_OLD,
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
                (SOL_IPV6, _) if self.family != AF_INET6 => return Err(Errno(ENOPROTOOPT)),
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
        })
    }

    fn getsockopt(&self, level: i32, name: i32, out: &mut [u8]) -> Result<usize, Errno> {
        stack::with(false, |s| {
            let sock = s.socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
            if (level, name) == (SOL_SOCKET, SO_ERROR) {
                let e = core::mem::take(&mut sock.error);
                return Ok(so::int_bytes(e, out));
            }
            let sock = s.socks.get(&self.id).unwrap();
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
        stack::with(false, |s| {
            s.socks
                .get(&self.id)
                .map_or((None, None), |x| (x.opts.rcvtimeo, x.opts.sndtimeo))
        })
    }
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
        let Some(lock) = stack::get() else {
            return EpollFlags::ERR | EpollFlags::HUP;
        };
        let mut g = stack::lock_at(lock, super::prof::Site::FilePoll);
        let s = &mut *g;
        let Some(sock) = s.socks.get_mut(&self.id) else {
            return EpollFlags::ERR | EpollFlags::HUP;
        };
        if let Some(h) = hook {
            sock.hooks.add(h);
        }
        sock.ready(&s.sockets).0
    }

    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        let n: i32 = match cmd {
            FIONREAD | SIOCOUTQ => stack::with(false, |s| {
                let Stack { socks, sockets, .. } = s;
                let sock = socks.get_mut(&self.id).ok_or(Errno(EBADF))?;
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
        if stack::get().is_some() {
            stack::with(true, |s| s.close_sock(self.id));
        }
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }
}
