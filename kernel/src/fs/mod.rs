//! File objects and the fd table (design §8).
//!
//! * [`file`]: the `File` trait, open file descriptions, readiness hooks
//!   and the blocking protocol — the contract Phase 3's VFS, devices,
//!   sockets and control channel implement;
//! * [`fdtable`]: the per-process fd table;
//! * [`console`], [`pipe`], [`eventfd`], [`epoll`]: the Phase 2 files.
//!
//! The syscalls live in `syscall::fs` (fd operations) and `syscall::poll`
//! (epoll, ppoll).

pub mod console;
pub mod epoll;
pub mod eventfd;
pub mod fdtable;
pub mod file;
pub mod pipe;

pub use fdtable::FdTable;
