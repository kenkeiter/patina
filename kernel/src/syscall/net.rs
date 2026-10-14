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

use super::fs::{MAX_RW, OBJ_RESERVE, clamp_iov_out, get, install, out_room, read_iov};
use super::{SysResult, with_space};
use crate::fs::file::{OpenFile, wait_ready};
use crate::mm::heap;
use crate::mm::vm::access_ok;
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
    parse_addr(&b[..len])
}

fn parse_addr(b: &[u8]) -> Result<SockAddr, Errno> {
    SockAddr::parse(b).map_err(|e| match e {
        AddrError::Invalid => Errno(EINVAL),
        AddrError::Family(_) => Errno(EAFNOSUPPORT),
    })
}

/// Linux `sysctl_optmem_max` (6.6 default): `sendmsg` control data of
/// this size or more cannot be allocated (`sock_kmalloc` → ENOBUFS).
const OPTMEM_MAX: usize = 20480;

/// The checks of Linux `net/socket.c __copy_msghdr` on a copied-in
/// `msghdr`: a NULL `msg_name` means length 0; a negative `msg_namelen`
/// → EINVAL; more than `sizeof(struct sockaddr_storage)` (128) is clamped
/// to it (not an error); more than `UIO_MAXIOV` iovecs → EMSGSIZE.
/// Returns the name length to use.
fn check_msghdr(m: &Msghdr) -> Result<usize, Errno> {
    let namelen = if m.msg_name == 0 {
        0
    } else {
        m.msg_namelen as i32
    };
    if namelen < 0 {
        return Err(Errno(EINVAL));
    }
    if m.msg_iovlen > patina_abi::fs::UIO_MAXIOV as u64 {
        return Err(Errno(EMSGSIZE));
    }
    Ok((namelen as usize).min(SOCKADDR_MAX))
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
    heap::reserve(OBJ_RESERVE)?;
    let file: Arc<dyn crate::fs::file::File> = match u16::try_from(domain) {
        Ok(AF_INET | AF_INET6) => crate::net::inet::create(domain as u16, ty, protocol)?,
        Ok(AF_UNIX) => crate::net::unix::create(ty, protocol)?,
        Ok(AF_NETLINK) => crate::net::netlink::create(ty, protocol)?,
        // AF_PACKET, AF_VSOCK, ...: not supported.
        _ => return Err(Errno(EAFNOSUPPORT)),
    };
    install(
        OpenFile::new(file, O_RDWR | (flags & O_NONBLOCK)),
        flags & SOCK_CLOEXEC != 0,
    )
}

pub fn socketpair(domain: i32, ty: i32, protocol: i32, sv: usize) -> SysResult {
    let (ty, flags) = split_type(ty)?;
    heap::reserve(OBJ_RESERVE)?;
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
    heap::reserve(OBJ_RESERVE)?;
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
        let mut k = heap::try_zeroed(total)?;
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
    let mut k = heap::try_zeroed(total.clamp(1, CHUNK))?;
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
        let mut k = heap::try_zeroed(total.min(o.max_dgram() + 1))?;
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
    let mut k = heap::try_zeroed(total.clamp(1, CHUNK))?;
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
    // Linux __sys_sendto: import_ubuf (clamp, access_ok → EFAULT) before
    // the socket lookup, the address (move_addr_to_kernel) after it.
    let len = len.min(MAX_RW);
    if !access_ok(buf, len) {
        return Err(Errno(EFAULT));
    }
    let of = sock(fd)?;
    let to = if addr != 0 {
        Some(read_addr(addr, alen)?)
    } else {
        None
    };
    send_common(
        &of,
        Iov::new(vec![(buf, len)]),
        to,
        MsgFlags::from_bits_retain(flags),
        &[],
    )
}

/// Whether `len` bytes at `p` can be written by a copy-out (checked
/// before data is consumed for results written after it, D86).
fn writable(p: usize, len: usize) -> bool {
    access_ok(p, len) && with_space(|s| s.writable_prefix(p, len)) == len
}

