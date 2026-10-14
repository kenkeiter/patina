//! Socket syscalls (design §7/§9, D68): `socket socketpair bind listen
//! accept accept4 connect getsockname getpeername setsockopt getsockopt
//! sendto recvfrom sendmsg recvmsg shutdown`.
//!
//! Family-independent: user memory (addresses, iovecs, option values,
//! `msghdr`), blocking (`fs::file::wait_ready` honouring `O_NONBLOCK`,
//! `MSG_DONTWAIT`, `SO_RCVTIMEO`/`SO_SNDTIMEO`), `MSG_WAITALL` loops,
//! `SIGPIPE`. The socket itself is reached through `File::as_socket`
//! (`net::socket::SocketOps`); families differ only in their constructors
//! ([`socket`]/[`socketpair`]). AF_UNIX (D71) also uses the generic hooks
//! `SocketOps::recv_msg` (control data sized to `msg_controllen`),
//! `max_dgram`, and `EAGAIN` from a blocking `connect` (full backlog: wait
//! for `OUT` and retry).
//!
//! Interrupted waits: accept/send/recv without a timeout return
//! `ERESTARTSYS` (restarted under `SA_RESTART`, D45), with a timeout
//! `EINTR` (Linux `sock_intr_errno`); connect returns `EINTR` and the
//! connection attempt continues in the background, as on Linux.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::fs::{O_NONBLOCK, O_RDWR};
use patina_abi::net::*;
use patina_abi::signal::{SI_USER, SIGPIPE, SigInfo};
use patina_inet::{AddrError, SOCKADDR_MAX, SockAddr};

use super::fs::{get, install, read_iov};
use super::{SysResult, with_space};
use crate::fs::file::{OpenFile, wait_ready};
use crate::net::socket::SocketOps;
use crate::task::signal::ERESTARTSYS;
use crate::task::{self, WakeReason};

/// Bounce-buffer size per send/recv step on streams.
const CHUNK: usize = 64 * 1024;
/// Largest option value / `msg_control` copied in.
const MAX_OPT: usize = 4096;

/// The open file at `fd`, which must be a socket.
fn sock(fd: i32) -> Result<Arc<OpenFile>, Errno> {
    let of = get(fd)?;
    if of.file.as_socket().is_none() {
        return Err(Errno(ENOTSOCK));
    }
    Ok(of)
}

fn ops(of: &OpenFile) -> &dyn SocketOps {
    of.file.as_socket().expect("checked by sock()")
}

/// Read a `sockaddr` of `len` bytes from user memory.
pub fn read_addr(ptr: usize, len: usize) -> Result<SockAddr, Errno> {
    if !(2..=SOCKADDR_MAX).contains(&len) {
        return Err(Errno(EINVAL));
    }
    let mut b = [0u8; SOCKADDR_MAX];
    with_space(|s| s.copy_from_user(ptr, &mut b[..len]))?;
    SockAddr::parse(&b[..len]).map_err(|e| match e {
        AddrError::Invalid => Errno(EINVAL),
        AddrError::Family(_) => Errno(EAFNOSUPPORT),
    })
}

/// Store `addr` at `uaddr` (truncated to `*ulenp`) and its full length at
/// `ulenp` (accept/getsockname/recvfrom convention). `uaddr` 0: nothing.
fn write_addr(addr: Option<&SockAddr>, uaddr: usize, ulenp: usize) -> Result<(), Errno> {
    if uaddr == 0 || ulenp == 0 {
        return Ok(());
    }
    let ulen: i32 = with_space(|s| s.read_val(ulenp))?;
    if ulen < 0 {
        return Err(Errno(EINVAL));
    }
    let mut b = [0u8; SOCKADDR_MAX];
    let n = addr.map_or(0, |a| a.encode(&mut b));
    with_space(|s| {
        s.copy_to_user(uaddr, &b[..n.min(ulen as usize)])?;
        s.write_val(ulenp, &(n as u32))
    })
}

fn deadline(t: Option<u64>) -> Option<u64> {
    t.map(|t| crate::arch::timer::now_ns().saturating_add(t))
}

/// Wait for `want`; `Ok(())`: retry the operation.
fn wait(of: &OpenFile, want: EpollFlags, until: Option<u64>) -> Result<(), Errno> {
    match wait_ready(&*of.file, want, until) {
        WakeReason::Woken => Ok(()),
        WakeReason::TimedOut => Err(Errno(EAGAIN)),
        WakeReason::Interrupted => Err(Errno(if until.is_some() { EINTR } else { ERESTARTSYS })),
    }
}

fn partial(n: usize, e: Errno) -> SysResult {
    if n > 0 { Ok(n as isize) } else { Err(e) }
}

