//! `SocketOps` for AF_UNIX: connection setup, data transfer with
//! ancillary data, shutdown, names and options. Every method is
//! non-blocking (`EAGAIN`); the syscall layer waits.

use alloc::sync::Arc;
use alloc::vec::Vec;

use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::net::*;
use patina_inet::unix::{CmsgWriter, SCM_MAX_FD, parse_cmsgs, rights_fds};

use super::name::{self, Name};
use super::{Anc, BUF_SIZE, Conn, MAX_DGRAM, Msg, Rx, Sock, UnixSocket, World, own_creds, with};
use crate::fs::file::File;
use crate::net::socket::{self as so, RecvInfo, SockAddr, SocketOps};

const EVERYTHING: EpollFlags = EpollFlags::IN
    .union(EpollFlags::RDNORM)
    .union(EpollFlags::OUT)
    .union(EpollFlags::WRNORM)
    .union(EpollFlags::RDHUP)
    .union(EpollFlags::HUP);

/// `SO_PEERCRED` of a socket that never had a peer: pid 0, overflow ids.
const NO_CREDS: Ucred = Ucred {
    pid: 0,
    uid: 65534,
    gid: 65534,
};

fn ucred_bytes(c: &Ucred) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..4].copy_from_slice(&c.pid.to_ne_bytes());
    b[4..8].copy_from_slice(&c.uid.to_ne_bytes());
    b[8..].copy_from_slice(&c.gid.to_ne_bytes());
    b
}

/// Decode `msg_control` of a send (Linux `__scm_send`): `EINVAL` for a
/// malformed record, an unknown `SOL_SOCKET` type, more than
/// `SCM_MAX_FD` fds or invalid credentials; `EBADF` for a bad fd.
fn parse_control(control: &[u8]) -> Result<Anc, Errno> {
    let mut anc = Anc::default();
    if control.is_empty() {
        return Ok(anc);
    }
    for (level, ty, data) in parse_cmsgs(control).map_err(|_| Errno(EINVAL))? {
        if level != SOL_SOCKET {
            continue;
        }
        match ty {
            SCM_RIGHTS => {
                let n = data.len() / 4;
                if anc.rights.len() + n > SCM_MAX_FD {
                    return Err(Errno(EINVAL));
                }
                for fd in rights_fds(data) {
                    anc.rights.push(crate::syscall::fd_get(fd)?);
                }
            }
            SCM_CREDENTIALS => {
                if data.len() != 12 {
                    return Err(Errno(EINVAL));
                }
                let w = |i: usize| u32::from_ne_bytes(data[i..i + 4].try_into().unwrap());
                let c = Ucred {
                    pid: w(0) as i32,
                    uid: w(4),
                    gid: w(8),
                };
                // Root may claim any pid; ids must be valid.
                if c.uid == u32::MAX || c.gid == u32::MAX {
                    return Err(Errno(EINVAL));
                }
                anc.creds = Some(c);
            }
            _ => return Err(Errno(EINVAL)),
        }
    }
    Ok(anc)
}

/// Build the receive-side control data (`scm_recv`): credentials when the
/// receiver set `SO_PASSCRED`, then the passed files, installed as new fds
/// for as many as fit in `room` (the rest are closed). Returns the bytes
/// and `MSG_CTRUNC`.
fn deliver(anc: Option<Anc>, passcred: bool, room: usize, flags: MsgFlags) -> (Vec<u8>, bool) {
    let anc = anc.unwrap_or_default();
    let mut w = CmsgWriter::new(room);
    if passcred {
        let c = anc.creds.unwrap_or_else(own_creds);
        w.put(SOL_SOCKET, SCM_CREDENTIALS, &ucred_bytes(&c));
    }
    if !anc.rights.is_empty() {
        let max = w.fd_room().min(anc.rights.len());
        let cloexec = flags.contains(MsgFlags::CMSG_CLOEXEC);
        let mut fds = Vec::new();
        for of in &anc.rights[..max] {
            match crate::syscall::fd_install(of.clone(), cloexec) {
                Ok(fd) => fds.push(fd as i32),
                Err(_) => break,
            }
        }
        w.put_rights(SOL_SOCKET, SCM_RIGHTS, &fds, anc.rights.len());
    }
    // Undelivered files are closed here, outside every lock.
    drop(anc);
    (w.out, w.ctrunc)
}

