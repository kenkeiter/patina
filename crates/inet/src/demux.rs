//! Received-frame demultiplexing for the kernel's per-socket smoltcp socket
//! sets (D67).
//!
//! The kernel keeps one smoltcp `SocketSet` per socket, so a received frame
//! has to be handed to the right set before smoltcp sees it. [`parse`]
//! reads just enough of an Ethernet frame to name its transport 4-tuple
//! ([`Flow`]); the kernel looks the tuple up in its flow table (exact
//! match: a connection, or a listener's not-yet-accepted connection) and
//! otherwise in a [`PortMap`] (listeners and UDP sockets by local port).
//! Frames with no transport tuple (ARP, ICMP, NDISC, later fragments, IPv6
//! extension headers other than one hop-by-hop header) go to an empty set:
//! smoltcp still answers them at the interface level.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::{Cover, Scope};

pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;
const IPPROTO_HOPOPTS: u8 = 0;
const IPPROTO_ICMPV6: u8 = 58;

const ETH_HDR: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_IPV6: u16 = 0x86dd;

/// A transport 4-tuple seen from this host (`local` is the packet's
/// destination). The fields are ordered so that comparisons (BTreeMap
/// keys) usually decide on the ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Flow {
    pub rport: u16,
    pub lport: u16,
    pub proto: u8,
    pub remote: IpAddr,
    pub local: IpAddr,
}

/// What [`parse`] found in a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A TCP or UDP header (of an unfragmented packet or a first fragment).
    Transport(Flow),
    /// ARP or ICMPv6 (NDISC): the interface may learn a neighbor from it.
    Neighbor,
    /// Anything else: ICMPv4, a later fragment, an unknown protocol, an
    /// IPv6 extension header we do not walk, a truncated frame.
    Other,
}

/// Read the transport tuple of an Ethernet frame (no checksums: smoltcp
/// checks those when it processes the frame).
pub fn parse(f: &[u8]) -> Parsed {
    if f.len() < ETH_HDR {
        return Parsed::Other;
    }
    match u16::from_be_bytes([f[12], f[13]]) {
        ETHERTYPE_ARP => Parsed::Neighbor,
        ETHERTYPE_IPV4 => parse_v4(&f[ETH_HDR..]),
        ETHERTYPE_IPV6 => parse_v6(&f[ETH_HDR..]),
        _ => Parsed::Other,
    }
}

fn parse_v4(p: &[u8]) -> Parsed {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return Parsed::Other;
    }
    let ihl = (p[0] & 0x0f) as usize * 4;
    let frag_off = u16::from_be_bytes([p[6], p[7]]) & 0x1fff;
    if ihl < 20 || frag_off != 0 {
        return Parsed::Other;
    }
    let proto = p[9];
    let src = Ipv4Addr::new(p[12], p[13], p[14], p[15]);
    let dst = Ipv4Addr::new(p[16], p[17], p[18], p[19]);
    ports(proto, IpAddr::V4(src), IpAddr::V4(dst), p, ihl)
}

fn parse_v6(p: &[u8]) -> Parsed {
    if p.len() < 40 || p[0] >> 4 != 6 {
        return Parsed::Other;
    }
    let mut nh = p[6];
    let mut t = 40;
    if nh == IPPROTO_HOPOPTS {
        // smoltcp walks one hop-by-hop header (MLD reports carry one).
        if p.len() < t + 2 {
            return Parsed::Other;
        }
        nh = p[t];
        t += (p[t + 1] as usize + 1) * 8;
    }
    if nh == IPPROTO_ICMPV6 {
        return Parsed::Neighbor;
    }
    let mut a = [0u8; 16];
    a.copy_from_slice(&p[8..24]);
    let src = Ipv6Addr::from(a);
    a.copy_from_slice(&p[24..40]);
    let dst = Ipv6Addr::from(a);
    ports(nh, IpAddr::V6(src), IpAddr::V6(dst), p, t)
}

fn ports(proto: u8, src: IpAddr, dst: IpAddr, p: &[u8], t: usize) -> Parsed {
    if (proto != IPPROTO_TCP && proto != IPPROTO_UDP) || p.len() < t + 4 {
        return Parsed::Other;
    }
    Parsed::Transport(Flow {
        rport: u16::from_be_bytes([p[t], p[t + 1]]),
        lport: u16::from_be_bytes([p[t + 2], p[t + 3]]),
        proto,
        remote: src,
        local: dst,
    })
}

/// A socket that receives on a local port: a TCP listener or a bound UDP
/// socket, with the addresses it covers ([`Cover`]).
#[derive(Clone, Copy, Debug)]
pub struct Bound<T> {
    pub target: T,
    pub cover: Cover,
}

/// Listeners and bound UDP sockets by (protocol, local port).
#[derive(Debug)]
pub struct PortMap<T> {
    map: BTreeMap<(u8, u16), Vec<Bound<T>>>,
}

