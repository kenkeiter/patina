//! `NETLINK_ROUTE` replies (host-testable): what a minimal rtnetlink
//! socket answers to the requests `syscall.NetlinkRIB` sends for
//! `net.Interfaces` / `net.InterfaceAddrs` (`RTM_GETLINK`, `RTM_GETADDR`
//! dumps), built from a description of the links and their addresses.
//!
//! [`respond`] takes the bytes of one `sendto` (one or more requests) and
//! returns the datagrams to queue for `recvfrom`: the `RTM_NEWLINK` /
//! `RTM_NEWADDR` messages (`NLM_F_MULTI`), packed into datagrams of at most
//! [`DATAGRAM`] bytes, then `NLMSG_DONE`. Every reply carries the request's
//! sequence number and the socket's port id. Other request types get an
//! `NLMSG_ERROR` with `-EOPNOTSUPP`; `NLM_F_ACK` on a non-dump request
//! adds an `NLMSG_ERROR` 0 acknowledgement.

use alloc::vec::Vec;
use core::net::IpAddr;

use patina_abi::errno::{EINVAL, ENODEV, EOPNOTSUPP};
use patina_abi::net::{AF_INET, AF_INET6, AF_UNSPEC};
use patina_abi::netlink::*;

/// Largest reply datagram (Linux dumps fill about a page per `recvmsg`).
pub const DATAGRAM: usize = 4096;

/// One network interface.
#[derive(Clone, Debug)]
pub struct Link<'a> {
    pub index: u32,
    pub name: &'a str,
    /// `IFF_*`.
    pub flags: u32,
    pub mtu: u32,
    /// `ARPHRD_*`.
    pub hwtype: u16,
    pub mac: [u8; 6],
}

/// One interface address.
#[derive(Clone, Debug)]
pub struct IfAddr {
    pub index: u32,
    pub addr: IpAddr,
    pub prefix: u8,
}

/// A parsed `nlmsghdr` and its payload.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub ty: u16,
    pub flags: u16,
    pub seq: u32,
    pub payload: &'a [u8],
    /// The whole message (echoed in `NLMSG_ERROR`).
    pub raw: &'a [u8],
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_ne_bytes(b[i..i + 4].try_into().unwrap())
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_ne_bytes(b[i..i + 2].try_into().unwrap())
}

/// Split a buffer into messages. A malformed header ends the list (Linux
/// silently ignores trailing garbage).
pub fn parse(mut b: &[u8]) -> Vec<Request<'_>> {
    let mut v = Vec::new();
    while b.len() >= NLMSG_HDRLEN {
        let len = u32_at(b, 0) as usize;
        if len < NLMSG_HDRLEN || len > b.len() {
            break;
        }
        v.push(Request {
            ty: u16_at(b, 4),
            flags: u16_at(b, 6),
            seq: u32_at(b, 8),
            payload: &b[NLMSG_HDRLEN..len],
            raw: &b[..len],
        });
        b = &b[nlmsg_align(len).min(b.len())..];
    }
    v
}

/// A message under construction.
struct Msg(Vec<u8>);

impl Msg {
    fn new(ty: u16, flags: u16, seq: u32, pid: u32) -> Msg {
        let h = NlMsghdr {
            nlmsg_len: 0,
            nlmsg_type: ty,
            nlmsg_flags: flags,
            nlmsg_seq: seq,
            nlmsg_pid: pid,
        };
        let mut v = Vec::with_capacity(128);
        v.extend_from_slice(zerocopy::IntoBytes::as_bytes(&h));
        Msg(v)
    }

    fn pad(&mut self) {
        while self.0.len() % NLMSG_ALIGNTO != 0 {
            self.0.push(0);
        }
    }

    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
        self.pad();
    }

    fn attr(&mut self, ty: u16, val: &[u8]) {
        let a = RtAttr {
            rta_len: (size_of::<RtAttr>() + val.len()) as u16,
            rta_type: ty,
        };
        self.0.extend_from_slice(zerocopy::IntoBytes::as_bytes(&a));
        self.0.extend_from_slice(val);
        self.pad();
    }

    fn finish(mut self) -> Vec<u8> {
        self.pad();
        let n = self.0.len() as u32;
        self.0[..4].copy_from_slice(&n.to_ne_bytes());
        self.0
    }
}