/// A copy of a message's ancillary data for `MSG_PEEK` (Linux dups the
/// fds).
fn peek_anc(a: &Anc) -> Anc {
    Anc {
        rights: a.rights.clone(),
        creds: a.creds,
    }
}

impl UnixSocket {
    fn send_stream(&self, data: &[u8], to: Option<&SockAddr>, anc: Anc) -> Result<usize, Errno> {
        let mut anc = (!anc.is_empty()).then_some(anc);
        with(|w, out| {
            let me = w.sock(self.id)?;
            let connected = matches!(me.conn, Conn::Connected { .. });
            if to.is_some() {
                return Err(Errno(if connected { EISCONN } else { EOPNOTSUPP }));
            }
            let Conn::Connected { peer } = me.conn else {
                return Err(Errno(ENOTCONN));
            };
            if me.snd_shut {
                return Err(Errno(EPIPE));
            }
            if me.error != 0 {
                return Err(Errno(core::mem::take(&mut me.error)));
            }
            let Some(p) = w.socks.get_mut(&peer) else {
                return Err(Errno(EPIPE));
            };
            if p.rcv_shut {
                return Err(Errno(EPIPE));
            }
            if data.is_empty() {
                return Ok(0);
            }
            let Rx::Stream(b) = &mut p.rx else {
                return Err(Errno(EINVAL));
            };
            if b.space() == 0 {
                return Err(Errno(EAGAIN));
            }
            let (n, back) = b.push(data, anc.take());
            anc = back;
            out.wake(w, peer, EpollFlags::IN | EpollFlags::RDNORM);
            Ok(n)
        })
    }

    fn send_msg(&self, data: &[u8], to: Option<&SockAddr>, anc: Anc) -> Result<usize, Errno> {
        if data.len() > MAX_DGRAM {
            return Err(Errno(EMSGSIZE));
        }
        // Seqpacket ignores a destination (Linux `unix_seqpacket_sendmsg`).
        let dest = match to {
            Some(a) if self.ty == SOCK_DGRAM => Some(name::dest(a)?),
            _ => None,
        };
        let mut anc = Some(anc);
        with(|w, out| {
            let id = self.id;
            let me = w.sock(id)?;
            if me.snd_shut {
                return Err(Errno(EPIPE));
            }
            if me.error != 0 && self.ty != SOCK_DGRAM {
                return Err(Errno(core::mem::take(&mut me.error)));
            }
            let t = if self.ty == SOCK_DGRAM {
                let t = match &dest {
                    Some(d) => w.target(d)?,
                    None => {
                        let p = me.dgram_peer.ok_or(Errno(ENOTCONN))?;
                        if !w.socks.contains_key(&p) {
                            // The peer is gone: disconnect (Linux).
                            w.sock(id)?.dgram_peer = None;
                            return Err(Errno(ECONNREFUSED));
                        }
                        p
                    }
                };
                if w.sock(id)?.passcred {
                    w.autobind(id)?;
                }
                let ts = w.sock(t)?;
                if ts.ty != SOCK_DGRAM {
                    return Err(Errno(EPROTOTYPE));
                }
                if ts.dgram_peer.is_some_and(|p| p != id) {
                    return Err(Errno(EPERM));
                }
                t
            } else {
                let Conn::Connected { peer } = me.conn else {
                    return Err(Errno(ENOTCONN));
                };
                if !w.socks.contains_key(&peer) {
                    return Err(Errno(EPIPE));
                }
                peer
            };
            let from = w.sock(id)?.name.clone().unwrap_or_default();
            let ts = w.sock(t)?;
            if ts.rcv_shut {
                return Err(Errno(EPIPE));
            }
            if ts.full(data.len()) {
                w.sock(id)?.blocked_on = Some(t);
                return Err(Errno(EAGAIN));
            }
            let Rx::Msgs { q, bytes } = &mut ts.rx else {
                return Err(Errno(EINVAL));
            };
            q.push_back(Msg {
                data: data.to_vec(),
                from,
                anc: anc.take().unwrap_or_default(),
            });
            *bytes += data.len();
            out.wake(w, t, EpollFlags::IN | EpollFlags::RDNORM);
            w.sock(id)?.blocked_on = None;
            Ok(data.len())
        })
    }

