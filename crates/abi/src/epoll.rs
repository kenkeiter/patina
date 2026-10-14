//! epoll / eventfd / getrandom constants. `struct epoll_event` differs per
//! arch (packed, 12 bytes on x86_64; 16 bytes on aarch64) and lives in the
//! arch modules.

use bitflags::bitflags;

pub const EPOLL_CTL_ADD: i32 = 1;
pub const EPOLL_CTL_DEL: i32 = 2;
pub const EPOLL_CTL_MOD: i32 = 3;
/// `epoll_create1` flag (= `O_CLOEXEC`).
pub const EPOLL_CLOEXEC: u32 = crate::fs::O_CLOEXEC;

bitflags! {
    /// `epoll_event.events`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct EpollFlags: u32 {
        const IN = 0x0001;
        const PRI = 0x0002;
        const OUT = 0x0004;
        const ERR = 0x0008;
        const HUP = 0x0010;
        const NVAL = 0x0020;
        const RDNORM = 0x0040;
        const RDBAND = 0x0080;
        const WRNORM = 0x0100;
        const WRBAND = 0x0200;
        const MSG = 0x0400;
        const RDHUP = 0x2000;
        const EXCLUSIVE = 1 << 28;
        const WAKEUP = 1 << 29;
        const ONESHOT = 1 << 30;
        const ET = 1 << 31;
    }
}

/// Flags Go's netpoller registers (`EPOLLIN|EPOLLOUT|EPOLLRDHUP|EPOLLET`).
pub const GO_NETPOLL_EVENTS: EpollFlags = EpollFlags::IN
    .union(EpollFlags::OUT)
    .union(EpollFlags::RDHUP)
    .union(EpollFlags::ET);

/// `eventfd2` flags.
pub const EFD_SEMAPHORE: u32 = 1;
pub const EFD_CLOEXEC: u32 = crate::fs::O_CLOEXEC;
pub const EFD_NONBLOCK: u32 = crate::fs::O_NONBLOCK;

/// `getrandom` flags.
pub const GRND_NONBLOCK: u32 = 1;
pub const GRND_RANDOM: u32 = 2;
pub const GRND_INSECURE: u32 = 4;

/// Generate a per-arch `EpollEvent` accessor impl (the struct itself is
/// defined per arch because of packing).
#[macro_export]
#[doc(hidden)]
macro_rules! epoll_event_impl {
    ($t:ident) => {
        impl $t {
            #[allow(clippy::needless_update)]
            pub const fn new(events: u32, data: u64) -> Self {
                $t {
                    events,
                    data,
                    ..Self::ZERO
                }
            }
            pub const fn events(&self) -> u32 {
                self.events
            }
            pub const fn data(&self) -> u64 {
                self.data
            }
            pub fn flags(&self) -> $crate::epoll::EpollFlags {
                $crate::epoll::EpollFlags::from_bits_retain(self.events)
            }
        }
    };
}
