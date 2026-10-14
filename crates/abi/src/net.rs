//! Socket ABI (same on x86_64 and aarch64): address families, socket types,
//! options, `sockaddr_*`, `msghdr`, `cmsghdr`.
//!
//! Ports and IPv4/IPv6 addresses inside `sockaddr_in*` are in network byte
//! order, exactly as in C; use the helper constructors/accessors.

use bitflags::bitflags;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const AF_UNSPEC: u16 = 0;
pub const AF_UNIX: u16 = 1;
pub const AF_LOCAL: u16 = AF_UNIX;
pub const AF_INET: u16 = 2;
pub const AF_INET6: u16 = 10;
pub const AF_NETLINK: u16 = 16;
pub const AF_PACKET: u16 = 17;
pub const AF_VSOCK: u16 = 40;

pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_RAW: u32 = 3;
pub const SOCK_RDM: u32 = 4;
pub const SOCK_SEQPACKET: u32 = 5;
/// Mask of the type bits in the `type` argument of `socket`.
pub const SOCK_TYPE_MASK: u32 = 0xf;
pub const SOCK_NONBLOCK: u32 = crate::fs::O_NONBLOCK;
pub const SOCK_CLOEXEC: u32 = crate::fs::O_CLOEXEC;

pub const SOL_SOCKET: i32 = 1;
pub const SOL_IP: i32 = 0;
pub const SOL_TCP: i32 = 6;
pub const SOL_UDP: i32 = 17;
pub const SOL_IPV6: i32 = 41;

pub const IPPROTO_IP: i32 = 0;
pub const IPPROTO_ICMP: i32 = 1;
pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;
pub const IPPROTO_IPV6: i32 = 41;
pub const IPPROTO_ICMPV6: i32 = 58;
pub const IPPROTO_RAW: i32 = 255;

pub const SO_DEBUG: i32 = 1;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_DONTROUTE: i32 = 5;
pub const SO_BROADCAST: i32 = 6;
pub const SO_SNDBUF: i32 = 7;
pub const SO_RCVBUF: i32 = 8;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_OOBINLINE: i32 = 10;
pub const SO_NO_CHECK: i32 = 11;
pub const SO_PRIORITY: i32 = 12;
pub const SO_LINGER: i32 = 13;
pub const SO_BSDCOMPAT: i32 = 14;
pub const SO_REUSEPORT: i32 = 15;
pub const SO_PASSCRED: i32 = 16;
pub const SO_PEERCRED: i32 = 17;
pub const SO_RCVLOWAT: i32 = 18;
pub const SO_SNDLOWAT: i32 = 19;
pub const SO_RCVTIMEO_OLD: i32 = 20;
pub const SO_SNDTIMEO_OLD: i32 = 21;
pub const SO_BINDTODEVICE: i32 = 25;
pub const SO_TIMESTAMP_OLD: i32 = 29;
pub const SO_ACCEPTCONN: i32 = 30;
pub const SO_SNDBUFFORCE: i32 = 32;
pub const SO_RCVBUFFORCE: i32 = 33;
pub const SO_PROTOCOL: i32 = 38;
pub const SO_DOMAIN: i32 = 39;
pub const SO_RCVTIMEO_NEW: i32 = 66;
pub const SO_SNDTIMEO_NEW: i32 = 67;

pub const TCP_NODELAY: i32 = 1;
pub const TCP_MAXSEG: i32 = 2;
pub const TCP_CORK: i32 = 3;
pub const TCP_KEEPIDLE: i32 = 4;
pub const TCP_KEEPINTVL: i32 = 5;
pub const TCP_KEEPCNT: i32 = 6;
pub const TCP_INFO: i32 = 11;
pub const TCP_QUICKACK: i32 = 12;
pub const TCP_CONGESTION: i32 = 13;
pub const TCP_USER_TIMEOUT: i32 = 18;
pub const TCP_FASTOPEN: i32 = 23;

pub const IP_TOS: i32 = 1;
pub const IP_TTL: i32 = 2;
pub const IP_HDRINCL: i32 = 3;
pub const IP_RECVERR: i32 = 11;
pub const IP_MULTICAST_IF: i32 = 32;
pub const IP_MULTICAST_TTL: i32 = 33;
pub const IP_MULTICAST_LOOP: i32 = 34;
pub const IP_ADD_MEMBERSHIP: i32 = 35;
pub const IP_DROP_MEMBERSHIP: i32 = 36;

