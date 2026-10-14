//! AF_UNIX sockets (design §9, D71): `SOCK_STREAM`, `SOCK_DGRAM` and
//! `SOCK_SEQPACKET`, entirely in the kernel.
//!
//! # Model
//!
//! Every socket is a [`Sock`] in one global table ([`World`]) keyed by a
//! never-reused id; the fd-side object is a [`UnixSocket`] (`fs::File` +
//! `SocketOps`) holding that id. Peers, listeners, filesystem nodes and the
//! abstract namespace refer to sockets by id, so there are no reference
//! cycles and a dead id simply means "gone" (`ECONNREFUSED`, EOF, `EPIPE`).
//! One lock covers the table: every operation that touches two sockets
//! (send, connect, close) is a single critical section, with no lock order
//! between sockets to get wrong.
//!
//! * Received data lives with the receiver: a byte ring with ancillary
//!   marks for streams (`patina_inet::unix::StreamBuf`), a message queue
//!   for datagram and seqpacket sockets.
//! * A stream `connect` to a listener creates the server-side socket at
//!   once (Linux's embryo) and queues it on the listener; `accept` hands it
//!   out. `connect` only waits when the backlog is full.
//! * Names: a pathname `bind` creates an `S_IFSOCK` inode whose special is
//!   [`name::SockSpecial`] (the socket id); `connect`/`sendto` resolve it
//!   through `namei`. Abstract names live in [`World::abstract_names`].
//! * Passed files (`SCM_RIGHTS`) travel as `Arc<OpenFile>`s with their
//!   message (or their stream bytes) and are installed in the receiver's
//!   fd table by `recvmsg`.
//!
//! # Locking
//!
//! The table lock is a leaf: no FS lock, fd-table lock or file lock is
//! taken under it, and poll hooks are fired and `Arc<OpenFile>`s dropped
//! only after it is released ([`with`] collects both). The FS lock is never
//! held while the table lock is taken either: path resolution runs first
//! and yields an id.

mod name;
mod ops;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::fs::S_IFSOCK;
use patina_abi::misc::FIONREAD;
use patina_abi::net::{MsgFlags, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET, SOCK_STREAM, Ucred};
use patina_inet::unix::StreamBuf;

use crate::fs::file::{
    File, FileFlags, Notify, OpenFile, PollHook, PollList, alloc_ino, anon_stat,
};
use crate::net::socket::SocketOps;
use crate::sync::SpinLock;

/// Receive buffer per socket, both for stream bytes and queued datagrams
/// (Linux's default `sk_sndbuf`/`sk_rcvbuf`, `net.core.wmem_default`).
pub const BUF_SIZE: usize = 212_992;
/// Largest datagram (Linux: `sk_sndbuf - 32`).
pub const MAX_DGRAM: usize = BUF_SIZE - 32;
/// Most datagrams queued on one receiver before senders wait.
pub const MAX_QLEN: usize = 64;
/// `TIOCOUTQ` / `SIOCOUTQ`.
const SIOCOUTQ: u32 = 0x5411;

/// Files passed with `SCM_RIGHTS`.
pub type Rights = Vec<Arc<OpenFile>>;

/// Ancillary data travelling with a message (or with stream bytes).
#[derive(Default)]
pub struct Anc {
    pub rights: Rights,
    /// `SCM_CREDENTIALS` given by the sender.
    pub creds: Option<Ucred>,
}

impl Anc {
    fn is_empty(&self) -> bool {
        self.rights.is_empty() && self.creds.is_none()
    }
}

/// A queued datagram / seqpacket record.
struct Msg {
    data: Vec<u8>,
    /// The sender's bound name (empty: unnamed).
    from: Vec<u8>,
    anc: Anc,
}

enum Rx {
    Stream(StreamBuf<Anc>),
    Msgs { q: VecDeque<Msg>, bytes: usize },
}

impl Rx {
    fn is_empty(&self) -> bool {
        match self {
            Rx::Stream(b) => b.is_empty(),
            Rx::Msgs { q, .. } => q.is_empty(),
        }
    }

    /// `FIONREAD`: buffered bytes, or the size of the next datagram.
    fn next_len(&self) -> usize {
        match self {
            Rx::Stream(b) => b.len(),
            Rx::Msgs { q, .. } => q.front().map_or(0, |m| m.data.len()),
        }
    }

    fn queued_bytes(&self) -> usize {
        match self {
            Rx::Stream(b) => b.len(),
            Rx::Msgs { bytes, .. } => *bytes,
        }
    }

