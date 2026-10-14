//! AF_VSOCK stream sockets (decision D76): the transport-independent core.
//!
//! [`Core`] holds every vsock stream socket of the kernel (the program's
//! and the control channel's) and runs the virtio-vsock connection state
//! machine of Linux `net/vmw_vsock/{af_vsock,virtio_transport_common}.c`
//! over them: the bound and connected tables, `REQUEST`/`RESPONSE`/`RST`/
//! `SHUTDOWN`/`RW`/`CREDIT_*` handling, per-connection credit
//! (`buf_alloc`, `fwd_cnt`, `tx_cnt`, the peer's values), the receive
//! buffers, ephemeral ports, connect and close timeouts, and readiness.
//! It never blocks and never touches hardware: packets come in through
//! [`Core::input`] and go out through [`Core::take_out`], each tagged with
//! its transport ([`Via`]): the virtio device, or the in-kernel loopback
//! (Linux `vsock_loopback`), whose packets the kernel feeds straight back
//! into `input`. Time is passed in (`now`, ns); [`Core::tick`] runs the
//! timeouts and [`Core::next_deadline`] says when it next has to.
//!
//! Sockets are ids; the kernel's fd-side object holds the id (as AF_UNIX
//! does, D71). [`Core::take_dirty`] lists the sockets whose readiness may
//! have changed since the last call.
//!
//! Faithful to Linux except: a packet reaching a bound socket that is not
//! listening and not waiting for that peer's `RESPONSE` is answered with
//! `RST` (Linux delivers it through the bound table to whatever state the
//! socket is in); a stalled sender sends one `CREDIT_REQUEST` (Linux only
//! answers them); `shutdown` does not clear `SOCK_DONE`; a fresh `connect`
//! resets the credit counters and the pending error; `SOCK_SEQPACKET` and
//! `SOCK_DGRAM` do not exist here.

pub mod hdr;

#[cfg(test)]
mod tests;

pub use hdr::*;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::net::{SOMAXCONN, VMADDR_CID_ANY, VMADDR_CID_LOCAL, VMADDR_PORT_ANY};

/// `VSOCK_DEFAULT_BUFFER_SIZE` / `_MAX_SIZE` / `_MIN_SIZE`.
pub const DEFAULT_BUFFER_SIZE: u64 = 256 * 1024;
pub const DEFAULT_BUFFER_MAX_SIZE: u64 = 256 * 1024;
pub const DEFAULT_BUFFER_MIN_SIZE: u64 = 128;
/// `VIRTIO_VSOCK_MAX_BUF_SIZE`: the largest `buf_alloc`.
pub const MAX_BUF_SIZE: u64 = 0xFFFF_FFFF;
/// `VIRTIO_VSOCK_MAX_PKT_BUF_SIZE`: largest loopback payload, and the
/// free-space threshold below which a reader sends `CREDIT_UPDATE`.
pub const MAX_PKT_BUF_SIZE: usize = 64 * 1024;
/// `VSOCK_DEFAULT_CONNECT_TIMEOUT` (2 s).
pub const DEFAULT_CONNECT_TIMEOUT_NS: u64 = 2_000_000_000;
/// `VSOCK_CLOSE_TIMEOUT` (8 s): how long a closed socket waits for the
/// peer's `RST` after sending `SHUTDOWN`.
pub const CLOSE_TIMEOUT_NS: u64 = 8_000_000_000;
/// Ports up to this need `CAP_NET_BIND_SERVICE` (always held here) and
/// are never picked for autobind.
pub const LAST_RESERVED_PORT: u32 = 1023;
/// Autobind tries this many consecutive ports.
pub const MAX_PORT_RETRIES: u32 = 24;
/// Default device payload per packet: one 4 KiB buffer minus the header.
pub const DEFAULT_DEV_PAYLOAD: usize = 4096 - HDR_LEN;

/// `sk_shutdown` / `peer_shutdown` bits.
pub const RCV_SHUTDOWN: u8 = 1;
pub const SEND_SHUTDOWN: u8 = 2;
pub const SHUTDOWN_MASK: u8 = 3;

/// A socket id.
pub type SockId = u64;

/// A vsock address (`sockaddr_vm` without the family).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr {
    pub cid: u32,
    pub port: u32,
}

impl Addr {
    /// Unbound: `VMADDR_CID_ANY`, `VMADDR_PORT_ANY`.
    pub const ANY: Addr = Addr {
        cid: VMADDR_CID_ANY,
        port: VMADDR_PORT_ANY,
    };

    pub const fn new(cid: u32, port: u32) -> Addr {
        Addr { cid, port }
    }

    fn of_src(h: &Hdr) -> Addr {
        Addr::new(h.src_cid as u32, h.src_port)
    }

    fn of_dst(h: &Hdr) -> Addr {
        Addr::new(h.dst_cid as u32, h.dst_port)
    }

    /// Bound-table match (Linux `__vsock_find_bound_socket`): same port,
    /// and the same CID or either side `VMADDR_CID_ANY`.
    fn covers(&self, o: &Addr) -> bool {
        self.port == o.port
            && (self.cid == o.cid || self.cid == VMADDR_CID_ANY || o.cid == VMADDR_CID_ANY)
    }
}

/// The transport a packet travels on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// The virtio-vsock device (to the host or, through it, elsewhere).
    Device,
    /// The in-kernel loopback (`VMADDR_CID_LOCAL` or the guest's own CID).
    Loopback,
}