pub const IPV6_UNICAST_HOPS: i32 = 16;
pub const IPV6_MULTICAST_IF: i32 = 17;
pub const IPV6_MULTICAST_HOPS: i32 = 18;
pub const IPV6_MULTICAST_LOOP: i32 = 19;
pub const IPV6_ADD_MEMBERSHIP: i32 = 20;
pub const IPV6_DROP_MEMBERSHIP: i32 = 21;
pub const IPV6_V6ONLY: i32 = 26;
pub const IPV6_TCLASS: i32 = 67;

bitflags! {
    /// `send*`/`recv*` flags.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct MsgFlags: u32 {
        const OOB = 0x1;
        const PEEK = 0x2;
        const DONTROUTE = 0x4;
        const CTRUNC = 0x8;
        /// `MSG_PROBE` (formerly `MSG_PROXY`).
        const PROBE = 0x10;
        const TRUNC = 0x20;
        const DONTWAIT = 0x40;
        const EOR = 0x80;
        const WAITALL = 0x100;
        const FIN = 0x200;
        const SYN = 0x400;
        const CONFIRM = 0x800;
        const RST = 0x1000;
        const ERRQUEUE = 0x2000;
        const NOSIGNAL = 0x4000;
        const MORE = 0x8000;
        const WAITFORONE = 0x1_0000;
        const FASTOPEN = 0x2000_0000;
        const CMSG_CLOEXEC = 0x4000_0000;
    }
}

pub const SHUT_RD: i32 = 0;
pub const SHUT_WR: i32 = 1;
pub const SHUT_RDWR: i32 = 2;

pub const SCM_RIGHTS: i32 = 1;
pub const SCM_CREDENTIALS: i32 = 2;

/// Max `listen` backlog the kernel clamps to (`/proc/sys/net/core/somaxconn`).
pub const SOMAXCONN: i32 = 4096;

/// `struct sockaddr_in` (16 bytes, 4-byte aligned: `sin_addr` is a `__be32`
/// in C).
#[repr(C, align(4))]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SockaddrIn {
    pub sin_family: u16,
    /// Network byte order.
    pub sin_port: u16,
    /// Network byte order (the four octets in order).
    pub sin_addr: [u8; 4],
    pub sin_zero: [u8; 8],
}

impl SockaddrIn {
    pub const fn new(addr: [u8; 4], port: u16) -> Self {
        SockaddrIn {
            sin_family: AF_INET,
            sin_port: port.to_be(),
            sin_addr: addr,
            sin_zero: [0; 8],
        }
    }
    pub const fn port(&self) -> u16 {
        u16::from_be(self.sin_port)
    }
}

/// `struct sockaddr_in6` (28 bytes).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SockaddrIn6 {
    pub sin6_family: u16,
    /// Network byte order.
    pub sin6_port: u16,
    /// Network byte order.
    pub sin6_flowinfo: u32,
    pub sin6_addr: [u8; 16],
    pub sin6_scope_id: u32,
}

impl SockaddrIn6 {
    pub const fn new(addr: [u8; 16], port: u16, flowinfo: u32, scope_id: u32) -> Self {
        SockaddrIn6 {
            sin6_family: AF_INET6,
            sin6_port: port.to_be(),
            sin6_flowinfo: flowinfo.to_be(),
            sin6_addr: addr,
            sin6_scope_id: scope_id,
        }
    }
    pub const fn port(&self) -> u16 {
        u16::from_be(self.sin6_port)
    }
}

/// Max `sun_path` length.
pub const UNIX_PATH_MAX: usize = 108;

/// `struct sockaddr_un` (110 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct SockaddrUn {
    pub sun_family: u16,
    pub sun_path: [u8; UNIX_PATH_MAX],
}

impl Default for SockaddrUn {
    fn default() -> Self {
        SockaddrUn {
            sun_family: AF_UNIX,
            sun_path: [0; UNIX_PATH_MAX],
        }
    }
}

/// Offset of `sun_path` in `sockaddr_un` (the minimum `addrlen`).
pub const SUN_PATH_OFFSET: usize = 2;

/// `struct sockaddr_vm` (16 bytes) for `AF_VSOCK`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SockaddrVm {
    pub svm_family: u16,
    pub svm_reserved1: u16,
    pub svm_port: u32,
    pub svm_cid: u32,
    pub svm_flags: u8,
    pub svm_zero: [u8; 3],
}