/// Before a receive consumes data: the source address goes to
/// `uaddr`/`ulenp` after the data, so both must be writable now (`EFAULT`
/// otherwise; Linux finds out in `move_addr_to_user` after the data is
/// gone).
fn check_addr_out(uaddr: usize, ulenp: usize) -> Result<(), Errno> {
    if uaddr == 0 || ulenp == 0 {
        return Ok(());
    }
    if !writable(ulenp, size_of::<u32>()) {
        return Err(Errno(EFAULT));
    }
    let ulen: i32 = with_space(|s| s.read_val(ulenp))?;
    if ulen > 0 && !writable(uaddr, (ulen as usize).min(SOCKADDR_MAX)) {
        return Err(Errno(EFAULT));
    }
    Ok(())
}

pub fn recvfrom(
    fd: i32,
    buf: usize,
    len: usize,
    flags: u32,
    addr: usize,
    alenp: usize,
) -> SysResult {
    // Linux __sys_recvfrom: import_ubuf (clamp, access_ok → EFAULT)
    // before the socket lookup.
    let len = len.min(MAX_RW);
    if !access_ok(buf, len) {
        return Err(Errno(EFAULT));
    }
    let of = sock(fd)?;
    let room = out_room(buf, len)?;
    check_addr_out(addr, alenp)?;
    let r = recv_common(
        &of,
        Iov::new(vec![(buf, room)]),
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

/// `sendmsg`, checking in Linux's order (`___sys_sendmsg`,
/// `copy_msghdr_from_user`, `____sys_sendmsg`): the msghdr (EFAULT), the
/// name length ([`check_msghdr`]: EINVAL, clamped to 128), the name bytes
/// (EFAULT), `msg_iovlen` (EMSGSIZE), the iovecs ([`read_iov`]),
/// `msg_controllen` > `INT_MAX` or too large to allocate → ENOBUFS, the
/// control bytes (EFAULT, also for a NULL `msg_control` with a length),
/// and only then the address itself (the protocol's EINVAL/EAFNOSUPPORT).
pub fn sendmsg(fd: i32, msg: usize, flags: u32) -> SysResult {
    let of = sock(fd)?;
    let m: Msghdr = with_space(|s| s.read_val(msg))?;
    let namelen = check_msghdr(&m)?;
    let mut name = [0u8; SOCKADDR_MAX];
    if namelen > 0 {
        with_space(|s| s.copy_from_user(m.msg_name as usize, &mut name[..namelen]))?;
    }
    let iov = Iov::new(read_iov(m.msg_iov as usize, m.msg_iovlen as usize)?);
    let clen = m.msg_controllen as usize;
    if clen > i32::MAX as usize || clen >= OPTMEM_MAX {
        return Err(Errno(ENOBUFS));
    }
    let mut control = Vec::new();
    if clen > 0 {
        control
            .try_reserve_exact(clen)
            .map_err(|_| Errno(ENOBUFS))?;
        control.resize(clen, 0);
        with_space(|s| s.copy_from_user(m.msg_control as usize, &mut control))?;
    }
    let to = if namelen > 0 {
        Some(parse_addr(&name[..namelen])?)
    } else {
        None
    };
    send_common(&of, iov, to, MsgFlags::from_bits_retain(flags), &control)
}

pub fn recvmsg(fd: i32, msg: usize, flags: u32) -> SysResult {
    let of = sock(fd)?;
    let mut m: Msghdr = with_space(|s| s.read_val(msg))?;
    // Linux __copy_msghdr: NULL name → 0, negative → EINVAL, clamped to
    // 128, more than UIO_MAXIOV iovecs → EMSGSIZE.
    m.msg_namelen = check_msghdr(&m)? as u32;
    let mut iovs = read_iov(m.msg_iov as usize, m.msg_iovlen as usize)?;
    // Everything written after the data is consumed must be writable
    // first (D86): the iovecs (trimmed to what can be filled), the
    // msghdr, the name; control data is sized to what can be stored.
    clamp_iov_out(&mut iovs)?;
    if !writable(msg, size_of::<Msghdr>()) {
        return Err(Errno(EFAULT));
    }
    if m.msg_name != 0
        && m.msg_namelen > 0
        && !writable(m.msg_name as usize, (m.msg_namelen as usize).min(SOCKADDR_MAX))
    {
        return Err(Errno(EFAULT));
    }
    let room = if m.msg_control != 0 {
        let want = m.msg_controllen as usize;
        if access_ok(m.msg_control as usize, want) {
            with_space(|s| s.writable_prefix(m.msg_control as usize, want))
        } else {
            0
        }
    } else {
        0
    };
    let iov = Iov::new(iovs);
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
    let cn = r.control.len().min(room);
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
    })
}