/// One packet: header and payload (`hdr.len == payload.len()` on output).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub via: Via,
    pub hdr: Hdr,
    pub payload: Vec<u8>,
}

/// `sk->sk_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sk {
    Close,
    Listen,
    SynSent,
    Established,
    Closing,
}

/// `sock->state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ss {
    Unconnected,
    Connecting,
    Connected,
    Disconnecting,
}

struct Sock {
    sk: Sk,
    ss: Ss,
    local: Addr,
    remote: Addr,
    /// In the bound table (bound by `bind`/autobind; accepted sockets are
    /// not).
    bound: bool,
    /// In the connected table.
    connected: bool,
    /// The transport (`vsk->transport`); `None` until `connect` or accept.
    via: Option<Via>,
    shutdown: u8,
    peer_shutdown: u8,
    /// `SOCK_DONE`: the connection is over (peer `RST`, or both sides shut).
    done: bool,
    /// `sk_err` (positive errno).
    err: i32,
    // Credit (`struct virtio_vsock_sock`).
    buf_alloc: u32,
    fwd_cnt: u32,
    last_fwd_cnt: u32,
    rx: VecDeque<u8>,
    peer_buf_alloc: u32,
    peer_fwd_cnt: u32,
    tx_cnt: u32,
    /// A `CREDIT_REQUEST` went out since the peer last told us its credit.
    credit_requested: bool,
    // Options.
    buffer_size: u64,
    buffer_min: u64,
    buffer_max: u64,
    connect_timeout: u64,
    // Timers.
    connect_deadline: Option<u64>,
    /// Set when the fd was released and the socket waits for the peer's
    /// `RST` (`close_work_scheduled`).
    close_deadline: Option<u64>,
    // Listener.
    backlog: usize,
    queue: VecDeque<SockId>,
    /// Kernel-owned (the control channel): exempt from the port
    /// reservation, and its listener accepts only device connections.
    kernel: bool,
}

impl Sock {
    fn new() -> Sock {
        Sock {
            sk: Sk::Close,
            ss: Ss::Unconnected,
            local: Addr::ANY,
            remote: Addr::ANY,
            bound: false,
            connected: false,
            via: None,
            shutdown: 0,
            peer_shutdown: 0,
            done: false,
            err: 0,
            buf_alloc: DEFAULT_BUFFER_SIZE as u32,
            fwd_cnt: 0,
            last_fwd_cnt: 0,
            rx: VecDeque::new(),
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
            credit_requested: false,
            buffer_size: DEFAULT_BUFFER_SIZE,
            buffer_min: DEFAULT_BUFFER_MIN_SIZE,
            buffer_max: DEFAULT_BUFFER_MAX_SIZE,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT_NS,
            connect_deadline: None,
            close_deadline: None,
            backlog: 0,
            queue: VecDeque::new(),
            kernel: false,
        }
    }

    /// `virtio_transport_has_space`: credit the peer gives us now.
    fn has_space(&self) -> u32 {
        let in_flight = self.tx_cnt.wrapping_sub(self.peer_fwd_cnt);
        // Signed like Linux's s64 arithmetic: a peer shrinking its
        // buf_alloc below what is in flight leaves no space, not 4 GiB.
        let v = self.peer_buf_alloc as i64 - in_flight as i64;
        v.max(0) as u32
    }

    /// `virtio_transport_get_credit`: reserve up to `want` bytes.
    fn get_credit(&mut self, want: usize) -> usize {
        let n = (self.has_space() as usize).min(want);
        self.tx_cnt = self.tx_cnt.wrapping_add(n as u32);
        n
    }

    fn rx_bytes(&self) -> usize {
        self.rx.len()
    }

    /// Reset the per-connection transport state (a new `connect`).
    fn reset_transport(&mut self) {
        self.fwd_cnt = 0;
        self.last_fwd_cnt = 0;
        self.rx.clear();
        self.peer_buf_alloc = 0;
        self.peer_fwd_cnt = 0;
        self.tx_cnt = 0;
        self.credit_requested = false;
        self.peer_shutdown = 0;
        self.done = false;
        self.err = 0;
        self.buf_alloc = self.buffer_size.min(MAX_BUF_SIZE) as u32;
    }
}

/// A read-only view of one socket (kernel glue and tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub sk: Sk,
    pub ss: Ss,
    pub local: Addr,
    pub remote: Addr,
    pub via: Option<Via>,
    pub rx_bytes: usize,
    pub shutdown: u8,
    pub peer_shutdown: u8,
    pub done: bool,
    pub err: i32,
    pub buf_alloc: u32,
    pub fwd_cnt: u32,
    pub tx_cnt: u32,
    pub peer_buf_alloc: u32,
    pub peer_fwd_cnt: u32,
    pub credit: u32,
    pub queued: usize,
    pub orphan: bool,
}

/// All vsock stream sockets and the transports' view of them.
pub struct Core {
    socks: BTreeMap<SockId, Sock>,
    next_id: SockId,
    /// Autobind cursor (Linux's `static u32 port`); 0 until first used.
    next_port: u32,
    seed: u64,
    /// The device's guest CID (`None`: no device; only loopback).
    guest_cid: Option<u32>,
    /// Ports only kernel sockets may bind (the control channel's 4242).
    reserved: Vec<u32>,
    /// Largest payload per device packet.
    dev_payload: usize,
    out: Vec<Packet>,
    dirty: BTreeSet<SockId>,
}

