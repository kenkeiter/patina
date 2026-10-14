//! File objects and the fd table (design §8).
//!
//! * [`file`]: the `File` trait, open file descriptions, readiness hooks
//!   and the blocking protocol — the contract Phase 3's VFS, devices,
//!   sockets and control channel implement;
//! * [`fdtable`]: the per-process fd table;
//! * [`console`], [`pipe`], [`eventfd`], [`epoll`]: the Phase 2 files;
//! * [`vfs`]: the root filesystem (`patina_vfs::Fs` behind a global lock),
//!   special-node plumbing (`DevNode`, `register_chrdev`);
//! * [`vfile`]: files opened by path (regular, directory, `O_PATH`,
//!   device/FIFO wrapper, generated contents);
//! * [`namei`]: the path layer (dirfd, cwd, procfs magic links, `open`);
//! * [`rootfs`], [`devfs`], [`procfs`]: boot-time tree, `/dev`, `/proc`
//!   and `/sys`.
//!
//! The syscalls live in `syscall::fs` (fd operations) and `syscall::poll`
//! (epoll, ppoll).

pub mod console;
pub mod devfs;
pub mod epoll;
pub mod eventfd;
pub mod fdtable;
pub mod file;
pub mod namei;
pub mod pipe;
pub mod procfs;
pub mod rootfs;
pub mod vfile;
pub mod vfs;

pub use fdtable::FdTable;