    /// Drop everything; the passed files are moved to `trash`.
    fn clear(&mut self, trash: &mut Rights) {
        match self {
            Rx::Stream(b) => {
                for a in b.clear() {
                    trash.extend(a.rights);
                }
            }
            Rx::Msgs { q, bytes } => {
                for m in q.drain(..) {
                    trash.extend(m.anc.rights);
                }
                *bytes = 0;
            }
        }
    }
}

/// Connection state.
enum Conn {
    /// Neither listening nor connected (a datagram socket is always here;
    /// its default destination is `Sock::dgram_peer`).
    Idle,
    Listening {
        backlog: usize,
        queue: VecDeque<u64>,
    },
    /// Connected stream/seqpacket socket (the peer id stays after the peer
    /// is gone; `rcv_shut`/`snd_shut` say so).
    Connected { peer: u64 },
}

struct Sock {
    ty: u32,
    conn: Conn,
    /// Bound name: a path, or an abstract name starting with NUL.
    name: Option<Vec<u8>>,
    /// A pathname bind is creating the inode.
    binding: bool,
    /// The peer's name as seen at connect time (empty: unnamed).
    peer_name: Vec<u8>,
    /// Connected datagram socket: the default destination and the only
    /// socket allowed to send to this one.
    dgram_peer: Option<u64>,
    rx: Rx,
    rcv_shut: bool,
    snd_shut: bool,
    /// Pending `SO_ERROR`.
    error: i32,
    /// The socket ever had credentials of a peer (connected or listening):
    /// `SO_PEERCRED` reports ours rather than the "none" values.
    has_peercred: bool,
    passcred: bool,
    rcvtimeo: Option<u64>,
    sndtimeo: Option<u64>,
    sndbuf: i32,
    rcvbuf: i32,
    /// Other `SOL_SOCKET` integer options, stored and reported.
    misc: BTreeMap<i32, i32>,
    /// The socket a send or connect last found full (its `peer_wait`
    /// tells us about space).
    blocked_on: Option<u64>,
    /// Hooks watching this socket's readiness.
    hooks: PollList,
    /// Hooks of other sockets waiting for room in this one's receive or
    /// accept queue.
    peer_wait: PollList,
}

impl Sock {
    fn new(ty: u32) -> Sock {
        Sock {
            ty,
            conn: Conn::Idle,
            name: None,
            binding: false,
            peer_name: Vec::new(),
            dgram_peer: None,
            rx: if ty == SOCK_STREAM {
                Rx::Stream(StreamBuf::new(BUF_SIZE))
            } else {
                Rx::Msgs {
                    q: VecDeque::new(),
                    bytes: 0,
                }
            },
            rcv_shut: false,
            snd_shut: false,
            error: 0,
            has_peercred: false,
            passcred: false,
            rcvtimeo: None,
            sndtimeo: None,
            sndbuf: BUF_SIZE as i32,
            rcvbuf: BUF_SIZE as i32,
            misc: BTreeMap::new(),
            blocked_on: None,
            hooks: PollList::new(),
            peer_wait: PollList::new(),
        }
    }

    fn connection_oriented(&self) -> bool {
        self.ty != SOCK_DGRAM
    }

    /// Can no more be queued on this socket (receive queue or, for a
    /// listener, accept backlog)?
    fn full(&self, len: usize) -> bool {
        match (&self.conn, &self.rx) {
            (Conn::Listening { backlog, queue }, _) => queue.len() > *backlog,
            (_, Rx::Stream(b)) => b.space() == 0,
            (_, Rx::Msgs { q, bytes }) => {
                !q.is_empty() && (q.len() >= MAX_QLEN || bytes + len > BUF_SIZE)
            }
        }
    }
}

/// All AF_UNIX sockets.
pub struct World {
    socks: BTreeMap<u64, Sock>,
    /// Abstract names (leading NUL included) → socket id.
    abstract_names: BTreeMap<Vec<u8>, u64>,
    next_id: u64,
    next_autobind: u32,
}

static WORLD: SpinLock<World> = SpinLock::new(World {
    socks: BTreeMap::new(),
    abstract_names: BTreeMap::new(),
    next_id: 1,
    next_autobind: 0,
});

/// What to do once the table lock is released.
struct Out {
    notify: Notify,
    trash: Rights,
}

impl Out {
    /// Notify the hooks of socket `id` (if it exists) of `ev`.
    fn wake(&mut self, w: &mut World, id: u64, ev: EpollFlags) {
        if let Some(s) = w.socks.get_mut(&id) {
            let n = s.hooks.collect(ev);
            self.add(n);
        }
    }