    /// Receive under the lock; the ancillary data comes back to be
    /// delivered outside it.
    fn recv_locked(
        &self,
        buf: &mut [u8],
        flags: MsgFlags,
    ) -> Result<(RecvInfo, Option<Anc>, bool), Errno> {
        let peek = flags.contains(MsgFlags::PEEK);
        with(|w, out| {
            let id = self.id;
            let me = w.sock(id)?;
            let passcred = me.passcred;
            let peer = match me.conn {
                Conn::Connected { peer } => Some(peer),
                Conn::Listening { .. } if self.ty != SOCK_DGRAM => return Err(Errno(EINVAL)),
                // Deviation: Linux blocks a read on an unconnected stream
                // socket (forever, unless shut down); patina refuses it.
                Conn::Idle if self.ty == SOCK_STREAM => return Err(Errno(EINVAL)),
                Conn::Idle if self.ty == SOCK_SEQPACKET => return Err(Errno(ENOTCONN)),
                _ => None,
            };
            let eof = |n: usize| RecvInfo {
                len: n,
                full: n,
                from: None,
                control: Vec::new(),
            };
            if me.rx.is_empty() {
                if me.error != 0 {
                    return Err(Errno(core::mem::take(&mut me.error)));
                }
                if me.rcv_shut {
                    return Ok((eof(0), None, false));
                }
                return Err(Errno(EAGAIN));
            }
            let peer_name = me.peer_name.clone();
            match &mut me.rx {
                Rx::Stream(b) => {
                    let (n, anc) = if peek {
                        let (n, a) = b.peek(buf);
                        (n, a.map(peek_anc))
                    } else {
                        let before = b.space();
                        let r = b.pop(buf);
                        let thresh = BUF_SIZE / 4;
                        if before < thresh
                            && b.space() >= thresh
                            && let Some(p) = peer
                        {
                            out.wake(w, p, EpollFlags::OUT | EpollFlags::WRNORM);
                        }
                        r
                    };
                    let mut info = eof(n);
                    info.from = Some(name::addr(&peer_name));
                    Ok((info, anc, passcred))
                }
                Rx::Msgs { q, bytes } => {
                    let (info, anc) = {
                        let m = q.front().unwrap();
                        let n = buf.len().min(m.data.len());
                        buf[..n].copy_from_slice(&m.data[..n]);
                        let info = RecvInfo {
                            len: n,
                            full: m.data.len(),
                            from: Some(name::addr(&m.from)),
                            control: Vec::new(),
                        };
                        if peek {
                            (info, Some(peek_anc(&m.anc)))
                        } else {
                            let m = q.pop_front().unwrap();
                            *bytes -= m.data.len();
                            (info, Some(m.anc))
                        }
                    };
                    if !peek {
                        out.wake_waiters(w, id);
                        if let Some(p) = peer {
                            out.wake(w, p, EpollFlags::OUT | EpollFlags::WRNORM);
                        }
                    }
                    Ok((info, anc, passcred))
                }
            }
        })
    }

    fn recv_inner(
        &self,
        buf: &mut [u8],
        flags: MsgFlags,
        room: usize,
    ) -> Result<(RecvInfo, bool), Errno> {
        if flags.contains(MsgFlags::OOB) {
            return Err(Errno(EOPNOTSUPP));
        }
        let (mut info, anc, passcred) = self.recv_locked(buf, flags)?;
        // Nothing to say alongside a stream EOF.
        if info.full == 0 && self.ty == SOCK_STREAM {
            return Ok((info, false));
        }
        let (control, ctrunc) = deliver(anc, passcred, room, flags);
        info.control = control;
        Ok((info, ctrunc))
    }