fn sock_mut(socks: &mut BTreeMap<SockId, Sock>, id: SockId) -> Result<&mut Sock, Errno> {
    socks.get_mut(&id).ok_or(Errno(EBADF))
}

impl Core {
    /// `seed` picks where ephemeral port allocation starts.
    pub fn new(seed: u64) -> Core {
        Core {
            socks: BTreeMap::new(),
            next_id: 1,
            next_port: 0,
            seed,
            guest_cid: None,
            reserved: Vec::new(),
            dev_payload: DEFAULT_DEV_PAYLOAD,
            out: Vec::new(),
            dirty: BTreeSet::new(),
        }
    }

    /// A device appeared (or its CID changed): `Some(cid)`; `None`: gone.
    pub fn set_device(&mut self, cid: Option<u32>) {
        self.guest_cid = cid;
    }

    /// The device's guest CID.
    pub fn guest_cid(&self) -> Option<u32> {
        self.guest_cid
    }

    /// `IOCTL_VM_SOCKETS_GET_LOCAL_CID`: the guest CID, or
    /// `VMADDR_CID_ANY` without a device (Linux with only the loopback
    /// transport has neither g2h nor h2g).
    pub fn local_cid(&self) -> u32 {
        self.guest_cid.unwrap_or(VMADDR_CID_ANY)
    }

    /// Largest payload per device packet (the device's buffer size minus
    /// the header).
    pub fn set_dev_payload(&mut self, n: usize) {
        self.dev_payload = n.max(1);
    }

    /// Reserve `port` for kernel sockets: user binds get `EADDRINUSE` and
    /// autobind skips it.
    pub fn reserve(&mut self, port: u32) {
        self.reserved.push(port);
    }

    /// Packets to transmit (loopback ones go back into [`Core::input`]).
    pub fn take_out(&mut self) -> Vec<Packet> {
        core::mem::take(&mut self.out)
    }

    /// Whether packets are waiting in the outbox.
    pub fn has_out(&self) -> bool {
        !self.out.is_empty()
    }

    /// Sockets whose readiness may have changed (including removed ones).
    pub fn take_dirty(&mut self) -> Vec<SockId> {
        let d = core::mem::take(&mut self.dirty);
        d.into_iter().collect()
    }

    fn dirty(&mut self, id: SockId) {
        self.dirty.insert(id);
    }

    fn add(&mut self, s: Sock) -> SockId {
        let id = self.next_id;
        self.next_id += 1;
        self.socks.insert(id, s);
        id
    }

    /// `socket(AF_VSOCK, SOCK_STREAM, 0)`.
    pub fn create(&mut self) -> SockId {
        self.add(Sock::new())
    }

    /// A kernel-owned socket (may bind reserved ports; as a listener it
    /// only accepts connections from the device).
    pub fn create_kernel(&mut self) -> SockId {
        let mut s = Sock::new();
        s.kernel = true;
        self.add(s)
    }

    pub fn exists(&self, id: SockId) -> bool {
        self.socks.contains_key(&id)
    }

    /// Number of sockets (open, orphaned or waiting for accept).
    pub fn len(&self) -> usize {
        self.socks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.socks.is_empty()
    }

    pub fn info(&self, id: SockId) -> Option<Info> {
        let s = self.socks.get(&id)?;
        Some(Info {
            sk: s.sk,
            ss: s.ss,
            local: s.local,
            remote: s.remote,
            via: s.via,
            rx_bytes: s.rx_bytes(),
            shutdown: s.shutdown,
            peer_shutdown: s.peer_shutdown,
            done: s.done,
            err: s.err,
            buf_alloc: s.buf_alloc,
            fwd_cnt: s.fwd_cnt,
            tx_cnt: s.tx_cnt,
            peer_buf_alloc: s.peer_buf_alloc,
            peer_fwd_cnt: s.peer_fwd_cnt,
            credit: s.has_space(),
            queued: s.queue.len(),
            orphan: s.close_deadline.is_some(),
        })
    }

    // -----------------------------------------------------------------
    // Transmit helpers.

    fn src_cid(&self, via: Via) -> u32 {
        match via {
            // `vsock_loopback_get_local_cid`.
            Via::Loopback => VMADDR_CID_LOCAL,
            Via::Device => self.guest_cid.unwrap_or(VMADDR_CID_ANY),
        }
    }

    /// `virtio_transport_send_pkt_info`: send `op` on socket `id` (to its
    /// peer, or to `to` for replies). `RW` is limited by the peer's credit
    /// and split into packets; returns the payload bytes sent.
    fn send_pkt(
        &mut self,
        id: SockId,
        op: u16,
        flags: u32,
        data: &[u8],
        to: Option<Addr>,
    ) -> usize {
        let Some(via) = self.socks.get(&id).and_then(|s| s.via) else {
            return 0;
        };
        let src_cid = self.src_cid(via);
        let max = match via {
            Via::Device => self.dev_payload,
            Via::Loopback => MAX_PKT_BUF_SIZE,
        };
        let s = self.socks.get_mut(&id).expect("checked");
        let mut len = data.len();
        if op == OP_RW {
            len = s.get_credit(len);
            if len == 0 {
                return 0;
            }
        }
        let dst = to.unwrap_or(s.remote);
        let mut off = 0;
        loop {
            let n = (len - off).min(max);
            // `virtio_transport_inc_tx_pkt`: every packet carries our
            // current credit.
            s.last_fwd_cnt = s.fwd_cnt;
            let hdr = Hdr {
                src_cid: src_cid as u64,
                dst_cid: dst.cid as u64,
                src_port: s.local.port,
                dst_port: dst.port,
                len: n as u32,
                ty: TYPE_STREAM,
                op,
                flags,
                buf_alloc: s.buf_alloc,
                fwd_cnt: s.fwd_cnt,
            };
            self.out.push(Packet {
                via,
                hdr,
                payload: data[off..off + n].to_vec(),
            });
            off += n;
            if off >= len {
                break;
            }
        }
        len
    }