    /// Notify the sockets waiting for room in `id`.
    fn wake_waiters(&mut self, w: &mut World, id: u64) {
        if let Some(s) = w.socks.get_mut(&id) {
            let n = s.peer_wait.collect(EpollFlags::OUT | EpollFlags::WRNORM);
            self.add(n);
        }
    }

    fn add(&mut self, n: Notify) {
        let cur = core::mem::replace(&mut self.notify, Notify::NONE);
        self.notify = cur.and(n);
    }
}

/// Run `f` with the table locked; fire the collected notifications and
/// drop the collected files afterwards.
fn with<R>(f: impl FnOnce(&mut World, &mut Out) -> R) -> R {
    let mut out = Out {
        notify: Notify::NONE,
        trash: Vec::new(),
    };
    let r = {
        let mut w = WORLD.lock();
        f(&mut w, &mut out)
    };
    out.notify.fire();
    drop(out.trash);
    r
}

impl World {
    fn sock(&mut self, id: u64) -> Result<&mut Sock, Errno> {
        self.socks.get_mut(&id).ok_or(Errno(EBADF))
    }

    fn add(&mut self, s: Sock) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.socks.insert(id, s);
        id
    }

    /// Readiness of socket `id`; with `hook`, register it (and, when the
    /// socket waits for room elsewhere, on that socket's `peer_wait`).
    fn ready(&mut self, id: u64, hook: Option<&Arc<dyn PollHook>>) -> EpollFlags {
        let Some(s) = self.socks.get_mut(&id) else {
            return EpollFlags::ERR | EpollFlags::HUP;
        };
        if let Some(h) = hook {
            s.hooks.add(h);
        }
        let mut ev = EpollFlags::empty();
        if s.error != 0 {
            ev |= EpollFlags::ERR;
        }
        if s.rcv_shut {
            ev |= EpollFlags::RDHUP | EpollFlags::IN | EpollFlags::RDNORM;
        }
        if s.rcv_shut && s.snd_shut {
            ev |= EpollFlags::HUP;
        }
        if let Conn::Listening { queue, .. } = &s.conn {
            if !queue.is_empty() {
                ev |= EpollFlags::IN | EpollFlags::RDNORM;
            }
            return ev;
        }
        if !s.rx.is_empty() {
            ev |= EpollFlags::IN | EpollFlags::RDNORM;
        }
        let ty = s.ty;
        let snd_shut = s.snd_shut;
        let (conn_peer, idle) = match s.conn {
            Conn::Connected { peer } => (Some(peer), false),
            _ => (None, true),
        };
        // Whom this socket's next send (or connect) would queue on.
        let target = if ty == SOCK_DGRAM {
            s.blocked_on.or(s.dgram_peer)
        } else if idle {
            s.blocked_on
        } else {
            conn_peer
        };
        let blocked = !snd_shut
            && match target.and_then(|t| self.socks.get_mut(&t)) {
                Some(t) if ty == SOCK_STREAM && !idle => {
                    // Writable once a quarter of the peer's buffer is free.
                    let free = match &t.rx {
                        Rx::Stream(b) => b.space(),
                        Rx::Msgs { .. } => BUF_SIZE,
                    };
                    !t.rcv_shut && free < BUF_SIZE / 4
                }
                Some(t) => {
                    let full = !t.rcv_shut && t.full(0);
                    if full && let Some(h) = hook {
                        t.peer_wait.add(h);
                    }
                    full
                }
                None => false,
            };
        if ty != SOCK_DGRAM && idle {
            // An unconnected stream/seqpacket socket is HUP (Linux
            // `TCP_CLOSE`) unless it waits for a full listener.
            if blocked {
                return ev;
            }
            ev |= EpollFlags::HUP;
        }
        if !blocked {
            ev |= EpollFlags::OUT | EpollFlags::WRNORM;
        }
        ev
    }

    /// Release socket `id` (its last description closed), and the
    /// not-yet-accepted connections of a listener.
    fn close(&mut self, id: u64, out: &mut Out) {
        let mut work = alloc::vec![(id, false)];
        while let Some((id, embryo)) = work.pop() {
            let Some(mut s) = self.socks.remove(&id) else {
                continue;
            };
            let unread = !s.rx.is_empty();
            s.rx.clear(&mut out.trash);
            if let Some(n) = &s.name
                && n.first() == Some(&0)
                && self.abstract_names.get(n) == Some(&id)
            {
                self.abstract_names.remove(n);
            }
            out.add(s.peer_wait.collect(EpollFlags::OUT | EpollFlags::WRNORM));
            match s.conn {
                Conn::Listening { queue, .. } => {
                    work.extend(queue.into_iter().map(|e| (e, true)));
                }
                Conn::Connected { peer } => {
                    if let Some(p) = self.socks.get_mut(&peer) {
                        p.rcv_shut = true;
                        p.snd_shut = true;
                        if unread || embryo {
                            p.error = ECONNRESET;
                        }
                        let n = p.hooks.collect(
                            EpollFlags::IN
                                | EpollFlags::RDNORM
                                | EpollFlags::OUT
                                | EpollFlags::WRNORM
                                | EpollFlags::RDHUP
                                | EpollFlags::HUP
                                | EpollFlags::ERR,
                        );
                        out.add(n);
                    }
                }
                Conn::Idle => {}
            }
        }
    }
}