    fn connect_stream(&self, addr: &SockAddr) -> Result<(), Errno> {
        let d = name::dest(addr)?;
        with(|w, out| {
            let id = self.id;
            let me = w.sock(id)?;
            match me.conn {
                Conn::Listening { .. } => return Err(Errno(EINVAL)),
                Conn::Connected { .. } => return Err(Errno(EISCONN)),
                Conn::Idle => {}
            }
            let t = w.target(&d)?;
            let ts = w.sock(t)?;
            if ts.ty != self.ty {
                return Err(Errno(EPROTOTYPE));
            }
            if !matches!(ts.conn, Conn::Listening { .. }) {
                return Err(Errno(ECONNREFUSED));
            }
            if ts.full(0) {
                w.sock(id)?.blocked_on = Some(t);
                return Err(Errno(EAGAIN));
            }
            if w.sock(id)?.passcred {
                w.autobind(id)?;
            }
            let ts = w.sock(t)?;
            let lname = ts.name.clone().unwrap_or_default();
            let lpass = ts.passcred;
            let myname = w.sock(id)?.name.clone().unwrap_or_default();
            // The server side (Linux's embryo), queued for accept.
            let mut e = Sock::new(self.ty);
            e.conn = Conn::Connected { peer: id };
            e.name = Some(lname.clone()).filter(|n| !n.is_empty());
            e.peer_name = myname;
            e.has_peercred = true;
            e.passcred = lpass;
            let eid = w.add(e);
            let me = w.sock(id)?;
            me.conn = Conn::Connected { peer: eid };
            me.peer_name = lname;
            me.has_peercred = true;
            me.blocked_on = None;
            if let Conn::Listening { queue, .. } = &mut w.sock(t)?.conn {
                queue.push_back(eid);
            }
            out.wake(w, t, EpollFlags::IN | EpollFlags::RDNORM);
            out.wake(w, id, EpollFlags::OUT | EpollFlags::WRNORM);
            Ok(())
        })
    }

    fn connect_dgram(&self, addr: &SockAddr) -> Result<(), Errno> {
        if *addr == SockAddr::Unspec {
            return with(|w, _| {
                let me = w.sock(self.id)?;
                me.dgram_peer = None;
                me.peer_name.clear();
                me.blocked_on = None;
                Ok(())
            });
        }
        let d = name::dest(addr)?;
        with(|w, _| {
            let id = self.id;
            let t = w.target(&d)?;
            let ts = w.sock(t)?;
            if ts.ty != SOCK_DGRAM {
                return Err(Errno(EPROTOTYPE));
            }
            if ts.dgram_peer.is_some_and(|p| p != id) {
                return Err(Errno(EPERM));
            }
            let tname = ts.name.clone().unwrap_or_default();
            if w.sock(id)?.passcred {
                w.autobind(id)?;
            }
            let me = w.sock(id)?;
            me.dgram_peer = Some(t);
            me.peer_name = tname;
            me.blocked_on = None;
            Ok(())
        })
    }
}

fn sol_socket_only(level: i32) -> Result<(), Errno> {
    if level == SOL_SOCKET {
        Ok(())
    } else {
        Err(Errno(EOPNOTSUPP))
    }
}

/// `SO_LINGER`'s second int lives in `misc` under this key.
const LINGER_SECS: i32 = 0x1_0000 | SO_LINGER;

impl SocketOps for UnixSocket {
    fn domain(&self) -> u16 {
        AF_UNIX
    }

    fn sock_type(&self) -> u32 {
        self.ty
    }

    fn protocol(&self) -> i32 {
        0
    }

    fn max_dgram(&self) -> usize {
        MAX_DGRAM
    }

    fn bind(&self, addr: &SockAddr) -> Result<(), Errno> {
        match name::decode(addr)? {
            Name::Unnamed => with(|w, _| {
                if w.sock(self.id)?.name.is_some() {
                    return Err(Errno(EINVAL));
                }
                w.autobind(self.id)
            }),
            Name::Abstract(n) => with(|w, _| w.bind_abstract(self.id, n)),
            Name::Path(p) => name::bind_path(self.id, p),
        }
    }