    /// `virtio_transport_reset`: `RST` to the peer unless answering one.
    fn reset(&mut self, id: SockId, answering: Option<u16>) {
        if answering == Some(OP_RST) {
            return;
        }
        self.send_pkt(id, OP_RST, 0, &[], None);
    }

    /// `virtio_transport_reset_no_sock`: answer a packet nobody takes.
    fn reset_no_sock(&mut self, via: Via, h: &Hdr) {
        if h.op == OP_RST {
            return;
        }
        let mut r = h.reply(OP_RST);
        r.ty = h.ty;
        self.out.push(Packet {
            via,
            hdr: r,
            payload: Vec::new(),
        });
    }

    fn send_credit_update(&mut self, id: SockId) {
        self.send_pkt(id, OP_CREDIT_UPDATE, 0, &[], None);
    }

    // -----------------------------------------------------------------
    // Tables.

    /// Is `a` taken in the bound table (or reserved, for user sockets)?
    fn in_use(&self, a: &Addr, kernel: bool) -> bool {
        (!kernel && self.reserved.contains(&a.port))
            || self.socks.values().any(|s| s.bound && s.local.covers(a))
    }

    /// `vsock_find_cid`: may a socket bind `cid`?
    fn find_cid(&self, cid: u32) -> bool {
        cid == VMADDR_CID_LOCAL || self.guest_cid == Some(cid)
    }

    /// Does `s` (connected over `via`) talk to `src`? Loopback packets
    /// always carry `VMADDR_CID_LOCAL` as the source while the connecting
    /// side may have named the guest CID, so only ports count there (Linux
    /// finds such a socket through its bound address instead).
    fn peer_is(s: &Sock, via: Via, src: &Addr) -> bool {
        s.via == Some(via)
            && s.remote.port == src.port
            && (via == Via::Loopback || s.remote.cid == src.cid)
    }

    fn find_connected(&self, via: Via, src: &Addr, dst: &Addr) -> Option<SockId> {
        self.socks
            .iter()
            .find(|(_, s)| s.connected && s.local.port == dst.port && Self::peer_is(s, via, src))
            .map(|(id, _)| *id)
    }

    /// The bound socket a packet for `dst` goes to: a listener, or a
    /// socket waiting for this peer's `RESPONSE`.
    fn find_bound(&self, via: Via, src: &Addr, dst: &Addr) -> Option<SockId> {
        let (id, s) = self
            .socks
            .iter()
            .find(|(_, s)| s.bound && s.local.covers(dst))?;
        match s.sk {
            Sk::Listen => Some(*id),
            Sk::SynSent if Self::peer_is(s, via, src) => Some(*id),
            _ => None,
        }
    }

    // -----------------------------------------------------------------
    // Receive path (`virtio_transport_recv_pkt`).

    /// Process one received packet (`payload.len()` is the data length).
    pub fn input(&mut self, p: Packet) {
        let Packet {
            via,
            hdr: h,
            payload,
        } = p;
        if h.ty != TYPE_STREAM && h.ty != TYPE_SEQPACKET {
            self.reset_no_sock(via, &h);
            return;
        }
        let src = Addr::of_src(&h);
        let dst = Addr::of_dst(&h);
        let id = self
            .find_connected(via, &src, &dst)
            .or_else(|| self.find_bound(via, &src, &dst));
        let Some(id) = id else {
            self.reset_no_sock(via, &h);
            return;
        };
        let s = self.socks.get_mut(&id).expect("found");
        // Only stream sockets exist; `SOCK_DONE` sockets take nothing.
        if h.ty != TYPE_STREAM || s.done {
            self.reset_no_sock(via, &h);
            return;
        }
        // `virtio_transport_space_update` (listeners have no transport).
        if s.sk != Sk::Listen {
            s.peer_buf_alloc = h.buf_alloc;
            s.peer_fwd_cnt = h.fwd_cnt;
            if s.has_space() > 0 {
                s.credit_requested = false;
                self.dirty.insert(id);
            }
        }
        // The CID may have changed after a transport reset.
        if s.local.cid != VMADDR_CID_ANY && s.sk != Sk::Listen {
            s.local.cid = dst.cid;
        }
        match s.sk {
            Sk::Listen => self.recv_listen(id, via, &h),
            Sk::SynSent => self.recv_connecting(id, &h),
            Sk::Established => self.recv_connected(id, &h, payload),
            Sk::Closing => {
                if h.op == OP_RST {
                    self.do_close(id);
                }
            }
            Sk::Close => self.reset_no_sock(via, &h),
        }
    }

