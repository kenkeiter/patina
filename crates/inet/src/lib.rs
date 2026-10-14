//! Pure, host-testable pieces of the socket layer (design §9):
//!
//! * [`SockAddr`]: a decoded `struct sockaddr` (`AF_INET`, `AF_INET6`,
//!   `AF_UNIX`) with the byte codec the socket syscalls use, plus the
//!   dual-stack helpers (`::ffff:a.b.c.d` ↔ `a.b.c.d`);
//! * [`PortTable`]: the inet bind table — which (protocol, port, address
//!   scope) bindings exist, `EADDRINUSE` decisions under `SO_REUSEADDR` /
//!   `SO_REUSEPORT` and dual-stack overlap, and ephemeral port allocation
//!   (32768–60999, Linux's `ip_local_port_range`).
//!
//! * [`netlink`]: `NETLINK_ROUTE` request parsing and the link/address
//!   dump replies (`net.Interfaces`).
//!
//! The kernel (`kernel/src/net/`) wires these to smoltcp.

#![no_std]

extern crate alloc;

pub mod netlink;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, Ipv6Addr};

use patina_abi::net::{AF_INET, AF_INET6, AF_NETLINK, AF_UNIX, AF_UNSPEC, SockaddrIn, SockaddrIn6};
use patina_abi::netlink::SockaddrNl;

// ---------------------------------------------------------------------------
// Socket addresses.

/// Size of `struct sockaddr_storage`: the largest address accepted.
pub const SOCKADDR_MAX: usize = 128;

/// A decoded socket address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SockAddr {
    V4 {
        addr: Ipv4Addr,
        port: u16,
    },
    V6 {
        addr: Ipv6Addr,
        port: u16,
        flowinfo: u32,
        scope_id: u32,
    },
    /// `AF_UNIX`: the `sun_path` bytes as given (an abstract name starts
    /// with NUL; empty is the unnamed address). Decoded here so a later
    /// `AF_UNIX` implementation shares the syscall layer.
    Unix(Vec<u8>),
    /// `AF_UNSPEC` (`connect` to dissolve a UDP association).
    Unspec,
    /// `AF_NETLINK`: port id and multicast group mask (`sockaddr_nl`).
    Netlink { pid: u32, groups: u32 },
}

/// Why a user `sockaddr` could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrError {
    /// Too short for its family (or longer than `sockaddr_storage`):
    /// `EINVAL`.
    Invalid,
    /// Unknown family: `EAFNOSUPPORT`.
    Family(u16),
}

impl SockAddr {
    /// Decode `bytes` (exactly `addrlen` bytes from user memory).
    pub fn parse(bytes: &[u8]) -> Result<SockAddr, AddrError> {
        if bytes.len() < 2 || bytes.len() > SOCKADDR_MAX {
            return Err(AddrError::Invalid);
        }
        let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
        match family {
            AF_INET => {
                if bytes.len() < size_of::<SockaddrIn>() {
                    return Err(AddrError::Invalid);
                }
                Ok(SockAddr::V4 {
                    addr: Ipv4Addr::new(bytes[4], bytes[5], bytes[6], bytes[7]),
                    port: u16::from_be_bytes([bytes[2], bytes[3]]),
                })
            }
            AF_INET6 => {
                // Linux accepts the 24-byte RFC 2133 form (no scope id).
                if bytes.len() < 24 {
                    return Err(AddrError::Invalid);
                }
                let mut a = [0u8; 16];
                a.copy_from_slice(&bytes[8..24]);
                let scope_id = if bytes.len() >= 28 {
                    u32::from_ne_bytes([bytes[24], bytes[25], bytes[26], bytes[27]])
                } else {
                    0
                };
                Ok(SockAddr::V6 {
                    addr: Ipv6Addr::from(a),
                    port: u16::from_be_bytes([bytes[2], bytes[3]]),
                    flowinfo: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
                    scope_id,
                })
            }
            AF_UNIX => {
                let path = &bytes[2..];
                // A pathname ends at the first NUL; an abstract name (leading
                // NUL) is the whole buffer.
                let path = if path.first() == Some(&0) {
                    path
                } else {
                    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
                    &path[..end]
                };
                Ok(SockAddr::Unix(path.to_vec()))
            }
            AF_UNSPEC => Ok(SockAddr::Unspec),
            AF_NETLINK => {
                if bytes.len() < size_of::<SockaddrNl>() {
                    return Err(AddrError::Invalid);
                }
                let w = |i: usize| u32::from_ne_bytes(bytes[i..i + 4].try_into().unwrap());
                Ok(SockAddr::Netlink {
                    pid: w(4),
                    groups: w(8),
                })
            }
            f => Err(AddrError::Family(f)),
        }
    }