    fn listen(&self, backlog: i32) -> Result<(), Errno> {
        with(|w, out| {
            let me = w.sock(self.id)?;
            if self.ty == SOCK_DGRAM {
                return Err(Errno(EOPNOTSUPP));
            }
            if me.name.is_none() {
                return Err(Errno(EINVAL));
            }
            let b = if !(0..=SOMAXCONN).contains(&backlog) {
                SOMAXCONN
            } else {
                backlog
            } as usize;
            match &mut me.conn {
                Conn::Idle => {
                    me.conn = Conn::Listening {
                        backlog: b,
                        queue: Default::default(),
                    }
                }
                Conn::Listening { backlog, .. } => *backlog = b,
                Conn::Connected { .. } => return Err(Errno(EINVAL)),
            }
            me.has_peercred = true;
            out.wake_waiters(w, self.id);
            Ok(())
        })
    }

    fn accept(&self) -> Result<(Arc<dyn File>, SockAddr), Errno> {
        let (eid, peer) = with(|w, out| {
            let me = w.sock(self.id)?;
            if self.ty == SOCK_DGRAM {
                return Err(Errno(EOPNOTSUPP));
            }
            let Conn::Listening { queue, .. } = &mut me.conn else {
                return Err(Errno(EINVAL));
            };
            let eid = queue.pop_front().ok_or(Errno(EAGAIN))?;
            out.wake_waiters(w, self.id);
            let peer = w
                .socks
                .get(&eid)
                .map(|e| e.peer_name.clone())
                .unwrap_or_default();
            Ok((eid, peer))
        })?;
        let f: Arc<dyn File> = UnixSocket::new(eid, self.ty);
        Ok((f, name::addr(&peer)))
    }

    fn connect(&self, addr: &SockAddr) -> Result<(), Errno> {
        if self.ty == SOCK_DGRAM {
            self.connect_dgram(addr)
        } else {
            self.connect_stream(addr)
        }
    }

    fn send(
        &self,
        data: &[u8],
        to: Option<&SockAddr>,
        flags: MsgFlags,
        control: &[u8],
    ) -> Result<usize, Errno> {
        if flags.contains(MsgFlags::OOB) {
            return Err(Errno(EOPNOTSUPP));
        }
        let anc = parse_control(control)?;
        if self.ty == SOCK_STREAM {
            self.send_stream(data, to, anc)
        } else {
            self.send_msg(data, to, anc)
        }
    }

    fn recv(&self, buf: &mut [u8], flags: MsgFlags) -> Result<RecvInfo, Errno> {
        self.recv_inner(buf, flags, 0).map(|r| r.0)
    }

    fn recv_msg(
        &self,
        buf: &mut [u8],
        flags: MsgFlags,
        room: usize,
    ) -> Result<(RecvInfo, bool), Errno> {
        self.recv_inner(buf, flags, room)
    }

    fn shutdown(&self, how: i32) -> Result<(), Errno> {
        if !(SHUT_RD..=SHUT_RDWR).contains(&how) {
            return Err(Errno(EINVAL));
        }
        let rd = how != SHUT_WR;
        let wr = how != SHUT_RD;
        with(|w, out| {
            let id = self.id;
            let me = w.sock(id)?;
            me.rcv_shut |= rd;
            me.snd_shut |= wr;
            let peer = match me.conn {
                Conn::Connected { peer } => Some(peer),
                _ => None,
            };
            out.wake(w, id, EVERYTHING);
            out.wake_waiters(w, id);
            if let Some(p) = peer
                && let Some(ps) = w.socks.get_mut(&p)
            {
                ps.snd_shut |= rd;
                ps.rcv_shut |= wr;
                out.wake(w, p, EVERYTHING);
                out.wake_waiters(w, p);
            }
            Ok(())
        })
    }

    fn local_addr(&self) -> Result<SockAddr, Errno> {
        with(|w, _| {
            let me = w.sock(self.id)?;
            Ok(name::addr(me.name.as_deref().unwrap_or_default()))
        })
    }