fn sigpipe() {
    let pid = task::process().pid as i32;
    crate::task::signal::send_to_thread(
        &task::sched::current_arc(),
        SigInfo::kill(SIGPIPE, SI_USER, pid, 0),
    );
}

/// A cursor over a user iovec array.
struct Iov {
    v: Vec<(usize, usize)>,
    i: usize,
    off: usize,
}

impl Iov {
    fn new(v: Vec<(usize, usize)>) -> Iov {
        Iov { v, i: 0, off: 0 }
    }

    fn total(&self) -> usize {
        self.v.iter().map(|x| x.1).sum()
    }

    /// Copy the next bytes from user memory into `out`; returns the count.
    fn gather(&mut self, out: &mut [u8]) -> Result<usize, Errno> {
        let mut n = 0;
        while n < out.len() && self.i < self.v.len() {
            let (base, len) = self.v[self.i];
            let m = (len - self.off).min(out.len() - n);
            with_space(|s| s.copy_from_user(base + self.off, &mut out[n..n + m]))?;
            n += m;
            self.off += m;
            if self.off == len {
                self.i += 1;
                self.off = 0;
            }
        }
        Ok(n)
    }

    /// Copy `data` to the next user bytes.
    fn scatter(&mut self, data: &[u8]) -> Result<(), Errno> {
        let mut n = 0;
        while n < data.len() && self.i < self.v.len() {
            let (base, len) = self.v[self.i];
            let m = (len - self.off).min(data.len() - n);
            with_space(|s| s.copy_to_user(base + self.off, &data[n..n + m]))?;
            n += m;
            self.off += m;
            if self.off == len {
                self.i += 1;
                self.off = 0;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Creation.

fn split_type(ty: i32) -> Result<(u32, u32), Errno> {
    let ty = ty as u32;
    let flags = ty & !SOCK_TYPE_MASK;
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno(EINVAL));
    }
    Ok((ty & SOCK_TYPE_MASK, flags))
}

pub fn socket(domain: i32, ty: i32, protocol: i32) -> SysResult {
    let (ty, flags) = split_type(ty)?;
    let file: Arc<dyn crate::fs::file::File> = match u16::try_from(domain) {
        Ok(AF_INET | AF_INET6) => crate::net::inet::create(domain as u16, ty, protocol)?,
        Ok(AF_UNIX) => crate::net::unix::create(ty, protocol)?,
        Ok(AF_NETLINK) => crate::net::netlink::create(ty, protocol)?,
        // EAFNOSUPPORT without a virtio-vsock device (D76).
        Ok(AF_VSOCK) => crate::net::vsock::create(ty, protocol)?,
        // AF_PACKET, ...: not supported.
        _ => return Err(Errno(EAFNOSUPPORT)),
    };
    install(
        OpenFile::new(file, O_RDWR | (flags & O_NONBLOCK)),
        flags & SOCK_CLOEXEC != 0,
    )
}

pub fn socketpair(domain: i32, ty: i32, protocol: i32, sv: usize) -> SysResult {
    let (ty, flags) = split_type(ty)?;
    let (a, b) = match u16::try_from(domain) {
        Ok(AF_INET | AF_INET6) => return Err(Errno(EOPNOTSUPP)),
        Ok(AF_UNIX) => crate::net::unix::pair(ty, protocol)?,
        _ => return Err(Errno(EAFNOSUPPORT)),
    };
    let open = |f: Arc<dyn crate::fs::file::File>| {
        install(
            OpenFile::new(f, O_RDWR | (flags & O_NONBLOCK)),
            flags & SOCK_CLOEXEC != 0,
        )
    };
    let close = |fd: isize| drop(task::process().files.lock().close(fd as i32));
    let fa = open(a)?;
    let fb = match open(b) {
        Ok(fd) => fd,
        Err(e) => {
            close(fa);
            return Err(e);
        }
    };
    if let Err(e) = with_space(|s| s.write_val(sv, &[fa as i32, fb as i32])) {
        close(fa);
        close(fb);
        return Err(e);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Addresses and connection setup.

pub fn bind(fd: i32, addr: usize, len: usize) -> SysResult {
    let of = sock(fd)?;
    let a = read_addr(addr, len)?;
    ops(&of).bind(&a)?;
    Ok(0)
}

pub fn listen(fd: i32, backlog: i32) -> SysResult {
    let of = sock(fd)?;
    ops(&of).listen(backlog)?;
    Ok(0)
}

pub fn accept4(fd: i32, addr: usize, alenp: usize, flags: u32) -> SysResult {
    if flags & !(SOCK_NONBLOCK | SOCK_CLOEXEC) != 0 {
        return Err(Errno(EINVAL));
    }
    let of = sock(fd)?;
    let o = ops(&of);
    let until = deadline(o.timeouts().0);
    let (file, peer) = loop {
        match o.accept() {
            Err(Errno(EAGAIN)) if !of.flags().nonblocking() => {
                wait(&of, EpollFlags::IN | EpollFlags::RDNORM, until)?;
            }
            r => break r?,
        }
    };
    let nfd = install(
        OpenFile::new(file, O_RDWR | (flags & O_NONBLOCK)),
        flags & SOCK_CLOEXEC != 0,
    )?;
    if let Err(e) = write_addr(Some(&peer), addr, alenp) {
        let undo = task::process().files.lock().close(nfd as i32);
        drop(undo);
        return Err(e);
    }
    Ok(nfd)
}

pub fn connect(fd: i32, addr: usize, len: usize) -> SysResult {
    let of = sock(fd)?;
    let a = read_addr(addr, len)?;
    let o = ops(&of);
    let until = deadline(o.timeouts().1);
    loop {
        match o.connect(&a) {
            Ok(()) => return Ok(0),
            Err(Errno(EINPROGRESS)) if !of.flags().nonblocking() => break,
            // AF_UNIX with a full backlog: wait for room, then retry.
            Err(Errno(EAGAIN)) if !of.flags().nonblocking() => {
                wait(&of, EpollFlags::OUT | EpollFlags::WRNORM, until)?;
            }
            Err(e) => return Err(e),
        }
    }
    loop {
        match wait_ready(&*of.file, EpollFlags::OUT | EpollFlags::WRNORM, until) {
            WakeReason::Woken => {}
            WakeReason::TimedOut => return Err(Errno(EINPROGRESS)),
            // The attempt goes on; a later connect reports EALREADY.
            WakeReason::Interrupted => return Err(Errno(EINTR)),
        }
        if let Some(r) = o.connect_result() {
            return r.map(|()| 0);
        }
    }
}

pub fn getsockname(fd: i32, addr: usize, alenp: usize) -> SysResult {
    let of = sock(fd)?;
    let a = ops(&of).local_addr()?;
    if addr == 0 || alenp == 0 {
        return Err(Errno(EFAULT));
    }
    write_addr(Some(&a), addr, alenp)?;
    Ok(0)
}

pub fn getpeername(fd: i32, addr: usize, alenp: usize) -> SysResult {
    let of = sock(fd)?;
    let a = ops(&of).peer_addr()?;
    if addr == 0 || alenp == 0 {
        return Err(Errno(EFAULT));
    }
    write_addr(Some(&a), addr, alenp)?;
    Ok(0)
}

pub fn shutdown(fd: i32, how: i32) -> SysResult {
    let of = sock(fd)?;
    ops(&of).shutdown(how)?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// Options.

pub fn setsockopt(fd: i32, level: i32, name: i32, val: usize, len: i32) -> SysResult {
    let of = sock(fd)?;
    if len < 0 {
        return Err(Errno(EINVAL));
    }
    let len = (len as usize).min(MAX_OPT);
    let mut b = vec![0u8; len];
    if len > 0 {
        with_space(|s| s.copy_from_user(val, &mut b))?;
    }
    ops(&of).setsockopt(level, name, &b)?;
    Ok(0)
}

pub fn getsockopt(fd: i32, level: i32, name: i32, val: usize, lenp: usize) -> SysResult {
    let of = sock(fd)?;
    let ulen: i32 = with_space(|s| s.read_val(lenp))?;
    if ulen < 0 {
        return Err(Errno(EINVAL));
    }
    let mut b = [0u8; 256];
    let n = ops(&of).getsockopt(level, name, &mut b)?;
    let m = n.min(ulen as usize);
    with_space(|s| {
        s.copy_to_user(val, &b[..m])?;
        s.write_val(lenp, &(m as u32))
    })?;
    Ok(0)
}

// ---------------------------------------------------------------------------
// Data transfer.

fn send_common(
    of: &OpenFile,
    mut iov: Iov,
    to: Option<SockAddr>,
    flags: MsgFlags,
    control: &[u8],
) -> SysResult {
    let o = ops(of);
    let nonblock = of.flags().nonblocking() || flags.contains(MsgFlags::DONTWAIT);
    let until = deadline(o.timeouts().1);
    let fail = |e: Errno| {
        if e == Errno(EPIPE) && !flags.contains(MsgFlags::NOSIGNAL) {
            sigpipe();
        }
        e
    };
    let total = iov.total();
    if !o.is_stream() {
        if total > o.max_dgram() {
            return Err(Errno(EMSGSIZE));
        }
        let mut k = vec![0u8; total];
        iov.gather(&mut k)?;
        loop {
            match o.send(&k, to.as_ref(), flags, control) {
                Err(Errno(EAGAIN)) if !nonblock => {
                    wait(of, EpollFlags::OUT | EpollFlags::WRNORM, until)?;
                }
                r => return r.map(|n| n as isize).map_err(fail),
            }
        }
    }
    let mut k = vec![0u8; total.clamp(1, CHUNK)];
    let mut done = 0;
    // Ancillary data goes with the first bytes sent only.
    let mut control = control;
    loop {
        let n = iov.gather(&mut k)?;
        let mut off = 0;
        while off < n || (total == 0 && off == 0) {
            match o.send(&k[off..n], to.as_ref(), flags, control) {
                Ok(0) if n == 0 => return Ok(0),
                Ok(w) => {
                    off += w;
                    if w > 0 {
                        control = &[];
                    }
                }
                Err(Errno(EAGAIN)) if !nonblock => {
                    if let Err(e) = wait(of, EpollFlags::OUT | EpollFlags::WRNORM, until) {
                        return partial(done + off, e);
                    }
                }
                Err(e) => return partial(done + off, fail(e)),
            }
        }
        done += n;
        if done >= total {
            return Ok(done as isize);
        }
    }
}

/// What a receive produced, for recvfrom/recvmsg.
struct Received {
    ret: usize,
    from: Option<SockAddr>,
    flags: MsgFlags,
    control: Vec<u8>,
}

/// `room`: bytes of `msg_control` the caller offers (0: none).
fn recv_common(
    of: &OpenFile,
    mut iov: Iov,
    flags: MsgFlags,
    room: usize,
) -> Result<Received, Errno> {
    let o = ops(of);
    let nonblock = of.flags().nonblocking() || flags.contains(MsgFlags::DONTWAIT);
    let until = deadline(o.timeouts().0);
    let want = EpollFlags::IN | EpollFlags::RDNORM;
    let total = iov.total();
    if !o.is_stream() {
        let mut k = vec![0u8; total.min(o.max_dgram() + 1)];
        loop {
            match o.recv_msg(&mut k, flags, room) {
                Err(Errno(EAGAIN)) if !nonblock => wait(of, want, until)?,
                Err(e) => return Err(e),
                Ok((r, ctrunc)) => {
                    iov.scatter(&k[..r.len])?;
                    let mut out = MsgFlags::empty();
                    if r.full > r.len {
                        out |= MsgFlags::TRUNC;
                    }
                    if ctrunc {
                        out |= MsgFlags::CTRUNC;
                    }
                    return Ok(Received {
                        ret: if flags.contains(MsgFlags::TRUNC) {
                            r.full
                        } else {
                            r.len
                        },
                        from: r.from,
                        flags: out,
                        control: r.control,
                    });
                }
            }
        }
    }
    let waitall = flags.contains(MsgFlags::WAITALL) && !flags.contains(MsgFlags::PEEK);
    let mut k = vec![0u8; total.clamp(1, CHUNK)];
    let mut got = 0;
    let mut control = Vec::new();
    let mut out = MsgFlags::empty();
    loop {
        let n = (total - got).min(k.len());
        match o.recv_msg(&mut k[..n], flags, room) {
            Ok((r, ctrunc)) => {
                iov.scatter(&k[..r.len])?;
                got += r.len;
                if ctrunc {
                    out |= MsgFlags::CTRUNC;
                }
                // Ancillary data ends a receive (Linux stops after the
                // data that carried it, even under MSG_WAITALL).
                if !r.control.is_empty() || ctrunc {
                    control = r.control;
                    break;
                }
                if r.len == 0 || !waitall || got >= total {
                    break;
                }
            }
            Err(Errno(EAGAIN)) if !nonblock => {
                if got > 0 && !waitall {
                    break;
                }
                if let Err(e) = wait(of, want, until) {
                    if got > 0 {
                        break;
                    }
                    return Err(e);
                }
            }
            Err(e) => {
                if got > 0 {
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(Received {
        ret: got,
        from: None,
        flags: out,
        control,
    })
}

pub fn sendto(fd: i32, buf: usize, len: usize, flags: u32, addr: usize, alen: usize) -> SysResult {
    let of = sock(fd)?;
    let to = if addr != 0 {
        Some(read_addr(addr, alen)?)
    } else {
        None
    };
    send_common(
        &of,
        Iov::new(vec![(buf, len.min(isize::MAX as usize))]),
        to,
        MsgFlags::from_bits_retain(flags),
        &[],
    )
}

pub fn recvfrom(
    fd: i32,
    buf: usize,
    len: usize,
    flags: u32,
    addr: usize,
    alenp: usize,
) -> SysResult {
    let of = sock(fd)?;
    let r = recv_common(
        &of,
        Iov::new(vec![(buf, len.min(isize::MAX as usize))]),
        MsgFlags::from_bits_retain(flags),
        0,
    )?;
    if addr != 0 && alenp != 0 {
        match &r.from {
            Some(a) => write_addr(Some(a), addr, alenp)?,
            None => with_space(|s| s.write_val(alenp, &0u32))?,
        }
    }
    Ok(r.ret as isize)
}

pub fn sendmsg(fd: i32, msg: usize, flags: u32) -> SysResult {
    let of = sock(fd)?;
    let m: Msghdr = with_space(|s| s.read_val(msg))?;
    let to = if m.msg_name != 0 && m.msg_namelen > 0 {
        Some(read_addr(m.msg_name as usize, m.msg_namelen as usize)?)
    } else {
        None
    };
    let iov = Iov::new(read_iov(m.msg_iov as usize, m.msg_iovlen as usize)?);
    let clen = (m.msg_controllen as usize).min(MAX_OPT);
    let mut control = vec![0u8; if m.msg_control != 0 { clen } else { 0 }];
    if !control.is_empty() {
        with_space(|s| s.copy_from_user(m.msg_control as usize, &mut control))?;
    }
    send_common(&of, iov, to, MsgFlags::from_bits_retain(flags), &control)
}

pub fn recvmsg(fd: i32, msg: usize, flags: u32) -> SysResult {
    let of = sock(fd)?;
    let mut m: Msghdr = with_space(|s| s.read_val(msg))?;
    if (m.msg_namelen as i32) < 0 {
        return Err(Errno(EINVAL));
    }
    let iov = Iov::new(read_iov(m.msg_iov as usize, m.msg_iovlen as usize)?);
    let room = if m.msg_control != 0 {
        m.msg_controllen as usize
    } else {
        0
    };
    let r = recv_common(&of, iov, MsgFlags::from_bits_retain(flags), room)?;
    // Linux (`____sys_recvmsg`) starts `msg_flags` from the caller's
    // MSG_CMSG_CLOEXEC and reports it back (Go's TestSCMCredentials).
    let mut out = r.flags | (MsgFlags::from_bits_retain(flags) & MsgFlags::CMSG_CLOEXEC);
    if m.msg_name != 0 {
        match &r.from {
            Some(a) => {
                let mut b = [0u8; SOCKADDR_MAX];
                let n = a.encode(&mut b);
                with_space(|s| {
                    s.copy_to_user(m.msg_name as usize, &b[..n.min(m.msg_namelen as usize)])
                })?;
                m.msg_namelen = n as u32;
            }
            None => m.msg_namelen = 0,
        }
    } else {
        m.msg_namelen = 0;
    }
    let cn = r.control.len().min(m.msg_controllen as usize);
    if cn < r.control.len() {
        out |= MsgFlags::CTRUNC;
    }
    if cn > 0 && m.msg_control != 0 {
        with_space(|s| s.copy_to_user(m.msg_control as usize, &r.control[..cn]))?;
    }
    m.msg_controllen = cn as u64;
    m.msg_flags = out.bits() as i32;
    with_space(|s| s.write_val(msg, &m))?;
    Ok(r.ret as isize)
}

/// strace: the address argument of bind/connect/sendto as `ip:port`.
pub fn trace_addr(n: usize, a: &[u64; 6]) -> Option<alloc::string::String> {
    use patina_abi::current::nr;
    let (p, l) = match n {
        nr::BIND | nr::CONNECT => (a[1] as usize, a[2] as usize),
        nr::SENDTO if a[4] != 0 => (a[4] as usize, a[5] as usize),
        _ => return None,
    };
    let addr = read_addr(p, l).ok()?;
    Some(match addr {
        SockAddr::V4 { addr, port } => alloc::format!("{}:{}", addr, port),
        SockAddr::V6 { addr, port, .. } => alloc::format!("[{}]:{}", addr, port),
        SockAddr::Unix(p) => alloc::format!("{:?}", alloc::string::String::from_utf8_lossy(&p)),
        SockAddr::Unspec => alloc::string::String::from("AF_UNSPEC"),
        SockAddr::Netlink { pid, groups } => alloc::format!("nl:{}/{:#x}", pid, groups),
        SockAddr::Vsock { cid, port, .. } => alloc::format!("vsock:{}:{}", cid, port),
    })
}