    /// Encode into `out`; returns the full length of the address (what
    /// `addrlen` reports, even when the caller's buffer is shorter).
    pub fn encode(&self, out: &mut [u8; SOCKADDR_MAX]) -> usize {
        match self {
            SockAddr::V4 { addr, port } => {
                let s = SockaddrIn::new(addr.octets(), *port);
                let n = size_of::<SockaddrIn>();
                out[..2].copy_from_slice(&s.sin_family.to_ne_bytes());
                out[2..4].copy_from_slice(&s.sin_port.to_ne_bytes());
                out[4..8].copy_from_slice(&s.sin_addr);
                out[8..16].fill(0);
                n
            }
            SockAddr::V6 {
                addr,
                port,
                flowinfo,
                scope_id,
            } => {
                let s = SockaddrIn6::new(addr.octets(), *port, *flowinfo, *scope_id);
                out[..2].copy_from_slice(&s.sin6_family.to_ne_bytes());
                out[2..4].copy_from_slice(&s.sin6_port.to_ne_bytes());
                out[4..8].copy_from_slice(&s.sin6_flowinfo.to_ne_bytes());
                out[8..24].copy_from_slice(&s.sin6_addr);
                out[24..28].copy_from_slice(&s.sin6_scope_id.to_ne_bytes());
                size_of::<SockaddrIn6>()
            }
            SockAddr::Unix(path) => {
                out[..2].copy_from_slice(&AF_UNIX.to_ne_bytes());
                let n = path.len().min(SOCKADDR_MAX - 2);
                out[2..2 + n].copy_from_slice(&path[..n]);
                // Linux reports a pathname with its terminating NUL.
                if !path.is_empty() && path[0] != 0 && 2 + n < SOCKADDR_MAX {
                    out[2 + n] = 0;
                    return 3 + n;
                }
                2 + n
            }
            SockAddr::Unspec => {
                out[..2].copy_from_slice(&AF_UNSPEC.to_ne_bytes());
                2
            }
            SockAddr::Netlink { pid, groups } => {
                out[..2].copy_from_slice(&AF_NETLINK.to_ne_bytes());
                out[2..4].fill(0);
                out[4..8].copy_from_slice(&pid.to_ne_bytes());
                out[8..12].copy_from_slice(&groups.to_ne_bytes());
                size_of::<SockaddrNl>()
            }
        }
    }

    pub fn family(&self) -> u16 {
        match self {
            SockAddr::V4 { .. } => AF_INET,
            SockAddr::V6 { .. } => AF_INET6,
            SockAddr::Unix(_) => AF_UNIX,
            SockAddr::Unspec => AF_UNSPEC,
            SockAddr::Netlink { .. } => AF_NETLINK,
        }
    }

    pub fn port(&self) -> Option<u16> {
        match self {
            SockAddr::V4 { port, .. } | SockAddr::V6 { port, .. } => Some(*port),
            _ => None,
        }
    }

    /// An IPv6 address of a socket of family `AF_INET6`, from an IP and port
    /// (an IPv4 address is reported IPv4-mapped).
    pub fn inet6(ip: IpAddr, port: u16) -> SockAddr {
        let addr = match ip {
            IpAddr::V4(a) => a.to_ipv6_mapped(),
            IpAddr::V6(a) => a,
        };
        SockAddr::V6 {
            addr,
            port,
            flowinfo: 0,
            scope_id: 0,
        }
    }

    /// The address as seen by a socket of `family` (`AF_INET6` sockets see
    /// IPv4 peers as `::ffff:a.b.c.d`).
    pub fn for_family(family: u16, ip: IpAddr, port: u16) -> SockAddr {
        match (family, ip) {
            (AF_INET6, ip) => SockAddr::inet6(ip, port),
            (_, IpAddr::V4(addr)) => SockAddr::V4 { addr, port },
            // An AF_INET socket never sees an IPv6 address.
            (_, IpAddr::V6(a)) => SockAddr::V4 {
                addr: a.to_ipv4_mapped().unwrap_or(Ipv4Addr::UNSPECIFIED),
                port,
            },
        }
    }