impl<T> Default for PortMap<T> {
    fn default() -> Self {
        PortMap {
            map: BTreeMap::new(),
        }
    }
}

impl<T: Copy + PartialEq> PortMap<T> {
    pub fn new() -> PortMap<T> {
        PortMap::default()
    }

    /// Add `target` on (`proto`, `port`) after the ones already there.
    pub fn add(&mut self, proto: u8, port: u16, target: T, cover: Cover) {
        self.map
            .entry((proto, port))
            .or_default()
            .push(Bound { target, cover });
    }

    /// Remove `target` from (`proto`, `port`).
    pub fn remove(&mut self, proto: u8, port: u16, target: T) {
        if let Some(v) = self.map.get_mut(&(proto, port)) {
            v.retain(|b| b.target != target);
            if v.is_empty() {
                self.map.remove(&(proto, port));
            }
        }
    }

    /// The socket a packet to `dst`:`port` goes to: one bound to exactly
    /// `dst` before a wildcard one, and only one that covers `dst`'s
    /// family (a dual-stack `[::]` socket covers IPv4; an `AF_INET` or
    /// `IPV6_V6ONLY` one does not). `any_addr` (a UDP broadcast or
    /// multicast destination) also admits sockets bound to another address
    /// of that family, last. Ties go to the earliest added.
    pub fn pick(&self, proto: u8, port: u16, dst: IpAddr, any_addr: bool) -> Option<T> {
        let v = self.map.get(&(proto, port))?;
        let mut best: Option<(u8, T)> = None;
        for b in v {
            let score = match dst {
                IpAddr::V4(d) => scope_score(&b.cover.v4, &d, any_addr),
                IpAddr::V6(d) => scope_score(&b.cover.v6, &d, any_addr),
            };
            let Some(score) = score else {
                continue;
            };
            if best.is_none_or(|(s, _)| score > s) {
                best = Some((score, b.target));
                if score == 2 {
                    break;
                }
            }
        }
        best.map(|(_, t)| t)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

fn scope_score<A: PartialEq>(s: &Scope<A>, dst: &A, any_addr: bool) -> Option<u8> {
    match s {
        Scope::None => None,
        Scope::One(a) if a == dst => Some(2),
        Scope::Any => Some(1),
        Scope::One(_) if any_addr => Some(0),
        Scope::One(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use patina_abi::net::{AF_INET, AF_INET6};

    fn eth(ethertype: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = alloc::vec![0u8; 14];
        f[12..14].copy_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    fn v4_packet(proto: u8, src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16) -> Vec<u8> {
        let mut p = alloc::vec![0u8; 20];
        p[0] = 0x45;
        p[9] = proto;
        p[12..16].copy_from_slice(&src);
        p[16..20].copy_from_slice(&dst);
        p.extend_from_slice(&sport.to_be_bytes());
        p.extend_from_slice(&dport.to_be_bytes());
        p.extend_from_slice(&[0u8; 16]);
        eth(ETHERTYPE_IPV4, &p)
    }

    fn v6_packet(nh: u8, src: Ipv6Addr, dst: Ipv6Addr, rest: &[u8]) -> Vec<u8> {
        let mut p = alloc::vec![0u8; 40];
        p[0] = 0x60;
        p[6] = nh;
        p[8..24].copy_from_slice(&src.octets());
        p[24..40].copy_from_slice(&dst.octets());
        p.extend_from_slice(rest);
        eth(ETHERTYPE_IPV6, &p)
    }

    #[test]
    fn tcp_v4_tuple() {
        let f = v4_packet(6, [10, 0, 2, 2], [10, 0, 2, 15], 51000, 8080);
        assert_eq!(
            parse(&f),
            Parsed::Transport(Flow {
                rport: 51000,
                lport: 8080,
                proto: IPPROTO_TCP,
                remote: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
                local: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
            })
        );
    }

    #[test]
    fn v4_options_and_fragments() {
        // IHL 6 (4 bytes of options): the ports follow the options.
        let mut p = alloc::vec![0u8; 24];
        p[0] = 0x46;
        p[9] = 17;
        p[12..16].copy_from_slice(&[1, 2, 3, 4]);
        p[16..20].copy_from_slice(&[5, 6, 7, 8]);
        p.extend_from_slice(&67u16.to_be_bytes());
        p.extend_from_slice(&68u16.to_be_bytes());
        match parse(&eth(ETHERTYPE_IPV4, &p)) {
            Parsed::Transport(f) => {
                assert_eq!((f.proto, f.rport, f.lport), (IPPROTO_UDP, 67, 68));
            }
            other => panic!("{:?}", other),
        }
        // A later fragment has no transport header.
        let mut f = v4_packet(6, [1, 1, 1, 1], [2, 2, 2, 2], 1, 2);
        f[14 + 7] = 8;
        assert_eq!(parse(&f), Parsed::Other);
        // A first fragment (MF set, offset 0) still names its ports.
        let mut f = v4_packet(6, [1, 1, 1, 1], [2, 2, 2, 2], 1, 2);
        f[14 + 6] = 0x20;
        assert!(matches!(parse(&f), Parsed::Transport(_)));
    }

    #[test]
    fn other_frames() {
        assert_eq!(parse(&eth(ETHERTYPE_ARP, &[0u8; 28])), Parsed::Neighbor);
        assert_eq!(parse(&[0u8; 10]), Parsed::Other);
        assert_eq!(parse(&eth(0x8100, &[0u8; 40])), Parsed::Other);
        // ICMP over IPv4.
        assert_eq!(
            parse(&v4_packet(1, [1, 1, 1, 1], [2, 2, 2, 2], 0, 0)),
            Parsed::Other
        );
        // Truncated TCP header.
        let f = v4_packet(6, [1, 1, 1, 1], [2, 2, 2, 2], 1, 2);
        assert_eq!(parse(&f[..14 + 22]), Parsed::Other);
    }

    #[test]
    fn v6_tuple_and_extension_headers() {
        let a = Ipv6Addr::LOCALHOST;
        let mut ports = alloc::vec![];
        ports.extend_from_slice(&40000u16.to_be_bytes());
        ports.extend_from_slice(&443u16.to_be_bytes());
        ports.extend_from_slice(&[0u8; 16]);
        match parse(&v6_packet(6, a, a, &ports)) {
            Parsed::Transport(f) => {
                assert_eq!((f.rport, f.lport), (40000, 443));
                assert_eq!(f.local, IpAddr::V6(a));
            }
            other => panic!("{:?}", other),
        }
        assert_eq!(
            parse(&v6_packet(58, a, a, &[135, 0, 0, 0])),
            Parsed::Neighbor
        );
        // One hop-by-hop header (8 bytes) before UDP.
        let mut hbh = alloc::vec![17u8, 0, 0, 0, 0, 0, 0, 0];
        hbh.extend_from_slice(&ports);
        match parse(&v6_packet(0, a, a, &hbh)) {
            Parsed::Transport(f) => assert_eq!((f.proto, f.lport), (IPPROTO_UDP, 443)),
            other => panic!("{:?}", other),
        }
        // A routing header is not walked.
        let mut rh = alloc::vec![6u8, 0, 0, 0, 0, 0, 0, 0];
        rh.extend_from_slice(&ports);
        assert_eq!(parse(&v6_packet(43, a, a, &rh)), Parsed::Other);
    }

    fn v4(a: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(a))
    }

    #[test]
    fn pick_prefers_exact_then_family() {
        let mut m: PortMap<u32> = PortMap::new();
        let lo = Ipv4Addr::LOCALHOST;
        // A v6-only wildcard, an AF_INET wildcard, then 127.0.0.1.
        m.add(6, 80, 1, Cover::of(AF_INET6, None, true));
        m.add(6, 80, 2, Cover::of(AF_INET, None, false));
        m.add(6, 80, 3, Cover::of(AF_INET, Some(IpAddr::V4(lo)), false));
        assert_eq!(m.pick(6, 80, IpAddr::V4(lo), false), Some(3));
        assert_eq!(m.pick(6, 80, v4([10, 0, 2, 15]), false), Some(2));
        assert_eq!(
            m.pick(6, 80, IpAddr::V6(Ipv6Addr::LOCALHOST), false),
            Some(1)
        );
        assert_eq!(m.pick(17, 80, IpAddr::V4(lo), false), None);
        assert_eq!(m.pick(6, 81, IpAddr::V4(lo), false), None);
        m.remove(6, 80, 2);
        assert_eq!(m.pick(6, 80, v4([10, 0, 2, 15]), false), None);
        m.remove(6, 80, 1);
        m.remove(6, 80, 3);
        assert!(m.is_empty());
    }

    #[test]
    fn pick_dual_stack_and_ties() {
        let mut m: PortMap<u32> = PortMap::new();
        m.add(17, 53, 7, Cover::of(AF_INET6, None, false));
        m.add(17, 53, 8, Cover::of(AF_INET6, None, false));
        // A dual-stack wildcard takes IPv4; the first of equals wins.
        assert_eq!(m.pick(17, 53, v4([127, 0, 0, 1]), false), Some(7));
        let mut m: PortMap<u32> = PortMap::new();
        m.add(
            17,
            68,
            1,
            Cover::of(AF_INET, Some(v4([10, 0, 0, 5])), false),
        );
        // A broadcast reaches a socket bound to another address only as a
        // broadcast.
        assert_eq!(m.pick(17, 68, v4([255, 255, 255, 255]), false), None);
        assert_eq!(m.pick(17, 68, v4([255, 255, 255, 255]), true), Some(1));
        m.add(17, 68, 2, Cover::of(AF_INET, None, false));
        assert_eq!(m.pick(17, 68, v4([255, 255, 255, 255]), true), Some(2));
    }
}
