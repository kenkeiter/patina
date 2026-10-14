//! `AF_NETLINK` / `NETLINK_ROUTE` ABI (same on x86_64 and aarch64): the
//! message header, `rtnetlink` link and address messages and their
//! attributes, as far as `net.Interfaces` / `net.InterfaceAddrs`
//! (`syscall.NetlinkRIB`) need them. Values from `linux/netlink.h`,
//! `linux/rtnetlink.h`, `linux/if_link.h`, `linux/if_addr.h`, `linux/if.h`
//! and `linux/if_arp.h`. All fields are host (little-endian) byte order.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const NETLINK_ROUTE: i32 = 0;
pub const SOL_NETLINK: i32 = 270;

/// `struct nlmsghdr` size; messages and attributes are 4-byte aligned.
pub const NLMSG_HDRLEN: usize = 16;
pub const NLMSG_ALIGNTO: usize = 4;
pub const RTA_ALIGNTO: usize = 4;

pub const NLMSG_NOOP: u16 = 1;
pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;

pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_MULTI: u16 = 0x2;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_ROOT: u16 = 0x100;
pub const NLM_F_MATCH: u16 = 0x200;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_GETADDR: u16 = 22;
pub const RTM_NEWROUTE: u16 = 24;
pub const RTM_GETROUTE: u16 = 26;

pub const IFLA_ADDRESS: u16 = 1;
pub const IFLA_BROADCAST: u16 = 2;
pub const IFLA_IFNAME: u16 = 3;
pub const IFLA_MTU: u16 = 4;
pub const IFLA_TXQLEN: u16 = 13;
pub const IFLA_OPERSTATE: u16 = 16;

pub const IFA_ADDRESS: u16 = 1;
pub const IFA_LOCAL: u16 = 2;
pub const IFA_LABEL: u16 = 3;
pub const IFA_BROADCAST: u16 = 4;

pub const IFF_UP: u32 = 0x1;
pub const IFF_BROADCAST: u32 = 0x2;
pub const IFF_LOOPBACK: u32 = 0x8;
pub const IFF_RUNNING: u32 = 0x40;
pub const IFF_MULTICAST: u32 = 0x1000;
pub const IFF_LOWER_UP: u32 = 0x10000;

pub const ARPHRD_ETHER: u16 = 1;
pub const ARPHRD_LOOPBACK: u16 = 772;

pub const IF_OPER_UNKNOWN: u8 = 0;
pub const IF_OPER_UP: u8 = 6;

pub const IFA_F_PERMANENT: u8 = 0x80;

pub const RT_SCOPE_UNIVERSE: u8 = 0;
pub const RT_SCOPE_LINK: u8 = 253;
pub const RT_SCOPE_HOST: u8 = 254;

/// `struct nlmsghdr` (16 bytes).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct NlMsghdr {
    pub nlmsg_len: u32,
    pub nlmsg_type: u16,
    pub nlmsg_flags: u16,
    pub nlmsg_seq: u32,
    pub nlmsg_pid: u32,
}

/// `struct sockaddr_nl` (12 bytes).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct SockaddrNl {
    pub nl_family: u16,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

/// `struct ifinfomsg` (16 bytes).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct IfInfomsg {
    pub ifi_family: u8,
    pub ifi_pad: u8,
    pub ifi_type: u16,
    pub ifi_index: i32,
    pub ifi_flags: u32,
    pub ifi_change: u32,
}

/// `struct ifaddrmsg` (8 bytes).
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct IfAddrmsg {
    pub ifa_family: u8,
    pub ifa_prefixlen: u8,
    pub ifa_flags: u8,
    pub ifa_scope: u8,
    pub ifa_index: u32,
}

/// `struct rtattr` (4 bytes), followed by the value.
#[repr(C)]
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout,
)]
pub struct RtAttr {
    pub rta_len: u16,
    pub rta_type: u16,
}

/// `NLMSG_ALIGN`.
pub const fn nlmsg_align(len: usize) -> usize {
    (len + NLMSG_ALIGNTO - 1) & !(NLMSG_ALIGNTO - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size_of::<NlMsghdr>(), NLMSG_HDRLEN);
        assert_eq!(size_of::<SockaddrNl>(), 12);
        assert_eq!(size_of::<IfInfomsg>(), 16);
        assert_eq!(size_of::<IfAddrmsg>(), 8);
        assert_eq!(size_of::<RtAttr>(), 4);
        assert_eq!(nlmsg_align(17), 20);
    }
}