    /// The IP and port of an inet address, an IPv4-mapped IPv6 address
    /// unmapped to IPv4.
    pub fn ip_port(&self) -> Option<(IpAddr, u16)> {
        match self {
            SockAddr::V4 { addr, port } => Some((IpAddr::V4(*addr), *port)),
            SockAddr::V6 { addr, port, .. } => Some((unmap(*addr), *port)),
            _ => None,
        }
    }
}

/// An IP address (`core::net::IpAddr` is the same shape; re-exported so the
/// kernel and tests share one name).
pub use core::net::IpAddr;

/// `::ffff:a.b.c.d` → `a.b.c.d`; other IPv6 addresses unchanged.
pub fn unmap(a: Ipv6Addr) -> IpAddr {
    match a.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(a),
    }
}

// ---------------------------------------------------------------------------
// The bind table.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proto {
    Tcp,
    Udp,
}

/// Which addresses of one family a binding covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<A> {
    None,
    Any,
    One(A),
}

impl<A: PartialEq> Scope<A> {
    fn overlaps(&self, o: &Scope<A>) -> bool {
        match (self, o) {
            (Scope::None, _) | (_, Scope::None) => false,
            (Scope::Any, _) | (_, Scope::Any) => true,
            (Scope::One(a), Scope::One(b)) => a == b,
        }
    }
}

/// The local addresses a bound socket receives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cover {
    pub v4: Scope<Ipv4Addr>,
    pub v6: Scope<Ipv6Addr>,
}

impl Cover {
    /// The cover of a socket of `family` bound to `ip` (`None`: the
    /// wildcard) with `IPV6_V6ONLY` = `v6only` (ignored for `AF_INET`).
    pub fn of(family: u16, ip: Option<IpAddr>, v6only: bool) -> Cover {
        let none = Cover {
            v4: Scope::None,
            v6: Scope::None,
        };
        match (family, ip) {
            (AF_INET6, None) => Cover {
                v4: if v6only { Scope::None } else { Scope::Any },
                v6: Scope::Any,
            },
            (_, None) => Cover {
                v4: Scope::Any,
                ..none
            },
            (_, Some(IpAddr::V4(a))) => Cover {
                v4: Scope::One(a),
                ..none
            },
            (_, Some(IpAddr::V6(a))) => Cover {
                v6: Scope::One(a),
                ..none
            },
        }
    }

    fn overlaps(&self, o: &Cover) -> bool {
        self.v4.overlaps(&o.v4) || self.v6.overlaps(&o.v6)
    }
}

/// `SO_REUSEADDR` / `SO_REUSEPORT` of a binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reuse {
    pub addr: bool,
    pub port: bool,
}

#[derive(Clone, Copy, Debug)]
struct Binding {
    id: u64,
    cover: Cover,
    reuse: Reuse,
    listening: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindError {
    /// `EADDRINUSE`.
    InUse,
    /// No free ephemeral port (`EADDRINUSE` from bind, `EADDRNOTAVAIL`
    /// from a connect autobind).
    NoPorts,
}

pub const EPHEMERAL_LOW: u16 = 32768;
pub const EPHEMERAL_HIGH: u16 = 60999;

type Key = (u8, u16);

fn key(p: Proto, port: u16) -> Key {
    (pidx(p) as u8, port)
}

fn pidx(p: Proto) -> usize {
    match p {
        Proto::Tcp => 0,
        Proto::Udp => 1,
    }
}

/// The bindings, by (protocol, port) and by owner id (one per socket).
#[derive(Debug)]
pub struct PortTable {
    ports: BTreeMap<Key, Vec<Binding>>,
    owners: BTreeMap<u64, Key>,
    cursor: [u16; 2],
}

impl PortTable {
    /// `seed` picks where ephemeral allocation starts.
    pub fn new(seed: u64) -> PortTable {
        let span = (EPHEMERAL_HIGH - EPHEMERAL_LOW) as u64 + 1;
        PortTable {
            ports: BTreeMap::new(),
            owners: BTreeMap::new(),
            cursor: [
                EPHEMERAL_LOW + (seed % span) as u16,
                EPHEMERAL_LOW + ((seed >> 20) % span) as u16,
            ],
        }
    }

    fn conflicts(&self, k: Key, cover: &Cover, reuse: Reuse) -> bool {
        self.ports.get(&k).is_some_and(|v| {
            v.iter().any(|e| {
                e.cover.overlaps(cover)
                    && !(reuse.port && e.reuse.port)
                    && !(reuse.addr && e.reuse.addr && !e.listening)
            })
        })
    }