/// `RTM_NEWLINK` for `l`.
pub fn link_msg(l: &Link, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut m = Msg::new(RTM_NEWLINK, flags, seq, pid);
    let ifi = IfInfomsg {
        ifi_family: AF_UNSPEC as u8,
        ifi_pad: 0,
        ifi_type: l.hwtype,
        ifi_index: l.index as i32,
        ifi_flags: l.flags,
        ifi_change: 0,
    };
    m.bytes(zerocopy::IntoBytes::as_bytes(&ifi));
    let mut name = Vec::from(l.name.as_bytes());
    name.push(0);
    m.attr(IFLA_IFNAME, &name);
    m.attr(IFLA_MTU, &l.mtu.to_ne_bytes());
    m.attr(IFLA_TXQLEN, &1000u32.to_ne_bytes());
    let oper = if l.flags & IFF_LOOPBACK != 0 { IF_OPER_UNKNOWN } else { IF_OPER_UP };
    m.attr(IFLA_OPERSTATE, &[oper]);
    m.attr(IFLA_ADDRESS, &l.mac);
    let bcast = if l.flags & IFF_BROADCAST != 0 { [0xff; 6] } else { [0; 6] };
    m.attr(IFLA_BROADCAST, &bcast);
    m.finish()
}

fn scope_of(a: IpAddr) -> u8 {
    match a {
        IpAddr::V4(v) if v.is_loopback() => RT_SCOPE_HOST,
        IpAddr::V6(v) if v.is_loopback() => RT_SCOPE_HOST,
        IpAddr::V6(v) if (v.segments()[0] & 0xffc0) == 0xfe80 => RT_SCOPE_LINK,
        _ => RT_SCOPE_UNIVERSE,
    }
}

/// `RTM_NEWADDR` for `a` (on the link named `label`).
pub fn addr_msg(a: &IfAddr, label: &str, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut m = Msg::new(RTM_NEWADDR, flags, seq, pid);
    let family = match a.addr {
        IpAddr::V4(_) => AF_INET,
        IpAddr::V6(_) => AF_INET6,
    };
    let ifa = IfAddrmsg {
        ifa_family: family as u8,
        ifa_prefixlen: a.prefix,
        ifa_flags: IFA_F_PERMANENT,
        ifa_scope: scope_of(a.addr),
        ifa_index: a.index,
    };
    m.bytes(zerocopy::IntoBytes::as_bytes(&ifa));
    match a.addr {
        IpAddr::V4(v) => {
            m.attr(IFA_ADDRESS, &v.octets());
            m.attr(IFA_LOCAL, &v.octets());
            let mut name = Vec::from(label.as_bytes());
            name.push(0);
            m.attr(IFA_LABEL, &name);
            if !v.is_loopback() && a.prefix < 31 {
                let host = u32::MAX >> a.prefix;
                m.attr(IFA_BROADCAST, &(u32::from(v) | host).to_be_bytes());
            }
        }
        IpAddr::V6(v) => m.attr(IFA_ADDRESS, &v.octets()),
    }
    m.finish()
}

/// `NLMSG_DONE` (payload: an `int` 0).
pub fn done_msg(seq: u32, pid: u32) -> Vec<u8> {
    let mut m = Msg::new(NLMSG_DONE, NLM_F_MULTI, seq, pid);
    m.bytes(&0i32.to_ne_bytes());
    m.finish()
}

/// `NLMSG_ERROR` carrying `-errno` (0: an ACK) and the request header.
pub fn error_msg(req: &Request, errno: i32, pid: u32) -> Vec<u8> {
    let mut m = Msg::new(NLMSG_ERROR, 0, req.seq, pid);
    m.bytes(&(-errno).to_ne_bytes());
    m.bytes(&req.raw[..NLMSG_HDRLEN]);
    m.finish()
}