    fn recv_listen(&mut self, lid: SockId, via: Via, h: &Hdr) {
        let l = &self.socks[&lid];
        // `sk_acceptq_is_full`: more queued than the backlog.
        if h.op != OP_REQUEST || l.queue.len() > l.backlog || (l.kernel && via != Via::Device) {
            self.reset_no_sock(via, h);
            return;
        }
        let mut c = Sock::new();
        c.sk = Sk::Established;
        c.ss = Ss::Connected;
        c.local = Addr::of_dst(h);
        c.remote = Addr::of_src(h);
        c.via = Some(via);
        c.connected = true;
        c.kernel = l.kernel;
        c.buffer_size = l.buffer_size;
        c.buffer_min = l.buffer_min;
        c.buffer_max = l.buffer_max;
        c.connect_timeout = l.connect_timeout;
        c.buf_alloc = l.buffer_size.min(MAX_BUF_SIZE) as u32;
        c.peer_buf_alloc = h.buf_alloc;
        c.peer_fwd_cnt = h.fwd_cnt;
        let remote = c.remote;
        let cid = self.add(c);
        self.socks
            .get_mut(&lid)
            .expect("listener")
            .queue
            .push_back(cid);
        self.send_pkt(cid, OP_RESPONSE, 0, &[], Some(remote));
        self.dirty(lid);
    }

    fn recv_connecting(&mut self, id: SockId, h: &Hdr) {
        let err = match h.op {
            OP_RESPONSE => {
                let s = self.socks.get_mut(&id).expect("exists");
                s.sk = Sk::Established;
                s.ss = Ss::Connected;
                s.connected = true;
                s.connect_deadline = None;
                self.dirty(id);
                return;
            }
            OP_INVALID => return,
            // Linux: a reset while connecting is ECONNRESET, not
            // ECONNREFUSED (`virtio_transport_recv_connecting`).
            OP_RST => ECONNRESET,
            _ => EPROTO,
        };
        self.reset(id, Some(h.op));
        let s = self.socks.get_mut(&id).expect("exists");
        s.sk = Sk::Close;
        s.ss = Ss::Unconnected;
        s.err = err;
        s.connect_deadline = None;
        self.dirty(id);
    }

    fn recv_connected(&mut self, id: SockId, h: &Hdr, payload: Vec<u8>) {
        match h.op {
            OP_RW => {
                let s = self.socks.get_mut(&id).expect("exists");
                // `virtio_transport_inc_rx_pkt`: a peer exceeding our
                // advertised buffer loses the packet.
                if s.rx_bytes() + payload.len() > s.buf_alloc as usize {
                    return;
                }
                s.rx.extend(payload.iter());
                self.dirty(id);
            }
            OP_CREDIT_REQUEST => self.send_credit_update(id),
            OP_CREDIT_UPDATE => self.dirty(id),
            OP_SHUTDOWN => {
                let s = self.socks.get_mut(&id).expect("exists");
                s.peer_shutdown |= (h.flags & (SHUTDOWN_RCV | SHUTDOWN_SEND)) as u8;
                if s.peer_shutdown == SHUTDOWN_MASK {
                    if s.rx_bytes() == 0 && !s.done {
                        self.reset(id, None);
                        self.do_close(id);
                    }
                    // Out of the connected table either way, so the peer
                    // can reconnect from the same port.
                    if let Some(s) = self.socks.get_mut(&id) {
                        s.connected = false;
                    }
                }
                if h.flags != 0 {
                    self.dirty(id);
                }
            }
            OP_RST => self.do_close(id),
            _ => {}
        }
    }

    /// `virtio_transport_do_close`: the connection is over. An orphan (its
    /// fd closed, waiting for this) is removed.
    fn do_close(&mut self, id: SockId) {
        let Some(s) = self.socks.get_mut(&id) else {
            return;
        };
        s.done = true;
        s.peer_shutdown = SHUTDOWN_MASK;
        if s.rx_bytes() == 0 {
            s.sk = Sk::Closing;
        }
        s.connect_deadline = None;
        let orphan = s.close_deadline.is_some();
        self.dirty(id);
        if orphan {
            self.socks.remove(&id);
        }
    }

