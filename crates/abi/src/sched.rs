//! Threads and synchronisation: `clone` flags, futex ops, `wait4` options.
//!
//! The *order* of `clone` arguments differs per arch; see
//! `x86_64::clone_args` / `aarch64::clone_args`.

use bitflags::bitflags;

bitflags! {
    /// `clone` flags (low byte is the exit signal, see [`CSIGNAL`]).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct CloneFlags: u64 {
        const VM = 0x0000_0100;
        const FS = 0x0000_0200;
        const FILES = 0x0000_0400;
        const SIGHAND = 0x0000_0800;
        const PIDFD = 0x0000_1000;
        const PTRACE = 0x0000_2000;
        const VFORK = 0x0000_4000;
        const PARENT = 0x0000_8000;
        const THREAD = 0x0001_0000;
        const NEWNS = 0x0002_0000;
        const SYSVSEM = 0x0004_0000;
        const SETTLS = 0x0008_0000;
        const PARENT_SETTID = 0x0010_0000;
        const CHILD_CLEARTID = 0x0020_0000;
        const DETACHED = 0x0040_0000;
        const UNTRACED = 0x0080_0000;
        const CHILD_SETTID = 0x0100_0000;
        const NEWCGROUP = 0x0200_0000;
        const NEWUTS = 0x0400_0000;
        const NEWIPC = 0x0800_0000;
        const NEWUSER = 0x1000_0000;
        const NEWPID = 0x2000_0000;
        const NEWNET = 0x4000_0000;
        const IO = 0x8000_0000;
    }
}

/// Exit-signal mask in `clone` flags.
pub const CSIGNAL: u64 = 0xff;

/// Flags the Go runtime passes in `newosproc` (`runtime/os_linux.go`
/// `cloneFlags`). TLS is set by the child itself, so `SETTLS` is absent.
pub const GO_CLONE_FLAGS: CloneFlags = CloneFlags::VM
    .union(CloneFlags::FS)
    .union(CloneFlags::FILES)
    .union(CloneFlags::SIGHAND)
    .union(CloneFlags::SYSVSEM)
    .union(CloneFlags::THREAD);

/// Flags patina accepts on top of [`GO_CLONE_FLAGS`] for thread creation.
pub const CLONE_THREAD_OPTIONAL: CloneFlags = CloneFlags::SETTLS
    .union(CloneFlags::PARENT_SETTID)
    .union(CloneFlags::CHILD_SETTID)
    .union(CloneFlags::CHILD_CLEARTID)
    .union(CloneFlags::DETACHED);

/// `clone` arguments in a fixed order, after undoing the per-arch ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloneArgs {
    pub flags: u64,
    pub stack: u64,
    pub parent_tid: u64,
    pub child_tid: u64,
    pub tls: u64,
}

impl CloneArgs {
    pub fn clone_flags(&self) -> CloneFlags {
        CloneFlags::from_bits_retain(self.flags & !CSIGNAL)
    }
    pub fn exit_signal(&self) -> i32 {
        (self.flags & CSIGNAL) as i32
    }
}

pub const FUTEX_WAIT: i32 = 0;
pub const FUTEX_WAKE: i32 = 1;
pub const FUTEX_FD: i32 = 2;
pub const FUTEX_REQUEUE: i32 = 3;
pub const FUTEX_CMP_REQUEUE: i32 = 4;
pub const FUTEX_WAKE_OP: i32 = 5;
pub const FUTEX_LOCK_PI: i32 = 6;
pub const FUTEX_UNLOCK_PI: i32 = 7;
pub const FUTEX_TRYLOCK_PI: i32 = 8;
pub const FUTEX_WAIT_BITSET: i32 = 9;
pub const FUTEX_WAKE_BITSET: i32 = 10;
pub const FUTEX_WAIT_REQUEUE_PI: i32 = 11;
pub const FUTEX_CMP_REQUEUE_PI: i32 = 12;
pub const FUTEX_LOCK_PI2: i32 = 13;
pub const FUTEX_PRIVATE_FLAG: i32 = 128;
pub const FUTEX_CLOCK_REALTIME: i32 = 256;
/// Linux 7.2 robust-futex op flags. patina does not implement them; they
/// are deliberately NOT stripped by [`FUTEX_CMD_MASK`], so an op carrying
/// them decodes to an unknown command (`ENOSYS`, as on Linux < 7.2) instead
/// of silently running the base op.
pub const FUTEX_ROBUST_UNLOCK: i32 = 512;
pub const FUTEX_ROBUST_LIST32: i32 = 1024;
/// Op-number mask as on Linux < 7.2 (see [`FUTEX_ROBUST_UNLOCK`]); Linux
/// 7.2 also clears the two robust flags.
pub const FUTEX_CMD_MASK: i32 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
pub const FUTEX_WAIT_PRIVATE: i32 = FUTEX_WAIT | FUTEX_PRIVATE_FLAG;
pub const FUTEX_WAKE_PRIVATE: i32 = FUTEX_WAKE | FUTEX_PRIVATE_FLAG;
pub const FUTEX_BITSET_MATCH_ANY: u32 = 0xffff_ffff;

/// `wait4` options.
pub const WNOHANG: i32 = 1;
pub const WUNTRACED: i32 = 2;
pub const WEXITED: i32 = 4;
pub const WCONTINUED: i32 = 8;
pub const WNOWAIT: i32 = 0x0100_0000;
pub const WALL: i32 = 0x4000_0000;
pub const WCLONE: i32 = 0x8000_0000u32 as i32;

/// Encode a normal-exit wait status.
pub const fn w_exitcode(code: i32) -> i32 {
    (code & 0xff) << 8
}

/// Encode a killed-by-signal wait status.
pub const fn w_termsig(sig: i32) -> i32 {
    sig & 0x7f
}

pub const SCHED_OTHER: i32 = 0;
pub const SCHED_FIFO: i32 = 1;
pub const SCHED_RR: i32 = 2;
pub const SCHED_BATCH: i32 = 3;
pub const SCHED_IDLE: i32 = 5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_flags() {
        // runtime/os_linux.go: cloneFlags = 0x50f00 (CLONE_VM|FS|FILES|SIGHAND|SYSVSEM|THREAD)
        assert_eq!(GO_CLONE_FLAGS.bits(), 0x50f00);
        let a = CloneArgs {
            flags: 0x50f00 | 17,
            ..Default::default()
        };
        assert_eq!(a.exit_signal(), 17);
        assert_eq!(a.clone_flags(), GO_CLONE_FLAGS);
        assert_eq!(FUTEX_WAIT_PRIVATE, 128);
        assert_eq!(FUTEX_WAKE_PRIVATE, 129);
        assert_eq!(FUTEX_WAIT_PRIVATE & FUTEX_CMD_MASK, FUTEX_WAIT);
    }
}
