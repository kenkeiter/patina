//! `AF_NETLINK` / `NETLINK_ROUTE` sockets, read-only (D73): enough
//! rtnetlink for `net.Interfaces`, `net.InterfaceAddrs` and
//! `net.InterfaceByName/Index` (`syscall.NetlinkRIB`).
//!
//! A `sendto` of a `RTM_GETLINK` / `RTM_GETADDR` dump request queues the
//! reply datagrams at once (built by `patina_inet::netlink::respond` from
//! the stack's interface): `lo` (index 1) with 127.0.0.1/8 and ::1/128,
//! and `eth0` (index 2, the NIC's MAC) with the other addresses, when
//! there is a NIC. `recvfrom` / `recvmsg` / `read` return one datagram
//! each (truncated to the buffer with `MSG_TRUNC` semantics; `MSG_PEEK`
//! keeps it). Other requests get `NLMSG_ERROR` `-EOPNOTSUPP`. No
//! multicast groups, no notifications, nothing can be changed.
//!
//! Port ids: `bind` with pid 0 (or the first send) assigns the process id
//! if no other netlink socket holds it, else a negative-style id counting
//! down from `0xffff_f000` (Linux's `netlink_autobind`); an explicit pid
//! already in use is `EADDRINUSE`.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::IpAddr;

use patina_abi::current::Stat;
use patina_abi::epoll::EpollFlags;
use patina_abi::errno::*;
use patina_abi::fs::S_IFSOCK;
use patina_abi::misc::FIONREAD;
use patina_abi::net::*;
use patina_abi::netlink::*;
use patina_inet::SockAddr;
use patina_inet::netlink::{IfAddr, Link};
use smoltcp::wire::IpCidr;

use super::socket::{self as so, RecvInfo, SocketOps};
use crate::fs::file::{File, FileFlags, PollHook, PollList, alloc_ino, anon_stat};
use crate::sync::SpinLock;

/// Bytes of queued replies beyond which further requests fail with
/// `ENOBUFS` (Linux: the receive buffer).
const RCVBUF: usize = 212_992;

/// Port ids in use.
static PIDS: SpinLock<BTreeSet<u32>> = SpinLock::new(BTreeSet::new());
static NEXT_AUTO: SpinLock<u32> = SpinLock::new(0xffff_f000);

struct State {
    /// Bound port id.
    pid: Option<u32>,
    queue: VecDeque<Vec<u8>>,
    queued: usize,
    shut_rd: bool,
    rcvtimeo: Option<u64>,
    sndtimeo: Option<u64>,
    hooks: PollList,
}

pub struct NetlinkSocket {
    ty: u32,
    ino: u64,
    st: SpinLock<State>,
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        crate::mm::heap::uncharge(self.st.lock().queued);
    }
}

/// `socket(AF_NETLINK, ty, protocol)` (`ty` without the flag bits).
pub fn create(ty: u32, protocol: i32) -> Result<Arc<NetlinkSocket>, Errno> {
    if ty != SOCK_RAW && ty != SOCK_DGRAM {
        return Err(Errno(ESOCKTNOSUPPORT));
    }
    if protocol != NETLINK_ROUTE {
        return Err(Errno(EPROTONOSUPPORT));
    }
    Ok(Arc::new(NetlinkSocket {
        ty,
        ino: alloc_ino(),
        st: SpinLock::new(State {
            pid: None,
            queue: VecDeque::new(),
            queued: 0,
            shut_rd: false,
            rcvtimeo: None,
            sndtimeo: None,
            hooks: PollList::new(),
        }),
    }))
}

fn autobind(st: &mut State) -> u32 {
    if let Some(p) = st.pid {
        return p;
    }
    let mut pids = PIDS.lock();
    let tgid = crate::task::process().pid;
    let pid = if pids.contains(&tgid) {
        let mut next = NEXT_AUTO.lock();
        while pids.contains(&*next) {
            *next = next.wrapping_sub(1);
        }
        *next
    } else {
        tgid
    };
    pids.insert(pid);
    st.pid = Some(pid);
    pid
}