    /// Bind owner `id` (unbinding it first if it was bound). `port` 0
    /// picks an unused ephemeral port. Returns the port.
    pub fn bind(
        &mut self,
        id: u64,
        proto: Proto,
        port: u16,
        cover: Cover,
        reuse: Reuse,
    ) -> Result<u16, BindError> {
        let port = if port == 0 {
            self.ephemeral(proto)?
        } else {
            if self.conflicts(key(proto, port), &cover, reuse) {
                return Err(BindError::InUse);
            }
            port
        };
        self.unbind(id);
        let k = key(proto, port);
        self.ports.entry(k).or_default().push(Binding {
            id,
            cover,
            reuse,
            listening: false,
        });
        self.owners.insert(id, k);
        Ok(port)
    }

    fn ephemeral(&mut self, proto: Proto) -> Result<u16, BindError> {
        let span = EPHEMERAL_HIGH - EPHEMERAL_LOW + 1;
        let mut p = self.cursor[pidx(proto)];
        for _ in 0..span {
            let cand = p;
            p = if p >= EPHEMERAL_HIGH {
                EPHEMERAL_LOW
            } else {
                p + 1
            };
            if !self.ports.contains_key(&key(proto, cand)) {
                self.cursor[pidx(proto)] = p;
                return Ok(cand);
            }
        }
        Err(BindError::NoPorts)
    }

    fn entry(&mut self, id: u64) -> Option<&mut Binding> {
        let k = self.owners.get(&id)?;
        self.ports.get_mut(k)?.iter_mut().find(|e| e.id == id)
    }

    /// Mark `id` as listening (a listener blocks `SO_REUSEADDR` rebinding).
    pub fn set_listening(&mut self, id: u64, on: bool) {
        if let Some(e) = self.entry(id) {
            e.listening = on;
        }
    }

    /// Update the reuse flags of `id` (`setsockopt` after `bind`).
    pub fn set_reuse(&mut self, id: u64, reuse: Reuse) {
        if let Some(e) = self.entry(id) {
            e.reuse = reuse;
        }
    }

    /// Drop the binding of `id`, if any.
    pub fn unbind(&mut self, id: u64) {
        if let Some(k) = self.owners.remove(&id)
            && let Some(v) = self.ports.get_mut(&k)
        {
            v.retain(|e| e.id != id);
            if v.is_empty() {
                self.ports.remove(&k);
            }
        }
    }

    /// Whether any binding uses `port` for `proto`.
    pub fn in_use(&self, proto: Proto, port: u16) -> bool {
        self.ports.contains_key(&key(proto, port))
    }