/// Pack messages into datagrams of at most [`DATAGRAM`] bytes.
fn pack(msgs: Vec<Vec<u8>>, out: &mut Vec<Vec<u8>>) {
    let mut cur: Vec<u8> = Vec::new();
    for m in msgs {
        if !cur.is_empty() && cur.len() + m.len() > DATAGRAM {
            out.push(core::mem::take(&mut cur));
        }
        cur.extend_from_slice(&m);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
}

/// Family filter of a dump request: the first payload byte (`rtgenmsg`,
/// `ifinfomsg` and `ifaddrmsg` all start with the family).
fn family_ok(req: &Request, a: IpAddr) -> bool {
    match req.payload.first().map(|&f| f as u16) {
        None | Some(AF_UNSPEC) => true,
        Some(AF_INET) => a.is_ipv4(),
        Some(AF_INET6) => a.is_ipv6(),
        Some(_) => false,
    }
}

/// Answer the requests in `buf` for a socket with port id `pid`: the
/// datagrams to queue, in order.
pub fn respond(buf: &[u8], pid: u32, links: &[Link], addrs: &[IfAddr]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for req in parse(buf) {
        if req.flags & NLM_F_REQUEST == 0 {
            continue;
        }
        let dump = req.flags & NLM_F_DUMP == NLM_F_DUMP;
        let label = |index: u32| {
            links
                .iter()
                .find(|l| l.index == index)
                .map_or("", |l| l.name)
        };
        let mut msgs = Vec::new();
        match req.ty {
            RTM_GETLINK if dump => {
                for l in links {
                    msgs.push(link_msg(l, NLM_F_MULTI, req.seq, pid));
                }
                msgs.push(done_msg(req.seq, pid));
            }
            RTM_GETLINK => {
                // By index (ifinfomsg.ifi_index) or by IFLA_IFNAME.
                let index = if req.payload.len() >= size_of::<IfInfomsg>() {
                    u32_at(req.payload, 4)
                } else {
                    0
                };
                let mut name: Option<&[u8]> = None;
                let mut a = req.payload.get(size_of::<IfInfomsg>()..).unwrap_or(&[]);
                while a.len() >= 4 {
                    let len = u16_at(a, 0) as usize;
                    if len < 4 || len > a.len() {
                        break;
                    }
                    if u16_at(a, 2) == IFLA_IFNAME {
                        let v = &a[4..len];
                        name = Some(&v[..v.iter().position(|&c| c == 0).unwrap_or(v.len())]);
                    }
                    a = &a[nlmsg_align(len).min(a.len())..];
                }
                let found = links.iter().find(|l| {
                    (index != 0 && l.index == index)
                        || (index == 0 && name == Some(l.name.as_bytes()))
                });
                match found {
                    Some(l) => msgs.push(link_msg(l, 0, req.seq, pid)),
                    None if index == 0 && name.is_none() => {
                        msgs.push(error_msg(&req, EINVAL, pid))
                    }
                    None => msgs.push(error_msg(&req, ENODEV, pid)),
                }
                if req.flags & NLM_F_ACK != 0 && found.is_some() {
                    msgs.push(error_msg(&req, 0, pid));
                }
            }
            RTM_GETADDR if dump => {
                for a in addrs.iter().filter(|a| family_ok(&req, a.addr)) {
                    msgs.push(addr_msg(a, label(a.index), NLM_F_MULTI, req.seq, pid));
                }
                msgs.push(done_msg(req.seq, pid));
            }
            _ => msgs.push(error_msg(&req, EOPNOTSUPP, pid)),
        }
        pack(msgs, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, Ipv6Addr};

    fn request(ty: u16, flags: u16, seq: u32, family: u8) -> Vec<u8> {
        // What syscall.newNetlinkRouteRequest builds: header + rtgenmsg.
        let mut b = Vec::new();
        b.extend_from_slice(&17u32.to_ne_bytes());
        b.extend_from_slice(&ty.to_ne_bytes());
        b.extend_from_slice(&flags.to_ne_bytes());
        b.extend_from_slice(&seq.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        b.push(family);
        b
    }

    fn links() -> [Link<'static>; 2] {
        [
            Link {
                index: 1,
                name: "lo",
                flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
                mtu: 65536,
                hwtype: ARPHRD_LOOPBACK,
                mac: [0; 6],
            },
            Link {
                index: 2,
                name: "eth0",
                flags: IFF_UP | IFF_BROADCAST | IFF_MULTICAST | IFF_RUNNING | IFF_LOWER_UP,
                mtu: 1500,
                hwtype: ARPHRD_ETHER,
                mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56],
            },
        ]
    }

    fn addrs() -> Vec<IfAddr> {
        alloc::vec![
            IfAddr { index: 1, addr: IpAddr::V4(Ipv4Addr::LOCALHOST), prefix: 8 },
            IfAddr { index: 1, addr: IpAddr::V6(Ipv6Addr::LOCALHOST), prefix: 128 },
            IfAddr { index: 2, addr: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)), prefix: 24 },
        ]
    }

    /// Walk the attributes of a NEWLINK/NEWADDR message (`fixed`: size of
    /// the family header).
    fn attrs(m: &Request, fixed: usize) -> Vec<(u16, Vec<u8>)> {
        let mut v = Vec::new();
        let mut a = &m.payload[fixed..];
        while a.len() >= 4 {
            let len = u16_at(a, 0) as usize;
            v.push((u16_at(a, 2), a[4..len].to_vec()));
            a = &a[nlmsg_align(len).min(a.len())..];
        }
        v
    }

    #[test]
    fn link_dump() {
        let req = request(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, 0);
        let dgs = respond(&req, 77, &links(), &addrs());
        assert_eq!(dgs.len(), 1);
        assert!(dgs[0].len() <= DATAGRAM);
        let msgs = parse(&dgs[0]);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].ty, NLMSG_DONE);
        for m in &msgs {
            assert_eq!(m.seq, 1);
            assert_eq!(u32_at(m.raw, 12), 77);
            assert_eq!(m.raw.len() % 4, 0);
        }
        assert_eq!(msgs[1].ty, RTM_NEWLINK);
        assert_eq!(msgs[1].flags, NLM_F_MULTI);
        assert_eq!(u16_at(msgs[1].payload, 2), ARPHRD_ETHER);
        assert_eq!(u32_at(msgs[1].payload, 4), 2);
        let a = attrs(&msgs[1], size_of::<IfInfomsg>());
        assert!(a.contains(&(IFLA_IFNAME, b"eth0\0".to_vec())));
        assert!(a.contains(&(IFLA_MTU, 1500u32.to_ne_bytes().to_vec())));
        assert!(a.contains(&(IFLA_ADDRESS, alloc::vec![0x52, 0x54, 0, 0x12, 0x34, 0x56])));
        let a = attrs(&msgs[0], size_of::<IfInfomsg>());
        assert!(a.contains(&(IFLA_IFNAME, b"lo\0".to_vec())));
    }

    #[test]
    fn addr_dump_and_filter() {
        let req = request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 5, 0);
        let dgs = respond(&req, 9, &links(), &addrs());
        let msgs = parse(&dgs[0]);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].ty, RTM_NEWADDR);
        // 127.0.0.1/8 on lo, host scope.
        assert_eq!(msgs[0].payload[..4], [AF_INET as u8, 8, IFA_F_PERMANENT, RT_SCOPE_HOST]);
        assert_eq!(u32_at(msgs[0].payload, 4), 1);
        let a = attrs(&msgs[0], size_of::<IfAddrmsg>());
        assert!(a.contains(&(IFA_LOCAL, alloc::vec![127, 0, 0, 1])));
        assert!(a.contains(&(IFA_LABEL, b"lo\0".to_vec())));
        // ::1/128.
        assert_eq!(msgs[1].payload[..2], [AF_INET6 as u8, 128]);
        let a = attrs(&msgs[1], size_of::<IfAddrmsg>());
        assert_eq!(a[0], (IFA_ADDRESS, Ipv6Addr::LOCALHOST.octets().to_vec()));
        // 10.0.2.15/24 on eth0 with its broadcast address.
        let a = attrs(&msgs[2], size_of::<IfAddrmsg>());
        assert!(a.contains(&(IFA_BROADCAST, alloc::vec![10, 0, 2, 255])));
        assert_eq!(msgs[3].ty, NLMSG_DONE);

        let req = request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 5, AF_INET6 as u8);
        let msgs = parse(&respond(&req, 9, &links(), &addrs())[0]).len();
        assert_eq!(msgs, 2);
    }

    #[test]
    fn big_dump_is_split() {
        let many: Vec<IfAddr> = (0..200u32)
            .map(|i| IfAddr {
                index: 2,
                addr: IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, i as u16)),
                prefix: 64,
            })
            .collect();
        let req = request(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 1, 0);
        let dgs = respond(&req, 1, &links(), &many);
        assert!(dgs.len() > 1);
        let n: usize = dgs.iter().map(|d| parse(d).len()).sum();
        assert_eq!(n, 201);
        assert!(dgs.iter().all(|d| d.len() <= DATAGRAM));
        assert_eq!(parse(dgs.last().unwrap()).last().unwrap().ty, NLMSG_DONE);
    }

    #[test]
    fn unsupported_and_single() {
        let req = request(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 3, 0);
        let msgs = respond(&req, 4, &links(), &addrs());
        let m = parse(&msgs[0]);
        assert_eq!(m[0].ty, NLMSG_ERROR);
        assert_eq!(i32::from_ne_bytes(m[0].payload[..4].try_into().unwrap()), -EOPNOTSUPP);
        assert_eq!(m[0].payload[4..20], req[..16]);

        // RTM_GETLINK for index 2 (ifinfomsg), no dump.
        let mut b = Vec::new();
        b.extend_from_slice(&32u32.to_ne_bytes());
        b.extend_from_slice(&RTM_GETLINK.to_ne_bytes());
        b.extend_from_slice(&NLM_F_REQUEST.to_ne_bytes());
        b.extend_from_slice(&8u32.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes());
        let ifi = IfInfomsg { ifi_index: 2, ..Default::default() };
        b.extend_from_slice(zerocopy::IntoBytes::as_bytes(&ifi));
        let m = respond(&b, 4, &links(), &addrs());
        let m = parse(&m[0]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].ty, RTM_NEWLINK);
        assert_eq!(u32_at(m[0].payload, 4), 2);
    }
}