pub const VMADDR_CID_ANY: u32 = u32::MAX;
pub const VMADDR_CID_HYPERVISOR: u32 = 0;
pub const VMADDR_CID_LOCAL: u32 = 1;
pub const VMADDR_CID_HOST: u32 = 2;
pub const VMADDR_PORT_ANY: u32 = u32::MAX;

/// AF_VSOCK socket options (level `AF_VSOCK`, `linux/vm_sockets.h`).
pub const SO_VM_SOCKETS_BUFFER_SIZE: i32 = 0;
pub const SO_VM_SOCKETS_BUFFER_MIN_SIZE: i32 = 1;
pub const SO_VM_SOCKETS_BUFFER_MAX_SIZE: i32 = 2;
pub const SO_VM_SOCKETS_PEER_HOST_VM_ID: i32 = 3;
pub const SO_VM_SOCKETS_TRUSTED: i32 = 5;
/// `struct __kernel_old_timeval`; the 64-bit `SO_VM_SOCKETS_CONNECT_TIMEOUT`.
pub const SO_VM_SOCKETS_CONNECT_TIMEOUT_OLD: i32 = 6;
pub const SO_VM_SOCKETS_NONBLOCK_TXRX: i32 = 7;
pub const SO_VM_SOCKETS_CONNECT_TIMEOUT_NEW: i32 = 8;
/// `IOCTL_VM_SOCKETS_GET_LOCAL_CID` = `_IO(7, 0xb9)` on `/dev/vsock`.
pub const IOCTL_VM_SOCKETS_GET_LOCAL_CID: u32 = 0x7b9;

/// `struct sockaddr_storage` size.
pub const SOCKADDR_STORAGE_SIZE: usize = 128;

/// `struct msghdr` (user pointers as `u64`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Msghdr {
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub _pad0: u32,
    pub msg_iov: u64,
    pub msg_iovlen: u64,
    pub msg_control: u64,
    pub msg_controllen: u64,
    pub msg_flags: i32,
    pub _pad1: u32,
}

/// `struct cmsghdr` header (16 bytes); data follows at `CMSG_DATA`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Cmsghdr {
    pub cmsg_len: u64,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

/// `CMSG_ALIGN`: round up to `sizeof(long)`.
pub const fn cmsg_align(len: usize) -> usize {
    (len + 7) & !7
}
/// `CMSG_LEN(n)`.
pub const fn cmsg_len(n: usize) -> usize {
    cmsg_align(core::mem::size_of::<Cmsghdr>()) + n
}
/// `CMSG_SPACE(n)`.
pub const fn cmsg_space(n: usize) -> usize {
    cmsg_align(core::mem::size_of::<Cmsghdr>()) + cmsg_align(n)
}

/// `struct linger`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Linger {
    pub l_onoff: i32,
    pub l_linger: i32,
}

/// `struct ucred` (`SO_PEERCRED`, `SCM_CREDENTIALS`).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// `struct ip_mreq` (two `struct in_addr`, i.e. `__be32`: 4-byte aligned).
#[repr(C, align(4))]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct IpMreq {
    pub imr_multiaddr: [u8; 4],
    pub imr_interface: [u8; 4],
}

/// `struct ipv6_mreq`.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct Ipv6Mreq {
    pub ipv6mr_multiaddr: [u8; 16],
    pub ipv6mr_interface: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sockaddr_bytes() {
        let a = SockaddrIn::new([127, 0, 0, 1], 8080);
        let b = a.as_bytes();
        assert_eq!(b[..8], [2, 0, 0x1f, 0x90, 127, 0, 0, 1]);
        assert_eq!(a.port(), 8080);
        let a6 = SockaddrIn6::new([0; 16], 443, 0, 3);
        assert_eq!(&a6.as_bytes()[..4], &[10, 0, 0x01, 0xbb]);
        assert_eq!(a6.port(), 443);
        assert_eq!(&a6.as_bytes()[24..28], &3u32.to_le_bytes());
    }

    #[test]
    fn cmsg() {
        assert_eq!(cmsg_len(4), 20);
        assert_eq!(cmsg_space(4), 24);
        assert_eq!(cmsg_space(12), 32);
    }
}