/// The interfaces and addresses, from the stack.
fn snapshot() -> (Vec<Link<'static>>, Vec<IfAddr>) {
    let mut links = Vec::new();
    let mut addrs = Vec::new();
    links.push(Link {
        index: 1,
        name: "lo",
        flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
        mtu: 65536,
        hwtype: ARPHRD_LOOPBACK,
        mac: [0; 6],
    });
    let Some(lock) = super::stack::get() else {
        return (links, addrs);
    };
    let s = lock.lock();
    let has_nic = s.out.has_nic();
    if has_nic {
        let mac = s.out.mac;
        links.push(Link {
            index: 2,
            name: "eth0",
            flags: IFF_UP | IFF_BROADCAST | IFF_MULTICAST | IFF_RUNNING | IFF_LOWER_UP,
            mtu: 1500,
            hwtype: ARPHRD_ETHER,
            mac,
        });
    }
    for c in &s.addrs {
        let (addr, prefix) = match c {
            IpCidr::Ipv4(c) => (IpAddr::V4(c.address()), c.prefix_len()),
            IpCidr::Ipv6(c) => (IpAddr::V6(c.address()), c.prefix_len()),
        };
        let index = if addr.is_loopback() || !has_nic { 1 } else { 2 };
        addrs.push(IfAddr {
            index,
            addr,
            prefix,
        });
    }
    // Linux lists IPv4 addresses before IPv6 ones.
    addrs.sort_by_key(|a| (a.addr.is_ipv6(), a.index));
    (links, addrs)
}

/// `/proc/net/igmp`: every link is in 224.0.0.1 (all-hosts), as on
/// Linux. Go's `Interface.MulticastAddrs` reads it.
pub fn proc_igmp() -> Vec<u8> {
    let (links, _) = snapshot();
    let mut s = alloc::string::String::from(
        "Idx\tDevice    : Count Querier\tGroup    Users Timer\tReporter\n",
    );
    for l in &links {
        s.push_str(&format!(
            "{}\t{:<10}: {:>5} {:>7}\n",
            l.index, l.name, 1, "V3"
        ));
        // The group is a __be32 printed as a host-order hex number.
        let g = u32::from_ne_bytes([224, 0, 0, 1]);
        s.push_str(&format!("\t\t\t\t{:08X} {:>5} 0:00000000\t\t0\n", g, 1));
    }
    s.into_bytes()
}

/// `/proc/net/igmp6`: ff02::1 and ff01::1 on every link, plus the
/// solicited-node group of each non-loopback IPv6 address.
pub fn proc_igmp6() -> Vec<u8> {
    let (links, addrs) = snapshot();
    let mut s = alloc::string::String::new();
    let mut line = |l: &Link, g: [u8; 16], flags: u32| {
        let mut hex = alloc::string::String::new();
        for b in g {
            hex.push_str(&format!("{:02x}", b));
        }
        s.push_str(&format!(
            "{:<4} {:<15} {} {:>5} {:08X} {}\n",
            l.index, l.name, hex, 1, flags, 0
        ));
    };
    for l in &links {
        for a in addrs.iter().filter(|a| a.index == l.index) {
            if let IpAddr::V6(v) = a.addr
                && !v.is_loopback()
            {
                let mut g = [0u8; 16];
                g[0] = 0xff;
                g[1] = 0x02;
                g[11] = 0x01;
                g[12] = 0xff;
                g[13..].copy_from_slice(&v.octets()[13..]);
                line(l, g, 4);
            }
        }
        let mut all = [0u8; 16];
        all[0] = 0xff;
        all[1] = 0x02;
        all[15] = 1;
        line(l, all, 0xc);
        all[1] = 0x01;
        line(l, all, 0xc);
    }
    s.into_bytes()
}