    /// An event-queue `TRANSPORT_RESET`: the device's connections are gone
    /// (`virtio_vsock_reset_sock`: `TCP_CLOSE`, `ECONNRESET`) and the guest
    /// CID may have changed.
    pub fn transport_reset(&mut self, cid: Option<u32>) {
        self.guest_cid = cid;
        let ids: Vec<SockId> = self
            .socks
            .iter()
            .filter(|(_, s)| s.connected && s.via == Some(Via::Device))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let s = self.socks.get_mut(&id).expect("listed");
            s.sk = Sk::Close;
            s.err = ECONNRESET;
            self.dirty(id);
        }
    }

    /// Run expired timers: connect timeouts (`ETIMEDOUT`) and orphans
    /// whose peer never reset (`RST`, then removed).
    pub fn tick(&mut self, now: u64) {
        let mut expired = Vec::new();
        for (id, s) in self.socks.iter_mut() {
            if s.sk == Sk::SynSent && s.connect_deadline.is_some_and(|d| now >= d) {
                s.sk = Sk::Close;
                s.ss = Ss::Unconnected;
                s.err = ETIMEDOUT;
                s.connect_deadline = None;
                self.dirty.insert(*id);
            }
            if s.close_deadline.is_some_and(|d| now >= d) {
                expired.push((*id, s.done));
            }
        }
        for (id, done) in expired {
            if !done {
                self.reset(id, None);
            }
            self.socks.remove(&id);
            self.dirty(id);
        }
    }

    /// The earliest pending timer.
    pub fn next_deadline(&self) -> Option<u64> {
        self.socks
            .values()
            .flat_map(|s| {
                let c = if s.sk == Sk::SynSent {
                    s.connect_deadline
                } else {
                    None
                };
                [c, s.close_deadline]
            })
            .flatten()
            .min()
    }

    // -----------------------------------------------------------------
    // Socket calls.

    /// `bind`. `VMADDR_PORT_ANY` picks an ephemeral port.
    pub fn bind(&mut self, id: SockId, a: Addr) -> Result<(), Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        if s.local.port != VMADDR_PORT_ANY {
            return Err(Errno(EINVAL));
        }
        if a.cid != VMADDR_CID_ANY && !self.find_cid(a.cid) {
            return Err(Errno(EADDRNOTAVAIL));
        }
        self.bind_connectible(id, a)
    }

    /// `__vsock_bind_connectible`.
    fn bind_connectible(&mut self, id: SockId, a: Addr) -> Result<(), Errno> {
        let kernel = self.socks[&id].kernel;
        let port = if a.port == VMADDR_PORT_ANY {
            if self.next_port == 0 {
                let span = (VMADDR_PORT_ANY - 1 - LAST_RESERVED_PORT) as u64;
                self.next_port = LAST_RESERVED_PORT + 1 + (self.seed % span) as u32;
            }
            let mut found = None;
            for _ in 0..MAX_PORT_RETRIES {
                if self.next_port <= LAST_RESERVED_PORT || self.next_port == VMADDR_PORT_ANY {
                    self.next_port = LAST_RESERVED_PORT + 1;
                }
                let p = self.next_port;
                self.next_port = self.next_port.wrapping_add(1);
                if !self.in_use(&Addr::new(a.cid, p), false) {
                    found = Some(p);
                    break;
                }
            }
            found.ok_or(Errno(EADDRNOTAVAIL))?
        } else {
            if self.in_use(&a, kernel) {
                return Err(Errno(EADDRINUSE));
            }
            a.port
        };
        let s = self.socks.get_mut(&id).expect("exists");
        s.local = Addr::new(a.cid, port);
        s.bound = true;
        Ok(())
    }

    fn auto_bind(&mut self, id: SockId) -> Result<(), Errno> {
        if self.socks[&id].local.port != VMADDR_PORT_ANY {
            return Ok(());
        }
        self.bind_connectible(id, Addr::ANY)
    }

    /// `listen`. The backlog is clamped like `__sys_listen` (as unsigned,
    /// to `SOMAXCONN`); the queue is full when longer than it.
    pub fn listen(&mut self, id: SockId, backlog: i32) -> Result<(), Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        if s.ss != Ss::Unconnected || s.local.port == VMADDR_PORT_ANY {
            return Err(Errno(EINVAL));
        }
        s.backlog = (backlog as u32).min(SOMAXCONN as u32) as usize;
        s.sk = Sk::Listen;
        self.dirty(id);
        Ok(())
    }

    /// `accept`: the first established connection and its peer address.
    pub fn accept(&mut self, id: SockId) -> Result<(SockId, Addr), Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        if s.sk != Sk::Listen {
            return Err(Errno(EINVAL));
        }
        let Some(c) = s.queue.pop_front() else {
            if s.err != 0 {
                return Err(Errno(s.err));
            }
            return Err(Errno(EAGAIN));
        };
        self.dirty(id);
        let remote = self.socks.get(&c).map_or(Addr::ANY, |c| c.remote);
        Ok((c, remote))
    }

    /// The transport for a connection to `cid` (`vsock_assign_transport`
    /// with only g2h + loopback).
    fn transport_for(&self, cid: u32) -> Result<Via, Errno> {
        if cid == VMADDR_CID_LOCAL {
            return Ok(Via::Loopback);
        }
        match self.guest_cid {
            Some(g) if g == cid => Ok(Via::Loopback),
            Some(_) => Ok(Via::Device),
            None => Err(Errno(ENODEV)),
        }
    }

    /// `connect`: sends `REQUEST` and returns `EINPROGRESS`; the outcome
    /// comes from [`Core::connect_result`] once the socket is writable.
    pub fn connect(&mut self, id: SockId, a: Addr, now: u64) -> Result<(), Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        match s.ss {
            Ss::Connected => return Err(Errno(EISCONN)),
            Ss::Disconnecting => return Err(Errno(EINVAL)),
            Ss::Connecting if s.sk == Sk::SynSent => return Err(Errno(EALREADY)),
            _ => {}
        }
        if s.sk == Sk::Listen {
            return Err(Errno(EINVAL));
        }
        s.remote = a;
        let via = self.transport_for(a.cid)?;
        let s = self.socks.get_mut(&id).expect("exists");
        s.via = Some(via);
        s.reset_transport();
        self.auto_bind(id)?;
        let s = self.socks.get_mut(&id).expect("exists");
        s.sk = Sk::SynSent;
        s.ss = Ss::Connecting;
        s.connect_deadline = Some(now.saturating_add(s.connect_timeout));
        self.send_pkt(id, OP_REQUEST, 0, &[], None);
        self.dirty(id);
        Err(Errno(EINPROGRESS))
    }

    /// The outcome of a started connect: `None` while in progress.
    pub fn connect_result(&self, id: SockId) -> Option<Result<(), Errno>> {
        let Some(s) = self.socks.get(&id) else {
            return Some(Err(Errno(EBADF)));
        };
        match (s.sk, s.ss) {
            (_, Ss::Connected | Ss::Disconnecting) => Some(Ok(())),
            (Sk::SynSent, _) => None,
            _ => Some(Err(Errno(if s.err != 0 { s.err } else { ECONNRESET }))),
        }
    }

    /// Send up to `data.len()` bytes; `EAGAIN` without peer credit.
    pub fn send(&mut self, id: SockId, data: &[u8]) -> Result<usize, Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        if s.shutdown & SEND_SHUTDOWN != 0 || s.peer_shutdown & RCV_SHUTDOWN != 0 {
            return Err(Errno(EPIPE));
        }
        if s.via.is_none() || s.sk != Sk::Established || s.local.port == VMADDR_PORT_ANY {
            return Err(Errno(ENOTCONN));
        }
        if s.err != 0 {
            return Err(Errno(s.err));
        }
        if data.is_empty() {
            return Ok(0);
        }
        if s.has_space() == 0 {
            if !s.credit_requested {
                s.credit_requested = true;
                self.send_pkt(id, OP_CREDIT_REQUEST, 0, &[], None);
            }
            return Err(Errno(EAGAIN));
        }
        Ok(self.send_pkt(id, OP_RW, 0, data, None))
    }

    /// Receive into `buf` (`peek`: leave the data). `Ok(0)` is EOF.
    pub fn recv(&mut self, id: SockId, buf: &mut [u8], peek: bool) -> Result<usize, Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        if s.via.is_none() || s.sk != Sk::Established {
            // An orderly end reads as EOF; never connected is ENOTCONN.
            return if s.done { Ok(0) } else { Err(Errno(ENOTCONN)) };
        }
        if s.shutdown & RCV_SHUTDOWN != 0 || buf.is_empty() {
            return Ok(0);
        }
        if s.rx.is_empty() {
            if s.err != 0 {
                return Err(Errno(s.err));
            }
            if s.peer_shutdown & SEND_SHUTDOWN != 0 {
                return Ok(0);
            }
            return Err(Errno(EAGAIN));
        }
        let n = buf.len().min(s.rx.len());
        for (d, b) in buf[..n].iter_mut().zip(s.rx.iter()) {
            *d = *b;
        }
        if peek {
            return Ok(n);
        }
        s.rx.drain(..n);
        s.fwd_cnt = s.fwd_cnt.wrapping_add(n as u32);
        // `virtio_transport_stream_do_dequeue`: tell the peer once little
        // advertised space is left, or the buffer ran dry.
        let delta = s.fwd_cnt.wrapping_sub(s.last_fwd_cnt);
        let free = s.buf_alloc.wrapping_sub(delta);
        let low = s.rx.is_empty();
        if delta != 0 && ((free as usize) < MAX_PKT_BUF_SIZE || low) {
            self.send_credit_update(id);
        }
        self.dirty(id);
        Ok(n)
    }

    /// `shutdown(how)` with `how` = `SHUT_RD`/`SHUT_WR`/`SHUT_RDWR`.
    pub fn shutdown(&mut self, id: SockId, how: i32) -> Result<(), Errno> {
        let mode = how.wrapping_add(1);
        if mode & !(SHUTDOWN_MASK as i32) != 0 || mode == 0 {
            return Err(Errno(EINVAL));
        }
        let s = sock_mut(&mut self.socks, id)?;
        if s.ss == Ss::Unconnected {
            return Err(Errno(ENOTCONN));
        }
        s.ss = Ss::Disconnecting;
        s.shutdown |= mode as u8;
        self.dirty(id);
        self.send_pkt(id, OP_SHUTDOWN, mode as u32, &[], None);
        Ok(())
    }

    /// The fd was closed (`__vsock_release`). A connection that may still
    /// carry data sends `SHUTDOWN` and lingers (orphaned) until the peer's
    /// `RST` or [`CLOSE_TIMEOUT_NS`]; unaccepted connections of a listener
    /// are released too.
    pub fn release(&mut self, id: SockId, now: u64) {
        let Some(s) = self.socks.get_mut(&id) else {
            return;
        };
        let pending: Vec<SockId> = s.queue.drain(..).collect();
        let remove = if s.via.is_none() {
            true
        } else {
            self.close(id, now)
        };
        self.dirty(id);
        if remove {
            self.socks.remove(&id);
        } else if let Some(s) = self.socks.get_mut(&id) {
            s.shutdown = SHUTDOWN_MASK;
        }
        for c in pending {
            self.release(c, now);
        }
    }

    /// `virtio_transport_close`: true if the socket can go now.
    fn close(&mut self, id: SockId, now: u64) -> bool {
        let s = &self.socks[&id];
        if !(s.sk == Sk::Established || s.sk == Sk::Closing) {
            return true;
        }
        if s.peer_shutdown == SHUTDOWN_MASK {
            self.reset(id, None);
            return true;
        }
        if s.shutdown != SHUTDOWN_MASK {
            self.send_pkt(id, OP_SHUTDOWN, SHUTDOWN_RCV | SHUTDOWN_SEND, &[], None);
        }
        let s = self.socks.get_mut(&id).expect("exists");
        if s.done {
            return true;
        }
        s.close_deadline = Some(now.saturating_add(CLOSE_TIMEOUT_NS));
        false
    }

    /// Drop a connection at once with `RST` (the control channel replacing
    /// its host connection).
    pub fn abort(&mut self, id: SockId) {
        let Some(s) = self.socks.get(&id) else {
            return;
        };
        if s.via.is_some() && matches!(s.sk, Sk::Established | Sk::Closing | Sk::SynSent) && !s.done
        {
            self.reset(id, None);
        }
        let pending: Vec<SockId> = self
            .socks
            .get_mut(&id)
            .map(|s| s.queue.drain(..).collect())
            .unwrap_or_default();
        self.socks.remove(&id);
        self.dirty(id);
        for c in pending {
            self.abort(c);
        }
    }

    /// `getsockname`: the bound address (`VMADDR_CID_ANY`/`PORT_ANY` when
    /// unbound; an autobound client keeps `VMADDR_CID_ANY`, as on Linux).
    pub fn local_addr(&self, id: SockId) -> Result<Addr, Errno> {
        self.socks.get(&id).map(|s| s.local).ok_or(Errno(EBADF))
    }

    /// `getpeername`: `ENOTCONN` unless connected (Linux: `SS_CONNECTED`,
    /// so not after `shutdown` either).
    pub fn peer_addr(&self, id: SockId) -> Result<Addr, Errno> {
        let s = self.socks.get(&id).ok_or(Errno(EBADF))?;
        if s.ss != Ss::Connected {
            return Err(Errno(ENOTCONN));
        }
        Ok(s.remote)
    }

    /// `SO_ERROR`: read and clear the pending error.
    pub fn take_error(&mut self, id: SockId) -> i32 {
        self.socks
            .get_mut(&id)
            .map_or(0, |s| core::mem::take(&mut s.err))
    }

    pub fn is_listening(&self, id: SockId) -> bool {
        self.socks.get(&id).is_some_and(|s| s.sk == Sk::Listen)
    }

    /// `SO_VM_SOCKETS_BUFFER_SIZE` / `_MIN_SIZE` / `_MAX_SIZE` (`which` 0,
    /// 1, 2), `vsock_update_buffer_size`.
    pub fn set_buffer(&mut self, id: SockId, which: u32, val: u64) -> Result<(), Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        let mut v = match which {
            0 => val,
            1 => {
                s.buffer_min = val;
                s.buffer_size
            }
            2 => {
                s.buffer_max = val;
                s.buffer_size
            }
            _ => return Err(Errno(ENOPROTOOPT)),
        };
        if v > s.buffer_max {
            v = s.buffer_max;
        }
        if v < s.buffer_min {
            v = s.buffer_min;
        }
        let v = v.min(MAX_BUF_SIZE);
        s.buffer_size = v;
        s.buf_alloc = v as u32;
        Ok(())
    }

    pub fn buffer(&self, id: SockId, which: u32) -> Result<u64, Errno> {
        let s = self.socks.get(&id).ok_or(Errno(EBADF))?;
        match which {
            0 => Ok(s.buffer_size),
            1 => Ok(s.buffer_min),
            2 => Ok(s.buffer_max),
            _ => Err(Errno(ENOPROTOOPT)),
        }
    }

    /// `SO_VM_SOCKETS_CONNECT_TIMEOUT` in ns (0: the 2 s default).
    pub fn set_connect_timeout(&mut self, id: SockId, ns: u64) -> Result<(), Errno> {
        let s = sock_mut(&mut self.socks, id)?;
        s.connect_timeout = if ns == 0 {
            DEFAULT_CONNECT_TIMEOUT_NS
        } else {
            ns
        };
        Ok(())
    }

    pub fn connect_timeout(&self, id: SockId) -> Result<u64, Errno> {
        self.socks
            .get(&id)
            .map(|s| s.connect_timeout)
            .ok_or(Errno(EBADF))
    }

    /// Readable bytes (`FIONREAD`-like; kernel glue and tests).
    pub fn rx_bytes(&self, id: SockId) -> usize {
        self.socks.get(&id).map_or(0, |s| s.rx_bytes())
    }

    /// `vsock_poll` for a stream socket.
    pub fn poll(&self, id: SockId) -> EpollFlags {
        let Some(s) = self.socks.get(&id) else {
            return EpollFlags::ERR | EpollFlags::HUP;
        };
        let mut m = EpollFlags::empty();
        if s.err != 0 {
            m |= EpollFlags::ERR;
        }
        if s.shutdown == SHUTDOWN_MASK
            || (s.shutdown & SEND_SHUTDOWN != 0 && s.peer_shutdown & SEND_SHUTDOWN != 0)
        {
            m |= EpollFlags::HUP;
        }
        let rd_hup = s.shutdown & RCV_SHUTDOWN != 0 || s.peer_shutdown & SEND_SHUTDOWN != 0;
        if rd_hup {
            m |= EpollFlags::RDHUP | EpollFlags::IN | EpollFlags::RDNORM;
        }
        if s.sk == Sk::Listen && !s.queue.is_empty() {
            m |= EpollFlags::IN | EpollFlags::RDNORM;
        }
        if s.via.is_some() && s.shutdown & RCV_SHUTDOWN == 0 && !s.rx.is_empty() {
            m |= EpollFlags::IN | EpollFlags::RDNORM;
        }
        if s.via.is_some()
            && s.sk == Sk::Established
            && s.shutdown & SEND_SHUTDOWN == 0
            && s.has_space() > 0
        {
            m |= EpollFlags::OUT | EpollFlags::WRNORM;
        }
        if matches!(s.sk, Sk::Close | Sk::Closing) && s.shutdown & SEND_SHUTDOWN == 0 {
            m |= EpollFlags::OUT | EpollFlags::WRNORM;
        }
        m
    }
}