    /// Number of bound owners.
    pub fn len(&self) -> usize {
        self.owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: [u8; 4]) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::from(a)))
    }

    #[test]
    fn codec_roundtrip() {
        let mut buf = [0u8; SOCKADDR_MAX];
        let a = SockAddr::V4 {
            addr: Ipv4Addr::new(127, 0, 0, 1),
            port: 8080,
        };
        let n = a.encode(&mut buf);
        assert_eq!(n, 16);
        assert_eq!(&buf[2..8], &[0x1f, 0x90, 127, 0, 0, 1]);
        assert_eq!(SockAddr::parse(&buf[..n]).unwrap(), a);
        assert_eq!(SockAddr::parse(&buf[..15]), Err(AddrError::Invalid));

        let b = SockAddr::inet6(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 443);
        let n = b.encode(&mut buf);
        assert_eq!(n, 28);
        assert_eq!(&buf[8..24], &Ipv4Addr::new(10, 0, 0, 2).to_ipv6_mapped().octets());
        assert_eq!(SockAddr::parse(&buf[..n]).unwrap(), b);
        // RFC 2133 length (no scope id).
        assert!(SockAddr::parse(&buf[..24]).is_ok());
        assert_eq!(
            b.ip_port(),
            Some((IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), 443))
        );

        let nl = SockAddr::Netlink { pid: 1, groups: 0x10 };
        let n = nl.encode(&mut buf);
        assert_eq!(n, 12);
        assert_eq!(&buf[..2], &AF_NETLINK.to_ne_bytes());
        assert_eq!(SockAddr::parse(&buf[..n]).unwrap(), nl);
        assert_eq!(SockAddr::parse(&buf[..8]), Err(AddrError::Invalid));

        buf[0..2].copy_from_slice(&99u16.to_ne_bytes());
        assert_eq!(SockAddr::parse(&buf[..16]), Err(AddrError::Family(99)));

        let mut u = [0u8; 12];
        u[..2].copy_from_slice(&AF_UNIX.to_ne_bytes());
        u[2..6].copy_from_slice(b"/sok");
        assert_eq!(SockAddr::parse(&u).unwrap(), SockAddr::Unix(b"/sok".to_vec()));
        assert_eq!(SockAddr::Unix(b"/sok".to_vec()).encode(&mut buf), 7);
    }

    #[test]
    fn family_view() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert_eq!(
            SockAddr::for_family(AF_INET6, ip, 1),
            SockAddr::V6 {
                addr: Ipv4Addr::LOCALHOST.to_ipv6_mapped(),
                port: 1,
                flowinfo: 0,
                scope_id: 0
            }
        );
        assert_eq!(
            SockAddr::for_family(AF_INET, ip, 1),
            SockAddr::V4 {
                addr: Ipv4Addr::LOCALHOST,
                port: 1
            }
        );
    }

    #[test]
    fn bind_conflicts() {
        let mut t = PortTable::new(0);
        let any6 = Cover::of(AF_INET6, None, false);
        let any6only = Cover::of(AF_INET6, None, true);
        let any4 = Cover::of(AF_INET, None, false);
        let lo4 = Cover::of(AF_INET, v4([127, 0, 0, 1]), false);
        let r = Reuse::default();
        assert_eq!(t.bind(1, Proto::Tcp, 80, any6, r), Ok(80));
        // Dual-stack [::] covers 0.0.0.0 and 127.0.0.1.
        assert_eq!(t.bind(2, Proto::Tcp, 80, any4, r), Err(BindError::InUse));
        assert_eq!(t.bind(2, Proto::Tcp, 80, lo4, r), Err(BindError::InUse));
        // Other protocol: fine.
        assert_eq!(t.bind(2, Proto::Udp, 80, any4, r), Ok(80));
        t.unbind(1);
        assert_eq!(t.bind(3, Proto::Tcp, 80, any6only, r), Ok(80));
        // A v6-only wildcard leaves IPv4 free.
        assert_eq!(t.bind(4, Proto::Tcp, 80, any4, r), Ok(80));
        // Distinct specific addresses do not overlap.
        assert_eq!(
            t.bind(5, Proto::Tcp, 81, lo4, r),
            Ok(81)
        );
        assert_eq!(
            t.bind(6, Proto::Tcp, 81, Cover::of(AF_INET, v4([10, 0, 0, 2]), false), r),
            Ok(81)
        );
    }

    #[test]
    fn reuse() {
        let mut t = PortTable::new(0);
        let any6 = Cover::of(AF_INET6, None, false);
        let ra = Reuse {
            addr: true,
            port: false,
        };
        assert_eq!(t.bind(1, Proto::Tcp, 8080, any6, ra), Ok(8080));
        t.set_listening(1, true);
        // SO_REUSEADDR does not let a second socket bind over a listener.
        assert_eq!(t.bind(2, Proto::Tcp, 8080, any6, ra), Err(BindError::InUse));
        t.set_listening(1, false);
        assert_eq!(t.bind(2, Proto::Tcp, 8080, any6, ra), Ok(8080));
        // Without SO_REUSEADDR on the newcomer: in use.
        assert_eq!(
            t.bind(3, Proto::Tcp, 8080, any6, Reuse::default()),
            Err(BindError::InUse)
        );
        let rp = Reuse {
            addr: false,
            port: true,
        };
        assert_eq!(t.bind(10, Proto::Udp, 53, any6, rp), Ok(53));
        assert_eq!(t.bind(11, Proto::Udp, 53, any6, rp), Ok(53));
    }

    #[test]
    fn ephemeral() {
        let mut t = PortTable::new(u64::MAX);
        let any4 = Cover::of(AF_INET, None, false);
        let mut seen = alloc::collections::BTreeSet::new();
        for id in 0..100 {
            let p = t.bind(id, Proto::Tcp, 0, any4, Reuse::default()).unwrap();
            assert!((EPHEMERAL_LOW..=EPHEMERAL_HIGH).contains(&p));
            assert!(seen.insert(p));
        }
        // The whole range can be exhausted.
        let mut t = PortTable::new(0);
        let span = (EPHEMERAL_HIGH - EPHEMERAL_LOW) as u64 + 1;
        for id in 0..span {
            t.bind(id, Proto::Udp, 0, any4, Reuse::default()).unwrap();
        }
        assert_eq!(
            t.bind(span, Proto::Udp, 0, any4, Reuse::default()),
            Err(BindError::NoPorts)
        );
        assert!(t.bind(span, Proto::Tcp, 0, any4, Reuse::default()).is_ok());
    }
}