impl NetlinkSocket {
    fn local(&self) -> SockAddr {
        SockAddr::Netlink {
            pid: self.st.lock().pid.unwrap_or(0),
            groups: 0,
        }
    }
}

impl SocketOps for NetlinkSocket {
    fn domain(&self) -> u16 {
        AF_NETLINK
    }

    fn sock_type(&self) -> u32 {
        self.ty
    }

    fn protocol(&self) -> i32 {
        NETLINK_ROUTE
    }

    fn bind(&self, addr: &SockAddr) -> Result<(), Errno> {
        let SockAddr::Netlink { pid, groups } = *addr else {
            return Err(Errno(EINVAL));
        };
        if groups != 0 {
            // No multicast notifications are ever sent; joining is harmless.
        }
        let mut st = self.st.lock();
        if let Some(cur) = st.pid {
            return if pid == 0 || pid == cur {
                Ok(())
            } else {
                Err(Errno(EINVAL))
            };
        }
        if pid == 0 {
            autobind(&mut st);
            return Ok(());
        }
        let mut pids = PIDS.lock();
        if !pids.insert(pid) {
            return Err(Errno(EADDRINUSE));
        }
        st.pid = Some(pid);
        Ok(())
    }

    fn listen(&self, _backlog: i32) -> Result<(), Errno> {
        Err(Errno(EOPNOTSUPP))
    }

    fn accept(&self) -> Result<(Arc<dyn File>, SockAddr), Errno> {
        Err(Errno(EOPNOTSUPP))
    }

    fn connect(&self, addr: &SockAddr) -> Result<(), Errno> {
        match addr {
            SockAddr::Netlink { .. } => {
                autobind(&mut self.st.lock());
                Ok(())
            }
            SockAddr::Unspec => Ok(()),
            _ => Err(Errno(EINVAL)),
        }
    }

    fn send(
        &self,
        data: &[u8],
        to: Option<&SockAddr>,
        _flags: MsgFlags,
        _control: &[u8],
    ) -> Result<usize, Errno> {
        match to {
            None | Some(SockAddr::Netlink { pid: 0, .. }) => {}
            // Only the kernel (pid 0) answers.
            Some(SockAddr::Netlink { .. }) => return Err(Errno(ECONNREFUSED)),
            Some(_) => return Err(Errno(EINVAL)),
        }
        let pid = autobind(&mut self.st.lock());
        let (links, addrs) = snapshot();
        let replies = patina_inet::netlink::respond(data, pid, &links, &addrs);
        let notify = {
            let mut st = self.st.lock();
            let bytes: usize = replies.iter().map(Vec::len).sum();
            if st.queued + bytes > RCVBUF {
                return Err(Errno(ENOBUFS));
            }
            // Queued replies are charged to the program until read (D87).
            if !crate::mm::heap::charge(bytes) {
                return Err(Errno(ENOBUFS));
            }
            st.queued += bytes;
            let any = !replies.is_empty();
            st.queue.extend(replies);
            if any {
                st.hooks.collect(EpollFlags::IN | EpollFlags::RDNORM)
            } else {
                crate::fs::file::Notify::NONE
            }
        };
        notify.fire();
        Ok(data.len())
    }

    fn recv(&self, buf: &mut [u8], flags: MsgFlags) -> Result<RecvInfo, Errno> {
        let mut st = self.st.lock();
        let Some(dg) = st.queue.front() else {
            if st.shut_rd {
                return Ok(RecvInfo {
                    len: 0,
                    full: 0,
                    from: None,
                    control: Vec::new(),
                });
            }
            return Err(Errno(EAGAIN));
        };
        let full = dg.len();
        let n = full.min(buf.len());
        buf[..n].copy_from_slice(&dg[..n]);
        if !flags.contains(MsgFlags::PEEK) {
            st.queue.pop_front();
            st.queued -= full;
            crate::mm::heap::uncharge(full);
        }
        Ok(RecvInfo {
            len: n,
            full,
            from: Some(SockAddr::Netlink { pid: 0, groups: 0 }),
            control: Vec::new(),
        })
    }