    fn peer_addr(&self) -> Result<SockAddr, Errno> {
        with(|w, _| {
            let me = w.sock(self.id)?;
            let connected = match me.conn {
                Conn::Connected { .. } => true,
                _ => me.dgram_peer.is_some(),
            };
            if !connected {
                return Err(Errno(ENOTCONN));
            }
            Ok(name::addr(&me.peer_name))
        })
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        sol_socket_only(level)?;
        with(|w, _| {
            let me = w.sock(self.id)?;
            match name {
                SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => {
                    me.rcvtimeo = so::timeval_opt(val)?;
                    return Ok(());
                }
                SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => {
                    me.sndtimeo = so::timeval_opt(val)?;
                    return Ok(());
                }
                SO_LINGER => {
                    if val.len() < 8 {
                        return Err(Errno(EINVAL));
                    }
                    me.misc.insert(SO_LINGER, so::int_opt(val)?);
                    me.misc.insert(LINGER_SECS, so::int_opt(&val[4..])?);
                    return Ok(());
                }
                SO_BINDTODEVICE => return Ok(()),
                _ => {}
            }
            let v = so::int_opt(val)?;
            // Linux: min(val, wmem_max/rmem_max) * 2, at least the minimum.
            let buf = |v: i32, min: i32| (v.clamp(0, BUF_SIZE as i32) * 2).max(min);
            match name {
                SO_PASSCRED => me.passcred = v != 0,
                SO_SNDBUF | SO_SNDBUFFORCE => me.sndbuf = buf(v, 4608),
                SO_RCVBUF | SO_RCVBUFFORCE => me.rcvbuf = buf(v, 2304),
                SO_REUSEADDR | SO_REUSEPORT | SO_KEEPALIVE | SO_BROADCAST | SO_OOBINLINE
                | SO_PRIORITY | SO_RCVLOWAT | SO_SNDLOWAT | SO_DONTROUTE | SO_NO_CHECK
                | SO_TIMESTAMP_OLD => {
                    me.misc.insert(name, v);
                }
                _ => return Err(Errno(ENOPROTOOPT)),
            }
            Ok(())
        })
    }

    fn getsockopt(&self, level: i32, name: i32, out: &mut [u8]) -> Result<usize, Errno> {
        sol_socket_only(level)?;
        with(|w: &mut World, _| {
            let me = w.sock(self.id)?;
            let int = |v: i32, out: &mut [u8]| Ok(so::int_bytes(v, out));
            match name {
                SO_TYPE => int(self.ty as i32, out),
                SO_DOMAIN => int(AF_UNIX as i32, out),
                SO_PROTOCOL => int(0, out),
                SO_ERROR => int(core::mem::take(&mut me.error), out),
                SO_ACCEPTCONN => int(matches!(me.conn, Conn::Listening { .. }) as i32, out),
                SO_SNDBUF => int(me.sndbuf, out),
                SO_RCVBUF => int(me.rcvbuf, out),
                SO_PASSCRED => int(me.passcred as i32, out),
                SO_PEERCRED => {
                    let c = if me.has_peercred {
                        own_creds()
                    } else {
                        NO_CREDS
                    };
                    out[..12].copy_from_slice(&ucred_bytes(&c));
                    Ok(12)
                }
                SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => Ok(so::timeval_bytes(me.rcvtimeo, out)),
                SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => Ok(so::timeval_bytes(me.sndtimeo, out)),
                SO_LINGER => {
                    let g = |k| *me.misc.get(&k).unwrap_or(&0);
                    out[..4].copy_from_slice(&g(SO_LINGER).to_ne_bytes());
                    out[4..8].copy_from_slice(&g(LINGER_SECS).to_ne_bytes());
                    Ok(8)
                }
                SO_REUSEADDR | SO_REUSEPORT | SO_KEEPALIVE | SO_BROADCAST | SO_OOBINLINE
                | SO_PRIORITY | SO_RCVLOWAT | SO_SNDLOWAT | SO_DONTROUTE | SO_NO_CHECK
                | SO_TIMESTAMP_OLD => {
                    let def = if name == SO_RCVLOWAT || name == SO_SNDLOWAT {
                        1
                    } else {
                        0
                    };
                    int(*me.misc.get(&name).unwrap_or(&def), out)
                }
                SO_BINDTODEVICE => Ok(0),
                _ => Err(Errno(ENOPROTOOPT)),
            }
        })
    }

    fn timeouts(&self) -> (Option<u64>, Option<u64>) {
        with(|w, _| {
            w.socks
                .get(&self.id)
                .map_or((None, None), |s| (s.rcvtimeo, s.sndtimeo))
        })
    }
}