/// Our credentials (`SO_PEERCRED`, `SCM_CREDENTIALS`): pid 1, root.
pub fn own_creds() -> Ucred {
    Ucred {
        pid: crate::task::process().pid as i32,
        uid: 0,
        gid: 0,
    }
}

/// The fd-side object of an AF_UNIX socket.
pub struct UnixSocket {
    id: u64,
    ty: u32,
    ino: u64,
}

impl UnixSocket {
    fn new(id: u64, ty: u32) -> Arc<UnixSocket> {
        Arc::new(UnixSocket {
            id,
            ty,
            ino: alloc_ino(),
        })
    }
}

/// Validate `socket(AF_UNIX, ty, protocol)`: the effective type.
fn check_type(ty: u32, protocol: i32) -> Result<u32, Errno> {
    // PF_UNIX is the only protocol (0 or 1).
    if protocol != 0 && protocol != 1 {
        return Err(Errno(EPROTONOSUPPORT));
    }
    match ty {
        SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET => Ok(ty),
        // Linux treats SOCK_RAW as SOCK_DGRAM.
        SOCK_RAW => Ok(SOCK_DGRAM),
        _ => Err(Errno(ESOCKTNOSUPPORT)),
    }
}

/// `socket(AF_UNIX, ty, protocol)`.
pub fn create(ty: u32, protocol: i32) -> Result<Arc<UnixSocket>, Errno> {
    let ty = check_type(ty, protocol)?;
    let id = with(|w, _| w.add(Sock::new(ty)));
    Ok(UnixSocket::new(id, ty))
}

/// `socketpair(AF_UNIX, ty, protocol)`: two connected sockets.
pub fn pair(ty: u32, protocol: i32) -> Result<(Arc<UnixSocket>, Arc<UnixSocket>), Errno> {
    let ty = check_type(ty, protocol)?;
    let (a, b) = with(|w, _| {
        let a = w.add(Sock::new(ty));
        let b = w.add(Sock::new(ty));
        for (x, y) in [(a, b), (b, a)] {
            let s = w.socks.get_mut(&x).unwrap();
            s.has_peercred = true;
            if ty == SOCK_DGRAM {
                s.dgram_peer = Some(y);
            } else {
                s.conn = Conn::Connected { peer: y };
            }
        }
        (a, b)
    });
    Ok((UnixSocket::new(a, ty), UnixSocket::new(b, ty)))
}

impl File for UnixSocket {
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
        WORLD.lock().ready(self.id, hook)
    }

    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        let n = with(|w, _| -> Result<usize, Errno> {
            let s = w.sock(self.id)?;
            match cmd {
                FIONREAD => match s.conn {
                    Conn::Listening { .. } => Err(Errno(EINVAL)),
                    _ => Ok(s.rx.next_len()),
                },
                SIOCOUTQ => {
                    let peer = match s.conn {
                        Conn::Connected { peer } => Some(peer),
                        _ => s.dgram_peer,
                    };
                    Ok(peer
                        .and_then(|p| w.socks.get(&p))
                        .map_or(0, |p| p.rx.queued_bytes()))
                }
                _ => Err(Errno(ENOTTY)),
            }
        })?;
        crate::syscall::with_space(|s| s.write_val(arg as usize, &(n as i32)))?;
        Ok(0)
    }

    fn stat(&self) -> Result<Stat, Errno> {
        Ok(anon_stat(S_IFSOCK | 0o777, self.ino))
    }

    fn proc_name(&self) -> Vec<u8> {
        format!("socket:[{}]", self.ino).into_bytes()
    }

    fn release(&self) {
        with(|w, out| w.close(self.id, out));
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }
}