    fn shutdown(&self, how: i32) -> Result<(), Errno> {
        if !(SHUT_RD..=SHUT_RDWR).contains(&how) {
            return Err(Errno(EINVAL));
        }
        let notify = {
            let mut st = self.st.lock();
            if how != SHUT_WR {
                st.shut_rd = true;
            }
            st.hooks.collect(EpollFlags::IN | EpollFlags::RDHUP)
        };
        notify.fire();
        Ok(())
    }

    fn local_addr(&self) -> Result<SockAddr, Errno> {
        Ok(self.local())
    }

    fn peer_addr(&self) -> Result<SockAddr, Errno> {
        Ok(SockAddr::Netlink { pid: 0, groups: 0 })
    }

    fn setsockopt(&self, level: i32, name: i32, val: &[u8]) -> Result<(), Errno> {
        let mut st = self.st.lock();
        match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => st.rcvtimeo = so::timeval_opt(val)?,
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => st.sndtimeo = so::timeval_opt(val)?,
            (SOL_SOCKET, SO_RCVBUF | SO_SNDBUF | SO_RCVBUFFORCE | SO_SNDBUFFORCE) => {
                so::int_opt(val)?;
            }
            // NETLINK_ADD_MEMBERSHIP .. NETLINK_GET_STRICT_CHK: accepted.
            (SOL_NETLINK, 1..=12) => {
                so::int_opt(val)?;
            }
            _ => return Err(Errno(ENOPROTOOPT)),
        }
        Ok(())
    }

    fn getsockopt(&self, level: i32, name: i32, out: &mut [u8]) -> Result<usize, Errno> {
        let st = self.st.lock();
        let int = |v: i32, out: &mut [u8]| Ok(so::int_bytes(v, out));
        match (level, name) {
            (SOL_SOCKET, SO_TYPE) => int(self.ty as i32, out),
            (SOL_SOCKET, SO_DOMAIN) => int(AF_NETLINK as i32, out),
            (SOL_SOCKET, SO_PROTOCOL) => int(NETLINK_ROUTE, out),
            (SOL_SOCKET, SO_ERROR | SO_ACCEPTCONN) => int(0, out),
            (SOL_SOCKET, SO_RCVBUF | SO_SNDBUF) => int(RCVBUF as i32, out),
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
                Ok(so::timeval_bytes(st.rcvtimeo, out))
            }
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
                Ok(so::timeval_bytes(st.sndtimeo, out))
            }
            (SOL_NETLINK, 1..=12) => int(0, out),
            _ => Err(Errno(ENOPROTOOPT)),
        }
    }

    fn timeouts(&self) -> (Option<u64>, Option<u64>) {
        let st = self.st.lock();
        (st.rcvtimeo, st.sndtimeo)
    }
}

impl File for NetlinkSocket {
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
        let mut st = self.st.lock();
        if let Some(h) = hook {
            st.hooks.add(h);
        }
        let mut ev = EpollFlags::OUT | EpollFlags::WRNORM;
        if !st.queue.is_empty() || st.shut_rd {
            ev |= EpollFlags::IN | EpollFlags::RDNORM;
        }
        ev
    }

    fn ioctl(&self, cmd: u32, arg: u64) -> Result<isize, Errno> {
        if cmd != FIONREAD {
            return Err(Errno(ENOTTY));
        }
        let n = self.st.lock().queue.front().map_or(0, Vec::len) as i32;
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
        if let Some(pid) = self.st.lock().pid.take() {
            PIDS.lock().remove(&pid);
        }
    }

    fn as_socket(&self) -> Option<&dyn SocketOps> {
        Some(self)
    }
}
